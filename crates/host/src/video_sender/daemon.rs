//! Real-hardware adapters and the daemon's start-up probe for the video sender.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use async_trait::async_trait;
use linux_quest_capture::{
    AutoCapture, CaptureBackendType, CaptureError, CaptureInfo, DisplayCapture, RawFrame,
    ScreenCastError,
};
use linux_quest_encoder::{
    create_encoder, probe_backends, BackendKind, BackendPreference, EncodedFrame, EncoderConfig,
    EncoderError, VideoEncoder,
};

use super::{
    spawn_video_sender, FrameSource, UdpSink, VideoSenderConfig, VideoSenderHandle,
    DEFAULT_SEND_BUFFER_BYTES,
};
use crate::codec_select::{plan_stream, StreamPlan, CLIENT_DECODE_CODECS, CODEC_PREFERENCE};
use crate::congestion::{RateCoordinator, StreamRate};
use crate::link_status::SharedLinkTracker;

/// Adapts the capture crate's [`DisplayCapture`] to [`FrameSource`].
pub struct CaptureFrameSource<C: DisplayCapture> {
    capture: C,
}

impl<C: DisplayCapture> CaptureFrameSource<C> {
    /// Initializes `capture`. Unless `allow_synthetic`, a capture that fell back to the
    /// synthetic test pattern is rejected: streaming a fake screen to a headset is never what
    /// the user asked for.
    pub async fn open(mut capture: C, allow_synthetic: bool) -> Result<Self, CaptureError> {
        capture.init().await?;
        if !allow_synthetic && capture.backend_type() == CaptureBackendType::Synthetic {
            let hint = if cfg!(feature = "pipewire-capture") {
                "PipeWire screencast capture was not usable (is the PipeWire socket in \
                 $XDG_RUNTIME_DIR and libpipewire-0.3 loadable?)"
            } else {
                "this build has no real capture; rebuild with --features pipewire-capture"
            };
            return Err(CaptureError::BackendUnavailable(format!(
                "only the synthetic test-pattern capture is available; {hint}"
            )));
        }
        Ok(Self { capture })
    }

    /// Size and rate the capture negotiated, if it reports them.
    pub fn capture_info(&self) -> Option<CaptureInfo> {
        self.capture.capture_info()
    }
}

/// Encoder size and rate for `info`: the capture's own resolution (so the whole screen is
/// encoded, not the top-left corner of it) with even dimensions, and its frame rate when known.
fn encode_geometry(info: Option<&CaptureInfo>, defaults: &EncoderConfig) -> (u32, u32, u32) {
    match info {
        Some(info) if info.width >= 2 && info.height >= 2 => (
            info.width & !1,
            info.height & !1,
            if info.fps > 0 { info.fps } else { defaults.fps },
        ),
        _ => (defaults.width, defaults.height, defaults.fps),
    }
}

/// True when the capture ended (session closed, node removed) and `init` can start it again.
fn capture_ended(e: &CaptureError) -> bool {
    matches!(
        e,
        CaptureError::ScreenCast(ScreenCastError::SessionClosed | ScreenCastError::NodeRemoved)
    )
}

/// True when restarting would only prompt the user again for something they refused.
fn user_refused(e: &CaptureError) -> bool {
    matches!(
        e,
        CaptureError::ScreenCast(ScreenCastError::UserCancelled | ScreenCastError::Denied(_))
    )
}

/// Longest wait between attempts to restart an ended capture.
const MAX_REOPEN_BACKOFF: std::time::Duration = std::time::Duration::from_secs(10);

#[async_trait]
impl<C: DisplayCapture + 'static> FrameSource for CaptureFrameSource<C> {
    async fn next_frame(&mut self) -> Result<RawFrame, CaptureError> {
        let err = match self.capture.capture_frame().await {
            Ok(frame) => return Ok(frame),
            Err(e) if capture_ended(&e) => e,
            Err(e) => return Err(e),
        };
        // GNOME closes the screen-cast session when the screen locks, or when the user stops
        // sharing. Start a new one (the stored restore token avoids a prompt) instead of
        // ending video for good.
        tracing::warn!(error = %err, "screen capture ended; restarting it");
        let mut backoff = std::time::Duration::from_millis(500);
        loop {
            tokio::time::sleep(backoff).await;
            match self.capture.init().await {
                Ok(()) => {
                    tracing::info!("screen capture restarted");
                    return self.capture.capture_frame().await;
                }
                Err(e) if user_refused(&e) => {
                    tracing::warn!(error = %e, "screen capture not restarted: the user declined");
                    return Err(e);
                }
                Err(e) => {
                    tracing::warn!(error = %e, retry_in = ?backoff, "screen capture restart failed");
                    backoff = (backoff * 2).min(MAX_REOPEN_BACKOFF);
                }
            }
        }
    }
}

/// Options the daemon passes from its command line.
#[derive(Debug, Clone, Copy)]
pub struct DaemonVideoOptions {
    /// UDP port on the headset to send video to.
    pub video_port: u16,
    /// Encoder backend preference (`--encoder` / `LINUX_QUEST_VIDEO_ENCODER`).
    pub encoder: BackendPreference,
    /// Stream the synthetic test pattern when no real screen capture is available
    /// (`--test-pattern`), to exercise encode, transport and decode end to end.
    pub test_pattern: bool,
    /// Video bitrate in Mbit/s (`--bitrate`); the encoder gets `bitrate_mbps * 1000` kbps.
    pub bitrate_mbps: u32,
}

/// Forwards [`VideoEncoder`] to a boxed encoder from [`create_encoder`].
struct BoxedEncoder(Box<dyn VideoEncoder>);

#[async_trait]
impl VideoEncoder for BoxedEncoder {
    async fn init(&mut self, config: EncoderConfig) -> Result<(), EncoderError> {
        self.0.init(config).await
    }

    async fn encode(&mut self, frame: &RawFrame) -> Result<EncodedFrame, EncoderError> {
        self.0.encode(frame).await
    }

    fn request_keyframe(&mut self) {
        self.0.request_keyframe();
    }

    fn invalidate_reference_picture(&mut self, pts_us: u64) {
        self.0.invalidate_reference_picture(pts_us);
    }

    fn set_bitrate(&mut self, kbps: u32) -> bool {
        self.0.set_bitrate(kbps)
    }
}

/// Opens the encoder the existing stream planner picks ([`plan_stream`]: the preference's
/// backend order, then the first codec that backend and the client both support). The mock
/// is used only when explicitly requested with `--encoder mock`: its output is not a
/// decodable bitstream.
async fn open_planned_encoder(
    preference: BackendPreference,
    bitrate_kbps: u32,
    (width, height, fps): (u32, u32, u32),
) -> Result<(BoxedEncoder, StreamPlan), String> {
    let probes = tokio::task::spawn_blocking(|| {
        CODEC_PREFERENCE
            .iter()
            .map(|&codec| (codec, probe_backends(codec)))
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| format!("encoder probing failed: {e}"))?;
    let plan = plan_stream(preference, CLIENT_DECODE_CODECS, |backend, codec| {
        probes.iter().any(|(c, results)| {
            *c == codec && results.iter().any(|p| p.kind == backend && p.available)
        })
    })
    .ok_or_else(|| {
        let reasons: Vec<String> = probes
            .iter()
            .flat_map(|(codec, results)| {
                results
                    .iter()
                    .filter(|p| p.kind != BackendKind::Mock && !p.available)
                    .map(move |p| format!("{} {:?}: {}", p.kind.name(), codec, p.reason))
            })
            .collect();
        format!("no usable hardware video encoder ({})", reasons.join("; "))
    })?;

    let config = EncoderConfig {
        codec: plan.codec,
        bitrate_kbps,
        width,
        height,
        fps,
        ..EncoderConfig::default()
    };
    let backend_only = match plan.backend {
        BackendKind::Nvenc => BackendPreference::Nvenc,
        BackendKind::Vaapi => BackendPreference::Vaapi,
        BackendKind::Mock => BackendPreference::Mock,
    };
    let (_, encoder) = create_encoder(config, backend_only)
        .await
        .map_err(|e| e.to_string())?;
    Ok((BoxedEncoder(encoder), plan))
}

/// Starts the UDP video sender for the headless daemon.
///
/// Returns `Err(reason)` as one human-readable line when it cannot run, so the caller logs a
/// single message and keeps the daemon up. Today that is the normal outcome on most machines:
/// it needs a real capture backend and a hardware encoder.
pub async fn start_daemon_video_sender(
    link: SharedLinkTracker,
    options: DaemonVideoOptions,
    keyframe_requests: Arc<AtomicBool>,
    rate: Option<Arc<RateCoordinator>>,
) -> Result<VideoSenderHandle, String> {
    let stream_rate = rate.as_ref().map(|r| r.register_stream(0));
    start_daemon_video_stream(
        link,
        options,
        0,
        0,
        options.bitrate_mbps.saturating_mul(1000),
        keyframe_requests,
        stream_rate,
    )
    .await
}

/// Starts independent capture/encode/send pipelines for the supplied active monitors. Each
/// pipeline owns its packetizer, FEC state, pacing state, keyframe flag and stream id.
/// Bitrate is divided evenly (remainder goes to lower ids) so the aggregate does not exceed
/// the configured session bitrate. With a `rate` coordinator each stream starts at the
/// coordinator's share and then follows its live, focus-weighted targets.
pub async fn start_daemon_video_senders(
    link: SharedLinkTracker,
    options: DaemonVideoOptions,
    monitors: &[linux_quest_protocol::DisplayInfo],
    rate: Option<Arc<RateCoordinator>>,
) -> Result<Vec<VideoSenderHandle>, String> {
    if monitors.is_empty() {
        return Err("no active monitors to stream".into());
    }
    if monitors.len() > usize::from(u16::MAX) {
        return Err(format!(
            "{} monitors exceed the protocol stream id limit",
            monitors.len()
        ));
    }
    let shares = bitrate_shares(options.bitrate_mbps, monitors.len())?;
    // Register every stream before starting any, so the first split already covers them all.
    let stream_rates: Vec<Option<Arc<StreamRate>>> = monitors
        .iter()
        .map(|m| rate.as_ref().map(|r| r.register_stream(m.display_id)))
        .collect();
    let mut handles = Vec::with_capacity(monitors.len());
    for ((monitor, bitrate), stream_rate) in monitors.iter().zip(shares).zip(stream_rates) {
        handles.push(
            start_daemon_video_stream(
                link.clone(),
                options,
                monitor.display_id,
                monitor.display_id,
                bitrate.saturating_mul(1000),
                Arc::new(AtomicBool::new(false)),
                stream_rate,
            )
            .await?,
        );
    }
    Ok(handles)
}

fn bitrate_shares(total_mbps: u32, streams: usize) -> Result<Vec<u32>, String> {
    if streams == 0 {
        return Err("no active monitors to stream".into());
    }
    let count = u32::try_from(streams).map_err(|_| "monitor count exceeds bitrate allocator")?;
    let base = total_mbps / count;
    let remainder = total_mbps % count;
    if base == 0 {
        return Err(format!(
            "{total_mbps} Mbit/s total bitrate cannot provide a positive share to {streams} monitor streams"
        ));
    }
    Ok((0..count)
        .map(|index| base + u32::from(index < remainder))
        .collect())
}

async fn start_daemon_video_stream(
    link: SharedLinkTracker,
    options: DaemonVideoOptions,
    display_id: u16,
    stream_id: u16,
    initial_bitrate_kbps: u32,
    keyframe_requests: Arc<AtomicBool>,
    rate: Option<Arc<StreamRate>>,
) -> Result<VideoSenderHandle, String> {
    let defaults = EncoderConfig::default();
    let capture = AutoCapture::new(display_id, defaults.width, defaults.height, defaults.fps);

    let source = CaptureFrameSource::open(capture, options.test_pattern)
        .await
        .map_err(|e| format!("capture: {e}"))?;
    let geometry = encode_geometry(source.capture_info().as_ref(), &defaults);
    // A coordinator's share (focus-weighted) replaces the even split.
    let bitrate_kbps = rate
        .as_ref()
        .map_or(initial_bitrate_kbps, |r| r.target_kbps());
    let (encoder, plan) = open_planned_encoder(options.encoder, bitrate_kbps, geometry)
        .await
        .map_err(|e| format!("encoder: {e}"))?;

    tracing::info!(
        backend = plan.backend.name(),
        codec = ?plan.codec,
        bitrate_kbps,
        stream_id,
        width = geometry.0,
        height = geometry.1,
        fps = geometry.2,
        "UDP video sender encoder chosen"
    );
    let sink = UdpSink::new(DEFAULT_SEND_BUFFER_BYTES, super::Dscp::Af41)
        .map_err(|e| format!("UDP socket: {e}"))?;
    let config = VideoSenderConfig {
        video_port: options.video_port,
        stream_id,
        keyframe_requests: Some(keyframe_requests),
        rate,
        ..VideoSenderConfig::default()
    };
    Ok(spawn_video_sender(source, encoder, sink, link, config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_quest_capture::SyntheticCapture;

    #[test]
    fn monitor_streams_receive_independent_bitrate_shares() {
        assert_eq!(bitrate_shares(30, 3).unwrap(), vec![10, 10, 10]);
        assert_eq!(bitrate_shares(32, 3).unwrap(), vec![11, 11, 10]);
        assert!(bitrate_shares(2, 3).unwrap_err().contains("positive share"));
    }

    #[tokio::test]
    async fn synthetic_capture_is_refused_unless_allowed() {
        let err = CaptureFrameSource::open(SyntheticCapture::new(0, 64, 64, 60), false)
            .await
            .err()
            .expect("synthetic capture must be rejected");
        assert!(matches!(err, CaptureError::BackendUnavailable(_)));
        #[cfg(not(feature = "pipewire-capture"))]
        assert!(err.to_string().contains("--features pipewire-capture"));

        let mut source = CaptureFrameSource::open(SyntheticCapture::new(0, 64, 64, 60), true)
            .await
            .expect("allowed explicitly");
        let frame = source.next_frame().await.unwrap();
        assert_eq!((frame.width, frame.height), (64, 64));
    }

    #[tokio::test]
    async fn daemon_start_reports_one_reason_and_does_not_panic() {
        let link =
            crate::link_status::new_shared_link_tracker(crate::link_status::DEFAULT_INPUT_LIVENESS);
        let result = start_daemon_video_sender(
            link,
            DaemonVideoOptions {
                video_port: 48440,
                encoder: BackendPreference::Auto,
                test_pattern: false,
                bitrate_mbps: 30,
            },
            Arc::new(AtomicBool::new(false)),
            None,
        )
        .await;
        // On a machine without a real capture backend and NVENC (CI, containers) this is an
        // Err naming the missing piece. On a real workstation it may legitimately start.
        match result {
            Err(reason) => assert!(
                reason.contains("capture") || reason.contains("encoder"),
                "reason should name what is missing: {reason}"
            ),
            Ok(handle) => handle.stop().await,
        }
    }

    #[test]
    fn encoder_geometry_follows_the_capture() {
        let defaults = EncoderConfig::default();
        let info = |width, height, fps| CaptureInfo {
            width,
            height,
            fps,
            format: linux_quest_capture::PixelFormat::Bgra8,
            dmabuf: false,
            modifier: None,
        };
        assert_eq!(
            encode_geometry(Some(&info(3840, 2160, 60)), &defaults),
            (3840, 2160, 60)
        );
        // Odd sizes are rounded down to even; an unknown rate keeps the default.
        assert_eq!(
            encode_geometry(Some(&info(1365, 767, 0)), &defaults),
            (1364, 766, defaults.fps)
        );
        assert_eq!(
            encode_geometry(None, &defaults),
            (defaults.width, defaults.height, defaults.fps)
        );
    }

    /// Ends after `frames_before_end` frames; `init` then succeeds or refuses.
    struct EndingCapture {
        frames_before_end: u32,
        frames: u32,
        inits: u32,
        refuse_restart: bool,
    }

    #[async_trait]
    impl DisplayCapture for EndingCapture {
        async fn init(&mut self) -> Result<(), CaptureError> {
            self.inits += 1;
            if self.inits > 1 && self.refuse_restart {
                return Err(ScreenCastError::UserCancelled.into());
            }
            self.frames = 0;
            Ok(())
        }
        async fn capture_frame(&mut self) -> Result<RawFrame, CaptureError> {
            if self.frames >= self.frames_before_end {
                return Err(ScreenCastError::SessionClosed.into());
            }
            self.frames += 1;
            SyntheticCapture::new(0, 16, 16, 1000).capture_frame().await
        }
        fn backend_type(&self) -> CaptureBackendType {
            CaptureBackendType::PipeWire
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_ended_capture_is_restarted() {
        let capture = EndingCapture {
            frames_before_end: 1,
            frames: 0,
            inits: 0,
            refuse_restart: false,
        };
        let mut source = CaptureFrameSource::open(capture, false).await.unwrap();
        assert!(source.next_frame().await.is_ok());
        // The session closes; the source restarts it and keeps delivering frames.
        assert!(source.next_frame().await.is_ok());
        assert_eq!(source.capture.inits, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_restart_is_not_retried() {
        let capture = EndingCapture {
            frames_before_end: 0,
            frames: 0,
            inits: 0,
            refuse_restart: true,
        };
        let mut source = CaptureFrameSource::open(capture, false).await.unwrap();
        let err = source.next_frame().await.unwrap_err();
        assert!(user_refused(&err), "{err}");
        assert_eq!(source.capture.inits, 2, "exactly one restart attempt");
    }
}

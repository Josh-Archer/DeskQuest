//! Real-hardware adapters and the daemon's start-up probe for the video sender.

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

use super::control::{CaptureKind, PausablePeers, ReselectRequest, VideoControl};
use super::{
    spawn_video_sender, FrameSource, KeyframeRequests, VideoSenderConfig, VideoSenderHandle,
    DEFAULT_SEND_BUFFER_BYTES,
};
use crate::codec_select::{plan_stream, StreamPlan, CLIENT_DECODE_CODECS, CODEC_PREFERENCE};
use crate::congestion::{RateCoordinator, StreamRate};
use crate::link_status::SharedLinkTracker;

/// Adapts the capture crate's [`DisplayCapture`] to [`FrameSource`].
pub struct CaptureFrameSource<C: DisplayCapture> {
    capture: C,
    /// Requests to let the user pick another screen, served between frames.
    reselect: Option<tokio::sync::mpsc::UnboundedReceiver<ReselectRequest>>,
    /// Told what the stream shows whenever that changes (the test pattern standing in for a
    /// lost screen, and the screen again).
    status: Option<Arc<VideoControl>>,
    reported: Option<CaptureKind>,
}

/// What a capture of `backend` shows.
fn capture_kind(backend: CaptureBackendType) -> CaptureKind {
    if backend == CaptureBackendType::Synthetic {
        CaptureKind::TestPattern
    } else {
        CaptureKind::Screen
    }
}

impl<C: DisplayCapture> CaptureFrameSource<C> {
    /// Initializes `capture`. Unless `allow_synthetic`, a capture that has only the synthetic
    /// test pattern is rejected: streaming a fake screen to a headset is never what the user
    /// asked for. The test pattern standing in for a real screen capture that is still
    /// starting, or waits to be restored or chosen, is accepted
    /// ([`DisplayCapture::is_standing_in`]): the screen replaces it once it is there.
    pub async fn open(mut capture: C, allow_synthetic: bool) -> Result<Self, CaptureError> {
        capture.init().await?;
        if !allow_synthetic
            && capture.backend_type() == CaptureBackendType::Synthetic
            && !capture.is_standing_in()
        {
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
        Ok(Self {
            capture,
            reselect: None,
            status: None,
            reported: None,
        })
    }

    /// Serves `requests` to pick another screen (see [`VideoControl::choose_screen`]).
    pub fn with_reselect(
        mut self,
        requests: tokio::sync::mpsc::UnboundedReceiver<ReselectRequest>,
    ) -> Self {
        self.reselect = Some(requests);
        self
    }

    /// Keeps `control`'s [`CaptureKind`] up to date as the capture switches between the screen
    /// and the test pattern.
    pub fn reporting_to(mut self, control: Arc<VideoControl>) -> Self {
        self.status = Some(control);
        self.report_kind();
        self
    }

    fn report_kind(&mut self) {
        let Some(control) = &self.status else {
            return;
        };
        let kind = capture_kind(self.capture.backend_type());
        if self.reported != Some(kind) {
            self.reported = Some(kind);
            control.set_capture_kind(kind);
        }
    }

    /// Runs one request to pick another screen and answers it.
    async fn serve_reselect(&mut self, request: ReselectRequest) {
        tracing::info!("showing the screen picker so the user can choose what to share");
        let result = self.capture.reselect().await.map_err(|e| match e {
            CaptureError::ScreenCast(ScreenCastError::UserCancelled)
                if self.capture.backend_type() == CaptureBackendType::Synthetic =>
            {
                "the screen picker was cancelled; still showing the test pattern".to_string()
            }
            CaptureError::ScreenCast(ScreenCastError::UserCancelled) => {
                "the screen picker was cancelled; still sharing the previous screen".to_string()
            }
            other => other.to_string(),
        });
        self.report_kind();
        match &result {
            Ok(()) => {
                tracing::info!(info = ?self.capture.capture_info(), "now sharing the chosen screen")
            }
            Err(e) => tracing::warn!(error = %e, "no new screen chosen"),
        }
        let _ = request.reply.send(result);
    }

    /// Size and rate the capture negotiated, if it reports them.
    pub fn capture_info(&self) -> Option<CaptureInfo> {
        self.capture.capture_info()
    }

    /// True when the user can pick what is captured: a real screen is shown, or the test
    /// pattern stands in for one.
    fn has_screen_picker(&self) -> bool {
        capture_kind(self.capture.backend_type()) == CaptureKind::Screen
            || self.capture.is_standing_in()
    }
}

/// Opens stream 0's `capture` (see [`CaptureFrameSource::open`]) and connects it to `control`:
/// it reports what it shows, and serves Choose Screen when it has a real screen, also while
/// the test pattern stands in for one (the screen is locked, or the portal has not answered).
async fn open_primary_source<C: DisplayCapture>(
    capture: C,
    allow_synthetic: bool,
    control: &Arc<VideoControl>,
) -> Result<CaptureFrameSource<C>, CaptureError> {
    let mut source = CaptureFrameSource::open(capture, allow_synthetic).await?;
    if source.has_screen_picker() {
        source = source.with_reselect(control.register_reselect());
    }
    if source.capture.is_standing_in() {
        tracing::info!(
            "video starts on the test pattern; the screen replaces it once the capture starts"
        );
    }
    control.set_running(1, capture_kind(source.capture.backend_type()));
    Ok(source.reporting_to(control.clone()))
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
        let captured = loop {
            let Some(requests) = self.reselect.as_mut() else {
                break self.capture.capture_frame().await;
            };
            // A static screen may send no frame for a long time, so a request must not wait
            // for one. Dropping the pending capture call is fine: frames wait in the capture's
            // own queue.
            tokio::select! {
                biased;
                request = requests.recv() => match request {
                    Some(request) => self.serve_reselect(request).await,
                    None => self.reselect = None,
                },
                frame = self.capture.capture_frame() => break frame,
            }
        };
        let err = match captured {
            Ok(frame) => {
                self.report_kind();
                return Ok(frame);
            }
            Err(e) if capture_ended(&e) => e,
            Err(e) => return Err(e),
        };
        // GNOME closes the screen-cast session when the screen locks, or when the user stops
        // sharing. Start a new one (the stored restore token avoids a prompt) instead of
        // ending video for good. The daemon's `AutoCapture` never gets here: it shows the
        // test pattern while it restores the capture itself.
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
pub(super) struct BoxedEncoder(Box<dyn VideoEncoder>);

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
pub(super) async fn open_planned_encoder(
    preference: BackendPreference,
    bitrate_kbps: u32,
    (width, height, fps): (u32, u32, u32),
) -> Result<(BoxedEncoder, StreamPlan, EncoderConfig), String> {
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
    let (_, encoder) = create_encoder(config.clone(), backend_only)
        .await
        .map_err(|e| e.to_string())?;
    Ok((BoxedEncoder(encoder), plan, config))
}

/// Starts the daemon video sender for the active UDP or negotiated USB tunnel transport.
///
/// Returns `Err(reason)` as one human-readable line when it cannot run, so the caller logs a
/// single message and keeps the daemon up. Today that is the normal outcome on most machines:
/// it needs a real capture backend and a hardware encoder.
pub async fn start_daemon_video_sender(
    link: SharedLinkTracker,
    options: DaemonVideoOptions,
    keyframe_requests: Arc<KeyframeRequests>,
    rate: Option<Arc<RateCoordinator>>,
    tunnel_video: Option<tokio::sync::broadcast::Sender<Vec<u8>>>,
) -> Result<VideoSenderHandle, String> {
    start_daemon_video_sender_with(
        link,
        options,
        keyframe_requests,
        rate,
        tunnel_video,
        VideoControl::new(),
    )
    .await
}

/// [`start_daemon_video_sender`] whose video `control` pauses, resumes and lets the user pick
/// another screen.
pub async fn start_daemon_video_sender_with(
    link: SharedLinkTracker,
    options: DaemonVideoOptions,
    keyframe_requests: Arc<KeyframeRequests>,
    rate: Option<Arc<RateCoordinator>>,
    tunnel_video: Option<tokio::sync::broadcast::Sender<Vec<u8>>>,
    control: Arc<VideoControl>,
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
        tunnel_video,
        control,
    )
    .await
}

/// Starts independent capture/encode/send pipelines for the supplied active monitors. Each
/// pipeline owns its packetizer, FEC state, pacing state and stream id, and takes its own
/// keyframe flag from `keyframe_requests`.
/// Bitrate is divided evenly (remainder goes to lower ids) so the aggregate does not exceed
/// the configured session bitrate. With a `rate` coordinator each stream starts at the
/// coordinator's share and then follows its live, focus-weighted targets.
pub async fn start_daemon_video_senders(
    link: SharedLinkTracker,
    options: DaemonVideoOptions,
    monitors: &[linux_quest_protocol::DisplayInfo],
    keyframe_requests: Arc<KeyframeRequests>,
    rate: Option<Arc<RateCoordinator>>,
    tunnel_video: Option<tokio::sync::broadcast::Sender<Vec<u8>>>,
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
                keyframe_requests.clone(),
                stream_rate,
                tunnel_video.clone(),
                VideoControl::new(),
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

#[allow(clippy::too_many_arguments)] // one pipeline gets link, capture, rate, keyframe, tunnel and control
async fn start_daemon_video_stream(
    link: SharedLinkTracker,
    options: DaemonVideoOptions,
    display_id: u16,
    stream_id: u16,
    initial_bitrate_kbps: u32,
    keyframe_requests: Arc<KeyframeRequests>,
    rate: Option<Arc<StreamRate>>,
    tunnel_video: Option<tokio::sync::broadcast::Sender<Vec<u8>>>,
    control: Arc<VideoControl>,
) -> Result<VideoSenderHandle, String> {
    let defaults = EncoderConfig::default();
    // Opening waits a few seconds at most: a capture the portal is slow to grant (the screen
    // is locked, or its share dialog waits for an answer) starts on the test pattern.
    let capture = AutoCapture::new(display_id, defaults.width, defaults.height, defaults.fps);
    let source = open_primary_source(capture, options.test_pattern, &control)
        .await
        .map_err(|e| format!("capture: {e}"))?;
    let geometry = encode_geometry(source.capture_info().as_ref(), &defaults);
    // A coordinator's share (focus-weighted) replaces the even split.
    let bitrate_kbps = rate
        .as_ref()
        .map_or(initial_bitrate_kbps, |r| r.target_kbps());
    let (encoder, plan, encoder_config) =
        open_planned_encoder(options.encoder, bitrate_kbps, geometry)
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
    let sink = super::HybridSink::new(tunnel_video, DEFAULT_SEND_BUFFER_BYTES, super::Dscp::Af41)
        .map_err(|e| format!("UDP socket: {e}"))?;
    let config = VideoSenderConfig {
        video_port: options.video_port,
        stream_id,
        keyframe_requests: Some(keyframe_requests),
        rate,
        encoder_config: Some(encoder_config),
        ..VideoSenderConfig::default()
    };
    let peers = PausablePeers {
        peers: link,
        control,
    };
    Ok(spawn_video_sender(source, encoder, sink, peers, config))
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
            KeyframeRequests::new(),
            None,
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

    /// Sends no frames (a static screen) until the user picks a screen, then frames of 1280x720.
    struct PickerCapture {
        picked: bool,
        refuse: bool,
    }

    #[async_trait]
    impl DisplayCapture for PickerCapture {
        async fn init(&mut self) -> Result<(), CaptureError> {
            Ok(())
        }
        async fn capture_frame(&mut self) -> Result<RawFrame, CaptureError> {
            if !self.picked {
                std::future::pending::<()>().await;
            }
            SyntheticCapture::new(0, 1280, 720, 1000)
                .capture_frame()
                .await
        }
        fn backend_type(&self) -> CaptureBackendType {
            CaptureBackendType::PipeWire
        }
        async fn reselect(&mut self) -> Result<(), CaptureError> {
            if self.refuse {
                return Err(ScreenCastError::UserCancelled.into());
            }
            self.picked = true;
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_screen_pick_is_served_while_the_screen_sends_no_frames() {
        let control = VideoControl::new();
        let capture = PickerCapture {
            picked: false,
            refuse: false,
        };
        let mut source = CaptureFrameSource::open(capture, false)
            .await
            .unwrap()
            .with_reselect(control.register_reselect());
        let next = tokio::spawn(async move {
            source
                .next_frame()
                .await
                .map(|frame| (frame.width, frame.height))
        });
        control
            .choose_screen(std::time::Duration::from_secs(5))
            .await
            .expect("picked");
        assert_eq!(next.await.unwrap().unwrap(), (1280, 720));
    }

    #[tokio::test]
    async fn a_cancelled_pick_says_the_old_screen_is_still_shared() {
        let control = VideoControl::new();
        let capture = PickerCapture {
            picked: false,
            refuse: true,
        };
        let mut source = CaptureFrameSource::open(capture, false)
            .await
            .unwrap()
            .with_reselect(control.register_reselect());
        let next = tokio::spawn(async move {
            let _ = source.next_frame().await;
        });
        let err = control
            .choose_screen(std::time::Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(
            err.contains("cancelled") && err.contains("previous"),
            "{err}"
        );
        next.abort();
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

    /// Frames of [`GnomeScreen`]; the test pattern standing in for it is `PATTERN`.
    const SCREEN: (u32, u32) = (64, 32);
    const PATTERN: (u32, u32) = (16, 16);
    /// How long the portal waits for its share dialog before the request fails.
    const PORTAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

    /// The desktop's lock screen, shared by [`GnomeScreen`] and (when the daemon can tell)
    /// the capture's lock probe.
    #[derive(Default)]
    struct LockScreen(std::sync::atomic::AtomicBool);

    impl LockScreen {
        fn set(&self, locked: bool) {
            self.0.store(locked, std::sync::atomic::Ordering::SeqCst);
        }
        fn locked(&self) -> bool {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl linux_quest_capture::ScreenLock for LockScreen {
        async fn is_locked(&self) -> Option<bool> {
            Some(self.locked())
        }
    }

    #[derive(Default)]
    struct Calls {
        inits: std::sync::atomic::AtomicU32,
        restores: std::sync::atomic::AtomicU32,
        reselects: std::sync::atomic::AtomicU32,
    }

    /// A portal screen capture as GNOME runs it: locking the screen closes the session, and
    /// a start or restore while locked gets no answer until the portal flow times out (the
    /// portal queues its share dialog behind the lock screen). Unlocked, the restore token
    /// works.
    struct GnomeScreen {
        lock: Arc<LockScreen>,
        calls: Arc<Calls>,
        /// The stored restore token is usable.
        has_token: bool,
        running: bool,
    }

    impl GnomeScreen {
        fn new(lock: Arc<LockScreen>, has_token: bool) -> (Self, Arc<Calls>) {
            let calls = Arc::new(Calls::default());
            let screen = Self {
                lock,
                calls: calls.clone(),
                has_token,
                running: false,
            };
            (screen, calls)
        }
    }

    #[async_trait]
    impl DisplayCapture for GnomeScreen {
        async fn init(&mut self) -> Result<(), CaptureError> {
            self.calls
                .inits
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.lock.locked() {
                tokio::time::sleep(PORTAL_TIMEOUT).await;
                return Err(ScreenCastError::NegotiationTimeout(PORTAL_TIMEOUT).into());
            }
            self.running = true;
            Ok(())
        }
        async fn capture_frame(&mut self) -> Result<RawFrame, CaptureError> {
            if self.lock.locked() {
                self.running = false;
            }
            if !self.running {
                return Err(ScreenCastError::SessionClosed.into());
            }
            SyntheticCapture::new(0, SCREEN.0, SCREEN.1, 1000)
                .capture_frame()
                .await
        }
        fn backend_type(&self) -> CaptureBackendType {
            CaptureBackendType::PipeWire
        }
        async fn restore(&mut self) -> Result<(), CaptureError> {
            self.calls
                .restores
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if !self.has_token {
                return Err(ScreenCastError::NoRestoreToken.into());
            }
            if self.lock.locked() {
                tokio::time::sleep(PORTAL_TIMEOUT).await;
                return Err(ScreenCastError::NegotiationTimeout(PORTAL_TIMEOUT).into());
            }
            self.running = true;
            Ok(())
        }
        async fn reselect(&mut self) -> Result<(), CaptureError> {
            self.calls
                .reselects
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.has_token = true;
            self.running = true;
            Ok(())
        }
        fn is_running(&self) -> bool {
            self.running
        }
    }

    fn frame_size(frame: &RawFrame) -> (u32, u32) {
        (frame.width, frame.height)
    }

    /// The daemon's stream 0 source over `screen`, reporting to `control`.
    async fn daemon_source(
        screen: GnomeScreen,
        lock: Arc<dyn linux_quest_capture::ScreenLock>,
        control: &Arc<VideoControl>,
    ) -> CaptureFrameSource<AutoCapture> {
        let capture =
            AutoCapture::with_capture(0, PATTERN.0, PATTERN.1, 10, Some(Box::new(screen)), lock);
        open_primary_source(capture, false, control)
            .await
            .expect("video starts")
    }

    /// Pulls frames until one of the screen arrives, checking that the others are the pattern.
    async fn until_screen(source: &mut CaptureFrameSource<AutoCapture>) {
        loop {
            let frame = source.next_frame().await.expect("frames keep coming");
            if frame_size(&frame) == SCREEN {
                return;
            }
            assert_eq!(frame_size(&frame), PATTERN);
        }
    }

    /// Pulls test pattern frames for `period` (paused clock).
    async fn pattern_for(
        source: &mut CaptureFrameSource<AutoCapture>,
        period: std::time::Duration,
    ) {
        let end = tokio::time::Instant::now() + period;
        while tokio::time::Instant::now() < end {
            let frame = source.next_frame().await.unwrap();
            assert_eq!(frame_size(&frame), PATTERN, "the test pattern stands in");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_screen_lost_to_the_lock_screen_comes_back_after_the_test_pattern() {
        // The daemon cannot tell that the screen is locked, so it keeps trying the restore
        // token while it is.
        let lock = Arc::new(LockScreen::default());
        let (screen, calls) = GnomeScreen::new(lock.clone(), true);
        let control = VideoControl::new();
        let mut source = daemon_source(
            screen,
            Arc::new(linux_quest_capture::UnknownScreenLock),
            &control,
        )
        .await;
        assert_eq!(frame_size(&source.next_frame().await.unwrap()), SCREEN);
        assert_eq!(control.capture_kind(), CaptureKind::Screen);

        lock.set(true);
        assert_eq!(frame_size(&source.next_frame().await.unwrap()), PATTERN);
        assert_eq!(control.capture_kind(), CaptureKind::TestPattern);
        assert!(control.can_choose_screen(), "the picker stays available");

        // Locked for 20 minutes: the pattern keeps playing, the restore keeps being retried.
        pattern_for(&mut source, std::time::Duration::from_secs(20 * 60)).await;
        let tried = calls.restores.load(std::sync::atomic::Ordering::SeqCst);
        assert!(tried >= 3, "retried while locked: {tried}");

        lock.set(false);
        let unlocked = tokio::time::Instant::now();
        until_screen(&mut source).await;
        // At worst a restore started just before the unlock times out first, and the next
        // one waits the longest backoff.
        assert!(unlocked.elapsed() <= std::time::Duration::from_secs(331));
        assert_eq!(control.capture_kind(), CaptureKind::Screen);
        assert_eq!(frame_size(&source.next_frame().await.unwrap()), SCREEN);
    }

    #[tokio::test(start_paused = true)]
    async fn the_screen_is_restored_right_after_the_unlock_when_the_lock_is_known() {
        let lock = Arc::new(LockScreen::default());
        let (screen, calls) = GnomeScreen::new(lock.clone(), true);
        let control = VideoControl::new();
        let mut source = daemon_source(screen, lock.clone(), &control).await;
        assert_eq!(frame_size(&source.next_frame().await.unwrap()), SCREEN);

        lock.set(true);
        pattern_for(&mut source, std::time::Duration::from_secs(20 * 60)).await;
        assert_eq!(
            calls.restores.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no portal request while locked"
        );

        lock.set(false);
        let unlocked = tokio::time::Instant::now();
        until_screen(&mut source).await;
        assert!(unlocked.elapsed() <= std::time::Duration::from_secs(5));
        assert_eq!(calls.restores.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(control.capture_kind(), CaptureKind::Screen);
    }

    #[tokio::test(start_paused = true)]
    async fn choose_screen_on_the_test_pattern_shows_the_picker_and_switches_back() {
        // Nothing to restore silently: the pattern stays until the user picks a screen.
        let lock = Arc::new(LockScreen::default());
        let (screen, calls) = GnomeScreen::new(lock.clone(), false);
        let control = VideoControl::new();
        let mut source = daemon_source(screen, lock.clone(), &control).await;
        assert_eq!(frame_size(&source.next_frame().await.unwrap()), SCREEN);
        lock.set(true);
        assert_eq!(frame_size(&source.next_frame().await.unwrap()), PATTERN);
        lock.set(false);
        pattern_for(&mut source, std::time::Duration::from_secs(60)).await;
        assert_eq!(calls.restores.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(control.capture_kind(), CaptureKind::TestPattern);

        let stream = tokio::spawn(async move {
            until_screen(&mut source).await;
            source
        });
        control
            .choose_screen(std::time::Duration::from_secs(5))
            .await
            .expect("the picked screen is shared");
        let mut source = stream.await.unwrap();
        assert_eq!(calls.reselects.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(control.capture_kind(), CaptureKind::Screen);
        assert_eq!(frame_size(&source.next_frame().await.unwrap()), SCREEN);
    }

    #[tokio::test]
    async fn only_the_test_pattern_is_refused_as_the_daemon_s_screen() {
        let control = VideoControl::new();
        let capture = AutoCapture::with_capture(
            0,
            PATTERN.0,
            PATTERN.1,
            10,
            None,
            Arc::new(linux_quest_capture::UnknownScreenLock),
        );
        let err = open_primary_source(capture, false, &control)
            .await
            .err()
            .expect("no real capture behind the pattern");
        assert!(matches!(err, CaptureError::BackendUnavailable(_)));
        assert!(!control.can_choose_screen());
    }

    /// The daemon's start on 2026-10-24 (the screen was locked, and the daemon could not tell):
    /// the portal did not answer the restore token, and after its 300 s timeout the daemon
    /// sent no video at all until it was restarted by hand.
    #[tokio::test(start_paused = true)]
    async fn video_starts_on_the_test_pattern_when_the_portal_does_not_answer_at_start_up() {
        let lock = Arc::new(LockScreen::default());
        lock.set(true);
        let (screen, calls) = GnomeScreen::new(lock.clone(), true);
        let control = VideoControl::new();
        let started = tokio::time::Instant::now();
        let mut source = daemon_source(
            screen,
            Arc::new(linux_quest_capture::UnknownScreenLock),
            &control,
        )
        .await;
        assert!(
            started.elapsed() <= std::time::Duration::from_secs(5),
            "start-up does not wait for the portal: {:?}",
            started.elapsed()
        );
        assert_eq!(control.capture_kind(), CaptureKind::TestPattern);
        assert!(control.can_choose_screen(), "the picker is available");
        assert_eq!(frame_size(&source.next_frame().await.unwrap()), PATTERN);

        // Locked for 20 minutes: the pattern keeps playing, the restore keeps being retried.
        pattern_for(&mut source, std::time::Duration::from_secs(20 * 60)).await;
        assert_eq!(calls.inits.load(std::sync::atomic::Ordering::SeqCst), 1);
        let tried = calls.restores.load(std::sync::atomic::Ordering::SeqCst);
        assert!(tried >= 2, "retried with the restore token: {tried}");

        lock.set(false);
        let unlocked = tokio::time::Instant::now();
        until_screen(&mut source).await;
        // At worst an attempt started just before the unlock times out first, and the next
        // one waits the longest backoff.
        assert!(unlocked.elapsed() <= std::time::Duration::from_secs(331));
        assert_eq!(control.capture_kind(), CaptureKind::Screen);
        assert_eq!(frame_size(&source.next_frame().await.unwrap()), SCREEN);
    }

    #[tokio::test(start_paused = true)]
    async fn a_screen_locked_at_start_up_is_captured_right_after_the_unlock() {
        let lock = Arc::new(LockScreen::default());
        lock.set(true);
        let (screen, calls) = GnomeScreen::new(lock.clone(), true);
        let control = VideoControl::new();
        let mut source = daemon_source(screen, lock.clone(), &control).await;
        assert_eq!(control.capture_kind(), CaptureKind::TestPattern);
        pattern_for(&mut source, std::time::Duration::from_secs(20 * 60)).await;
        assert_eq!(
            calls.inits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no portal request while locked"
        );

        lock.set(false);
        let unlocked = tokio::time::Instant::now();
        until_screen(&mut source).await;
        assert!(unlocked.elapsed() <= std::time::Duration::from_secs(5));
        assert_eq!(calls.inits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(calls.restores.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(control.capture_kind(), CaptureKind::Screen);
    }

    #[tokio::test(start_paused = true)]
    async fn choose_screen_works_while_the_first_capture_is_still_starting() {
        let lock = Arc::new(LockScreen::default());
        lock.set(true);
        let (screen, calls) = GnomeScreen::new(lock.clone(), true);
        let control = VideoControl::new();
        let mut source = daemon_source(
            screen,
            Arc::new(linux_quest_capture::UnknownScreenLock),
            &control,
        )
        .await;
        pattern_for(&mut source, std::time::Duration::from_secs(30)).await;
        lock.set(false);

        // The start sent at start-up still waits for the portal; the pick replaces it.
        let stream = tokio::spawn(async move {
            until_screen(&mut source).await;
            source
        });
        control
            .choose_screen(std::time::Duration::from_secs(5))
            .await
            .expect("the picked screen is shared");
        let mut source = stream.await.unwrap();
        assert_eq!(calls.reselects.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(control.capture_kind(), CaptureKind::Screen);
        assert_eq!(frame_size(&source.next_frame().await.unwrap()), SCREEN);
    }
}

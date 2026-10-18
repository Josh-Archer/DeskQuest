//! Real-hardware adapters and the daemon's start-up probe for the video sender.

use async_trait::async_trait;
use linux_quest_capture::{
    AutoCapture, CaptureBackendType, CaptureError, DisplayCapture, RawFrame,
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
            return Err(CaptureError::BackendUnavailable(
                "only the synthetic test-pattern capture is available; PipeWire screencast \
                 capture does not yet negotiate a stream node"
                    .into(),
            ));
        }
        Ok(Self { capture })
    }
}

#[async_trait]
impl<C: DisplayCapture + 'static> FrameSource for CaptureFrameSource<C> {
    async fn next_frame(&mut self) -> Result<RawFrame, CaptureError> {
        self.capture.capture_frame().await
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
}

/// Opens the encoder the existing stream planner picks ([`plan_stream`]: the preference's
/// backend order, then the first codec that backend and the client both support). The mock
/// is used only when explicitly requested with `--encoder mock`: its output is not a
/// decodable bitstream.
async fn open_planned_encoder(
    preference: BackendPreference,
    bitrate_kbps: u32,
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
) -> Result<VideoSenderHandle, String> {
    let defaults = EncoderConfig::default();
    let capture = AutoCapture::new(0, defaults.width, defaults.height, defaults.fps);

    let source = CaptureFrameSource::open(capture, options.test_pattern).await;
    let bitrate_kbps = options.bitrate_mbps.saturating_mul(1000);
    let encoder = open_planned_encoder(options.encoder, bitrate_kbps).await;

    let (source, (encoder, plan)) = match (source, encoder) {
        (Ok(source), Ok(encoder)) => (source, encoder),
        (source, encoder) => {
            let mut reasons = Vec::new();
            if let Err(e) = source {
                reasons.push(format!("capture: {e}"));
            }
            if let Err(e) = encoder {
                reasons.push(format!("encoder: {e}"));
            }
            return Err(reasons.join("; "));
        }
    };

    tracing::info!(
        backend = plan.backend.name(),
        codec = ?plan.codec,
        bitrate_mbps = options.bitrate_mbps,
        "UDP video sender encoder chosen"
    );
    let sink = UdpSink::new(DEFAULT_SEND_BUFFER_BYTES, super::Dscp::Af41)
        .map_err(|e| format!("UDP socket: {e}"))?;
    let config = VideoSenderConfig {
        video_port: options.video_port,
        ..VideoSenderConfig::default()
    };
    Ok(spawn_video_sender(source, encoder, sink, link, config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_quest_capture::SyntheticCapture;

    #[tokio::test]
    async fn synthetic_capture_is_refused_unless_allowed() {
        let err = CaptureFrameSource::open(SyntheticCapture::new(0, 64, 64, 60), false)
            .await
            .err()
            .expect("synthetic capture must be rejected");
        assert!(matches!(err, CaptureError::BackendUnavailable(_)));

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
}

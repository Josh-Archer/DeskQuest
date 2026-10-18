//! Real-hardware adapters and the daemon's start-up probe for the video sender.

use async_trait::async_trait;
use linux_quest_capture::{
    AutoCapture, CaptureBackendType, CaptureError, DisplayCapture, RawFrame,
};
use linux_quest_encoder::{EncoderConfig, NvencEncoder, VideoEncoder};

use super::{
    spawn_video_sender, FrameSource, UdpSink, VideoSenderConfig, VideoSenderHandle,
    DEFAULT_SEND_BUFFER_BYTES,
};
use crate::advertise::ADVERTISED_CODECS;
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
}

/// Opens the first hardware encoder that works for the codecs the host advertises, in
/// advertised order. There is no software fallback and the mock encoder is never used here: its
/// output is not a decodable bitstream.
async fn open_hardware_encoder() -> Result<(NvencEncoder, EncoderConfig), String> {
    if !NvencEncoder::is_available() {
        return Err(
            "no hardware video encoder (NVENC library not found; VA-API is not implemented)".into(),
        );
    }
    let mut errors = Vec::new();
    for codec in ADVERTISED_CODECS {
        let config = EncoderConfig {
            codec,
            ..EncoderConfig::default()
        };
        let mut encoder = NvencEncoder::new(config.clone());
        match encoder.init(config.clone()).await {
            Ok(()) => return Ok((encoder, config)),
            Err(e) => errors.push(format!("{codec:?}: {e}")),
        }
    }
    Err(format!("NVENC could not start ({})", errors.join("; ")))
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

    let source = CaptureFrameSource::open(capture, false).await;
    let encoder = open_hardware_encoder().await;

    let (source, (encoder, _config)) = match (source, encoder) {
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
        let result =
            start_daemon_video_sender(link, DaemonVideoOptions { video_port: 48440 }).await;
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

//! Several video streams, one per headset screen, from one daemon.
//!
//! Each stream is an ordinary [`VideoSenderHandle`]: its own capture, encoder, packetizer (and
//! so its own frame ids and FEC blocks) and UDP socket. They share only what must be shared:
//!
//! * the link tracker, so the headset address learned from the input and control channels feeds
//!   every sender, and each one pauses and resumes with the link on its own;
//! * the [`KeyframeRequests`] flags, one per stream, so a client request for stream 1 forces an
//!   IDR on stream 1 only;
//! * the destination, the headset's UDP port (48440 by default). Streams are told apart by
//!   `PacketHeader::stream_id`, which equals the display id; stream 0 is the primary capture.
//!
//! Stream 0 is started by the unchanged single-stream path ([`start_daemon_video_sender`]).
//! Streams 1.. are synthetic test patterns for now, one tint per display, until virtual
//! monitors provide real captures for them.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use linux_quest_capture::SyntheticCapture;
use tracing::{info, warn};

use super::daemon::{
    open_planned_encoder, start_daemon_video_sender, CaptureFrameSource, DaemonVideoOptions,
};
use super::{
    spawn_video_sender, Dscp, HybridSink, KeyframeRequests, VideoSenderConfig, VideoSenderHandle,
    VideoSenderStats, DEFAULT_SEND_BUFFER_BYTES,
};
use crate::congestion::{RateCoordinator, StreamRate};
use crate::link_status::SharedLinkTracker;

/// Most video streams the daemon sends: the headset shows at most three screens.
pub const MAX_VIDEO_STREAMS: u16 = 3;

/// Size and frame rate of a synthetic stream, written `WxH@FPS` on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamSize {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl StreamSize {
    /// Smallest and largest width or height accepted. Hardware encoders refuse tiny pictures,
    /// and 8192 is the largest picture they take.
    pub const MIN_DIMENSION: u32 = 128;
    pub const MAX_DIMENSION: u32 = 8192;
    pub const MAX_FPS: u32 = 240;
}

impl Default for StreamSize {
    /// What `--extra-stream-size` defaults to: `1920x1080@60`.
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 60,
        }
    }
}

impl fmt::Display for StreamSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}@{}", self.width, self.height, self.fps)
    }
}

impl FromStr for StreamSize {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, String> {
        let bad = |why: &str| {
            format!("invalid stream size {value:?}: {why} (expected WxH@FPS, such as 1920x1080@60)")
        };
        let (size, fps) = value.split_once('@').ok_or_else(|| bad("missing @FPS"))?;
        let (width, height) = size
            .split_once(['x', 'X'])
            .ok_or_else(|| bad("missing the x between width and height"))?;
        let number = |text: &str, what: &str| -> Result<u32, String> {
            text.trim()
                .parse::<u32>()
                .map_err(|_| bad(&format!("{what} {text:?} is not a whole number")))
        };
        let size = Self {
            width: number(width, "width")?,
            height: number(height, "height")?,
            fps: number(fps, "FPS")?,
        };
        for (what, dimension) in [("width", size.width), ("height", size.height)] {
            if !(Self::MIN_DIMENSION..=Self::MAX_DIMENSION).contains(&dimension) {
                return Err(bad(&format!(
                    "{what} must be {} to {}",
                    Self::MIN_DIMENSION,
                    Self::MAX_DIMENSION
                )));
            }
            if dimension % 2 != 0 {
                return Err(bad(&format!("{what} must be even")));
            }
        }
        if !(1..=Self::MAX_FPS).contains(&size.fps) {
            return Err(bad(&format!("FPS must be 1 to {}", Self::MAX_FPS)));
        }
        Ok(size)
    }
}

/// Parses `--streams`: a whole number from 1 to [`MAX_VIDEO_STREAMS`].
pub fn parse_stream_count(value: &str) -> Result<u16, String> {
    let count: u16 = value.trim().parse().map_err(|_| {
        format!(
            "invalid stream count {value:?}: expected a whole number from 1 to {MAX_VIDEO_STREAMS}"
        )
    })?;
    if (1..=MAX_VIDEO_STREAMS).contains(&count) {
        Ok(count)
    } else {
        Err(format!(
            "invalid stream count {count}: the daemon sends 1 to {MAX_VIDEO_STREAMS} video streams"
        ))
    }
}

/// What [`start_daemon_video_senders`] starts.
#[derive(Debug, Clone, Copy)]
pub struct MultiStreamOptions {
    /// Options of stream 0 (and the port, encoder preference and bitrate every stream uses).
    pub video: DaemonVideoOptions,
    /// Number of streams, 1 to [`MAX_VIDEO_STREAMS`].
    pub streams: u16,
    /// Size and rate of the synthetic streams 1..
    pub extra_stream: StreamSize,
}

/// The running senders of all streams. Dropping it does not stop them; call
/// [`MultiVideoSenderHandle::stop`].
pub struct MultiVideoSenderHandle {
    /// `(stream_id, sender)`, stream 0 first.
    streams: Vec<(u16, VideoSenderHandle)>,
}

impl MultiVideoSenderHandle {
    /// Wraps already running senders; the first one is the primary stream.
    pub fn from_streams(streams: Vec<(u16, VideoSenderHandle)>) -> Self {
        assert!(!streams.is_empty(), "at least the primary stream");
        Self { streams }
    }

    /// Number of streams that are running.
    pub fn stream_count(&self) -> usize {
        self.streams.len()
    }

    /// Ids of the running streams, in start order.
    pub fn stream_ids(&self) -> Vec<u16> {
        self.streams.iter().map(|(id, _)| *id).collect()
    }

    /// Counters of stream `stream_id`.
    pub fn stats_for(&self, stream_id: u16) -> Option<&Arc<VideoSenderStats>> {
        self.streams
            .iter()
            .find(|(id, _)| *id == stream_id)
            .map(|(_, handle)| handle.stats())
    }

    /// The headset UDP port every stream is sent to.
    pub fn video_port(&self) -> u16 {
        self.streams[0].1.video_port()
    }

    /// Stops every stream's capture and sender and waits for them.
    pub async fn stop(self) {
        for (_, handle) in self.streams {
            handle.stop().await;
        }
    }
}

/// Starts the daemon's video streams: stream 0 exactly as a single-stream daemon does
/// ([`start_daemon_video_sender`]), then `options.streams - 1` synthetic test-pattern streams.
///
/// Returns `Err(reason)` only when stream 0 cannot start, as the single-stream path does. An
/// extra stream that cannot start (for example the GPU is out of encoder sessions) is logged and
/// skipped, so the desktop keeps streaming.
pub async fn start_daemon_video_senders(
    link: SharedLinkTracker,
    options: MultiStreamOptions,
    keyframe_requests: Arc<KeyframeRequests>,
    rate: Option<Arc<RateCoordinator>>,
    tunnel_video: Option<tokio::sync::broadcast::Sender<Vec<u8>>>,
) -> Result<MultiVideoSenderHandle, String> {
    if !(1..=MAX_VIDEO_STREAMS).contains(&options.streams) {
        return Err(format!(
            "{} video streams requested; the daemon sends 1 to {MAX_VIDEO_STREAMS}",
            options.streams
        ));
    }
    let primary = start_daemon_video_sender(
        link.clone(),
        options.video,
        keyframe_requests.clone(),
        rate.clone(),
        tunnel_video.clone(),
    )
    .await?;
    let mut streams = vec![(0, primary)];

    for stream_id in 1..options.streams {
        match start_synthetic_stream(
            link.clone(),
            &options.video,
            stream_id,
            options.extra_stream,
            keyframe_requests.clone(),
            rate.as_ref().map(|r| r.register_stream(stream_id)),
            tunnel_video.clone(),
        )
        .await
        {
            Ok(handle) => streams.push((stream_id, handle)),
            Err(reason) => warn!(
                "Extra video stream {stream_id} not started ({reason}); continuing without it"
            ),
        }
    }
    if options.streams > 1 {
        info!(
            requested = options.streams,
            running = streams.len(),
            size = %options.extra_stream,
            "Extra synthetic video streams started"
        );
    }
    Ok(MultiVideoSenderHandle::from_streams(streams))
}

/// One extra stream: a synthetic test pattern tinted for display `stream_id`, its own encoder
/// session and its own socket.
async fn start_synthetic_stream(
    link: SharedLinkTracker,
    options: &DaemonVideoOptions,
    stream_id: u16,
    size: StreamSize,
    keyframe_requests: Arc<KeyframeRequests>,
    rate: Option<Arc<StreamRate>>,
    tunnel_video: Option<tokio::sync::broadcast::Sender<Vec<u8>>>,
) -> Result<VideoSenderHandle, String> {
    let capture = SyntheticCapture::new(stream_id, size.width, size.height, size.fps);
    let source = CaptureFrameSource::open(capture, true)
        .await
        .map_err(|e| format!("capture: {e}"))?;
    let bitrate_kbps = rate.as_ref().map_or_else(
        || options.bitrate_mbps.saturating_mul(1000),
        |r| r.target_kbps(),
    );
    let (encoder, plan) = open_planned_encoder(
        options.encoder,
        bitrate_kbps,
        (size.width, size.height, size.fps),
    )
    .await
    .map_err(|e| format!("encoder: {e}"))?;
    info!(
        stream_id,
        backend = plan.backend.name(),
        codec = ?plan.codec,
        bitrate_mbps = options.bitrate_mbps,
        width = size.width,
        height = size.height,
        fps = size.fps,
        "UDP video sender encoder chosen for extra synthetic stream"
    );
    let sink = HybridSink::new(tunnel_video, DEFAULT_SEND_BUFFER_BYTES, Dscp::Af41)
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
    use std::time::Duration;

    use linux_quest_encoder::BackendPreference;

    use crate::link_status::{new_shared_link_tracker, DEFAULT_INPUT_LIVENESS};

    #[test]
    fn stream_size_parses_width_height_and_fps() {
        assert_eq!(
            "1920x1080@60".parse::<StreamSize>().unwrap(),
            StreamSize::default()
        );
        assert_eq!(
            "1280X720@30".parse::<StreamSize>().unwrap(),
            StreamSize {
                width: 1280,
                height: 720,
                fps: 30
            }
        );
        assert_eq!(StreamSize::default().to_string(), "1920x1080@60");
        assert_eq!("3840x2160@120".parse::<StreamSize>().unwrap().fps, 120);
    }

    #[test]
    fn stream_size_rejects_malformed_and_unusable_values() {
        for bad in [
            "",
            "1920x1080",
            "1920@60",
            "x1080@60",
            "1920x@60",
            "1920x1080@",
            "1920x1080@sixty",
            "1920,1080@60",
            "-1920x1080@60",
            "1920x1080@0",
            "1920x1080@241",
            "1921x1080@60",
            "1920x1081@60",
            "64x64@60",
            "16384x1080@60",
            "0x0@0",
        ] {
            let err = bad.parse::<StreamSize>().expect_err(bad);
            assert!(
                err.contains("WxH@FPS"),
                "the error shows the expected form for {bad:?}: {err}"
            );
        }
    }

    #[test]
    fn stream_count_is_one_to_three() {
        assert_eq!(parse_stream_count("1"), Ok(1));
        assert_eq!(parse_stream_count("3"), Ok(3));
        for bad in ["0", "4", "99", "-1", "two", "", "1.5"] {
            let err = parse_stream_count(bad).expect_err(bad);
            assert!(err.contains("1 to 3"), "{bad:?}: {err}");
        }
    }

    /// The real start-up path with the software mock encoder: every stream gets its own capture,
    /// pipeline and counters. No headset is known, so frames are captured and then skipped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_requested_stream_starts_with_its_own_pipeline() {
        let link = new_shared_link_tracker(DEFAULT_INPUT_LIVENESS);
        let options = MultiStreamOptions {
            video: DaemonVideoOptions {
                video_port: 48440,
                encoder: BackendPreference::Mock,
                test_pattern: true,
                bitrate_mbps: 30,
            },
            streams: 3,
            // Small and slow so the test stays cheap in debug builds.
            extra_stream: StreamSize {
                width: 128,
                height: 128,
                fps: 30,
            },
        };
        let handle = start_daemon_video_senders(link, options, KeyframeRequests::new(), None, None)
            .await
            .expect("mock encoder and synthetic capture need no hardware");
        assert_eq!(handle.stream_count(), 3);
        assert_eq!(handle.stream_ids(), vec![0, 1, 2]);
        assert_eq!(handle.video_port(), 48440);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let captured: Vec<u64> = (0..3)
                .map(|id| VideoSenderStats::get(&handle.stats_for(id).unwrap().frames_captured))
                .collect();
            if captured.iter().all(|&n| n >= 2) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "every stream captures frames: {captured:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(handle.stats_for(3).is_none());
        handle.stop().await;
    }

    #[tokio::test]
    async fn one_stream_is_just_the_primary_stream() {
        let link = new_shared_link_tracker(DEFAULT_INPUT_LIVENESS);
        let options = MultiStreamOptions {
            video: DaemonVideoOptions {
                video_port: 48440,
                encoder: BackendPreference::Mock,
                test_pattern: true,
                bitrate_mbps: 30,
            },
            streams: 1,
            extra_stream: StreamSize::default(),
        };
        let handle =
            start_daemon_video_senders(link.clone(), options, KeyframeRequests::new(), None, None)
                .await
                .unwrap();
        assert_eq!(handle.stream_ids(), vec![0]);
        handle.stop().await;

        let too_many = MultiStreamOptions {
            streams: 4,
            ..options
        };
        let err = start_daemon_video_senders(link, too_many, KeyframeRequests::new(), None, None)
            .await
            .err()
            .expect("four streams are refused");
        assert!(err.contains("1 to 3"), "{err}");
    }
}

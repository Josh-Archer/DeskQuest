pub mod audio;
#[cfg(feature = "pipewire-audio")]
pub mod audio_pipewire;
mod auto;
pub mod dmabuf;
pub mod pipewire;
pub mod screen_lock;
pub mod screencast;

use async_trait::async_trait;
use bytes::Bytes;
use thiserror::Error;

pub use audio::{AudioSampleFormat, AudioSource, AutoAudioSource, RawAudioFrame, SineAudioSource};
#[cfg(feature = "pipewire-audio")]
pub use audio_pipewire::PipeWireAudioSource;
pub use auto::AutoCapture;
pub use dmabuf::DmaBufPlane;
pub use pipewire::PipeWireCapture;
pub use screen_lock::{session_screen_lock, ScreenLock, UnknownScreenLock};
pub use screencast::{CaptureStats, ScreenCastConfig, ScreenCastError};

#[derive(Error, Debug)]
pub enum CaptureError {
    #[error("Initialization failed: {0}")]
    InitFailed(String),

    #[error("Capture failed: {0}")]
    CaptureFailed(String),

    #[error("Display not found: {0}")]
    DisplayNotFound(u16),

    #[error("Backend unavailable: {0}")]
    BackendUnavailable(String),

    /// Portal / PipeWire screen capture could not start (typed, e.g. user cancelled the
    /// picker) or ended (session closed, node removed). After this error from
    /// `capture_frame`, `init` may be called again to restart the capture.
    #[error("Screen capture: {0}")]
    ScreenCast(#[from] ScreenCastError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Rgba8,
    Bgra8,
    Nv12,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureBackendType {
    PipeWire,
    DmaBuf,
    X11Shm,
    Synthetic,
}

/// The parameters of the stream a capture backend is actually delivering.
///
/// For real screen capture these come from the negotiated PipeWire format, not from the
/// hints passed to the constructor, so a video sender should read them (after `init`) to
/// configure the encoder, and again whenever the value changes (the compositor can
/// re-negotiate on a mode change; every `RawFrame` also carries its own `width`/`height`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureInfo {
    pub width: u32,
    pub height: u32,
    /// Negotiated (or maximum, for variable-rate sources) frames per second, rounded.
    pub fps: u32,
    /// Layout of the delivered frames.
    pub format: PixelFormat,
    /// True when frames arrive as DMA-BUFs (`RawFrame::dma_buf`), false for CPU memory.
    pub dmabuf: bool,
    /// DRM format modifier of the DMA-BUFs, when `dmabuf`.
    pub modifier: Option<u64>,
}

/// A captured raw frame prior to video encoding.
/// If `dma_buf` is `Some`, the frame is held in GPU VRAM (strictly zero-copy).
/// If `dma_buf` is `None`, `data` contains system RAM pixels (software fallback/synthetic).
#[derive(Debug)]
pub struct RawFrame {
    pub display_id: u16,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: PixelFormat,
    pub pts_us: u64,
    pub dma_buf: Option<Vec<DmaBufPlane>>,
    pub data: Bytes,
}

impl RawFrame {
    pub fn is_zero_copy(&self) -> bool {
        match &self.dma_buf {
            Some(planes) if !planes.is_empty() => planes.iter().all(|p| p.is_valid()),
            _ => false,
        }
    }
}

#[async_trait]
pub trait DisplayCapture: Send + Sync {
    async fn init(&mut self) -> Result<(), CaptureError>;
    async fn capture_frame(&mut self) -> Result<RawFrame, CaptureError>;
    fn backend_type(&self) -> CaptureBackendType;

    /// Size, rate and layout of the frames this capture delivers, once known (after a
    /// successful `init`). Backends that cannot tell return `None`.
    fn capture_info(&self) -> Option<CaptureInfo> {
        None
    }

    /// Lets the user pick another screen or window to capture (the screen-cast portal shows
    /// its picker again). On success the capture continues with the new source; on failure
    /// (cancelled, refused) it continues with the old one where it can. Backends without a
    /// picker return `BackendUnavailable`.
    async fn reselect(&mut self) -> Result<(), CaptureError> {
        Err(CaptureError::BackendUnavailable(
            "this capture source has no screen picker".into(),
        ))
    }

    /// Starts the capture again without asking the user anything: only what the user already
    /// granted is captured again (for the screen-cast portal, through the stored restore
    /// token; no picker is shown). Used to bring back a capture that ended. The default is
    /// [`init`](Self::init), for backends that never ask.
    async fn restore(&mut self) -> Result<(), CaptureError> {
        self.init().await
    }

    /// False when the capture cannot deliver frames until it is started again: it ended, or
    /// a failed [`reselect`](Self::reselect) could not get the previous source back.
    fn is_running(&self) -> bool {
        true
    }
}

/// Deterministic synthetic screen generator for integration tests and headless benchmarking.
///
/// Every display gets its own look, so several streams can be told apart on the headset: the
/// colour cycles through a hue chosen by the display id, and displays other than 0 also carry a
/// white bar sweeping across the picture, which makes motion and latency visible even when the
/// colour changes slowly. Display 0 is the plain cycling colour, as it always was.
pub struct SyntheticCapture {
    display_id: u16,
    width: u32,
    height: u32,
    fps: u32,
    frame_counter: u64,
    /// When the next frame is due; frames are paced to `fps` like a real capture.
    next_due: Option<tokio::time::Instant>,
}

impl SyntheticCapture {
    pub fn new(display_id: u16, width: u32, height: u32, fps: u32) -> Self {
        Self {
            display_id,
            width,
            height,
            fps,
            frame_counter: 0,
            next_due: None,
        }
    }
}

#[async_trait]
impl DisplayCapture for SyntheticCapture {
    async fn init(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }

    async fn capture_frame(&mut self) -> Result<RawFrame, CaptureError> {
        // Pace to `fps`. Without an await this returned instantly in a loop, pinning a tokio
        // worker at 100% and starving tasks queued on it (the video sender never ran).
        let interval = std::time::Duration::from_micros(1_000_000 / (self.fps as u64).max(1));
        let now = tokio::time::Instant::now();
        let due = self.next_due.unwrap_or(now);
        if due > now {
            tokio::time::sleep_until(due).await;
        } else {
            // Running late: still give other tasks on this worker a turn.
            tokio::task::yield_now().await;
        }
        // A late caller is not owed a burst of catch-up frames.
        self.next_due = Some(due.max(now) + interval);

        self.frame_counter += 1;
        let stride = self.width * 4;
        let frame_size = (stride * self.height) as usize;

        // Solid colour that cycles every 256 frames (BGRA). `repeat` is a block copy; a
        // per-byte loop over a 1080p frame cost more than a frame interval in debug builds.
        let offset = (self.frame_counter % 256) as u8;
        let mut buffer = synthetic_pixel(self.display_id, offset).repeat(frame_size / 4);
        if self.display_id != 0 {
            draw_sweep_bar(&mut buffer, self.width, self.height, self.frame_counter);
        }

        let interval_us = 1_000_000 / (self.fps as u64).max(1);
        let pts_us = self.frame_counter * interval_us;

        Ok(RawFrame {
            display_id: self.display_id,
            width: self.width,
            height: self.height,
            stride,
            format: PixelFormat::Bgra8,
            pts_us,
            dma_buf: None,
            data: Bytes::from(buffer),
        })
    }

    fn backend_type(&self) -> CaptureBackendType {
        CaptureBackendType::Synthetic
    }

    fn capture_info(&self) -> Option<CaptureInfo> {
        Some(CaptureInfo {
            width: self.width,
            height: self.height,
            fps: self.fps,
            format: PixelFormat::Bgra8,
            dmabuf: false,
            modifier: None,
        })
    }
}

/// One BGRA pixel of the cycling background for `display_id` at colour phase `offset`.
///
/// Display 0 keeps the original blue-to-red cycle with green fixed at 128. Display 1 holds blue
/// at 128 and cycles green against red; display 2 holds red at 128 and cycles blue against
/// green. At any phase the three pixels differ.
fn synthetic_pixel(display_id: u16, offset: u8) -> [u8; 4] {
    let (rising, falling, fixed) = (offset, 255 - offset, 128);
    match display_id % 3 {
        0 => [falling, fixed, rising, 255],
        1 => [fixed, rising, falling, 255],
        _ => [rising, falling, fixed, 255],
    }
}

/// Frames a sweep bar takes to cross the picture.
const SWEEP_PERIOD_FRAMES: u64 = 120;

/// Paints a white bar one sixteenth of the width wide, at a position that advances with
/// `frame_counter` and wraps every [`SWEEP_PERIOD_FRAMES`] frames (2 s at 60 fps).
fn draw_sweep_bar(buffer: &mut [u8], width: u32, height: u32, frame_counter: u64) {
    let bar_width = (width / 16).max(1) as usize;
    let travel = (width as usize).saturating_sub(bar_width);
    let x = travel * (frame_counter % SWEEP_PERIOD_FRAMES) as usize / SWEEP_PERIOD_FRAMES as usize;
    let stride = width as usize * 4;
    for row in 0..height as usize {
        let start = row * stride + x * 4;
        buffer[start..start + bar_width * 4].fill(255);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn synthetic_capture_is_paced_to_fps() {
        let mut capture = SyntheticCapture::new(0, 16, 16, 100);
        capture.init().await.unwrap();
        let start = std::time::Instant::now();
        for _ in 0..5 {
            capture.capture_frame().await.unwrap();
        }
        // First frame is immediate, the next four wait ~10 ms each.
        let elapsed = start.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(40),
            "{elapsed:?}"
        );
        assert!(elapsed < std::time::Duration::from_secs(2), "{elapsed:?}");
    }

    #[tokio::test]
    async fn test_synthetic_capture_frame() {
        let mut capture = SyntheticCapture::new(0, 1920, 1080, 60);
        capture
            .init()
            .await
            .expect("Failed to init synthetic capture");

        let frame1 = capture
            .capture_frame()
            .await
            .expect("Failed to capture frame 1");
        assert_eq!(frame1.width, 1920);
        assert_eq!(frame1.height, 1080);
        assert_eq!(frame1.stride, 1920 * 4);
        assert_eq!(frame1.format, PixelFormat::Bgra8);
        assert_eq!(frame1.data.len(), 1920 * 1080 * 4);
        assert!(!frame1.is_zero_copy());

        let frame2 = capture
            .capture_frame()
            .await
            .expect("Failed to capture frame 2");
        assert!(frame2.pts_us > frame1.pts_us);
    }

    /// Capture one frame of display `id` after `frames_before` earlier ones.
    async fn nth_frame(id: u16, width: u32, height: u32, frames_before: usize) -> RawFrame {
        let mut capture = SyntheticCapture::new(id, width, height, 1000);
        capture.init().await.unwrap();
        for _ in 0..frames_before {
            capture.capture_frame().await.unwrap();
        }
        capture.capture_frame().await.unwrap()
    }

    #[tokio::test]
    async fn synthetic_displays_are_tinted_differently() {
        let frames = [
            nth_frame(0, 64, 32, 0).await,
            nth_frame(1, 64, 32, 0).await,
            nth_frame(2, 64, 32, 0).await,
        ];
        for (i, frame) in frames.iter().enumerate() {
            assert_eq!(frame.display_id, i as u16, "frames carry the display id");
            assert_eq!(frame.data.len(), 64 * 32 * 4);
        }
        // Compare a pixel far from the sweep bar, which starts at the left edge.
        let corner = |f: &RawFrame| {
            let at = (31 * 64 + 48) * 4;
            f.data[at..at + 4].to_vec()
        };
        let (c0, c1, c2) = (corner(&frames[0]), corner(&frames[1]), corner(&frames[2]));
        assert_ne!(c0, c1);
        assert_ne!(c0, c2);
        assert_ne!(c1, c2);
    }

    #[tokio::test]
    async fn display_zero_is_the_unchanged_cycling_colour() {
        // Frame n has offset n % 256; display 0 stays BGRA = (255 - offset, 128, offset, 255)
        // over the whole picture, with no bar.
        let frame = nth_frame(0, 64, 32, 4).await;
        let offset = 5u8;
        assert_eq!(
            frame.data[..],
            [255 - offset, 128, offset, 255].repeat(64 * 32)[..]
        );
    }

    #[tokio::test]
    async fn extra_displays_carry_a_bar_that_moves_each_frame() {
        let first = nth_frame(1, 320, 64, 0).await;
        let second = nth_frame(1, 320, 64, 30).await;
        // The bar is white; the cycling background never has all three channels at 255.
        let white_columns = |f: &RawFrame| -> Vec<usize> {
            (0..320usize)
                .filter(|x| f.data[x * 4..x * 4 + 4] == [255, 255, 255, 255])
                .collect()
        };
        let (a, b) = (white_columns(&first), white_columns(&second));
        assert_eq!(a.len(), 320 / 16, "bar is a sixteenth of the width");
        assert_eq!(b.len(), 320 / 16);
        assert!(b[0] > a[0], "the bar moves right: {a:?} -> {b:?}");
        // Every row carries the bar, so it is a vertical stripe.
        let last_row = 63 * 320 * 4;
        assert_eq!(
            first.data[last_row + a[0] * 4..last_row + a[0] * 4 + 4],
            [255, 255, 255, 255]
        );
    }

    #[tokio::test]
    async fn synthetic_bar_stays_inside_tiny_pictures() {
        for width in [2u32, 6, 17, 100] {
            let frame = nth_frame(2, width, 4, 7).await;
            assert_eq!(frame.data.len(), (width * 4 * 4) as usize);
        }
    }

    #[test]
    fn test_dmabuf_plane_creation() {
        let plane = DmaBufPlane::create_test_memfd(7680, 0, 0);
        assert!(plane.is_valid());
        assert_eq!(plane.stride, 7680);
        assert_eq!(plane.offset, 0);
        assert_eq!(plane.modifier, 0);
        assert!(plane.raw_fd() >= 0);

        let cloned = plane.try_clone().expect("try_clone failed");
        assert!(cloned.is_valid());
        assert_ne!(plane.raw_fd(), cloned.raw_fd());
    }

    #[test]
    fn test_raw_frame_zero_copy_check() {
        let plane = DmaBufPlane::create_test_memfd(7680, 0, 0);
        let frame = RawFrame {
            display_id: 0,
            width: 1920,
            height: 1080,
            stride: 7680,
            format: PixelFormat::Bgra8,
            pts_us: 1000,
            dma_buf: Some(vec![plane]),
            data: Bytes::new(),
        };
        assert!(frame.is_zero_copy());

        let frame_empty = RawFrame {
            display_id: 0,
            width: 1920,
            height: 1080,
            stride: 7680,
            format: PixelFormat::Bgra8,
            pts_us: 1000,
            dma_buf: Some(vec![]),
            data: Bytes::new(),
        };
        assert!(!frame_empty.is_zero_copy());
    }

    #[tokio::test]
    async fn test_auto_capture_initialization() {
        let mut auto = AutoCapture::new(0, 1280, 720, 60);
        auto.init().await.expect("Auto capture init failed");
        let frame = auto.capture_frame().await.expect("Capture frame failed");
        assert_eq!(frame.width, 1280);
        assert_eq!(frame.height, 720);
        let info = auto.capture_info().expect("capture info after init");
        assert_eq!((info.width, info.height), (1280, 720));
        assert_eq!(info.fps, 60);
        assert!(!info.dmabuf);
    }

    #[tokio::test]
    async fn test_audio_source_sine_stream() {
        let mut source = SineAudioSource::new_default();
        source.init().await.expect("Sine source init failed");
        assert_eq!(source.sample_rate(), 48000);
        assert_eq!(source.channels(), 2);
        assert_eq!(source.format(), AudioSampleFormat::F32Le);

        let chunk1 = source
            .capture_chunk()
            .await
            .expect("Chunk 1 capture failed");
        assert_eq!(chunk1.samples_per_channel, 120);
        assert_eq!(chunk1.pts_us, 0);

        let chunk2 = source
            .capture_chunk()
            .await
            .expect("Chunk 2 capture failed");
        assert_eq!(chunk2.samples_per_channel, 120);
        assert_eq!(chunk2.pts_us, 2500); // 2.5ms later
    }
}

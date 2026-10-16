pub mod audio;
#[cfg(feature = "pipewire-audio")]
pub mod audio_pipewire;
pub mod dmabuf;
pub mod pipewire;

use async_trait::async_trait;
use bytes::Bytes;
use thiserror::Error;

pub use audio::{AudioSampleFormat, AudioSource, AutoAudioSource, RawAudioFrame, SineAudioSource};
#[cfg(feature = "pipewire-audio")]
pub use audio_pipewire::PipeWireAudioSource;
pub use dmabuf::DmaBufPlane;
pub use pipewire::PipeWireCapture;

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
}

/// Deterministic synthetic screen generator for integration tests and headless benchmarking.
pub struct SyntheticCapture {
    display_id: u16,
    width: u32,
    height: u32,
    fps: u32,
    frame_counter: u64,
}

impl SyntheticCapture {
    pub fn new(display_id: u16, width: u32, height: u32, fps: u32) -> Self {
        Self {
            display_id,
            width,
            height,
            fps,
            frame_counter: 0,
        }
    }
}

#[async_trait]
impl DisplayCapture for SyntheticCapture {
    async fn init(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }

    async fn capture_frame(&mut self) -> Result<RawFrame, CaptureError> {
        self.frame_counter += 1;
        let stride = self.width * 4;
        let frame_size = (stride * self.height) as usize;

        // Generate lightweight animated gradient
        let mut buffer = vec![0u8; frame_size];
        let offset = (self.frame_counter % 256) as u8;
        for i in (0..frame_size).step_by(4) {
            buffer[i] = 255 - offset; // B
            buffer[i + 1] = 128; // G
            buffer[i + 2] = offset; // R
            buffer[i + 3] = 255; // A
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
}

/// Dynamically discovers and initializes the best available capture engine.
pub struct AutoCapture {
    display_id: u16,
    width: u32,
    height: u32,
    fps: u32,
    inner: Box<dyn DisplayCapture>,
}

impl AutoCapture {
    pub fn new(display_id: u16, width: u32, height: u32, fps: u32) -> Self {
        let inner: Box<dyn DisplayCapture> = if PipeWireCapture::is_available() {
            Box::new(PipeWireCapture::new(display_id, width, height, fps))
        } else {
            Box::new(SyntheticCapture::new(display_id, width, height, fps))
        };
        Self {
            display_id,
            width,
            height,
            fps,
            inner,
        }
    }
}

#[async_trait]
impl DisplayCapture for AutoCapture {
    async fn init(&mut self) -> Result<(), CaptureError> {
        match self.inner.init().await {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::warn!(
                    "Primary capture backend failed to initialize: {e}. Gracefully demoting to SyntheticCapture"
                );
                let mut fallback =
                    SyntheticCapture::new(self.display_id, self.width, self.height, self.fps);
                fallback.init().await?;
                self.inner = Box::new(fallback);
                Ok(())
            }
        }
    }

    async fn capture_frame(&mut self) -> Result<RawFrame, CaptureError> {
        self.inner.capture_frame().await
    }

    fn backend_type(&self) -> CaptureBackendType {
        self.inner.backend_type()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

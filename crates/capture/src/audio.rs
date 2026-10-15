use async_trait::async_trait;
use bytes::{BufMut, Bytes, BytesMut};

use crate::CaptureError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSampleFormat {
    F32Le,
    I16Le,
}

/// A captured raw audio chunk prior to audio encoding.
#[derive(Debug, Clone, PartialEq)]
pub struct RawAudioFrame {
    pub sample_rate: u32,
    pub channels: u16,
    pub format: AudioSampleFormat,
    pub samples_per_channel: u32,
    pub pts_us: u64,
    pub data: Bytes,
}

impl RawAudioFrame {
    pub fn new(
        sample_rate: u32,
        channels: u16,
        format: AudioSampleFormat,
        samples_per_channel: u32,
        pts_us: u64,
        data: Bytes,
    ) -> Self {
        Self {
            sample_rate,
            channels,
            format,
            samples_per_channel,
            pts_us,
            data,
        }
    }

    /// Total samples across all channels in this audio frame.
    pub fn total_samples(&self) -> usize {
        (self.samples_per_channel as usize) * (self.channels as usize)
    }

    /// Frame duration in microseconds.
    pub fn duration_us(&self) -> u64 {
        if self.sample_rate == 0 {
            0
        } else {
            (self.samples_per_channel as u64 * 1_000_000) / self.sample_rate as u64
        }
    }

    /// Construct a frame from an interleaved slice of f32 samples.
    pub fn from_f32_slice(
        sample_rate: u32,
        channels: u16,
        samples_per_channel: u32,
        pts_us: u64,
        samples: &[f32],
    ) -> Self {
        let mut buf = BytesMut::with_capacity(samples.len() * 4);
        for &s in samples {
            buf.put_f32_le(s);
        }
        Self {
            sample_rate,
            channels,
            format: AudioSampleFormat::F32Le,
            samples_per_channel,
            pts_us,
            data: buf.freeze(),
        }
    }

    /// Construct a frame from an interleaved slice of i16 samples.
    pub fn from_i16_slice(
        sample_rate: u32,
        channels: u16,
        samples_per_channel: u32,
        pts_us: u64,
        samples: &[i16],
    ) -> Self {
        let mut buf = BytesMut::with_capacity(samples.len() * 2);
        for &s in samples {
            buf.put_i16_le(s);
        }
        Self {
            sample_rate,
            channels,
            format: AudioSampleFormat::I16Le,
            samples_per_channel,
            pts_us,
            data: buf.freeze(),
        }
    }

    /// Returns samples converted to an interleaved f32 vector in [-1.0, 1.0].
    pub fn to_f32_vec(&self) -> Result<Vec<f32>, CaptureError> {
        let expected_samples = self.total_samples();
        match self.format {
            AudioSampleFormat::F32Le => {
                let bytes_needed = expected_samples * 4;
                if self.data.len() < bytes_needed {
                    return Err(CaptureError::CaptureFailed(format!(
                        "Audio buffer underflow: expected {bytes_needed} bytes, got {}",
                        self.data.len()
                    )));
                }
                let mut out = Vec::with_capacity(expected_samples);
                let (chunks, _) = self.data[..bytes_needed].as_chunks::<4>();
                for chunk in chunks {
                    let val = f32::from_le_bytes(*chunk);
                    out.push(val);
                }
                Ok(out)
            }
            AudioSampleFormat::I16Le => {
                let bytes_needed = expected_samples * 2;
                if self.data.len() < bytes_needed {
                    return Err(CaptureError::CaptureFailed(format!(
                        "Audio buffer underflow: expected {bytes_needed} bytes, got {}",
                        self.data.len()
                    )));
                }
                let mut out = Vec::with_capacity(expected_samples);
                let (chunks, _) = self.data[..bytes_needed].as_chunks::<2>();
                for chunk in chunks {
                    let val = i16::from_le_bytes(*chunk);
                    out.push((val as f32) / 32768.0);
                }
                Ok(out)
            }
        }
    }

    /// Returns samples converted to an interleaved i16 vector in [-32768, 32767].
    pub fn to_i16_vec(&self) -> Result<Vec<i16>, CaptureError> {
        let expected_samples = self.total_samples();
        match self.format {
            AudioSampleFormat::I16Le => {
                let bytes_needed = expected_samples * 2;
                if self.data.len() < bytes_needed {
                    return Err(CaptureError::CaptureFailed(format!(
                        "Audio buffer underflow: expected {bytes_needed} bytes, got {}",
                        self.data.len()
                    )));
                }
                let mut out = Vec::with_capacity(expected_samples);
                let (chunks, _) = self.data[..bytes_needed].as_chunks::<2>();
                for chunk in chunks {
                    let val = i16::from_le_bytes(*chunk);
                    out.push(val);
                }
                Ok(out)
            }
            AudioSampleFormat::F32Le => {
                let bytes_needed = expected_samples * 4;
                if self.data.len() < bytes_needed {
                    return Err(CaptureError::CaptureFailed(format!(
                        "Audio buffer underflow: expected {bytes_needed} bytes, got {}",
                        self.data.len()
                    )));
                }
                let mut out = Vec::with_capacity(expected_samples);
                let (chunks, _) = self.data[..bytes_needed].as_chunks::<4>();
                for chunk in chunks {
                    let val = f32::from_le_bytes(*chunk);
                    let clamped = val.clamp(-1.0, 1.0);
                    let i16_val = if clamped >= 0.0 {
                        (clamped * 32767.0) as i16
                    } else {
                        (clamped * 32768.0) as i16
                    };
                    out.push(i16_val);
                }
                Ok(out)
            }
        }
    }
}

/// Abstract audio source providing discrete PCM audio chunks.
#[async_trait]
pub trait AudioSource: Send + Sync {
    async fn init(&mut self) -> Result<(), CaptureError>;
    async fn capture_chunk(&mut self) -> Result<RawAudioFrame, CaptureError>;
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u16;
    fn format(&self) -> AudioSampleFormat;
}

/// Deterministic sine wave audio generator for testing, benchmarking and offline simulation.
pub struct SineAudioSource {
    sample_rate: u32,
    channels: u16,
    format: AudioSampleFormat,
    frequency_hz: f32,
    samples_per_frame: u32,
    phase: f32,
    frame_counter: u64,
    initialized: bool,
}

impl SineAudioSource {
    pub fn new(
        sample_rate: u32,
        channels: u16,
        format: AudioSampleFormat,
        samples_per_frame: u32,
    ) -> Self {
        Self {
            sample_rate,
            channels,
            format,
            frequency_hz: 440.0,
            samples_per_frame,
            phase: 0.0,
            frame_counter: 0,
            initialized: false,
        }
    }

    /// Creates a 48kHz stereo 2.5ms (120 samples/channel) f32 sine generator.
    pub fn new_default() -> Self {
        Self::new(48000, 2, AudioSampleFormat::F32Le, 120)
    }

    pub fn with_frequency(mut self, freq: f32) -> Self {
        self.frequency_hz = freq;
        self
    }

    pub fn with_format(mut self, format: AudioSampleFormat) -> Self {
        self.format = format;
        self
    }

    pub fn with_frame_size(mut self, samples_per_frame: u32) -> Self {
        self.samples_per_frame = samples_per_frame;
        self
    }

    pub fn frame_counter(&self) -> u64 {
        self.frame_counter
    }
}

#[async_trait]
impl AudioSource for SineAudioSource {
    async fn init(&mut self) -> Result<(), CaptureError> {
        self.phase = 0.0;
        self.frame_counter = 0;
        self.initialized = true;
        Ok(())
    }

    async fn capture_chunk(&mut self) -> Result<RawAudioFrame, CaptureError> {
        if !self.initialized {
            self.init().await?;
        }

        self.frame_counter += 1;
        let pts_us = (self.frame_counter - 1)
            * (self.samples_per_frame as u64 * 1_000_000 / self.sample_rate as u64);

        let phase_step = 2.0 * std::f32::consts::PI * self.frequency_hz / (self.sample_rate as f32);

        let mut samples =
            Vec::with_capacity((self.samples_per_frame * self.channels as u32) as usize);
        for _ in 0..self.samples_per_frame {
            let sample_val = self.phase.sin() * 0.8; // Amplitude 0.8 to avoid clipping
            self.phase += phase_step;
            if self.phase >= 2.0 * std::f32::consts::PI {
                self.phase -= 2.0 * std::f32::consts::PI;
            }

            for _ in 0..self.channels {
                samples.push(sample_val);
            }
        }

        match self.format {
            AudioSampleFormat::F32Le => Ok(RawAudioFrame::from_f32_slice(
                self.sample_rate,
                self.channels,
                self.samples_per_frame,
                pts_us,
                &samples,
            )),
            AudioSampleFormat::I16Le => {
                let i16_samples: Vec<i16> = samples
                    .into_iter()
                    .map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
                    .collect();
                Ok(RawAudioFrame::from_i16_slice(
                    self.sample_rate,
                    self.channels,
                    self.samples_per_frame,
                    pts_us,
                    &i16_samples,
                ))
            }
        }
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn format(&self) -> AudioSampleFormat {
        self.format
    }
}

/// Auto-discovering audio source that tries hardware PipeWire sink monitor capture
/// and gracefully demotes to synthetic sine generator if unavailable.
pub struct AutoAudioSource {
    sample_rate: u32,
    channels: u16,
    format: AudioSampleFormat,
    samples_per_frame: u32,
    inner: Box<dyn AudioSource>,
}

impl AutoAudioSource {
    pub fn new(
        sample_rate: u32,
        channels: u16,
        format: AudioSampleFormat,
        samples_per_frame: u32,
    ) -> Self {
        #[cfg(feature = "pipewire-audio")]
        let inner: Box<dyn AudioSource> =
            if crate::audio_pipewire::PipeWireAudioSource::is_available() {
                Box::new(crate::audio_pipewire::PipeWireAudioSource::new(
                    sample_rate,
                    channels,
                    format,
                    samples_per_frame,
                ))
            } else {
                Box::new(SineAudioSource::new(
                    sample_rate,
                    channels,
                    format,
                    samples_per_frame,
                ))
            };

        #[cfg(not(feature = "pipewire-audio"))]
        let inner: Box<dyn AudioSource> = Box::new(SineAudioSource::new(
            sample_rate,
            channels,
            format,
            samples_per_frame,
        ));

        Self {
            sample_rate,
            channels,
            format,
            samples_per_frame,
            inner,
        }
    }
}

#[async_trait]
impl AudioSource for AutoAudioSource {
    async fn init(&mut self) -> Result<(), CaptureError> {
        match self.inner.init().await {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::warn!(
                    "Primary audio capture backend failed: {e}. Gracefully demoting to SineAudioSource"
                );
                let mut fallback = SineAudioSource::new(
                    self.sample_rate,
                    self.channels,
                    self.format,
                    self.samples_per_frame,
                );
                fallback.init().await?;
                self.inner = Box::new(fallback);
                Ok(())
            }
        }
    }

    async fn capture_chunk(&mut self) -> Result<RawAudioFrame, CaptureError> {
        self.inner.capture_chunk().await
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn format(&self) -> AudioSampleFormat {
        self.format
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_sine_audio_source_f32_2_5ms() {
        let mut source = SineAudioSource::new_default(); // 48kHz, 2 channels, F32Le, 120 samples
        source.init().await.unwrap();

        let chunk = source.capture_chunk().await.unwrap();
        assert_eq!(chunk.sample_rate, 48000);
        assert_eq!(chunk.channels, 2);
        assert_eq!(chunk.samples_per_channel, 120);
        assert_eq!(chunk.duration_us(), 2500); // 2.5ms
        assert_eq!(chunk.format, AudioSampleFormat::F32Le);
        assert_eq!(chunk.total_samples(), 240);
        assert_eq!(chunk.data.len(), 240 * 4);

        let f32_vec = chunk.to_f32_vec().unwrap();
        assert_eq!(f32_vec.len(), 240);
        // Verify non-zero sine wave generated
        assert!(f32_vec.iter().any(|&s| s.abs() > 0.01));
    }

    #[tokio::test]
    async fn test_sine_audio_source_i16_5ms() {
        let mut source = SineAudioSource::new(48000, 2, AudioSampleFormat::I16Le, 240); // 5ms
        source.init().await.unwrap();

        let chunk = source.capture_chunk().await.unwrap();
        assert_eq!(chunk.sample_rate, 48000);
        assert_eq!(chunk.channels, 2);
        assert_eq!(chunk.samples_per_channel, 240);
        assert_eq!(chunk.duration_us(), 5000); // 5ms
        assert_eq!(chunk.format, AudioSampleFormat::I16Le);
        assert_eq!(chunk.total_samples(), 480);
        assert_eq!(chunk.data.len(), 480 * 2);

        let i16_vec = chunk.to_i16_vec().unwrap();
        assert_eq!(i16_vec.len(), 480);
        assert!(i16_vec.iter().any(|&s| s.abs() > 100));
    }

    #[test]
    fn test_format_conversion_roundtrip() {
        let samples = vec![0.0f32, 0.5, -0.5, 0.8, -0.8];
        let frame = RawAudioFrame::from_f32_slice(48000, 1, 5, 1000, &samples);
        let i16_converted = frame.to_i16_vec().unwrap();
        assert_eq!(i16_converted[0], 0);
        assert!((i16_converted[1] - 16383).abs() <= 2);
        assert!((i16_converted[2] - -16384).abs() <= 2);

        let frame_i16 = RawAudioFrame::from_i16_slice(48000, 1, 5, 1000, &i16_converted);
        let f32_back = frame_i16.to_f32_vec().unwrap();
        for (orig, back) in samples.iter().zip(f32_back.iter()) {
            assert!((orig - back).abs() < 0.001);
        }
    }

    #[tokio::test]
    async fn test_auto_audio_source_init() {
        let mut auto = AutoAudioSource::new(48000, 2, AudioSampleFormat::F32Le, 120);
        auto.init().await.unwrap();
        let chunk = auto.capture_chunk().await.unwrap();
        assert_eq!(chunk.sample_rate, 48000);
        assert_eq!(chunk.channels, 2);
        assert_eq!(chunk.samples_per_channel, 120);
    }
}

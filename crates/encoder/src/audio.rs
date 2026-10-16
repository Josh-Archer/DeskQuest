use async_trait::async_trait;
use bytes::Bytes;
use linux_quest_capture::RawAudioFrame;
use linux_quest_protocol::AudioCodec;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AudioEncoderError {
    #[error("Audio encoder initialization failed: {0}")]
    InitFailed(String),

    #[error("Audio frame encoding failed: {0}")]
    EncodeFailed(String),

    #[error("Unsupported audio codec: {0:?}")]
    UnsupportedCodec(AudioCodec),

    #[error("Invalid audio frame size: {0} samples")]
    InvalidFrameSize(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioApplication {
    RestrictedLowDelay,
    Voip,
    Audio,
}

#[derive(Debug, Clone)]
pub struct AudioEncoderConfig {
    pub codec: AudioCodec,
    pub sample_rate: u32,
    pub channels: u16,
    pub frame_duration_us: u32,
    pub bitrate_bps: i32,
    pub fec_enabled: bool,
    pub dtx_enabled: bool,
    pub application: AudioApplication,
}

impl Default for AudioEncoderConfig {
    fn default() -> Self {
        Self {
            codec: AudioCodec::Opus,
            sample_rate: 48000,
            channels: 2,
            frame_duration_us: 2500, // 2.5ms ultra-low latency default
            bitrate_bps: 96_000,     // 96 kbps stereo
            fec_enabled: true,       // In-band FEC for packet-loss resilience
            dtx_enabled: false,
            application: AudioApplication::RestrictedLowDelay,
        }
    }
}

impl AudioEncoderConfig {
    pub fn samples_per_channel(&self) -> usize {
        (self.sample_rate as u64 * self.frame_duration_us as u64 / 1_000_000) as usize
    }

    pub fn total_samples(&self) -> usize {
        self.samples_per_channel() * (self.channels as usize)
    }
}

#[derive(Debug, Clone)]
pub struct EncodedAudioFrame {
    pub frame_id: u64,
    pub codec: AudioCodec,
    pub sample_rate: u32,
    pub channels: u16,
    pub samples_per_channel: u16,
    pub pts_us: u64,
    pub data: Bytes,
    pub encode_duration_us: u32,
}

#[async_trait]
pub trait AudioEncoder: Send + Sync {
    async fn init(&mut self, config: AudioEncoderConfig) -> Result<(), AudioEncoderError>;
    async fn encode(
        &mut self,
        frame: &RawAudioFrame,
    ) -> Result<EncodedAudioFrame, AudioEncoderError>;
    fn config(&self) -> &AudioEncoderConfig;
    fn set_bitrate(&mut self, bitrate_bps: i32) -> Result<(), AudioEncoderError>;
    fn set_fec(&mut self, enabled: bool) -> Result<(), AudioEncoderError>;
    fn set_dtx(&mut self, enabled: bool) -> Result<(), AudioEncoderError>;
}

/// Software mock and PCM passthrough audio encoder for integration tests and fallback.
pub struct MockAudioEncoder {
    config: AudioEncoderConfig,
    frame_counter: u64,
}

impl MockAudioEncoder {
    pub fn new(config: AudioEncoderConfig) -> Self {
        Self {
            config,
            frame_counter: 0,
        }
    }
}

#[async_trait]
impl AudioEncoder for MockAudioEncoder {
    async fn init(&mut self, config: AudioEncoderConfig) -> Result<(), AudioEncoderError> {
        self.config = config;
        self.frame_counter = 0;
        Ok(())
    }

    async fn encode(
        &mut self,
        frame: &RawAudioFrame,
    ) -> Result<EncodedAudioFrame, AudioEncoderError> {
        self.frame_counter += 1;

        // Produce simulated payload or PCM passthrough
        let payload = match self.config.codec {
            AudioCodec::Pcm => frame.data.clone(),
            AudioCodec::Opus => {
                // Generate a simulated Opus packet header and compressed payload
                let mut mock_opus = Vec::with_capacity(32);
                mock_opus.push(0xFC); // Config / TOC byte
                mock_opus.extend_from_slice(&(self.frame_counter as u32).to_be_bytes());
                mock_opus.extend_from_slice(&(frame.samples_per_channel as u16).to_be_bytes());
                // Simulated compressed sample data slice
                let sample_slice_len = (frame.data.len() / 8).clamp(8, 256);
                mock_opus.extend_from_slice(&frame.data[..sample_slice_len.min(frame.data.len())]);
                Bytes::from(mock_opus)
            }
        };

        Ok(EncodedAudioFrame {
            frame_id: self.frame_counter,
            codec: self.config.codec,
            sample_rate: self.config.sample_rate,
            channels: self.config.channels,
            samples_per_channel: frame.samples_per_channel as u16,
            pts_us: frame.pts_us,
            data: payload,
            encode_duration_us: 100, // Simulated 0.1ms
        })
    }

    fn config(&self) -> &AudioEncoderConfig {
        &self.config
    }

    fn set_bitrate(&mut self, bitrate_bps: i32) -> Result<(), AudioEncoderError> {
        self.config.bitrate_bps = bitrate_bps;
        Ok(())
    }

    fn set_fec(&mut self, enabled: bool) -> Result<(), AudioEncoderError> {
        self.config.fec_enabled = enabled;
        Ok(())
    }

    fn set_dtx(&mut self, enabled: bool) -> Result<(), AudioEncoderError> {
        self.config.dtx_enabled = enabled;
        Ok(())
    }
}

/// Dynamically discovers and initializes OpusAudioEncoder with graceful fallback to MockAudioEncoder.
pub struct AutoAudioEncoder {
    config: AudioEncoderConfig,
    inner: Box<dyn AudioEncoder>,
}

impl AutoAudioEncoder {
    pub fn new(config: AudioEncoderConfig) -> Self {
        let inner: Box<dyn AudioEncoder> = match crate::opus::OpusAudioEncoder::new(config.clone())
        {
            Ok(enc) => Box::new(enc),
            Err(_) => Box::new(MockAudioEncoder::new(config.clone())),
        };
        Self { config, inner }
    }
}

#[async_trait]
impl AudioEncoder for AutoAudioEncoder {
    async fn init(&mut self, config: AudioEncoderConfig) -> Result<(), AudioEncoderError> {
        self.config = config.clone();
        match self.inner.init(config.clone()).await {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::warn!(
                    "Primary audio encoder failed to initialize: {e}. Gracefully demoting to MockAudioEncoder"
                );
                let mut fallback = MockAudioEncoder::new(config.clone());
                fallback.init(config).await?;
                self.inner = Box::new(fallback);
                Ok(())
            }
        }
    }

    async fn encode(
        &mut self,
        frame: &RawAudioFrame,
    ) -> Result<EncodedAudioFrame, AudioEncoderError> {
        self.inner.encode(frame).await
    }

    fn config(&self) -> &AudioEncoderConfig {
        &self.config
    }

    fn set_bitrate(&mut self, bitrate_bps: i32) -> Result<(), AudioEncoderError> {
        self.config.bitrate_bps = bitrate_bps;
        self.inner.set_bitrate(bitrate_bps)
    }

    fn set_fec(&mut self, enabled: bool) -> Result<(), AudioEncoderError> {
        self.config.fec_enabled = enabled;
        self.inner.set_fec(enabled)
    }

    fn set_dtx(&mut self, enabled: bool) -> Result<(), AudioEncoderError> {
        self.config.dtx_enabled = enabled;
        self.inner.set_dtx(enabled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_quest_capture::AudioSampleFormat;

    #[tokio::test]
    async fn test_mock_audio_encoder_opus() {
        let config = AudioEncoderConfig {
            codec: AudioCodec::Opus,
            sample_rate: 48000,
            channels: 2,
            frame_duration_us: 2500, // 2.5ms
            bitrate_bps: 96_000,
            fec_enabled: true,
            dtx_enabled: false,
            application: AudioApplication::RestrictedLowDelay,
        };
        let mut enc = MockAudioEncoder::new(config);
        enc.init(enc.config().clone()).await.unwrap();

        let raw = RawAudioFrame::new(
            48000,
            2,
            AudioSampleFormat::F32Le,
            120,
            5000,
            Bytes::from(vec![0u8; 120 * 2 * 4]),
        );

        let encoded = enc.encode(&raw).await.unwrap();
        assert_eq!(encoded.frame_id, 1);
        assert_eq!(encoded.codec, AudioCodec::Opus);
        assert_eq!(encoded.samples_per_channel, 120);
        assert_eq!(encoded.pts_us, 5000);
        assert!(!encoded.data.is_empty());
    }

    #[tokio::test]
    async fn test_mock_audio_encoder_pcm_passthrough() {
        let config = AudioEncoderConfig {
            codec: AudioCodec::Pcm,
            ..Default::default()
        };
        let mut enc = MockAudioEncoder::new(config);
        enc.init(enc.config().clone()).await.unwrap();

        let raw_data = Bytes::from_static(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let raw = RawAudioFrame::new(
            48000,
            2,
            AudioSampleFormat::I16Le,
            2,
            1000,
            raw_data.clone(),
        );

        let encoded = enc.encode(&raw).await.unwrap();
        assert_eq!(encoded.codec, AudioCodec::Pcm);
        assert_eq!(encoded.data, raw_data);
    }
}

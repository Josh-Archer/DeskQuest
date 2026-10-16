use std::sync::Mutex;
use std::time::Instant;

use async_trait::async_trait;
use audiopus::{
    coder::Encoder, Application as OpusApplication, Bitrate, Channels as OpusChannels,
    SampleRate as OpusSampleRate,
};
use bytes::Bytes;
use linux_quest_capture::{AudioSampleFormat, RawAudioFrame};
use linux_quest_protocol::AudioCodec;

use crate::audio::{
    AudioApplication, AudioEncoder, AudioEncoderConfig, AudioEncoderError, EncodedAudioFrame,
};

/// Hardware/native Opus audio encoder for ultra-low latency desktop streaming.
///
/// Supports 2.5ms and 5ms frames with OPUS_APPLICATION_RESTRICTED_LOWDELAY,
/// configurable bitrate, in-band forward error correction (FEC), and discontinuous transmission (DTX).
pub struct OpusAudioEncoder {
    config: AudioEncoderConfig,
    encoder: Mutex<Option<Encoder>>,
    frame_counter: u64,
}

impl OpusAudioEncoder {
    pub fn new(config: AudioEncoderConfig) -> Result<Self, AudioEncoderError> {
        let enc = Self::create_internal_encoder(&config)?;
        Ok(Self {
            config,
            encoder: Mutex::new(Some(enc)),
            frame_counter: 0,
        })
    }

    fn create_internal_encoder(config: &AudioEncoderConfig) -> Result<Encoder, AudioEncoderError> {
        let sample_rate = match config.sample_rate {
            8000 => OpusSampleRate::Hz8000,
            12000 => OpusSampleRate::Hz12000,
            16000 => OpusSampleRate::Hz16000,
            24000 => OpusSampleRate::Hz24000,
            48000 => OpusSampleRate::Hz48000,
            other => {
                return Err(AudioEncoderError::InitFailed(format!(
                    "Unsupported sample rate for Opus: {other} Hz (expected 48000, 24000, 16000, 12000, or 8000)"
                )));
            }
        };

        let channels = match config.channels {
            1 => OpusChannels::Mono,
            2 => OpusChannels::Stereo,
            other => {
                return Err(AudioEncoderError::InitFailed(format!(
                    "Unsupported channel count for Opus: {other} (expected 1 or 2)"
                )));
            }
        };

        let mode = match config.application {
            AudioApplication::RestrictedLowDelay => OpusApplication::LowDelay,
            AudioApplication::Voip => OpusApplication::Voip,
            AudioApplication::Audio => OpusApplication::Audio,
        };

        let mut encoder = Encoder::new(sample_rate, channels, mode).map_err(|e| {
            AudioEncoderError::InitFailed(format!("Failed to create Opus encoder: {e}"))
        })?;

        encoder
            .set_bitrate(Bitrate::BitsPerSecond(config.bitrate_bps))
            .map_err(|e| {
                AudioEncoderError::InitFailed(format!("Failed to set Opus bitrate: {e}"))
            })?;

        encoder
            .set_inband_fec(config.fec_enabled)
            .map_err(|e| AudioEncoderError::InitFailed(format!("Failed to set Opus FEC: {e}")))?;

        encoder
            .set_dtx(config.dtx_enabled)
            .map_err(|e| AudioEncoderError::InitFailed(format!("Failed to set Opus DTX: {e}")))?;

        Ok(encoder)
    }
}

#[async_trait]
impl AudioEncoder for OpusAudioEncoder {
    async fn init(&mut self, config: AudioEncoderConfig) -> Result<(), AudioEncoderError> {
        let new_enc = Self::create_internal_encoder(&config)?;
        let mut guard = self.encoder.lock().unwrap();
        *guard = Some(new_enc);
        self.config = config;
        self.frame_counter = 0;
        Ok(())
    }

    async fn encode(
        &mut self,
        frame: &RawAudioFrame,
    ) -> Result<EncodedAudioFrame, AudioEncoderError> {
        let start = Instant::now();
        self.frame_counter += 1;

        let samples_per_channel = frame.samples_per_channel as usize;
        // Validate valid Opus frame sizes at 48kHz: 120 (2.5ms), 240 (5ms), 480 (10ms), 960 (20ms), 1920 (40ms), 2880 (60ms)
        if config_is_48k(self.config.sample_rate)
            && !matches!(samples_per_channel, 120 | 240 | 480 | 960 | 1920 | 2880)
        {
            return Err(AudioEncoderError::InvalidFrameSize(samples_per_channel));
        }

        let mut guard = self.encoder.lock().unwrap();
        let encoder = guard.as_mut().ok_or_else(|| {
            AudioEncoderError::EncodeFailed("Opus encoder not initialized".into())
        })?;

        let mut output_buf = [0u8; 2048];
        let bytes_written = match frame.format {
            AudioSampleFormat::F32Le => {
                let f32_samples = frame
                    .to_f32_vec()
                    .map_err(|e| AudioEncoderError::EncodeFailed(e.to_string()))?;
                encoder
                    .encode_float(&f32_samples, &mut output_buf)
                    .map_err(|e| {
                        AudioEncoderError::EncodeFailed(format!("Opus encode_float error: {e}"))
                    })?
            }
            AudioSampleFormat::I16Le => {
                let i16_samples = frame
                    .to_i16_vec()
                    .map_err(|e| AudioEncoderError::EncodeFailed(e.to_string()))?;
                encoder.encode(&i16_samples, &mut output_buf).map_err(|e| {
                    AudioEncoderError::EncodeFailed(format!("Opus encode error: {e}"))
                })?
            }
        };

        let encode_duration_us = start.elapsed().as_micros() as u32;

        Ok(EncodedAudioFrame {
            frame_id: self.frame_counter,
            codec: AudioCodec::Opus,
            sample_rate: self.config.sample_rate,
            channels: self.config.channels,
            samples_per_channel: frame.samples_per_channel as u16,
            pts_us: frame.pts_us,
            data: Bytes::copy_from_slice(&output_buf[..bytes_written]),
            encode_duration_us,
        })
    }

    fn config(&self) -> &AudioEncoderConfig {
        &self.config
    }

    fn set_bitrate(&mut self, bitrate_bps: i32) -> Result<(), AudioEncoderError> {
        let mut guard = self.encoder.lock().unwrap();
        if let Some(encoder) = guard.as_mut() {
            encoder
                .set_bitrate(Bitrate::BitsPerSecond(bitrate_bps))
                .map_err(|e| {
                    AudioEncoderError::EncodeFailed(format!("Failed to set Opus bitrate: {e}"))
                })?;
        }
        self.config.bitrate_bps = bitrate_bps;
        Ok(())
    }

    fn set_fec(&mut self, enabled: bool) -> Result<(), AudioEncoderError> {
        let mut guard = self.encoder.lock().unwrap();
        if let Some(encoder) = guard.as_mut() {
            encoder.set_inband_fec(enabled).map_err(|e| {
                AudioEncoderError::EncodeFailed(format!("Failed to toggle Opus FEC: {e}"))
            })?;
        }
        self.config.fec_enabled = enabled;
        Ok(())
    }

    fn set_dtx(&mut self, enabled: bool) -> Result<(), AudioEncoderError> {
        let mut guard = self.encoder.lock().unwrap();
        if let Some(encoder) = guard.as_mut() {
            encoder.set_dtx(enabled).map_err(|e| {
                AudioEncoderError::EncodeFailed(format!("Failed to toggle Opus DTX: {e}"))
            })?;
        }
        self.config.dtx_enabled = enabled;
        Ok(())
    }
}

fn config_is_48k(sample_rate: u32) -> bool {
    sample_rate == 48000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_opus_encoder_2_5ms_f32() {
        let config = AudioEncoderConfig {
            codec: AudioCodec::Opus,
            sample_rate: 48000,
            channels: 2,
            frame_duration_us: 2500, // 2.5ms = 120 samples @ 48kHz
            bitrate_bps: 96_000,
            fec_enabled: true,
            dtx_enabled: false,
            application: AudioApplication::RestrictedLowDelay,
        };

        let mut encoder = OpusAudioEncoder::new(config).unwrap();
        let raw = RawAudioFrame::new(
            48000,
            2,
            AudioSampleFormat::F32Le,
            120,
            1000,
            Bytes::from(vec![0u8; 120 * 2 * 4]),
        );

        let encoded = encoder.encode(&raw).await.unwrap();
        assert_eq!(encoded.frame_id, 1);
        assert_eq!(encoded.codec, AudioCodec::Opus);
        assert_eq!(encoded.sample_rate, 48000);
        assert_eq!(encoded.channels, 2);
        assert_eq!(encoded.samples_per_channel, 120);
        assert_eq!(encoded.pts_us, 1000);
        assert!(!encoded.data.is_empty());
    }

    #[tokio::test]
    async fn test_opus_encoder_5ms_i16() {
        let config = AudioEncoderConfig {
            codec: AudioCodec::Opus,
            sample_rate: 48000,
            channels: 2,
            frame_duration_us: 5000, // 5ms = 240 samples @ 48kHz
            bitrate_bps: 128_000,
            fec_enabled: false,
            dtx_enabled: false,
            application: AudioApplication::RestrictedLowDelay,
        };

        let mut encoder = OpusAudioEncoder::new(config).unwrap();
        let raw = RawAudioFrame::new(
            48000,
            2,
            AudioSampleFormat::I16Le,
            240,
            2000,
            Bytes::from(vec![0u8; 240 * 2 * 2]),
        );

        let encoded = encoder.encode(&raw).await.unwrap();
        assert_eq!(encoded.frame_id, 1);
        assert_eq!(encoded.codec, AudioCodec::Opus);
        assert_eq!(encoded.samples_per_channel, 240);
        assert_eq!(encoded.pts_us, 2000);
        assert!(!encoded.data.is_empty());
    }

    #[tokio::test]
    async fn test_opus_encoder_fec_dtx_and_bitrate_adjustment() {
        let mut encoder = OpusAudioEncoder::new(AudioEncoderConfig::default()).unwrap();

        encoder.set_bitrate(192_000).unwrap();
        assert_eq!(encoder.config().bitrate_bps, 192_000);

        encoder.set_fec(true).unwrap();
        assert!(encoder.config().fec_enabled);

        encoder.set_dtx(true).unwrap();
        assert!(encoder.config().dtx_enabled);
    }
}

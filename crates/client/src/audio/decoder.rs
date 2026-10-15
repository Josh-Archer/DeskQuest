//! Audio decoder traits and implementations (Opus native decoder & mock decoder).

use audiopus::{
    coder::Decoder, Channels as OpusChannels, Error as OpusError, MutSignals,
    SampleRate as OpusSampleRate,
};
use std::time::Duration;
use thiserror::Error;

use crate::error::ClientError;

/// Error variants during audio decoding.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum AudioDecoderError {
    /// Initialization failed.
    #[error("Audio decoder init failed: {0}")]
    Init(String),

    /// Frame decoding failed.
    #[error("Audio decode error: {0}")]
    Decode(String),

    /// Invalid channel count.
    #[error("Unsupported audio channel count: {0} (expected 1 or 2)")]
    InvalidChannels(u16),

    /// Invalid sample rate.
    #[error("Unsupported audio sample rate: {0} Hz")]
    InvalidSampleRate(u32),

    /// Buffer size or signal error.
    #[error("Buffer/Signal conversion error: {0}")]
    BufferError(String),
}

impl From<AudioDecoderError> for ClientError {
    fn from(err: AudioDecoderError) -> Self {
        ClientError::AudioDecoder(err.to_string())
    }
}

/// A decoded audio frame containing interleaved floating-point PCM audio samples.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedAudioFrame {
    /// Sample rate in Hertz (e.g. 48,000 Hz).
    pub sample_rate: u32,
    /// Channel count (e.g. 2 for stereo, 1 for mono).
    pub channels: u16,
    /// Number of audio samples per channel.
    pub samples_per_channel: usize,
    /// Presentation timestamp in microseconds (PTS) from the shared capture clock.
    pub pts_us: u64,
    /// Interleaved PCM audio samples in range `[-1.0, 1.0]`.
    pub samples: Vec<f32>,
    /// Whether this frame was synthesized via Packet Loss Concealment (PLC) or FEC.
    pub is_concealed: bool,
}

impl DecodedAudioFrame {
    /// Creates a new decoded audio frame.
    pub fn new(
        sample_rate: u32,
        channels: u16,
        samples_per_channel: usize,
        pts_us: u64,
        samples: Vec<f32>,
        is_concealed: bool,
    ) -> Self {
        Self {
            sample_rate,
            channels,
            samples_per_channel,
            pts_us,
            samples,
            is_concealed,
        }
    }

    /// Duration of the audio frame in microseconds.
    pub fn duration_us(&self) -> u64 {
        if self.sample_rate == 0 {
            0
        } else {
            (self.samples_per_channel as u64 * 1_000_000) / self.sample_rate as u64
        }
    }

    /// Frame duration as a `Duration`.
    pub fn duration(&self) -> Duration {
        Duration::from_micros(self.duration_us())
    }

    /// Total number of sample values across all interleaved channels.
    pub fn total_samples(&self) -> usize {
        self.samples.len()
    }

    /// Converts interleaved `f32` samples to interleaved 16-bit signed PCM `i16` samples.
    pub fn to_i16_vec(&self) -> Vec<i16> {
        self.samples
            .iter()
            .map(|&s| {
                let clamped = s.clamp(-1.0, 1.0);
                if clamped < 0.0 {
                    (clamped * 32768.0) as i16
                } else {
                    (clamped * 32767.0) as i16
                }
            })
            .collect()
    }
}

/// Abstract audio decoder trait supporting Packet Loss Concealment (PLC) and Forward Error Correction (FEC).
pub trait AudioDecoder: Send + Sync {
    /// Decodes an audio packet.
    ///
    /// - If `packet` is `Some(bytes)`: decodes the compressed bitstream. If `fec` is true,
    ///   decodes in-band Forward Error Correction payload from this packet for the previous lost frame.
    /// - If `packet` is `None`: invokes Packet Loss Concealment (PLC) to conceal the missing frame.
    fn decode(
        &mut self,
        packet: Option<&[u8]>,
        fec: bool,
        samples_per_channel: usize,
    ) -> Result<DecodedAudioFrame, AudioDecoderError>;

    /// Resets decoder state (e.g., clears internal prediction filters on seek or stream restart).
    fn reset(&mut self) -> Result<(), AudioDecoderError>;

    /// Configured audio sample rate in Hz.
    fn sample_rate(&self) -> u32;

    /// Configured channel count.
    fn channels(&self) -> u16;

    /// Sets the presentation timestamp (PTS) reference for subsequent decoded frames.
    fn set_pts_us(&mut self, pts_us: u64);
}

use audiopus::coder::GenericCtl;
use parking_lot::Mutex;

/// Hardware/native Opus audio decoder utilizing `audiopus` (RFC 6716).
///
/// Supports 2.5ms (120 samples) and 5ms (240 samples) ultra-low-latency frames at 48kHz,
/// native packet loss concealment (PLC) when missing packets are encountered,
/// and in-band Forward Error Correction (FEC).
pub struct OpusAudioDecoder {
    decoder: Mutex<Decoder>,
    sample_rate: u32,
    channels: u16,
    current_pts_us: u64,
}

impl OpusAudioDecoder {
    /// Creates a new native Opus decoder.
    pub fn new(sample_rate: u32, channels: u16) -> Result<Self, AudioDecoderError> {
        let opus_rate = match sample_rate {
            8000 => OpusSampleRate::Hz8000,
            12000 => OpusSampleRate::Hz12000,
            16000 => OpusSampleRate::Hz16000,
            24000 => OpusSampleRate::Hz24000,
            48000 => OpusSampleRate::Hz48000,
            other => return Err(AudioDecoderError::InvalidSampleRate(other)),
        };

        let opus_channels = match channels {
            1 => OpusChannels::Mono,
            2 => OpusChannels::Stereo,
            other => return Err(AudioDecoderError::InvalidChannels(other)),
        };

        let decoder = Decoder::new(opus_rate, opus_channels).map_err(|e| {
            AudioDecoderError::Init(format!("Failed to initialize Opus decoder: {e}"))
        })?;

        Ok(Self {
            decoder: Mutex::new(decoder),
            sample_rate,
            channels,
            current_pts_us: 0,
        })
    }
}

impl AudioDecoder for OpusAudioDecoder {
    fn decode(
        &mut self,
        packet: Option<&[u8]>,
        fec: bool,
        samples_per_channel: usize,
    ) -> Result<DecodedAudioFrame, AudioDecoderError> {
        let channel_count = self.channels as usize;
        let total_samples = samples_per_channel * channel_count;
        let mut output_buf = vec![0.0f32; total_samples];

        let is_concealed = packet.is_none() || fec;
        let mut decoder_guard = self.decoder.lock();

        let decoded_samples = match packet {
            Some(data) => {
                let opus_packet = audiopus::packet::Packet::try_from(data)
                    .map_err(|e: OpusError| AudioDecoderError::Decode(e.to_string()))?;
                let signals = MutSignals::try_from(output_buf.as_mut_slice())
                    .map_err(|e| AudioDecoderError::BufferError(e.to_string()))?;
                decoder_guard
                    .decode_float(Some(opus_packet), signals, fec)
                    .map_err(|e| AudioDecoderError::Decode(format!("Opus decode failed: {e}")))?
            }
            None => {
                // Packet Loss Concealment (PLC): Opus synthesizes the lost audio frame
                let signals = MutSignals::try_from(output_buf.as_mut_slice())
                    .map_err(|e| AudioDecoderError::BufferError(e.to_string()))?;
                decoder_guard
                    .decode_float(None, signals, false)
                    .map_err(|e| AudioDecoderError::Decode(format!("Opus PLC failed: {e}")))?
            }
        };

        output_buf.truncate(decoded_samples * channel_count);

        let frame_duration_us = if self.sample_rate == 0 {
            0
        } else {
            (decoded_samples as u64 * 1_000_000) / self.sample_rate as u64
        };

        let frame = DecodedAudioFrame::new(
            self.sample_rate,
            self.channels,
            decoded_samples,
            self.current_pts_us,
            output_buf,
            is_concealed,
        );

        self.current_pts_us += frame_duration_us;
        Ok(frame)
    }

    fn reset(&mut self) -> Result<(), AudioDecoderError> {
        self.decoder
            .lock()
            .reset_state()
            .map_err(|e| AudioDecoderError::Decode(format!("Failed to reset Opus state: {e}")))?;
        Ok(())
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn set_pts_us(&mut self, pts_us: u64) {
        self.current_pts_us = pts_us;
    }
}

/// Software mock audio decoder for testing jitter buffer, loss concealment, and A/V sync.
pub struct MockAudioDecoder {
    sample_rate: u32,
    channels: u16,
    current_pts_us: u64,
    last_samples: Vec<f32>,
    pub frames_decoded: u64,
    pub frames_concealed: u64,
    pub reset_count: u64,
    pub should_fail: bool,
}

impl MockAudioDecoder {
    /// Creates a new mock audio decoder.
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        Self {
            sample_rate,
            channels,
            current_pts_us: 0,
            last_samples: Vec::new(),
            frames_decoded: 0,
            frames_concealed: 0,
            reset_count: 0,
            should_fail: false,
        }
    }
}

impl AudioDecoder for MockAudioDecoder {
    fn decode(
        &mut self,
        packet: Option<&[u8]>,
        fec: bool,
        samples_per_channel: usize,
    ) -> Result<DecodedAudioFrame, AudioDecoderError> {
        if self.should_fail {
            return Err(AudioDecoderError::Decode(
                "Simulated decoder failure".into(),
            ));
        }

        let channel_count = self.channels as usize;
        let total_samples = samples_per_channel * channel_count;

        let frame_duration_us = if self.sample_rate == 0 {
            0
        } else {
            (samples_per_channel as u64 * 1_000_000) / self.sample_rate as u64
        };

        let (samples, is_concealed) = match packet {
            Some(data) => {
                self.frames_decoded += 1;
                // If payload has enough bytes for raw f32 samples, read them; otherwise synthesize
                let mut s = Vec::with_capacity(total_samples);
                if data.len() >= total_samples * 4 {
                    let (chunks, _) = data[..total_samples * 4].as_chunks::<4>();
                    for &chunk in chunks {
                        s.push(f32::from_le_bytes(chunk));
                    }
                } else {
                    // Generate synthetic test tone (440Hz sine wave)
                    let base_phase = (self.frames_decoded as f32) * 0.1;
                    for i in 0..samples_per_channel {
                        let sample = ((base_phase + i as f32 * 0.05).sin()) * 0.5;
                        for _ in 0..channel_count {
                            s.push(sample);
                        }
                    }
                }
                self.last_samples = s.clone();
                (s, fec)
            }
            None => {
                // Mock PLC: fade out / attenuate previous samples by 50%
                self.frames_concealed += 1;
                let mut s = Vec::with_capacity(total_samples);
                if !self.last_samples.is_empty() {
                    for &prev in &self.last_samples {
                        s.push(prev * 0.5);
                    }
                } else {
                    s.resize(total_samples, 0.0);
                }
                self.last_samples = s.clone();
                (s, true)
            }
        };

        let frame = DecodedAudioFrame::new(
            self.sample_rate,
            self.channels,
            samples_per_channel,
            self.current_pts_us,
            samples,
            is_concealed,
        );

        self.current_pts_us += frame_duration_us;
        Ok(frame)
    }

    fn reset(&mut self) -> Result<(), AudioDecoderError> {
        self.reset_count += 1;
        self.last_samples.clear();
        Ok(())
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn set_pts_us(&mut self, pts_us: u64) {
        self.current_pts_us = pts_us;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decoded_audio_frame_properties_and_conversions() {
        let samples = vec![0.0f32, 1.0, -1.0, 0.5, -0.5, 2.0]; // 2.0 should clamp to 1.0
        let frame = DecodedAudioFrame::new(48000, 2, 3, 1000, samples, false);

        assert_eq!(frame.sample_rate, 48000);
        assert_eq!(frame.channels, 2);
        assert_eq!(frame.samples_per_channel, 3);
        assert_eq!(frame.pts_us, 1000);
        assert_eq!(frame.duration_us(), 62); // 3 * 1000000 / 48000 = 62.5 -> 62 us
        assert_eq!(frame.total_samples(), 6);
        assert!(!frame.is_concealed);

        let i16_samples = frame.to_i16_vec();
        assert_eq!(i16_samples[0], 0);
        assert_eq!(i16_samples[1], 32767);
        assert_eq!(i16_samples[2], -32768);
        assert_eq!(i16_samples[5], 32767); // Clamped from 2.0
    }

    #[test]
    fn test_mock_audio_decoder_lifecycle() {
        let mut decoder = MockAudioDecoder::new(48000, 2);
        assert_eq!(decoder.sample_rate(), 48000);
        assert_eq!(decoder.channels(), 2);

        decoder.set_pts_us(50_000);

        // Decode normal packet
        let frame1 = decoder.decode(Some(&[1, 2, 3, 4]), false, 120).unwrap();
        assert_eq!(frame1.samples_per_channel, 120);
        assert_eq!(frame1.total_samples(), 240);
        assert_eq!(frame1.pts_us, 50_000);
        assert!(!frame1.is_concealed);
        assert_eq!(decoder.frames_decoded, 1);

        // Decode packet loss concealment (PLC)
        let frame2 = decoder.decode(None, false, 120).unwrap();
        assert_eq!(frame2.samples_per_channel, 120);
        assert_eq!(frame2.pts_us, 52_500); // 50000 + (120*1000000/48000) = 52500
        assert!(frame2.is_concealed);
        assert_eq!(decoder.frames_concealed, 1);

        // Reset
        decoder.reset().unwrap();
        assert_eq!(decoder.reset_count, 1);
    }

    #[test]
    fn test_opus_audio_decoder_init_and_plc() {
        let mut decoder = OpusAudioDecoder::new(48000, 2).expect("Opus decoder init failed");
        assert_eq!(decoder.sample_rate(), 48000);
        assert_eq!(decoder.channels(), 2);

        // Decode PLC (None packet) on uninitialized stream produces concealed silence / fade
        let plc_frame = decoder.decode(None, false, 120).expect("PLC decode failed");
        assert_eq!(plc_frame.samples_per_channel, 120);
        assert_eq!(plc_frame.channels, 2);
        assert_eq!(plc_frame.total_samples(), 240);
        assert!(plc_frame.is_concealed);

        // Reset state
        decoder.reset().expect("Opus reset failed");
    }

    #[test]
    fn test_opus_audio_decoder_invalid_params() {
        assert!(matches!(
            OpusAudioDecoder::new(44100, 2),
            Err(AudioDecoderError::InvalidSampleRate(44100))
        ));

        assert!(matches!(
            OpusAudioDecoder::new(48000, 6),
            Err(AudioDecoderError::InvalidChannels(6))
        ));
    }
}

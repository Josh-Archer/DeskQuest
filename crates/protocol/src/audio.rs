use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum AudioCodec {
    Opus = 1,
    Pcm = 2,
}

impl TryFrom<u8> for AudioCodec {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Opus),
            2 => Ok(Self::Pcm),
            other => Err(ProtocolError::DeserializationError(format!(
                "Unknown audio codec ID: {other}"
            ))),
        }
    }
}

/// Metadata describing an audio frame packet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioFrameMeta {
    pub frame_id: u64,
    pub sequence: u32,
    pub sample_rate: u32,
    pub channels: u16,
    pub samples_per_channel: u16,
    pub codec: AudioCodec,
    pub pts_us: u64,
}

/// Fully packaged audio frame packet ready for network transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPacket {
    pub meta: AudioFrameMeta,
    pub payload: Bytes,
}

impl AudioPacket {
    pub fn new(meta: AudioFrameMeta, payload: Bytes) -> Self {
        Self { meta, payload }
    }

    /// Frame duration in microseconds based on samples_per_channel and sample_rate.
    pub fn duration_us(&self) -> u64 {
        if self.meta.sample_rate == 0 {
            0
        } else {
            (self.meta.samples_per_channel as u64 * 1_000_000) / self.meta.sample_rate as u64
        }
    }

    pub fn serialize(&self) -> Result<Bytes, ProtocolError> {
        let meta_bytes = bincode::serialize(&self.meta)
            .map_err(|e| ProtocolError::SerializationError(e.to_string()))?;

        let mut buf = bytes::BytesMut::with_capacity(4 + meta_bytes.len() + self.payload.len());
        buf.extend_from_slice(&(meta_bytes.len() as u32).to_be_bytes());
        buf.extend_from_slice(&meta_bytes);
        buf.extend_from_slice(&self.payload);
        Ok(buf.freeze())
    }

    pub fn deserialize(data: &[u8]) -> Result<Self, ProtocolError> {
        if data.len() < 4 {
            return Err(ProtocolError::BufferTooShort {
                required: 4,
                found: data.len(),
            });
        }

        let meta_len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
        if data.len() < 4 + meta_len {
            return Err(ProtocolError::BufferTooShort {
                required: 4 + meta_len,
                found: data.len(),
            });
        }

        let meta: AudioFrameMeta = bincode::deserialize(&data[4..4 + meta_len])
            .map_err(|e| ProtocolError::DeserializationError(e.to_string()))?;

        let payload = Bytes::copy_from_slice(&data[4 + meta_len..]);
        Ok(Self { meta, payload })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audio_codec_try_from() {
        assert_eq!(AudioCodec::try_from(1).unwrap(), AudioCodec::Opus);
        assert_eq!(AudioCodec::try_from(2).unwrap(), AudioCodec::Pcm);
        assert!(AudioCodec::try_from(99).is_err());
    }

    #[test]
    fn test_audio_packet_serialization_roundtrip() {
        let meta = AudioFrameMeta {
            frame_id: 42,
            sequence: 101,
            sample_rate: 48000,
            channels: 2,
            samples_per_channel: 120, // 2.5ms @ 48kHz
            codec: AudioCodec::Opus,
            pts_us: 1_234_567,
        };
        let payload = Bytes::from_static(&[0xFA, 0xCE, 0x12, 0x34, 0x56]);
        let packet = AudioPacket::new(meta.clone(), payload.clone());

        assert_eq!(packet.duration_us(), 2500);

        let serialized = packet.serialize().expect("Serialization failed");
        let deserialized = AudioPacket::deserialize(&serialized).expect("Deserialization failed");

        assert_eq!(deserialized.meta, meta);
        assert_eq!(deserialized.payload, payload);
        assert_eq!(deserialized.duration_us(), 2500);
    }

    #[test]
    fn test_audio_packet_5ms_duration() {
        let meta = AudioFrameMeta {
            frame_id: 1,
            sequence: 1,
            sample_rate: 48000,
            channels: 2,
            samples_per_channel: 240, // 5ms @ 48kHz
            codec: AudioCodec::Opus,
            pts_us: 1000,
        };
        let packet = AudioPacket::new(meta, Bytes::new());
        assert_eq!(packet.duration_us(), 5000);
    }

    #[test]
    fn test_audio_packet_deserialize_too_short() {
        let short = [0u8, 0, 0];
        assert!(matches!(
            AudioPacket::deserialize(&short),
            Err(ProtocolError::BufferTooShort {
                required: 4,
                found: 3
            })
        ));

        let invalid_len = [0u8, 0, 1, 0, 1, 2];
        assert!(matches!(
            AudioPacket::deserialize(&invalid_len),
            Err(ProtocolError::BufferTooShort {
                required: 260,
                found: 6
            })
        ));
    }
}

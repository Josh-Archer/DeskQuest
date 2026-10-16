use bytes::{Buf, BufMut, Bytes, BytesMut};
use crc32fast::Hasher;
use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;

pub const PROTOCOL_MAGIC: [u8; 4] = *b"LQST";
pub const PROTOCOL_VERSION: u8 = 1;
pub const HEADER_SIZE: usize = 32;

pub const FLAG_NONE: u16 = 0x0000;
pub const FLAG_KEYFRAME: u16 = 0x0001;
pub const FLAG_LAST_CHUNK: u16 = 0x0002;
pub const FLAG_INTRA_REFRESH: u16 = 0x0004;
pub const FLAG_COMPRESSED: u16 = 0x0008;
pub const FLAG_FEC_PROTECTED: u16 = 0x0010;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum PacketType {
    HandshakeRequest = 1,
    HandshakeResponse = 2,
    Ping = 3,
    Pong = 4,
    DisplayConfig = 5,
    VideoFrameChunk = 6,
    ReferencePictureInvalidation = 7,
    InputEvent = 8,
    TelemetryReport = 9,
    Disconnect = 10,
    FecParity = 11,
    Clipboard = 12,
    AudioFrame = 13,
}

impl TryFrom<u8> for PacketType {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::HandshakeRequest),
            2 => Ok(Self::HandshakeResponse),
            3 => Ok(Self::Ping),
            4 => Ok(Self::Pong),
            5 => Ok(Self::DisplayConfig),
            6 => Ok(Self::VideoFrameChunk),
            7 => Ok(Self::ReferencePictureInvalidation),
            8 => Ok(Self::InputEvent),
            9 => Ok(Self::TelemetryReport),
            10 => Ok(Self::Disconnect),
            11 => Ok(Self::FecParity),
            12 => Ok(Self::Clipboard),
            13 => Ok(Self::AudioFrame),
            unknown => Err(ProtocolError::InvalidPacketType(unknown)),
        }
    }
}

/// 32-byte wire header for all linux-quest protocol packets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PacketHeader {
    pub magic: [u8; 4],
    pub version: u8,
    pub packet_type: PacketType,
    pub flags: u16,
    pub stream_id: u16,
    pub reserved: u16,
    pub sequence: u32,
    pub timestamp_us: u64,
    pub payload_len: u32,
    pub checksum: u32,
}

impl Default for PacketHeader {
    fn default() -> Self {
        Self {
            magic: PROTOCOL_MAGIC,
            version: PROTOCOL_VERSION,
            packet_type: PacketType::Ping,
            flags: FLAG_NONE,
            stream_id: 0,
            reserved: 0,
            sequence: 0,
            timestamp_us: 0,
            payload_len: 0,
            checksum: 0,
        }
    }
}

impl PacketHeader {
    pub fn new(
        packet_type: PacketType,
        stream_id: u16,
        sequence: u32,
        timestamp_us: u64,
        payload: &[u8],
    ) -> Self {
        let mut hasher = Hasher::new();
        hasher.update(payload);
        let checksum = hasher.finalize();

        Self {
            magic: PROTOCOL_MAGIC,
            version: PROTOCOL_VERSION,
            packet_type,
            flags: FLAG_NONE,
            stream_id,
            reserved: 0,
            sequence,
            timestamp_us,
            payload_len: payload.len() as u32,
            checksum,
        }
    }

    pub fn with_flags(mut self, flags: u16) -> Self {
        self.flags = flags;
        self
    }

    pub fn with_fec_index(mut self, index: u16) -> Self {
        self.reserved = index;
        self
    }

    pub fn fec_index(&self) -> u16 {
        self.reserved
    }

    pub fn encode(&self, dst: &mut BytesMut) {
        dst.put_slice(&self.magic);
        dst.put_u8(self.version);
        dst.put_u8(self.packet_type as u8);
        dst.put_u16(self.flags);
        dst.put_u16(self.stream_id);
        dst.put_u16(self.reserved);
        dst.put_u32(self.sequence);
        dst.put_u64(self.timestamp_us);
        dst.put_u32(self.payload_len);
        dst.put_u32(self.checksum);
    }

    pub fn decode(src: &mut &[u8]) -> Result<Self, ProtocolError> {
        if src.len() < HEADER_SIZE {
            return Err(ProtocolError::BufferTooShort {
                required: HEADER_SIZE,
                found: src.len(),
            });
        }

        let mut magic = [0u8; 4];
        magic.copy_from_slice(&src[0..4]);
        if magic != PROTOCOL_MAGIC {
            return Err(ProtocolError::InvalidMagic {
                expected: PROTOCOL_MAGIC,
                found: magic,
            });
        }

        let version = src[4];
        if version != PROTOCOL_VERSION {
            return Err(ProtocolError::UnsupportedVersion(version));
        }

        let packet_type = PacketType::try_from(src[5])?;
        let flags = u16::from_be_bytes([src[6], src[7]]);
        let stream_id = u16::from_be_bytes([src[8], src[9]]);
        let reserved = u16::from_be_bytes([src[10], src[11]]);
        let sequence = u32::from_be_bytes([src[12], src[13], src[14], src[15]]);
        let timestamp_us = u64::from_be_bytes([
            src[16], src[17], src[18], src[19], src[20], src[21], src[22], src[23],
        ]);
        let payload_len = u32::from_be_bytes([src[24], src[25], src[26], src[27]]);
        let checksum = u32::from_be_bytes([src[28], src[29], src[30], src[31]]);

        src.advance(HEADER_SIZE);

        Ok(Self {
            magic,
            version,
            packet_type,
            flags,
            stream_id,
            reserved,
            sequence,
            timestamp_us,
            payload_len,
            checksum,
        })
    }
}

/// Fully framed network packet with header and byte payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub header: PacketHeader,
    pub payload: Bytes,
}

impl Packet {
    pub fn new(header: PacketHeader, payload: Bytes) -> Self {
        Self { header, payload }
    }

    pub fn to_bytes(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(HEADER_SIZE + self.payload.len());
        self.header.encode(&mut buf);
        buf.put_slice(&self.payload);
        buf.freeze()
    }

    pub fn from_bytes(mut data: &[u8]) -> Result<Self, ProtocolError> {
        let header = PacketHeader::decode(&mut data)?;
        if data.len() < header.payload_len as usize {
            return Err(ProtocolError::PayloadLengthMismatch {
                expected: header.payload_len as usize,
                actual: data.len(),
            });
        }

        let payload_slice = &data[..header.payload_len as usize];
        let mut hasher = Hasher::new();
        hasher.update(payload_slice);
        let calculated = hasher.finalize();

        if calculated != header.checksum {
            return Err(ProtocolError::ChecksumMismatch {
                expected: header.checksum,
                calculated,
            });
        }

        let payload = Bytes::copy_from_slice(payload_slice);
        Ok(Self { header, payload })
    }
}

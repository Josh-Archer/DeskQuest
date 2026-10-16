use thiserror::Error;

#[derive(Error, Debug, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("Buffer too short to read header: required {required}, found {found}")]
    BufferTooShort { required: usize, found: usize },

    #[error("Invalid magic bytes: expected {expected:?}, found {found:?}")]
    InvalidMagic { expected: [u8; 4], found: [u8; 4] },

    #[error("Unsupported protocol version: {0}")]
    UnsupportedVersion(u8),

    #[error("Invalid packet type ID: {0}")]
    InvalidPacketType(u8),

    #[error("Checksum mismatch: expected {expected:#x}, calculated {calculated:#x}")]
    ChecksumMismatch { expected: u32, calculated: u32 },

    #[error("Payload length mismatch: header specifies {expected}, buffer has {actual}")]
    PayloadLengthMismatch { expected: usize, actual: usize },

    #[error("Serialization failed: {0}")]
    SerializationError(String),

    #[error("Deserialization failed: {0}")]
    DeserializationError(String),

    #[error("Frame reassembly timeout for frame {0}")]
    ReassemblyTimeout(u64),

    #[error("Corrupt frame: chunk {chunk} exceeds total chunks {total}")]
    CorruptChunkIndex { chunk: u16, total: u16 },

    #[error("Clipboard text size {size} exceeds limit {limit}")]
    ClipboardTooLarge { size: usize, limit: usize },

    #[error("Invalid clipboard chunk: index {index} exceeds or equals total chunks {total}")]
    InvalidClipboardChunk { index: u32, total: u32 },

    #[error("Clipboard reassembly mismatch: expected {expected} bytes, got {actual}")]
    ClipboardReassemblyMismatch { expected: usize, actual: usize },
}

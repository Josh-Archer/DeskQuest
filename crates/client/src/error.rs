//! Error types for the linux-quest-client crate.

use thiserror::Error;

/// Result alias for linux-quest-client operations.
pub type ClientResult<T> = Result<T, ClientError>;

/// Client runtime error variants.
#[derive(Debug, Error)]
pub enum ClientError {
    /// OpenXR runtime error.
    #[error("OpenXR error: {0}")]
    OpenXr(String),

    /// Vulkan graphics API error.
    #[error("Vulkan error: {0}")]
    Vulkan(String),

    /// Hardware or mock video decoder error.
    #[error("Decoder error: {0}")]
    Decoder(String),

    /// Network or USB transport error.
    #[error("Transport error: {0}")]
    Transport(String),

    /// Protocol serialization or framing error.
    #[error("Protocol error: {0}")]
    Protocol(String),

    /// Configuration validation error.
    #[error("Configuration error: {0}")]
    Config(String),

    /// Android NDK or JNI system error.
    #[error("Android system error: {0}")]
    Android(String),

    /// Audio receiver or jitter buffer error.
    #[error("Audio error: {0}")]
    Audio(String),

    /// Audio decoder error.
    #[error("Audio decoder error: {0}")]
    AudioDecoder(String),

    /// Audio sink error.
    #[error("Audio sink error: {0}")]
    AudioSink(String),

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<openxr::sys::Result> for ClientError {
    fn from(res: openxr::sys::Result) -> Self {
        ClientError::OpenXr(format!("OpenXR error code: {res:?}"))
    }
}

impl From<ash::vk::Result> for ClientError {
    fn from(res: ash::vk::Result) -> Self {
        ClientError::Vulkan(format!("Vulkan error code: {res:?}"))
    }
}

impl From<linux_quest_transport::TransportError> for ClientError {
    fn from(err: linux_quest_transport::TransportError) -> Self {
        ClientError::Transport(err.to_string())
    }
}

impl From<linux_quest_protocol::error::ProtocolError> for ClientError {
    fn from(err: linux_quest_protocol::error::ProtocolError) -> Self {
        ClientError::Protocol(err.to_string())
    }
}

//! Audio playback subsystem for the Meta Quest client.
//!
//! Includes an adaptive jitter buffer keyed by packet sequence numbers,
//! an abstract Opus/PCM audio decoder trait with Packet Loss Concealment (PLC) and FEC,
//! an A/V presentation clock synchronizer with drift correction,
//! and an `AudioSink` trait with AAudio (Android NDK low-latency mode) and mock implementations.

pub mod decoder;
pub mod pipeline;
pub mod receiver;
pub mod sink;
pub mod sync;

pub use decoder::{
    AudioDecoder, AudioDecoderError, DecodedAudioFrame, MockAudioDecoder, OpusAudioDecoder,
};
pub use pipeline::{AudioStepSummary, ClientAudioPipeline};
pub use receiver::{
    AudioJitterBuffer, AudioPlayoutItem, AudioReceiver, AudioReceiverConfig, AudioReceiverStats,
};
#[cfg(target_os = "android")]
pub use sink::AAudioSink;
pub use sink::{
    create_default_audio_sink, AudioSink, AudioSinkConfig, AudioSinkError, MockAudioSink,
};
pub use sync::{AvSyncConfig, AvSyncMetrics, AvSynchronizer, DriftCorrectionMode};

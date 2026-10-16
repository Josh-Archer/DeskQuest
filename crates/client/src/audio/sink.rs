//! Audio sink abstractions and hardware implementations.
//!
//! Provides the `AudioSink` trait, an AAudio (Android NDK low-latency mode) implementation
//! for Meta Quest 3 on Horizon OS (`target_os = "android"`), comprehensive architectural notes
//! on OpenSL ES fallback, and a `MockAudioSink` for unit tests and desktop Linux environments.

use std::fmt::Debug;
use thiserror::Error;

use crate::error::ClientError;

/// Error variants from audio sink operations.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum AudioSinkError {
    /// Failed to open audio device or stream.
    #[error("Failed to open audio sink: {0}")]
    Open(String),

    /// Failed to start playback.
    #[error("Failed to start audio playback: {0}")]
    Start(String),

    /// Failed to pause playback.
    #[error("Failed to pause audio playback: {0}")]
    Pause(String),

    /// Failed to stop playback or flush buffers.
    #[error("Failed to stop audio playback: {0}")]
    Stop(String),

    /// Error writing audio samples to output hardware.
    #[error("Failed to write audio samples: {0}")]
    Write(String),

    /// Hardware audio sink is not opened or ready.
    #[error("Audio sink is not ready or has been disconnected")]
    NotReady,

    /// Audio buffer underrun occurred.
    #[error("Audio buffer underrun occurred")]
    Underrun,
}

impl From<AudioSinkError> for ClientError {
    fn from(err: AudioSinkError) -> Self {
        ClientError::AudioSink(err.to_string())
    }
}

/// Configuration options for audio sinks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSinkConfig {
    /// Sample rate in Hz (e.g. 48,000 Hz).
    pub sample_rate: u32,
    /// Channel count (e.g. 2 for stereo).
    pub channels: u16,
    /// Target buffer capacity in audio frames (default: 480 frames = 10ms @ 48kHz).
    pub buffer_capacity_frames: u32,
    /// Request low-latency performance mode from the audio HAL.
    pub low_latency: bool,
}

impl Default for AudioSinkConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48000,
            channels: 2,
            buffer_capacity_frames: 480, // 10ms
            low_latency: true,
        }
    }
}

/// Common abstraction for hardware and simulated audio output sinks.
pub trait AudioSink: Send + Sync {
    /// Initializes and opens the audio sink with the given configuration.
    fn open(&mut self, config: AudioSinkConfig) -> Result<(), AudioSinkError>;

    /// Starts audio playback stream.
    fn start(&mut self) -> Result<(), AudioSinkError>;

    /// Pauses audio playback without dropping queued frames.
    fn pause(&mut self) -> Result<(), AudioSinkError>;

    /// Stops audio playback and flushes hardware buffers.
    fn stop(&mut self) -> Result<(), AudioSinkError>;

    /// Writes interleaved 32-bit floating point audio samples `[-1.0, 1.0]` to the output sink.
    ///
    /// Returns the number of samples actually written.
    fn write_samples(&mut self, samples: &[f32]) -> Result<usize, AudioSinkError>;

    /// Current hardware output latency estimate in frames.
    fn latency_frames(&self) -> Result<u32, AudioSinkError>;

    /// Whether the audio sink is currently active and playing.
    fn is_playing(&self) -> bool;

    /// Current audio sink configuration.
    fn config(&self) -> &AudioSinkConfig;
}

/// Software mock audio sink for unit testing, CI pipelines, and desktop host environments.
pub struct MockAudioSink {
    config: AudioSinkConfig,
    is_open: bool,
    is_playing: bool,
    written_samples: Vec<f32>,
    total_samples_written: usize,
    simulated_latency_frames: u32,
    pub should_fail: bool,
}

impl MockAudioSink {
    /// Creates a new mock audio sink with the specified configuration.
    pub fn new(config: AudioSinkConfig) -> Self {
        Self {
            config,
            is_open: false,
            is_playing: false,
            written_samples: Vec::new(),
            total_samples_written: 0,
            simulated_latency_frames: 120, // 2.5ms simulated latency @ 48kHz
            should_fail: false,
        }
    }

    /// Access to all raw audio samples written to this sink since creation or last clear.
    pub fn written_samples(&self) -> &[f32] {
        &self.written_samples
    }

    /// Total count of samples written over the sink's lifetime.
    pub fn total_samples_written(&self) -> usize {
        self.total_samples_written
    }

    /// Clears recorded sample buffer.
    pub fn clear(&mut self) {
        self.written_samples.clear();
    }

    /// Overrides simulated output latency in frames.
    pub fn set_simulated_latency_frames(&mut self, frames: u32) {
        self.simulated_latency_frames = frames;
    }
}

impl AudioSink for MockAudioSink {
    fn open(&mut self, config: AudioSinkConfig) -> Result<(), AudioSinkError> {
        if self.should_fail {
            return Err(AudioSinkError::Open("Simulated open failure".into()));
        }
        self.config = config;
        self.is_open = true;
        self.written_samples.clear();
        self.total_samples_written = 0;
        Ok(())
    }

    fn start(&mut self) -> Result<(), AudioSinkError> {
        if !self.is_open {
            return Err(AudioSinkError::NotReady);
        }
        if self.should_fail {
            return Err(AudioSinkError::Start("Simulated start failure".into()));
        }
        self.is_playing = true;
        Ok(())
    }

    fn pause(&mut self) -> Result<(), AudioSinkError> {
        if !self.is_open {
            return Err(AudioSinkError::NotReady);
        }
        self.is_playing = false;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), AudioSinkError> {
        self.is_playing = false;
        self.is_open = false;
        Ok(())
    }

    fn write_samples(&mut self, samples: &[f32]) -> Result<usize, AudioSinkError> {
        if !self.is_open || !self.is_playing {
            return Err(AudioSinkError::NotReady);
        }
        if self.should_fail {
            return Err(AudioSinkError::Write("Simulated write failure".into()));
        }
        self.written_samples.extend_from_slice(samples);
        self.total_samples_written += samples.len();
        Ok(samples.len())
    }

    fn latency_frames(&self) -> Result<u32, AudioSinkError> {
        Ok(self.simulated_latency_frames)
    }

    fn is_playing(&self) -> bool {
        self.is_playing
    }

    fn config(&self) -> &AudioSinkConfig {
        &self.config
    }
}

// -------------------------------------------------------------------------------------------------
// AAudio Native Sink for Android / Meta Quest 3 (target_os = "android")
// -------------------------------------------------------------------------------------------------

/// Native low-latency audio output sink using Android NDK's AAudio API.
///
/// Configures AAudio in `AAUDIO_PERFORMANCE_MODE_LOW_LATENCY` with `AAUDIO_SHARING_MODE_EXCLUSIVE`
/// (or `SHARED` fallback), bypassing intermediate Android audio mixer layers and achieving sub-10ms
/// audio output latency on the Meta Quest 3's Snapdragon XR2 Gen 2 DSP audio hardware.
///
/// ### OpenSL ES Fallback Architecture Note
///
/// On older Android platforms (API < 26, such as early Oculus Quest 1 headsets based on Android 7.1 Nougat),
/// low-latency audio output was implemented using OpenSL ES (`libOpenSLES.so`):
/// - A global engine object was created via `slCreateEngine(&engineObject, ...)`.
/// - An output mix was created via `(*engine)->CreateOutputMix(engine, &outputMixObject, ...)`.
/// - An audio player was initialized with `SLDataLocator_AndroidSimpleBufferQueue` and `SLDataFormat_PCM_EX`.
/// - Playout was driven by buffer queue callbacks (`SLAndroidSimpleBufferQueueItf::Enqueue`) exchanging
///   interleaved PCM chunks.
///
/// However, on modern Horizon OS (Android 10 / 12L / 14 on Meta Quest 3, Quest Pro, and Quest 2):
/// 1. OpenSL ES is deprecated by Google and Meta.
/// 2. OpenSL ES on modern Android is merely a legacy wrapper implemented internally on top of AudioFlinger,
///    adding intermediate memory copies, thread hops, and 10–15ms of unavoidable latency.
/// 3. AAudio provides direct Memory-Mapped (MMAP) access to the ALSA DSP hardware buffers, eliminating
///    IPC overhead and guaranteeing glitch-free low latency.
///
/// Therefore, `linux-quest-client` targets AAudio as its first-class audio engine on Quest 3.
/// If OpenSL ES backwards compatibility is ever required for Android API < 26 devices, an `OpenSlSink`
/// implementing the `AudioSink` trait can be added alongside `AAudioSink`.
#[cfg(target_os = "android")]
pub struct AAudioSink {
    config: AudioSinkConfig,
    stream: Option<ndk::audio::AudioStream>,
    is_playing: bool,
}

#[cfg(target_os = "android")]
impl AAudioSink {
    /// Creates a new uninitialized AAudio sink.
    pub fn new() -> Self {
        Self {
            config: AudioSinkConfig::default(),
            stream: None,
            is_playing: false,
        }
    }
}

#[cfg(target_os = "android")]
impl Default for AAudioSink {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "android")]
impl AudioSink for AAudioSink {
    fn open(&mut self, config: AudioSinkConfig) -> Result<(), AudioSinkError> {
        use ndk::audio::{
            AudioContentType, AudioDirection, AudioFormat, AudioPerformanceMode, AudioSharingMode,
            AudioStreamBuilder, AudioUsage,
        };

        let performance_mode = if config.low_latency {
            AudioPerformanceMode::LowLatency
        } else {
            AudioPerformanceMode::None
        };

        let builder = AudioStreamBuilder::new()
            .map_err(|e| {
                AudioSinkError::Open(format!("Failed to create AAudioStreamBuilder: {e}"))
            })?
            .direction(AudioDirection::Output)
            .performance_mode(performance_mode)
            .sharing_mode(AudioSharingMode::Exclusive) // Attempt exclusive low-latency mode first
            .format(AudioFormat::PCM_Float)
            .sample_rate(config.sample_rate as i32)
            .channel_count(config.channels as i32)
            .buffer_capacity_frames(config.buffer_capacity_frames as i32)
            .usage(AudioUsage::Game)
            .content_type(AudioContentType::Music);

        // Attempt opening the stream, falling back to Shared mode if Exclusive is unavailable
        let stream = match builder.open_stream() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "Exclusive AAudio stream open failed: {e}. Retrying with AAUDIO_SHARING_MODE_SHARED."
                );
                let fallback_builder = AudioStreamBuilder::new()
                    .map_err(|e| {
                        AudioSinkError::Open(format!("Failed to recreate AAudioStreamBuilder: {e}"))
                    })?
                    .direction(AudioDirection::Output)
                    .performance_mode(performance_mode)
                    .sharing_mode(AudioSharingMode::Shared)
                    .format(AudioFormat::PCM_Float)
                    .sample_rate(config.sample_rate as i32)
                    .channel_count(config.channels as i32)
                    .usage(AudioUsage::Game)
                    .content_type(AudioContentType::Music);

                fallback_builder
                    .open_stream()
                    .map_err(|e| AudioSinkError::Open(format!("Shared AAudio open failed: {e}")))?
            }
        };

        self.stream = Some(stream);
        self.config = config;
        self.is_playing = false;
        Ok(())
    }

    fn start(&mut self) -> Result<(), AudioSinkError> {
        let stream = self.stream.as_mut().ok_or(AudioSinkError::NotReady)?;
        stream
            .request_start()
            .map_err(|e| AudioSinkError::Start(format!("AAudio requestStart failed: {e}")))?;
        self.is_playing = true;
        Ok(())
    }

    fn pause(&mut self) -> Result<(), AudioSinkError> {
        let stream = self.stream.as_mut().ok_or(AudioSinkError::NotReady)?;
        stream
            .request_pause()
            .map_err(|e| AudioSinkError::Pause(format!("AAudio requestPause failed: {e}")))?;
        self.is_playing = false;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), AudioSinkError> {
        if let Some(stream) = self.stream.as_mut() {
            let _ = stream.request_stop();
            let _ = stream.request_flush();
        }
        self.is_playing = false;
        self.stream = None;
        Ok(())
    }

    fn write_samples(&mut self, samples: &[f32]) -> Result<usize, AudioSinkError> {
        let stream = self.stream.as_ref().ok_or(AudioSinkError::NotReady)?;
        if !self.is_playing {
            return Err(AudioSinkError::NotReady);
        }

        let channels = self.config.channels as usize;
        if channels == 0 || samples.is_empty() {
            return Ok(0);
        }

        let num_frames = (samples.len() / channels) as i32;
        // Non-blocking write with 0 timeout to prevent audio glitches from blocking render thread
        let frames_written = unsafe {
            stream
                .write(samples.as_ptr() as *const std::ffi::c_void, num_frames, 0)
                .map_err(|e| AudioSinkError::Write(format!("AAudio write failed: {e}")))?
        };

        Ok((frames_written as usize) * channels)
    }

    fn latency_frames(&self) -> Result<u32, AudioSinkError> {
        let stream = self.stream.as_ref().ok_or(AudioSinkError::NotReady)?;
        let buffer_size = stream.buffer_size_in_frames();
        Ok(buffer_size.max(0) as u32)
    }

    fn is_playing(&self) -> bool {
        self.is_playing
    }

    fn config(&self) -> &AudioSinkConfig {
        &self.config
    }
}

/// Helper function to create the default audio sink for the current platform.
/// On Android, instantiates `AAudioSink`; on desktop Linux/CI, instantiates `MockAudioSink`.
pub fn create_default_audio_sink() -> Box<dyn AudioSink> {
    #[cfg(target_os = "android")]
    {
        Box::new(AAudioSink::new())
    }
    #[cfg(not(target_os = "android"))]
    {
        Box::new(MockAudioSink::new(AudioSinkConfig::default()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_audio_sink_lifecycle() {
        let mut sink = MockAudioSink::new(AudioSinkConfig::default());
        assert!(!sink.is_playing());

        sink.open(AudioSinkConfig {
            sample_rate: 48000,
            channels: 2,
            buffer_capacity_frames: 240,
            low_latency: true,
        })
        .unwrap();

        sink.start().unwrap();
        assert!(sink.is_playing());

        // Write stereo samples
        let samples = vec![0.1f32, -0.1, 0.2, -0.2, 0.3, -0.3];
        let written = sink.write_samples(&samples).unwrap();
        assert_eq!(written, 6);
        assert_eq!(sink.total_samples_written(), 6);
        assert_eq!(sink.written_samples(), &samples[..]);

        sink.pause().unwrap();
        assert!(!sink.is_playing());

        // Write while paused returns NotReady
        assert!(matches!(
            sink.write_samples(&samples),
            Err(AudioSinkError::NotReady)
        ));

        sink.stop().unwrap();
        assert!(!sink.is_playing());
    }

    #[test]
    fn test_create_default_audio_sink() {
        let mut sink = create_default_audio_sink();
        sink.open(AudioSinkConfig::default()).unwrap();
        sink.start().unwrap();
        assert!(sink.is_playing());
        sink.stop().unwrap();
    }
}

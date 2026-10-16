//! Integrated client audio playback pipeline coordinating reception, jitter buffering,
//! Opus decoding (with PLC/FEC), A/V synchronization, and audio sink playout.

use std::time::Instant;

use linux_quest_protocol::audio::AudioPacket;
use linux_quest_protocol::packet::Packet;

use crate::audio::decoder::AudioDecoder;
use crate::audio::receiver::{
    AudioPlayoutItem, AudioReceiver, AudioReceiverConfig, AudioReceiverStats,
};
use crate::audio::sink::{AudioSink, AudioSinkConfig};
use crate::audio::sync::{AvSyncConfig, AvSyncMetrics, AvSynchronizer};
use crate::error::ClientResult;

/// Playout statistics for a single stepped audio iteration.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioStepSummary {
    /// Playout PTS in microseconds.
    pub pts_us: u64,
    /// Number of audio samples written to the sink.
    pub samples_written: usize,
    /// Whether this frame was synthesized via Packet Loss Concealment (PLC).
    pub is_concealed: bool,
    /// Active A/V drift in milliseconds.
    pub drift_ms: f64,
    /// Whether presentation is within in-sync window.
    pub in_sync: bool,
}

/// Quest client audio streaming pipeline orchestrating receiver -> jitter buffer -> decode -> sync -> sink.
pub struct ClientAudioPipeline<D, S>
where
    D: AudioDecoder,
    S: AudioSink,
{
    receiver: AudioReceiver,
    decoder: D,
    sync: AvSynchronizer,
    sink: S,
    is_running: bool,
    fec_enabled: bool,
}

impl<D, S> ClientAudioPipeline<D, S>
where
    D: AudioDecoder,
    S: AudioSink,
{
    /// Creates a new client audio pipeline.
    pub fn new(
        receiver_config: AudioReceiverConfig,
        sync_config: AvSyncConfig,
        decoder: D,
        sink: S,
    ) -> Self {
        Self {
            receiver: AudioReceiver::new(receiver_config),
            decoder,
            sync: AvSynchronizer::new(sync_config),
            sink,
            is_running: false,
            fec_enabled: true,
        }
    }

    /// Initializes and starts the audio output sink.
    pub fn start(&mut self, sink_config: AudioSinkConfig) -> ClientResult<()> {
        self.sink.open(sink_config)?;
        self.sink.start()?;
        self.is_running = true;
        Ok(())
    }

    /// Stops audio playback and flushes buffers.
    pub fn stop(&mut self) -> ClientResult<()> {
        self.is_running = false;
        self.sink.stop()?;
        self.receiver.reset();
        self.decoder.reset()?;
        self.sync.reset();
        Ok(())
    }

    /// Ingests a raw network packet into the audio jitter buffer.
    pub fn ingest_packet(&mut self, packet: Packet, now: Instant) -> ClientResult<bool> {
        self.receiver.ingest_packet(packet, now)
    }

    /// Ingests an audio packet directly into the jitter buffer.
    pub fn ingest_audio_packet(&mut self, packet: AudioPacket, now: Instant) {
        self.receiver.ingest_audio_packet(packet, now);
    }

    /// Updates the video presentation clock anchor for A/V synchronization.
    pub fn update_video_presentation(&mut self, video_pts_us: u64, presentation_instant: Instant) {
        self.sync
            .update_video_clock(video_pts_us, presentation_instant);
    }

    /// Processes one playout step at instant `now`.
    ///
    /// Pops ready packets or loss events from the jitter buffer, decodes through Opus
    /// (with Packet Loss Concealment on missing packets), corrects clock drift against video,
    /// and writes synchronized samples into the audio sink.
    pub fn step(&mut self, now: Instant) -> ClientResult<Option<AudioStepSummary>> {
        if !self.is_running {
            return Ok(None);
        }

        let ready_item = match self.receiver.pop_ready_item(now) {
            Some(item) => item,
            None => return Ok(None),
        };

        let (decoded_frame, is_concealed) = match ready_item {
            AudioPlayoutItem::Packet(audio_pkt) => {
                self.decoder.set_pts_us(audio_pkt.meta.pts_us);
                let samples_per_ch = audio_pkt.meta.samples_per_channel as usize;
                let frame = self.decoder.decode(
                    Some(&audio_pkt.payload),
                    self.fec_enabled,
                    samples_per_ch,
                )?;
                (frame, false)
            }
            AudioPlayoutItem::Lost {
                pts_us,
                samples_per_channel,
                ..
            } => {
                // Packet loss detected: invoke Packet Loss Concealment (PLC)
                self.decoder.set_pts_us(pts_us);
                let frame = self
                    .decoder
                    .decode(None, false, samples_per_channel as usize)?;
                (frame, true)
            }
        };

        // Align audio samples with video presentation clock via small resampling or drop/insert
        let synced_samples = self.sync.process_frame(&decoded_frame, now);
        let written = self.sink.write_samples(&synced_samples)?;
        let metrics = self.sync.metrics();

        Ok(Some(AudioStepSummary {
            pts_us: decoded_frame.pts_us,
            samples_written: written,
            is_concealed,
            drift_ms: metrics.smoothed_drift_ms,
            in_sync: metrics.in_sync,
        }))
    }

    /// Enables or disables in-band FEC decoding.
    pub fn set_fec_enabled(&mut self, enabled: bool) {
        self.fec_enabled = enabled;
    }

    /// Whether the audio pipeline is currently running.
    pub fn is_running(&self) -> bool {
        self.is_running
    }

    /// Reference to the audio receiver.
    pub fn receiver(&self) -> &AudioReceiver {
        &self.receiver
    }

    /// Mutable reference to the audio receiver.
    pub fn receiver_mut(&mut self) -> &mut AudioReceiver {
        &mut self.receiver
    }

    /// Receiver and jitter buffer statistics.
    pub fn receiver_stats(&self) -> &AudioReceiverStats {
        self.receiver.stats()
    }

    /// A/V synchronization metrics.
    pub fn sync_metrics(&self) -> AvSyncMetrics {
        self.sync.metrics()
    }

    /// Reference to the audio sink.
    pub fn sink(&self) -> &S {
        &self.sink
    }

    /// Mutable reference to the audio sink.
    pub fn sink_mut(&mut self) -> &mut S {
        &mut self.sink
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use linux_quest_protocol::audio::{AudioCodec, AudioFrameMeta};
    use std::time::Duration;

    use crate::audio::decoder::MockAudioDecoder;
    use crate::audio::sink::MockAudioSink;

    fn make_test_audio_packet(seq: u32, pts_us: u64) -> AudioPacket {
        let meta = AudioFrameMeta {
            frame_id: seq as u64,
            sequence: seq,
            sample_rate: 48000,
            channels: 2,
            samples_per_channel: 120, // 2.5ms
            codec: AudioCodec::Opus,
            pts_us,
        };
        AudioPacket::new(meta, Bytes::from_static(&[0xFC, 0x11, 0x22, 0x33]))
    }

    #[test]
    fn test_client_audio_pipeline_lifecycle_and_plc() {
        let rx_cfg = AudioReceiverConfig {
            min_delay: Duration::from_millis(2),
            loss_hold_timeout: Duration::from_millis(5),
            ..Default::default()
        };
        let sync_cfg = AvSyncConfig::default();
        let decoder = MockAudioDecoder::new(48000, 2);
        let sink = MockAudioSink::new(AudioSinkConfig::default());

        let mut pipeline = ClientAudioPipeline::new(rx_cfg, sync_cfg, decoder, sink);
        pipeline.start(AudioSinkConfig::default()).unwrap();
        assert!(pipeline.is_running());

        let t0 = Instant::now();
        // Video presented at PTS 100_000
        pipeline.update_video_presentation(100_000, t0);

        // Ingest packet 1 (PTS 100_000)
        pipeline.ingest_audio_packet(make_test_audio_packet(1, 100_000), t0);
        // Packet 2 dropped!
        // Ingest packet 3 (PTS 105_000)
        pipeline.ingest_audio_packet(
            make_test_audio_packet(3, 105_000),
            t0 + Duration::from_millis(1),
        );

        // Playout step 1: packet 1 played
        let step1 = pipeline
            .step(t0 + Duration::from_millis(3))
            .unwrap()
            .expect("Step 1 expected");
        assert_eq!(step1.pts_us, 100_000);
        assert!(!step1.is_concealed);
        assert_eq!(step1.samples_written, 240);

        // Playout step 2: after loss timeout, missing packet 2 is concealed via PLC
        let step2 = pipeline
            .step(t0 + Duration::from_millis(9))
            .unwrap()
            .expect("Step 2 (PLC) expected");
        assert!(step2.is_concealed);
        assert_eq!(step2.samples_written, 240);

        // Playout step 3: packet 3 played
        let step3 = pipeline
            .step(t0 + Duration::from_millis(9))
            .unwrap()
            .expect("Step 3 expected");
        assert_eq!(step3.pts_us, 105_000);
        assert!(!step3.is_concealed);

        assert_eq!(pipeline.receiver_stats().packets_delivered, 2);
        assert_eq!(pipeline.receiver_stats().packets_lost, 1);
        assert_eq!(pipeline.sink().total_samples_written(), 240 * 3);

        pipeline.stop().unwrap();
        assert!(!pipeline.is_running());
    }
}

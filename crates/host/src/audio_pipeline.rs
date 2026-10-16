use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use linux_quest_capture::AudioSource;
use linux_quest_encoder::{AudioEncoder, AudioEncoderConfig};
use linux_quest_protocol::{AudioFrameMeta, AudioPacket, Packet, PacketHeader, PacketType};
use linux_quest_transport::TransportEndpoint;

use crate::HostError;

/// Host audio streaming pipeline managing capture, encoding, and packetization over transport.
pub struct HostAudioPipeline<A, AE, T>
where
    A: AudioSource,
    AE: AudioEncoder,
    T: TransportEndpoint,
{
    source: A,
    encoder: AE,
    transport: T,
    stream_id: u16,
    sequence: u32,
    running: Arc<AtomicBool>,
}

impl<A, AE, T> HostAudioPipeline<A, AE, T>
where
    A: AudioSource,
    AE: AudioEncoder,
    T: TransportEndpoint,
{
    /// Creates a new host audio streaming pipeline.
    pub fn new(source: A, encoder: AE, transport: T, stream_id: u16) -> Self {
        Self {
            source,
            encoder,
            transport,
            stream_id,
            sequence: 0,
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn running_flag(&self) -> Arc<AtomicBool> {
        self.running.clone()
    }

    pub fn source(&self) -> &A {
        &self.source
    }

    pub fn source_mut(&mut self) -> &mut A {
        &mut self.source
    }

    pub fn encoder(&self) -> &AE {
        &self.encoder
    }

    pub fn encoder_mut(&mut self) -> &mut AE {
        &mut self.encoder
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    pub fn sequence(&self) -> u32 {
        self.sequence
    }

    pub fn stream_id(&self) -> u16 {
        self.stream_id
    }

    /// Initializes audio capture source and encoder backends.
    pub async fn init(&mut self, config: AudioEncoderConfig) -> Result<(), HostError> {
        self.source.init().await?;
        if let Err(e) = self.encoder.init(config).await {
            self.running.store(false, Ordering::SeqCst);
            return Err(HostError::AudioEncoder(e));
        }
        self.running.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Process one audio frame: Audio Source -> Encode (Opus/PCM) -> Packetize -> Transport Send.
    pub async fn step_stream_audio_frame(&mut self) -> Result<u64, HostError> {
        if !self.running.load(Ordering::Relaxed) {
            return Err(HostError::NotRunning);
        }

        let raw_chunk = self.source.capture_chunk().await?;
        let encoded = self.encoder.encode(&raw_chunk).await?;

        self.sequence += 1;
        let meta = AudioFrameMeta {
            frame_id: encoded.frame_id,
            sequence: self.sequence,
            sample_rate: encoded.sample_rate,
            channels: encoded.channels,
            samples_per_channel: encoded.samples_per_channel,
            codec: encoded.codec,
            pts_us: encoded.pts_us,
        };

        let audio_packet = AudioPacket::new(meta, encoded.data);
        let serialized_payload = audio_packet.serialize()?;

        let header = PacketHeader::new(
            PacketType::AudioFrame,
            self.stream_id,
            self.sequence,
            raw_chunk.pts_us,
            &serialized_payload,
        );

        let packet = Packet::new(header, serialized_payload);
        self.transport.send_packet(packet).await?;

        Ok(encoded.frame_id)
    }

    /// Process an incoming control packet from the client (e.g. ping/pong, disconnect).
    pub async fn handle_incoming_packet(&mut self, packet: Packet) -> Result<(), HostError> {
        match packet.header.packet_type {
            PacketType::Ping => {
                let pong_header = PacketHeader::new(
                    PacketType::Pong,
                    self.stream_id,
                    self.sequence + 1,
                    packet.header.timestamp_us,
                    &packet.payload,
                );
                self.transport
                    .send_packet(Packet::new(pong_header, packet.payload))
                    .await?;
            }
            PacketType::Disconnect => {
                self.running.store(false, Ordering::SeqCst);
            }
            _ => (),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_quest_capture::SineAudioSource;
    use linux_quest_encoder::{AudioEncoderConfig, MockAudioEncoder};
    use linux_quest_protocol::AudioPacket;
    use linux_quest_transport::LoopbackEndpoint;

    #[tokio::test]
    async fn test_host_audio_pipeline_framing_and_packetization() {
        let source = SineAudioSource::new_default(); // 48kHz, stereo, 2.5ms (120 samples)
        let config = AudioEncoderConfig::default();
        let encoder = MockAudioEncoder::new(config.clone());
        let (ep_host, mut ep_client) = LoopbackEndpoint::create_pair();

        let mut pipeline = HostAudioPipeline::new(source, encoder, ep_host, 1);
        pipeline.init(config).await.expect("Init failed");

        let frame_id = pipeline
            .step_stream_audio_frame()
            .await
            .expect("Step audio failed");
        assert_eq!(frame_id, 1);
        assert_eq!(pipeline.sequence(), 1);

        // Receive packet on client side
        let packet = ep_client.recv_packet().await.expect("Recv failed");
        assert_eq!(packet.header.packet_type, PacketType::AudioFrame);
        assert_eq!(packet.header.stream_id, 1);
        assert_eq!(packet.header.sequence, 1);
        assert_eq!(packet.header.timestamp_us, 0);

        let audio =
            AudioPacket::deserialize(&packet.payload).expect("AudioPacket deserialize failed");
        assert_eq!(audio.meta.frame_id, 1);
        assert_eq!(audio.meta.sequence, 1);
        assert_eq!(audio.meta.sample_rate, 48000);
        assert_eq!(audio.meta.channels, 2);
        assert_eq!(audio.meta.samples_per_channel, 120);
        assert_eq!(audio.duration_us(), 2500); // 2.5ms
    }

    #[tokio::test]
    async fn test_host_audio_pipeline_not_running_guard() {
        let source = SineAudioSource::new_default();
        let encoder = MockAudioEncoder::new(AudioEncoderConfig::default());
        let (ep_host, _) = LoopbackEndpoint::create_pair();

        let mut pipeline = HostAudioPipeline::new(source, encoder, ep_host, 1);
        let err = pipeline.step_stream_audio_frame().await.unwrap_err();
        assert!(matches!(err, HostError::NotRunning));
    }
}

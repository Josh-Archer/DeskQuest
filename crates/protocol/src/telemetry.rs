use serde::{Deserialize, Serialize};

/// Detailed latency telemetry tracking a single frame's journey from Linux host capture to Quest VSync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FrameLatencyBreakdown {
    pub frame_id: u64,
    /// Host frame capture time (microsecond epoch)
    pub capture_start_us: u64,
    /// DMA-BUF export / acquisition duration
    pub capture_duration_us: u32,
    /// NVENC / VA-API hardware encode duration
    pub encode_duration_us: u32,
    /// Network packetization & queue delay
    pub host_queue_duration_us: u32,
    /// Transport transit delay (USB or Wi-Fi)
    pub transport_duration_us: u32,
    /// AMediaCodec hardware decode duration on Quest
    pub client_decode_duration_us: u32,
    /// OpenXR composition layer blit duration
    pub client_render_duration_us: u32,
    /// Software figure in microseconds: the sum of the stages above, or (in the daemon
    /// dashboard) the headset's HUD estimate. A stage left at 0 was not measured. This is
    /// NOT optical glass-to-glass latency; see docs/latency-measurement.md.
    pub total_motion_to_photon_us: u32,
}

impl FrameLatencyBreakdown {
    pub fn new(frame_id: u64, capture_start_us: u64) -> Self {
        Self {
            frame_id,
            capture_start_us,
            ..Default::default()
        }
    }

    pub fn compute_total(&mut self) -> u32 {
        self.total_motion_to_photon_us = self.capture_duration_us
            + self.encode_duration_us
            + self.host_queue_duration_us
            + self.transport_duration_us
            + self.client_decode_duration_us
            + self.client_render_duration_us;
        self.total_motion_to_photon_us
    }

    pub fn total_ms(&self) -> f32 {
        self.total_motion_to_photon_us as f32 / 1000.0
    }
}

/// Detailed latency telemetry tracking an audio frame's journey from PipeWire host capture to Quest AAudio playout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AudioLatencyBreakdown {
    pub frame_id: u64,
    /// Host audio capture time (microsecond epoch)
    pub capture_start_us: u64,
    /// PipeWire capture buffer duration in microseconds
    pub capture_duration_us: u32,
    /// Opus audio encode duration in microseconds
    pub encode_duration_us: u32,
    /// Transport transit delay (USB or Wi-Fi) in microseconds
    pub transport_duration_us: u32,
    /// Client adaptive jitter buffer queueing delay in microseconds
    pub jitter_buffer_delay_us: u32,
    /// Opus client decode duration in microseconds
    pub decode_duration_us: u32,
    /// Client AAudio playout buffer latency in microseconds
    pub playout_buffer_duration_us: u32,
    /// End-to-end glass-to-ear / mic-to-speaker latency in microseconds
    pub total_latency_us: u32,
}

impl AudioLatencyBreakdown {
    pub fn new(frame_id: u64, capture_start_us: u64) -> Self {
        Self {
            frame_id,
            capture_start_us,
            ..Default::default()
        }
    }

    pub fn compute_total(&mut self) -> u32 {
        self.total_latency_us = self.capture_duration_us
            + self.encode_duration_us
            + self.transport_duration_us
            + self.jitter_buffer_delay_us
            + self.decode_duration_us
            + self.playout_buffer_duration_us;
        self.total_latency_us
    }

    pub fn total_ms(&self) -> f32 {
        self.total_latency_us as f32 / 1000.0
    }
}

/// Connection transport mode for streaming telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ClientConnectionMode {
    #[default]
    Disconnected,
    UsbAdb,
    Wifi,
}

impl std::fmt::Display for ClientConnectionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disconnected => write!(f, "Disconnected"),
            Self::UsbAdb => write!(f, "USB 3.0 (ADB Reverse)"),
            Self::Wifi => write!(f, "Wi-Fi (UDP/QUIC)"),
        }
    }
}

/// Real-time client network quality and transport telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct ClientTransportTelemetry {
    pub mode: ClientConnectionMode,
    pub rtt_us: u32,
    pub smoothed_rtt_us: u32,
    pub packet_loss_rate: f32,
    pub packets_sent: u64,
    pub packets_recv: u64,
    pub packets_lost: u64,
    pub fec_recoveries: u64,
    pub bitrate_mbps: f32,
}

impl ClientTransportTelemetry {
    pub fn rtt_ms(&self) -> f32 {
        self.smoothed_rtt_us as f32 / 1000.0
    }

    pub fn loss_percentage(&self) -> f32 {
        self.packet_loss_rate * 100.0
    }
}

/// Real-time audio stream telemetry.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AudioStreamTelemetry {
    pub active: bool,
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub frame_size_ms: f32,
    pub bitrate_kbps: u32,
    pub jitter_us: u32,
    pub buffer_delay_us: u32,
    pub buffer_health_pct: f32,
    pub plc_count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audio_latency_breakdown_compute() {
        let mut audio_lat = AudioLatencyBreakdown::new(100, 5000);
        audio_lat.capture_duration_us = 300;
        audio_lat.encode_duration_us = 450;
        audio_lat.transport_duration_us = 800;
        audio_lat.jitter_buffer_delay_us = 2500;
        audio_lat.decode_duration_us = 250;
        audio_lat.playout_buffer_duration_us = 500;

        let total = audio_lat.compute_total();
        assert_eq!(total, 4800);
        assert!((audio_lat.total_ms() - 4.8).abs() < 0.001);
    }

    #[test]
    fn test_client_transport_telemetry_helpers() {
        let telem = ClientTransportTelemetry {
            mode: ClientConnectionMode::UsbAdb,
            rtt_us: 1200,
            smoothed_rtt_us: 1100,
            packet_loss_rate: 0.0025,
            packets_sent: 5000,
            packets_recv: 4995,
            packets_lost: 5,
            fec_recoveries: 5,
            bitrate_mbps: 120.5,
        };

        assert!((telem.rtt_ms() - 1.1).abs() < 0.001);
        assert!((telem.loss_percentage() - 0.25).abs() < 0.001);
        assert_eq!(telem.mode.to_string(), "USB 3.0 (ADB Reverse)");
    }

    #[test]
    fn test_telemetry_bincode_roundtrip() {
        let mut v_lat = FrameLatencyBreakdown::new(1, 100);
        v_lat.capture_duration_us = 1000;
        v_lat.compute_total();

        let mut a_lat = AudioLatencyBreakdown::new(1, 100);
        a_lat.encode_duration_us = 400;
        a_lat.compute_total();

        let v_bytes = bincode::serialize(&v_lat).unwrap();
        let v_decoded: FrameLatencyBreakdown = bincode::deserialize(&v_bytes).unwrap();
        assert_eq!(v_lat, v_decoded);

        let a_bytes = bincode::serialize(&a_lat).unwrap();
        let a_decoded: AudioLatencyBreakdown = bincode::deserialize(&a_bytes).unwrap();
        assert_eq!(a_lat, a_decoded);
    }
}

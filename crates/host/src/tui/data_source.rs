use async_trait::async_trait;
use std::time::Instant;

use linux_quest_protocol::{
    AudioLatencyBreakdown, AudioStreamTelemetry, ClientConnectionMode, ClientTransportTelemetry,
    FrameLatencyBreakdown,
};

use std::sync::Arc;

use crate::ipc::{
    build_dashboard_snapshot, daemon_uptime_secs, mark_daemon_start, send_daemon_request,
    DaemonRequest, DaemonResponse,
};
use crate::link_status::SharedLinkTracker;
use crate::tui::model::{DashboardSnapshot, DisplaySnapshot};
use crate::{HostError, MultiDisplayHost};

/// Trait abstracting telemetry acquisition for the TUI dashboard.
#[async_trait]
pub trait TelemetrySource: Send + Sync {
    /// Fetches the latest snapshot of host streaming and client telemetry.
    async fn fetch_snapshot(&mut self) -> Result<DashboardSnapshot, HostError>;
}

/// Simulated telemetry source providing realistic dynamic metrics for offline testing and demo mode.
pub struct MockTelemetrySource {
    start_time: Instant,
    step: u64,
    connection_mode: ClientConnectionMode,
}

impl Default for MockTelemetrySource {
    fn default() -> Self {
        Self::new()
    }
}

impl MockTelemetrySource {
    pub fn new() -> Self {
        Self {
            start_time: Instant::now(),
            step: 0,
            connection_mode: ClientConnectionMode::UsbAdb,
        }
    }

    pub fn with_connection_mode(mut self, mode: ClientConnectionMode) -> Self {
        self.connection_mode = mode;
        self
    }
}

#[async_trait]
impl TelemetrySource for MockTelemetrySource {
    async fn fetch_snapshot(&mut self) -> Result<DashboardSnapshot, HostError> {
        self.step += 1;
        let elapsed_secs = self.start_time.elapsed().as_secs();
        let t = self.step as f64 * 0.1;

        // Subtly fluctuate latency stages using sine waves to produce live sparklines
        let capture_jitter = (t.sin() * 150.0) as i32;
        let encode_jitter = ((t * 1.5).cos() * 250.0) as i32;
        let transport_jitter = ((t * 2.0).sin() * 120.0) as i32;
        let decode_jitter = ((t * 1.1).cos() * 200.0) as i32;

        let capture_us = (1150 + capture_jitter).max(800) as u32;
        let encode_us = (2850 + encode_jitter).max(2000) as u32;
        let queue_us = 320u32;
        let transport_us = match self.connection_mode {
            ClientConnectionMode::UsbAdb => (750 + transport_jitter).max(400) as u32,
            ClientConnectionMode::Wifi => (2200 + transport_jitter * 2).max(1200) as u32,
            ClientConnectionMode::Disconnected => 0,
        };
        let client_decode_us = (3100 + decode_jitter).max(2200) as u32;
        let client_render_us = 650u32;

        let mut video_lat = FrameLatencyBreakdown {
            frame_id: self.step,
            capture_start_us: elapsed_secs * 1_000_000 + (self.step % 1_000_000),
            capture_duration_us: capture_us,
            encode_duration_us: encode_us,
            host_queue_duration_us: queue_us,
            transport_duration_us: transport_us,
            client_decode_duration_us: client_decode_us,
            client_render_duration_us: client_render_us,
            total_motion_to_photon_us: 0,
        };
        video_lat.compute_total();

        let audio_capture_us = 300u32;
        let audio_encode_us = 420u32;
        let audio_transport_us = transport_us;
        let audio_jitter_buf_us = 2500u32;
        let audio_decode_us = 210u32;
        let audio_playout_us = 500u32;

        let mut audio_lat = AudioLatencyBreakdown {
            frame_id: self.step * 4,
            capture_start_us: elapsed_secs * 1_000_000,
            capture_duration_us: audio_capture_us,
            encode_duration_us: audio_encode_us,
            transport_duration_us: audio_transport_us,
            jitter_buffer_delay_us: audio_jitter_buf_us,
            decode_duration_us: audio_decode_us,
            playout_buffer_duration_us: audio_playout_us,
            total_latency_us: 0,
        };
        audio_lat.compute_total();

        let rtt_base = match self.connection_mode {
            ClientConnectionMode::UsbAdb => 1100,
            ClientConnectionMode::Wifi => 4500,
            ClientConnectionMode::Disconnected => 0,
        };
        let rtt_us = (rtt_base as i32 + (t.cos() * 150.0) as i32).max(500) as u32;

        let fps1 = 90.0 + (t.sin() * 0.4);
        let fps2 = 72.0 + ((t * 0.8).cos() * 0.3);

        let displays = if self.connection_mode == ClientConnectionMode::Disconnected {
            vec![]
        } else {
            vec![
                DisplaySnapshot {
                    id: 1,
                    name: "Quest-Virtual-1".into(),
                    width: 2560,
                    height: 1440,
                    refresh_rate: 90,
                    fps: fps1 as f32,
                    bitrate_mbps: 78.5 + (t.sin() * 2.0) as f32,
                    codec: "AV1".into(),
                    is_virtual: true,
                },
                DisplaySnapshot {
                    id: 2,
                    name: "Quest-Virtual-2".into(),
                    width: 1920,
                    height: 1080,
                    refresh_rate: 72,
                    fps: fps2 as f32,
                    bitrate_mbps: 34.1 + (t.cos() * 1.5) as f32,
                    codec: "AV1".into(),
                    is_virtual: true,
                },
            ]
        };

        Ok(DashboardSnapshot {
            uptime_secs: elapsed_secs,
            streaming: self.connection_mode != ClientConnectionMode::Disconnected,
            displays,
            video_latency: video_lat,
            audio_latency: audio_lat,
            client: ClientTransportTelemetry {
                mode: self.connection_mode,
                rtt_us,
                smoothed_rtt_us: rtt_base,
                packet_loss_rate: 0.0001,
                packets_sent: 50000 + self.step * 10,
                packets_recv: 49995 + self.step * 10,
                packets_lost: 5,
                fec_recoveries: 12 + (self.step / 50),
                bitrate_mbps: 112.6,
            },
            audio: AudioStreamTelemetry {
                active: self.connection_mode != ClientConnectionMode::Disconnected,
                codec: "Opus (Low-Delay)".into(),
                sample_rate: 48000,
                channels: 2,
                frame_size_ms: 2.5,
                bitrate_kbps: 128,
                jitter_us: 380,
                buffer_delay_us: 2500,
                buffer_health_pct: 100.0,
                plc_count: 0,
            },
        })
    }
}

/// Telemetry source for the dashboard running inside the daemon process (`daemon --tui`).
/// Reads the live link state directly instead of simulated data.
pub struct LiveDaemonTelemetrySource {
    host: Arc<MultiDisplayHost>,
    link: SharedLinkTracker,
}

impl LiveDaemonTelemetrySource {
    pub fn new(host: Arc<MultiDisplayHost>, link: SharedLinkTracker) -> Self {
        mark_daemon_start();
        Self { host, link }
    }
}

#[async_trait]
impl TelemetrySource for LiveDaemonTelemetrySource {
    async fn fetch_snapshot(&mut self) -> Result<DashboardSnapshot, HostError> {
        let monitors = self.host.list_monitors().await;
        let status = self.link.lock().status(Instant::now());
        Ok(build_dashboard_snapshot(
            monitors,
            status,
            daemon_uptime_secs(),
        ))
    }
}

/// IPC telemetry source querying the persistent background daemon.
pub struct IpcTelemetrySource;

#[async_trait]
impl TelemetrySource for IpcTelemetrySource {
    async fn fetch_snapshot(&mut self) -> Result<DashboardSnapshot, HostError> {
        let resp = send_daemon_request(DaemonRequest::DashboardSnapshot)
            .await
            .map_err(|e| HostError::Ipc(format!("Failed to connect to daemon: {e}")))?;

        match resp {
            DaemonResponse::DashboardSnapshot(snap) => Ok(*snap),
            DaemonResponse::Error(err) => Err(HostError::Ipc(err)),
            other => Err(HostError::Ipc(format!(
                "Unexpected daemon response: {other:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_telemetry_source_generation() {
        let mut source = MockTelemetrySource::new();
        let snap1 = source.fetch_snapshot().await.unwrap();
        assert!(snap1.streaming);
        assert_eq!(snap1.displays.len(), 2);
        assert!(snap1.video_latency.total_motion_to_photon_us > 0);
        assert!(snap1.audio_latency.total_latency_us > 0);
        assert_eq!(snap1.client.mode, ClientConnectionMode::UsbAdb);

        let snap2 = source.fetch_snapshot().await.unwrap();
        assert_eq!(snap2.video_latency.frame_id, 2);
    }

    #[tokio::test]
    async fn test_mock_telemetry_source_wifi_mode() {
        let mut source =
            MockTelemetrySource::new().with_connection_mode(ClientConnectionMode::Wifi);
        let snap = source.fetch_snapshot().await.unwrap();
        assert_eq!(snap.client.mode, ClientConnectionMode::Wifi);
        assert!(snap.client.rtt_us >= 4000);
    }
}

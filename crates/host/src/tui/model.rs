pub use linux_quest_protocol::{
    AudioLatencyBreakdown, AudioStreamTelemetry, ClientConnectionMode, ClientTransportTelemetry,
    FrameLatencyBreakdown,
};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Generic fixed-capacity rolling window for recording and aggregating time-series metrics.
#[derive(Debug, Clone)]
pub struct RollingWindow<T> {
    capacity: usize,
    buffer: VecDeque<T>,
}

impl<T> RollingWindow<T> {
    /// Creates a new rolling window with the specified maximum capacity.
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(1);
        Self {
            capacity: cap,
            buffer: VecDeque::with_capacity(cap),
        }
    }

    /// Pushes a new sample into the rolling window, evicting the oldest if capacity is reached.
    pub fn push(&mut self, item: T) {
        if self.buffer.len() >= self.capacity {
            self.buffer.pop_front();
        }
        self.buffer.push_back(item);
    }

    /// Number of samples currently held.
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// True if the window contains no samples.
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Maximum capacity of the rolling window.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Clears all samples.
    pub fn clear(&mut self) {
        self.buffer.clear();
    }

    /// Returns an iterator over the samples from oldest to newest.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.buffer.iter()
    }

    /// Returns the most recently pushed sample, if any.
    pub fn latest(&self) -> Option<&T> {
        self.buffer.back()
    }
}

impl RollingWindow<u64> {
    /// Returns the arithmetic mean of all samples in the window, or 0.0 if empty.
    pub fn average(&self) -> f64 {
        if self.buffer.is_empty() {
            0.0
        } else {
            let sum: u64 = self.buffer.iter().sum();
            sum as f64 / self.buffer.len() as f64
        }
    }

    /// Returns the minimum sample value in the window.
    pub fn min(&self) -> Option<u64> {
        self.buffer.iter().copied().min()
    }

    /// Returns the maximum sample value in the window.
    pub fn max(&self) -> Option<u64> {
        self.buffer.iter().copied().max()
    }

    /// Calculates the percentile `p` (in `[0.0, 100.0]`) of the samples in the window.
    pub fn percentile(&self, p: f64) -> Option<f64> {
        if self.buffer.is_empty() {
            return None;
        }
        let mut sorted: Vec<u64> = self.buffer.iter().copied().collect();
        sorted.sort_unstable();

        let clamped_p = p.clamp(0.0, 100.0);
        let rank = (clamped_p / 100.0) * (sorted.len() - 1) as f64;
        let lower = rank.floor() as usize;
        let upper = rank.ceil() as usize;
        if lower == upper {
            Some(sorted[lower] as f64)
        } else {
            let frac = rank - lower as f64;
            Some(sorted[lower] as f64 * (1.0 - frac) + sorted[upper] as f64 * frac)
        }
    }

    /// Converts samples into a contiguous vector formatted for ratatui `Sparkline`.
    pub fn as_sparkline(&self) -> Vec<u64> {
        self.buffer.iter().copied().collect()
    }
}

impl RollingWindow<f64> {
    /// Returns the arithmetic mean of all samples in the window, or 0.0 if empty.
    pub fn average(&self) -> f64 {
        if self.buffer.is_empty() {
            0.0
        } else {
            let sum: f64 = self.buffer.iter().sum();
            sum / self.buffer.len() as f64
        }
    }

    /// Returns the minimum sample value in the window.
    pub fn min(&self) -> Option<f64> {
        self.buffer
            .iter()
            .copied()
            .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    }

    /// Returns the maximum sample value in the window.
    pub fn max(&self) -> Option<f64> {
        self.buffer
            .iter()
            .copied()
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    }
}

/// Display metric model for an active virtual monitor.
#[derive(Debug, Clone)]
pub struct DisplayMetricModel {
    pub display_id: u16,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub refresh_rate: u32,
    pub fps: f64,
    pub fps_history: RollingWindow<u64>,
    pub bitrate_mbps: f64,
    pub bitrate_history: RollingWindow<u64>,
    pub codec: String,
    pub is_virtual: bool,
    pub active: bool,
}

impl DisplayMetricModel {
    pub fn new(display_id: u16, name: String, width: u32, height: u32, refresh_rate: u32) -> Self {
        Self {
            display_id,
            name,
            width,
            height,
            refresh_rate,
            fps: refresh_rate as f64,
            fps_history: RollingWindow::new(60),
            bitrate_mbps: 0.0,
            bitrate_history: RollingWindow::new(60),
            codec: "AV1".to_string(),
            is_virtual: true,
            active: true,
        }
    }

    pub fn update_metrics(&mut self, fps: f64, bitrate_mbps: f64) {
        self.fps = fps;
        self.fps_history.push((fps * 10.0).round() as u64); // tenths of FPS
        self.bitrate_mbps = bitrate_mbps;
        self.bitrate_history
            .push((bitrate_mbps * 10.0).round() as u64); // tenths of Mbps
    }
}

/// Stages of the video streaming pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoWaterfallStage {
    Capture,
    Encode,
    HostQueue,
    Transport,
    ClientDecode,
    ClientRender,
}

/// Aggregated real-time video latency waterfall breakdown.
#[derive(Debug, Clone)]
pub struct LatencyWaterfallModel {
    pub latest: FrameLatencyBreakdown,
    pub capture_history: RollingWindow<u64>,
    pub encode_history: RollingWindow<u64>,
    pub queue_history: RollingWindow<u64>,
    pub transport_history: RollingWindow<u64>,
    pub decode_history: RollingWindow<u64>,
    pub render_history: RollingWindow<u64>,
    pub total_history: RollingWindow<u64>,
}

impl LatencyWaterfallModel {
    pub fn new(capacity: usize) -> Self {
        Self {
            latest: FrameLatencyBreakdown::default(),
            capture_history: RollingWindow::new(capacity),
            encode_history: RollingWindow::new(capacity),
            queue_history: RollingWindow::new(capacity),
            transport_history: RollingWindow::new(capacity),
            decode_history: RollingWindow::new(capacity),
            render_history: RollingWindow::new(capacity),
            total_history: RollingWindow::new(capacity),
        }
    }

    pub fn push(&mut self, mut sample: FrameLatencyBreakdown) {
        sample.compute_total();
        self.latest = sample;
        self.capture_history.push(sample.capture_duration_us as u64);
        self.encode_history.push(sample.encode_duration_us as u64);
        self.queue_history
            .push(sample.host_queue_duration_us as u64);
        self.transport_history
            .push(sample.transport_duration_us as u64);
        self.decode_history
            .push(sample.client_decode_duration_us as u64);
        self.render_history
            .push(sample.client_render_duration_us as u64);
        self.total_history
            .push(sample.total_motion_to_photon_us as u64);
    }

    pub fn capture_ms(&self) -> f64 {
        self.latest.capture_duration_us as f64 / 1000.0
    }

    pub fn encode_ms(&self) -> f64 {
        self.latest.encode_duration_us as f64 / 1000.0
    }

    pub fn queue_ms(&self) -> f64 {
        self.latest.host_queue_duration_us as f64 / 1000.0
    }

    pub fn transport_ms(&self) -> f64 {
        self.latest.transport_duration_us as f64 / 1000.0
    }

    pub fn decode_ms(&self) -> f64 {
        self.latest.client_decode_duration_us as f64 / 1000.0
    }

    pub fn render_ms(&self) -> f64 {
        self.latest.client_render_duration_us as f64 / 1000.0
    }

    pub fn total_ms(&self) -> f64 {
        self.latest.total_ms() as f64
    }

    pub fn avg_total_ms(&self) -> f64 {
        self.total_history.average() / 1000.0
    }

    pub fn min_total_ms(&self) -> f64 {
        self.total_history.min().unwrap_or(0) as f64 / 1000.0
    }

    pub fn max_total_ms(&self) -> f64 {
        self.total_history.max().unwrap_or(0) as f64 / 1000.0
    }

    pub fn p95_total_ms(&self) -> f64 {
        self.total_history.percentile(95.0).unwrap_or(0.0) / 1000.0
    }

    /// Relative percentage of total latency taken by each stage (0.0 to 100.0).
    pub fn stage_percentage(&self, stage: VideoWaterfallStage) -> f64 {
        let total = self.latest.total_motion_to_photon_us as f64;
        if total == 0.0 {
            return 0.0;
        }
        let dur = match stage {
            VideoWaterfallStage::Capture => self.latest.capture_duration_us as f64,
            VideoWaterfallStage::Encode => self.latest.encode_duration_us as f64,
            VideoWaterfallStage::HostQueue => self.latest.host_queue_duration_us as f64,
            VideoWaterfallStage::Transport => self.latest.transport_duration_us as f64,
            VideoWaterfallStage::ClientDecode => self.latest.client_decode_duration_us as f64,
            VideoWaterfallStage::ClientRender => self.latest.client_render_duration_us as f64,
        };
        (dur / total) * 100.0
    }

    /// Converts total latency history into tenths-of-a-millisecond integers for ratatui Sparkline.
    pub fn sparkline_total_tenths_ms(&self) -> Vec<u64> {
        self.total_history
            .iter()
            .map(|&us| (us + 50) / 100) // 100us = 0.1ms
            .collect()
    }
}

/// Aggregated real-time audio latency breakdown.
#[derive(Debug, Clone)]
pub struct AudioLatencyModel {
    pub latest: AudioLatencyBreakdown,
    pub total_history: RollingWindow<u64>,
}

impl AudioLatencyModel {
    pub fn new(capacity: usize) -> Self {
        Self {
            latest: AudioLatencyBreakdown::default(),
            total_history: RollingWindow::new(capacity),
        }
    }

    pub fn push(&mut self, mut sample: AudioLatencyBreakdown) {
        sample.compute_total();
        self.latest = sample;
        self.total_history.push(sample.total_latency_us as u64);
    }

    pub fn capture_ms(&self) -> f64 {
        self.latest.capture_duration_us as f64 / 1000.0
    }

    pub fn encode_ms(&self) -> f64 {
        self.latest.encode_duration_us as f64 / 1000.0
    }

    pub fn transport_ms(&self) -> f64 {
        self.latest.transport_duration_us as f64 / 1000.0
    }

    pub fn jitter_buffer_ms(&self) -> f64 {
        self.latest.jitter_buffer_delay_us as f64 / 1000.0
    }

    pub fn decode_ms(&self) -> f64 {
        self.latest.decode_duration_us as f64 / 1000.0
    }

    pub fn playout_ms(&self) -> f64 {
        self.latest.playout_buffer_duration_us as f64 / 1000.0
    }

    pub fn total_ms(&self) -> f64 {
        self.latest.total_ms() as f64
    }

    pub fn avg_total_ms(&self) -> f64 {
        self.total_history.average() / 1000.0
    }

    pub fn sparkline_tenths_ms(&self) -> Vec<u64> {
        self.total_history
            .iter()
            .map(|&us| (us + 50) / 100)
            .collect()
    }
}

/// Connected client transport and link health model.
#[derive(Debug, Clone)]
pub struct ClientStatusModel {
    pub connection_mode: ClientConnectionMode,
    pub device_name: String,
    pub rtt_ms: f64,
    pub rtt_history: RollingWindow<u64>,
    pub packet_loss_rate: f32,
    pub packets_sent: u64,
    pub packets_recv: u64,
    pub packets_lost: u64,
    pub fec_recoveries: u64,
    pub fec_scheme: String,
    pub bitrate_mbps: f32,
}

impl ClientStatusModel {
    pub fn new(capacity: usize) -> Self {
        Self {
            connection_mode: ClientConnectionMode::Disconnected,
            device_name: "Meta Quest 3".to_string(),
            rtt_ms: 0.0,
            rtt_history: RollingWindow::new(capacity),
            packet_loss_rate: 0.0,
            packets_sent: 0,
            packets_recv: 0,
            packets_lost: 0,
            fec_recoveries: 0,
            fec_scheme: "Reed-Solomon RS(10,4)".to_string(),
            bitrate_mbps: 0.0,
        }
    }

    pub fn update(&mut self, telem: ClientTransportTelemetry) {
        self.connection_mode = telem.mode;
        self.rtt_ms = telem.rtt_ms() as f64;
        self.rtt_history.push((self.rtt_ms * 10.0).round() as u64); // tenths of ms
        self.packet_loss_rate = telem.packet_loss_rate;
        self.packets_sent = telem.packets_sent;
        self.packets_recv = telem.packets_recv;
        self.packets_lost = telem.packets_lost;
        self.fec_recoveries = telem.fec_recoveries;
        self.bitrate_mbps = telem.bitrate_mbps;
    }

    pub fn loss_percentage(&self) -> f32 {
        self.packet_loss_rate * 100.0
    }
}

/// Audio pipeline status model.
#[derive(Debug, Clone)]
pub struct AudioStatusModel {
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

impl Default for AudioStatusModel {
    fn default() -> Self {
        Self {
            active: true,
            codec: "Opus (Low Delay)".to_string(),
            sample_rate: 48000,
            channels: 2,
            frame_size_ms: 2.5,
            bitrate_kbps: 128,
            jitter_us: 400,
            buffer_delay_us: 2500,
            buffer_health_pct: 100.0,
            plc_count: 0,
        }
    }
}

impl AudioStatusModel {
    pub fn update(&mut self, telem: AudioStreamTelemetry) {
        self.active = telem.active;
        self.codec = telem.codec;
        self.sample_rate = telem.sample_rate;
        self.channels = telem.channels;
        self.frame_size_ms = telem.frame_size_ms;
        self.bitrate_kbps = telem.bitrate_kbps;
        self.jitter_us = telem.jitter_us;
        self.buffer_delay_us = telem.buffer_delay_us;
        self.buffer_health_pct = telem.buffer_health_pct;
        self.plc_count = telem.plc_count;
    }

    pub fn jitter_ms(&self) -> f64 {
        self.jitter_us as f64 / 1000.0
    }

    pub fn buffer_delay_ms(&self) -> f64 {
        self.buffer_delay_us as f64 / 1000.0
    }
}

/// Serializable snapshot representing a moment in host streaming state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardSnapshot {
    pub uptime_secs: u64,
    pub streaming: bool,
    pub displays: Vec<DisplaySnapshot>,
    pub video_latency: FrameLatencyBreakdown,
    pub audio_latency: AudioLatencyBreakdown,
    pub client: ClientTransportTelemetry,
    pub audio: AudioStreamTelemetry,
}

/// Snapshot of an individual display for IPC.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplaySnapshot {
    pub id: u16,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub refresh_rate: u32,
    pub fps: f32,
    pub bitrate_mbps: f32,
    pub codec: String,
    pub is_virtual: bool,
}

/// Complete pure dashboard state model aggregating telemetry over rolling windows.
#[derive(Debug, Clone)]
pub struct DashboardModel {
    pub window_size: usize,
    pub uptime_secs: u64,
    pub streaming: bool,
    pub displays: Vec<DisplayMetricModel>,
    pub video_latency: LatencyWaterfallModel,
    pub audio_latency: AudioLatencyModel,
    pub client: ClientStatusModel,
    pub audio: AudioStatusModel,
    pub frame_counter: u64,
}

impl DashboardModel {
    /// Creates a new dashboard model with the given rolling window capacity.
    pub fn new(window_size: usize) -> Self {
        Self {
            window_size,
            uptime_secs: 0,
            streaming: false,
            displays: Vec::new(),
            video_latency: LatencyWaterfallModel::new(window_size),
            audio_latency: AudioLatencyModel::new(window_size),
            client: ClientStatusModel::new(window_size),
            audio: AudioStatusModel::default(),
            frame_counter: 0,
        }
    }

    /// Ingests a full snapshot from a data source.
    pub fn apply_snapshot(&mut self, snap: DashboardSnapshot) {
        self.uptime_secs = snap.uptime_secs;
        self.streaming = snap.streaming;
        self.frame_counter += 1;

        // Ingest displays
        for d in snap.displays {
            if let Some(existing) = self
                .displays
                .iter_mut()
                .find(|disp| disp.display_id == d.id)
            {
                existing.width = d.width;
                existing.height = d.height;
                existing.refresh_rate = d.refresh_rate;
                existing.codec = d.codec;
                existing.is_virtual = d.is_virtual;
                existing.update_metrics(d.fps as f64, d.bitrate_mbps as f64);
            } else {
                let mut new_disp =
                    DisplayMetricModel::new(d.id, d.name, d.width, d.height, d.refresh_rate);
                new_disp.codec = d.codec;
                new_disp.is_virtual = d.is_virtual;
                new_disp.update_metrics(d.fps as f64, d.bitrate_mbps as f64);
                self.displays.push(new_disp);
            }
        }

        // Retain only currently active displays in snapshot
        // (if snapshot displays are specified)
        if !self.displays.is_empty() && self.displays.len() > 10 {
            self.displays.truncate(10);
        }

        self.video_latency.push(snap.video_latency);
        self.audio_latency.push(snap.audio_latency);
        self.client.update(snap.client);
        self.audio.update(snap.audio);
    }

    /// Ingests a new video frame latency measurement.
    pub fn apply_frame_latency(&mut self, lat: FrameLatencyBreakdown) {
        self.video_latency.push(lat);
    }

    /// Ingests a new audio frame latency measurement.
    pub fn apply_audio_latency(&mut self, lat: AudioLatencyBreakdown) {
        self.audio_latency.push(lat);
    }

    /// Resets all accumulated rolling windows and statistics.
    pub fn reset_stats(&mut self) {
        let cap = self.window_size;
        self.video_latency = LatencyWaterfallModel::new(cap);
        self.audio_latency = AudioLatencyModel::new(cap);
        self.client = ClientStatusModel::new(cap);
        for d in &mut self.displays {
            d.fps_history.clear();
            d.bitrate_history.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rolling_window_numeric_aggregation() {
        let mut window = RollingWindow::<u64>::new(5);
        assert!(window.is_empty());
        assert_eq!(window.average(), 0.0);
        assert_eq!(window.min(), None);
        assert_eq!(window.max(), None);

        window.push(10);
        window.push(20);
        window.push(30);

        assert_eq!(window.len(), 3);
        assert_eq!(window.average(), 20.0);
        assert_eq!(window.min(), Some(10));
        assert_eq!(window.max(), Some(30));
        assert_eq!(window.latest(), Some(&30));

        // Push past capacity
        window.push(40);
        window.push(50);
        assert_eq!(window.len(), 5);
        assert_eq!(window.min(), Some(10));

        window.push(60); // 10 is evicted, now [20, 30, 40, 50, 60]
        assert_eq!(window.len(), 5);
        assert_eq!(window.min(), Some(20));
        assert_eq!(window.max(), Some(60));
        assert_eq!(window.average(), 40.0);

        let sparkline = window.as_sparkline();
        assert_eq!(sparkline, vec![20, 30, 40, 50, 60]);
    }

    #[test]
    fn test_rolling_window_percentiles() {
        let mut window = RollingWindow::new(10);
        for val in 1..=10 {
            window.push(val * 10); // 10, 20, 30, ..., 100
        }

        assert_eq!(window.percentile(0.0), Some(10.0));
        assert_eq!(window.percentile(100.0), Some(100.0));
        assert_eq!(window.percentile(50.0), Some(55.0));
    }

    #[test]
    fn test_latency_waterfall_model_computations() {
        let mut model = LatencyWaterfallModel::new(10);
        let mut sample = FrameLatencyBreakdown::new(1, 1000);
        sample.capture_duration_us = 1200;
        sample.encode_duration_us = 3000;
        sample.host_queue_duration_us = 300;
        sample.transport_duration_us = 1000;
        sample.client_decode_duration_us = 2500;
        sample.client_render_duration_us = 800;

        model.push(sample);

        assert!((model.capture_ms() - 1.2).abs() < 0.001);
        assert!((model.encode_ms() - 3.0).abs() < 0.001);
        assert!((model.queue_ms() - 0.3).abs() < 0.001);
        assert!((model.transport_ms() - 1.0).abs() < 0.001);
        assert!((model.decode_ms() - 2.5).abs() < 0.001);
        assert!((model.render_ms() - 0.8).abs() < 0.001);
        assert!((model.total_ms() - 8.8).abs() < 0.001);

        assert_eq!(model.sparkline_total_tenths_ms(), vec![88]);

        // Stage percentages
        let enc_pct = model.stage_percentage(VideoWaterfallStage::Encode);
        assert!((enc_pct - (3000.0 / 8800.0 * 100.0)).abs() < 0.01);
    }

    #[test]
    fn test_dashboard_model_apply_snapshot() {
        let mut dashboard = DashboardModel::new(10);

        let snap = DashboardSnapshot {
            uptime_secs: 42,
            streaming: true,
            displays: vec![DisplaySnapshot {
                id: 1,
                name: "Virtual-1".into(),
                width: 2560,
                height: 1440,
                refresh_rate: 90,
                fps: 89.9,
                bitrate_mbps: 75.4,
                codec: "AV1".into(),
                is_virtual: true,
            }],
            video_latency: FrameLatencyBreakdown {
                frame_id: 100,
                capture_start_us: 1000,
                capture_duration_us: 1000,
                encode_duration_us: 2500,
                host_queue_duration_us: 200,
                transport_duration_us: 800,
                client_decode_duration_us: 3000,
                client_render_duration_us: 500,
                total_motion_to_photon_us: 8000,
            },
            audio_latency: AudioLatencyBreakdown {
                frame_id: 50,
                capture_start_us: 1000,
                capture_duration_us: 300,
                encode_duration_us: 400,
                transport_duration_us: 800,
                jitter_buffer_delay_us: 2500,
                decode_duration_us: 200,
                playout_buffer_duration_us: 500,
                total_latency_us: 4700,
            },
            client: ClientTransportTelemetry {
                mode: ClientConnectionMode::UsbAdb,
                rtt_us: 1100,
                smoothed_rtt_us: 1050,
                packet_loss_rate: 0.0001,
                packets_sent: 1000,
                packets_recv: 999,
                packets_lost: 1,
                fec_recoveries: 1,
                bitrate_mbps: 85.0,
            },
            audio: AudioStreamTelemetry {
                active: true,
                codec: "Opus".into(),
                sample_rate: 48000,
                channels: 2,
                frame_size_ms: 2.5,
                bitrate_kbps: 128,
                jitter_us: 350,
                buffer_delay_us: 2500,
                buffer_health_pct: 100.0,
                plc_count: 0,
            },
        };

        dashboard.apply_snapshot(snap);

        assert_eq!(dashboard.uptime_secs, 42);
        assert!(dashboard.streaming);
        assert_eq!(dashboard.displays.len(), 1);
        assert_eq!(dashboard.displays[0].width, 2560);
        assert!((dashboard.displays[0].fps - 89.9).abs() < 0.01);
        assert!((dashboard.video_latency.total_ms() - 8.0).abs() < 0.001);
        assert!((dashboard.audio_latency.total_ms() - 4.7).abs() < 0.001);
        assert_eq!(
            dashboard.client.connection_mode,
            ClientConnectionMode::UsbAdb
        );
        assert_eq!(dashboard.audio.sample_rate, 48000);
    }
}

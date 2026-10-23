//! Debug HUD overlay rendering FPS, frame pacing, and latency metrics.

pub mod font;
pub mod latency;

use std::collections::VecDeque;
use std::time::Instant;

use crate::config::{ClientConfig, DisplayMode};
use crate::decoder::DecoderStats;
use crate::openxr::FramePacingMetrics;
use linux_quest_protocol::{ClientTransportTelemetry, FrameLatencyBreakdown, MeasuredFrameLatency};

/// Result of a completed controller shortcut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutAction {
    /// Both thumbstick clicks pressed together.
    ToggleHud,
    /// Left thumbstick click alone.
    TogglePanel,
}

/// Detects the HUD and panel shortcuts from thumbstick clicks.
///
/// The action is decided when all clicks are released, so pressing both sticks a frame apart
/// still counts as the HUD combo and never also opens the panel.
#[derive(Debug, Default)]
pub struct ShortcutDetector {
    left_seen: bool,
    right_seen: bool,
    both_seen: bool,
}

impl ShortcutDetector {
    pub fn update(&mut self, left_click: bool, right_click: bool) -> Option<ShortcutAction> {
        if left_click || right_click {
            self.left_seen |= left_click;
            self.right_seen |= right_click;
            self.both_seen |= left_click && right_click;
            return None;
        }
        let action = if self.both_seen {
            Some(ShortcutAction::ToggleHud)
        } else if self.left_seen {
            Some(ShortcutAction::TogglePanel)
        } else {
            None
        };
        *self = Self::default();
        action
    }
}

/// Performance telemetry data collected for HUD display.
#[derive(Debug, Clone)]
pub struct HudSnapshot {
    /// Current rendered frames per second.
    pub fps: f32,
    /// Target display refresh rate in Hz.
    pub target_fps: f32,
    /// Software estimate in milliseconds: frame-pacing budget + decoder time + capped
    /// wait_frame time. Omits host capture, encode, transport and panel scanout, so it is
    /// not glass-to-glass latency (only an optical measurement is; see docs/latency-measurement.md).
    pub motion_to_photon_ms: f32,
    /// Decoder latency in milliseconds.
    pub decode_latency_ms: f32,
    /// Network jitter / transit estimate in milliseconds.
    pub network_jitter_ms: f32,
    /// Active decoder hardware name.
    pub decoder_name: String,
    /// Active display presentation mode.
    pub display_mode: DisplayMode,
    /// Total received packets.
    pub packets_received: u64,
    /// Packets recovered via FEC.
    pub fec_recovered: u64,
    /// Packets lost unrecoverable.
    pub packets_lost: u64,
}

/// Head-locked debug HUD calculating real-time frame pacing and metrics.
pub struct DebugHud {
    frame_times: VecDeque<Instant>,
    last_snapshot: HudSnapshot,
    hud_width: u32,
    hud_height: u32,
    input_latency_us: u64,
    received_kbps: u32,
    breakdown: Option<FrameLatencyBreakdown>,
    transport: Option<ClientTransportTelemetry>,
    measured: Option<MeasuredFrameLatency>,
}

impl Default for DebugHud {
    fn default() -> Self {
        Self::new(512, 256)
    }
}

impl DebugHud {
    /// Creates a new debug HUD with specified texture resolution.
    pub fn new(hud_width: u32, hud_height: u32) -> Self {
        Self {
            frame_times: VecDeque::with_capacity(128),
            last_snapshot: HudSnapshot {
                fps: 0.0,
                target_fps: 90.0,
                motion_to_photon_ms: 0.0,
                decode_latency_ms: 0.0,
                network_jitter_ms: 0.0,
                decoder_name: "uninitialized".to_string(),
                display_mode: DisplayMode::CurvedCylinder,
                packets_received: 0,
                fec_recovered: 0,
                packets_lost: 0,
            },
            hud_width,
            hud_height,
            input_latency_us: 0,
            received_kbps: 0,
            breakdown: None,
            transport: None,
            measured: None,
        }
    }

    /// Records the newest measured (timestamped) frame latency, or `None` when no fresh
    /// measurement exists, so the HUD falls back to the labeled estimate.
    pub fn set_measured_latency(&mut self, measured: Option<MeasuredFrameLatency>) {
        self.measured = measured;
    }

    /// The measured latency currently shown, if any.
    pub fn measured_latency(&self) -> Option<&MeasuredFrameLatency> {
        self.measured.as_ref()
    }

    /// Records the latest per-stage latency breakdown reported by telemetry.
    pub fn set_latency_breakdown(&mut self, breakdown: FrameLatencyBreakdown) {
        self.breakdown = Some(breakdown);
    }

    /// Records the latest transport telemetry (bitrate, loss, FEC recoveries).
    pub fn set_transport_telemetry(&mut self, telemetry: ClientTransportTelemetry) {
        self.transport = Some(telemetry);
    }

    /// Records a new frame presentation and updates running telemetry metrics.
    pub fn update(
        &mut self,
        config: &ClientConfig,
        pacing: &FramePacingMetrics,
        decoder_stats: &DecoderStats,
        packets_received: u64,
        fec_recovered: u64,
        packets_lost: u64,
    ) {
        let now = Instant::now();
        self.frame_times.push_back(now);

        // Retain frame timestamps within last 1.0 second
        while let Some(&first) = self.frame_times.front() {
            if now.duration_since(first).as_secs_f32() > 1.0 {
                self.frame_times.pop_front();
            } else {
                break;
            }
        }

        let fps = if self.frame_times.len() > 1 {
            let duration = now
                .duration_since(*self.frame_times.front().unwrap())
                .as_secs_f32();
            if duration > 0.0 {
                (self.frame_times.len() - 1) as f32 / duration
            } else {
                config.target_refresh_rate
            }
        } else {
            config.target_refresh_rate
        };

        let decode_ms = decoder_stats.last_latency_us as f32 / 1000.0;
        let jitter_ms = (pacing.wait_frame_duration_us as f32 / 1000.0).min(5.0);
        let m2p_ms = (pacing.motion_to_photon_latency_us as f32 / 1000.0) + decode_ms + jitter_ms;

        self.last_snapshot = HudSnapshot {
            fps,
            target_fps: config.target_refresh_rate,
            motion_to_photon_ms: m2p_ms,
            decode_latency_ms: decode_ms,
            network_jitter_ms: jitter_ms,
            decoder_name: decoder_stats.decoder_name.clone(),
            display_mode: config.display_mode,
            packets_received,
            fec_recovered,
            packets_lost,
        };
    }

    /// Records the most recent client pointer processing latency for the telemetry HUD.
    pub fn set_input_latency_us(&mut self, latency_us: u64) {
        self.input_latency_us = latency_us;
    }

    /// Records the video rate actually arriving from the host, in kbit/s.
    pub fn set_received_kbps(&mut self, kbps: u32) {
        self.received_kbps = kbps;
    }

    /// Formats the HUD telemetry summary text.
    pub fn formatted_text(&self) -> String {
        let mut text = self.base_text();
        if let Some(t) = &self.transport {
            text.push_str(&format!(
                "\nBitrate: {:.1} Mbps | Loss: {:.2}% | FEC rec: {}",
                t.bitrate_mbps,
                t.loss_percentage(),
                t.fec_recoveries
            ));
        }
        if let Some(b) = &self.breakdown {
            // A zero stage was not reported; show "--" rather than a measured 0.0.
            let ms = |us: u32| {
                if us == 0 {
                    "--".to_string()
                } else {
                    format!("{:.1}", us as f32 / 1000.0)
                }
            };
            text.push_str(&format!(
                "\nCap {} Enc {} Q {} Net {}\nDec {} Rnd {} | stage sum {:.1}ms",
                ms(b.capture_duration_us),
                ms(b.encode_duration_us),
                ms(b.host_queue_duration_us),
                ms(b.transport_duration_us),
                ms(b.client_decode_duration_us),
                ms(b.client_render_duration_us),
                (b.capture_duration_us
                    + b.encode_duration_us
                    + b.host_queue_duration_us
                    + b.transport_duration_us
                    + b.client_decode_duration_us
                    + b.client_render_duration_us) as f32
                    / 1000.0,
            ));
        }
        text
    }

    /// Latency lines: measured capture-to-present when the host clock is calibrated, else the
    /// software estimate, labeled as such. Measured single-clock stages are shown either way.
    fn latency_lines(&self) -> String {
        let ms =
            |v: Option<u64>| v.map_or("--".to_string(), |us| format!("{:.1}", us as f32 / 1000.0));
        let est = format!(
            "Est (SW, not g2g): {:.1}ms | Dec: {:.1}ms",
            self.last_snapshot.motion_to_photon_ms, self.last_snapshot.decode_latency_ms
        );
        let Some(m) = &self.measured else {
            return est;
        };
        let stages = format!(
            "Enc {} Snd {} Net {} Buf {} Dec {} Pres {}",
            ms(m.encode_us),
            ms(m.send_us),
            ms(m.network_us),
            ms(m.jitter_us),
            ms(m.decode_us),
            ms(m.present_us)
        );
        match (m.capture_to_present_us, m.clock_rtt_us) {
            (Some(total), Some(rtt)) => format!(
                "Meas cap>submit: {:.1}ms +-{:.1} (no scanout)\n{}",
                total as f32 / 1000.0,
                rtt as f32 / 2000.0,
                stages
            ),
            _ => format!("{est}\nMeas stages (no clock sync): {stages}"),
        }
    }

    fn base_text(&self) -> String {
        format!(
            "FPS: {:.1} / {:.0}Hz\n{}\nInput: {:.2}ms\nCodec: {}\nPackets: {} | FEC: {} | Loss: {}\nRx: {:.1} Mbps",
            self.last_snapshot.fps,
            self.last_snapshot.target_fps,
            self.latency_lines(),
            self.input_latency_us as f32 / 1000.0,
            self.last_snapshot.decoder_name,
            self.last_snapshot.packets_received,
            self.last_snapshot.fec_recovered,
            self.last_snapshot.packets_lost,
            self.received_kbps as f32 / 1000.0,
        )
    }

    /// Generates an RGBA pixel buffer representing the HUD overlay.
    pub fn render_to_rgba(&self) -> Vec<u8> {
        let mut buffer = vec![0u8; (self.hud_width * self.hud_height * 4) as usize];

        // Fill background with semi-transparent dark charcoal (R: 16, G: 18, B: 24, A: 200)
        for pixel in buffer.as_chunks_mut::<4>().0 {
            pixel[0] = 16;
            pixel[1] = 18;
            pixel[2] = 24;
            pixel[3] = 200;
        }

        // Draw top accent bar (cyan / teal: R: 0, G: 220, B: 255, A: 255)
        let bar_height = (self.hud_height / 16).max(2);
        for y in 0..bar_height {
            for x in 0..self.hud_width {
                let idx = ((y * self.hud_width + x) * 4) as usize;
                buffer[idx] = 0;
                buffer[idx + 1] = 220;
                buffer[idx + 2] = 255;
                buffer[idx + 3] = 255;
            }
        }

        // Draw telemetry text using 5x7 font
        let text = self.formatted_text();
        font::draw_text_rgba(
            &mut buffer,
            self.hud_width,
            self.hud_height,
            16,
            bar_height + 12,
            2,
            &text,
            [255, 255, 255, 255],
        );

        buffer
    }

    /// Returns the current snapshot of metrics.
    /// Video received, in kbit/s, as last set by [`Self::set_received_kbps`].
    pub fn received_kbps(&self) -> u32 {
        self.received_kbps
    }

    pub fn snapshot(&self) -> &HudSnapshot {
        &self.last_snapshot
    }

    /// HUD pixel width.
    pub fn width(&self) -> u32 {
        self.hud_width
    }

    /// HUD pixel height.
    pub fn height(&self) -> u32 {
        self.hud_height
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hud_update_and_formatting() {
        let mut hud = DebugHud::new(256, 128);
        let config = ClientConfig::default();
        let pacing = FramePacingMetrics {
            motion_to_photon_latency_us: 8500,
            wait_frame_duration_us: 1200,
            render_duration_us: 300,
            predicted_display_time: 1_000_000,
            should_render: true,
        };
        let stats = DecoderStats {
            last_latency_us: 2400,
            decoder_name: "c2.qti.av1.decoder.low_latency".to_string(),
            ..Default::default()
        };

        hud.update(&config, &pacing, &stats, 100, 5, 0);
        hud.set_input_latency_us(1250);
        hud.set_received_kbps(28_400);

        let text = hud.formatted_text();
        assert!(text.contains("Rx: 28.4 Mbps"));
        assert!(text.contains("Est (SW, not g2g): 12.1ms"));
        assert!(text.contains("Dec: 2.4ms"));
        assert!(text.contains("Input: 1.25ms"));
        assert!(text.contains("c2.qti.av1"));

        let rgba = hud.render_to_rgba();
        assert_eq!(rgba.len(), 256 * 128 * 4);
        assert!(rgba.as_chunks::<4>().0.contains(&[255, 255, 255, 255]));
    }

    fn measured(calibrated: bool) -> MeasuredFrameLatency {
        MeasuredFrameLatency {
            frame_id: 3,
            capture_to_encode_us: Some(500),
            encode_us: Some(3_000),
            send_us: Some(500),
            network_us: calibrated.then_some(2_000),
            jitter_us: Some(1_000),
            decode_us: Some(2_500),
            present_us: Some(5_500),
            capture_to_present_us: calibrated.then_some(15_000),
            clock_rtt_us: calibrated.then_some(1_000),
        }
    }

    #[test]
    fn hud_prefers_measured_latency_and_marks_estimates_otherwise() {
        let mut hud = DebugHud::new(256, 128);
        assert!(hud.formatted_text().contains("Est (SW, not g2g)"));
        assert!(!hud.formatted_text().contains("Meas"));

        hud.set_measured_latency(Some(measured(true)));
        let text = hud.formatted_text();
        assert!(
            text.contains("Meas cap>submit: 15.0ms +-0.5 (no scanout)"),
            "{text}"
        );
        assert!(text.contains("Enc 3.0 Snd 0.5 Net 2.0 Buf 1.0 Dec 2.5 Pres 5.5"));
        assert!(!text.contains("Est (SW"));

        hud.set_measured_latency(Some(measured(false)));
        let text = hud.formatted_text();
        assert!(text.contains("Est (SW, not g2g)"));
        assert!(text.contains("Meas stages (no clock sync): Enc 3.0 Snd 0.5 Net -- "));

        hud.set_measured_latency(None);
        assert!(!hud.formatted_text().contains("Meas"));
    }

    #[test]
    fn hud_shows_stage_breakdown_bitrate_and_loss() {
        let mut hud = DebugHud::new(256, 128);
        let mut b = FrameLatencyBreakdown::new(1, 0);
        b.capture_duration_us = 1000;
        b.encode_duration_us = 2000;
        b.host_queue_duration_us = 500;
        b.transport_duration_us = 3000;
        b.client_decode_duration_us = 2500;
        b.client_render_duration_us = 1000;
        hud.set_latency_breakdown(b);
        hud.set_transport_telemetry(ClientTransportTelemetry {
            bitrate_mbps: 64.0,
            packet_loss_rate: 0.015,
            fec_recoveries: 7,
            ..Default::default()
        });
        let text = hud.formatted_text();
        assert!(text.contains("Bitrate: 64.0 Mbps | Loss: 1.50% | FEC rec: 7"));
        assert!(text.contains("Cap 1.0 Enc 2.0 Q 0.5 Net 3.0"));
        assert!(text.contains("Dec 2.5 Rnd 1.0 | stage sum 10.0ms"));
    }

    #[test]
    fn shortcut_both_sticks_toggle_hud_even_when_pressed_a_frame_apart() {
        let mut d = ShortcutDetector::default();
        assert_eq!(d.update(true, false), None);
        assert_eq!(d.update(true, true), None);
        assert_eq!(d.update(false, true), None);
        assert_eq!(d.update(false, false), Some(ShortcutAction::ToggleHud));
        assert_eq!(d.update(false, false), None);
    }

    #[test]
    fn shortcut_left_alone_toggles_panel_and_right_alone_does_nothing() {
        let mut d = ShortcutDetector::default();
        d.update(true, false);
        assert_eq!(d.update(false, false), Some(ShortcutAction::TogglePanel));
        d.update(false, true);
        assert_eq!(d.update(false, false), None);
    }
}

//! Debug HUD overlay rendering FPS, frame pacing, and latency metrics.

pub mod font;

use std::collections::VecDeque;
use std::time::Instant;

use crate::config::{ClientConfig, DisplayMode};
use crate::decoder::DecoderStats;
use crate::openxr::FramePacingMetrics;

/// Performance telemetry data collected for HUD display.
#[derive(Debug, Clone)]
pub struct HudSnapshot {
    /// Current rendered frames per second.
    pub fps: f32,
    /// Target display refresh rate in Hz.
    pub target_fps: f32,
    /// Motion-to-photon latency in milliseconds.
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
        }
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

    /// Formats the HUD telemetry summary text.
    pub fn formatted_text(&self) -> String {
        format!(
            "FPS: {:.1} / {:.0}Hz\nM2P: {:.1}ms | Dec: {:.1}ms\nInput: {:.2}ms\nCodec: {}\nPackets: {} | FEC: {} | Loss: {}",
            self.last_snapshot.fps,
            self.last_snapshot.target_fps,
            self.last_snapshot.motion_to_photon_ms,
            self.last_snapshot.decode_latency_ms,
            self.input_latency_us as f32 / 1000.0,
            self.last_snapshot.decoder_name,
            self.last_snapshot.packets_received,
            self.last_snapshot.fec_recovered,
            self.last_snapshot.packets_lost,
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

        let text = hud.formatted_text();
        assert!(text.contains("M2P: 12.1ms"));
        assert!(text.contains("Dec: 2.4ms"));
        assert!(text.contains("Input: 1.25ms"));
        assert!(text.contains("c2.qti.av1"));

        let rgba = hud.render_to_rgba();
        assert_eq!(rgba.len(), 256 * 128 * 4);
        assert!(rgba.as_chunks::<4>().0.contains(&[255, 255, 255, 255]));
    }
}

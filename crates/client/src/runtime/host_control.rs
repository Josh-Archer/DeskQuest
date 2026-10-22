//! The runtime's side of the host settings channel: the status the host sees, and applying the
//! layout and settings the host sends. See `crate::host_control`.

use linux_quest_protocol::{
    HeadsetSettings, HeadsetStats, HeadsetStatus, ScreenCurve, ScreenPlacement,
};

use super::{extra_screen_slot, QuestClientRuntime};
use crate::host_control::{convert, HostCommand};
use crate::spatial::ScreenTransform;

/// Display ids above this are ignored in a layout from the host, so a bad request cannot grow
/// the saved layout without bound.
pub const MAX_LAYOUT_DISPLAY_ID: u32 = 15;

impl QuestClientRuntime {
    /// Names the headset model in the status (on Android, `ro.product.model`).
    pub fn set_device_name(&mut self, name: impl Into<String>) {
        self.device_name = name.into();
    }

    /// True once after anything the host sees changed (layout, settings) or it asked for a
    /// report.
    pub fn take_host_status_dirty(&mut self) -> bool {
        std::mem::take(&mut self.host_status_dirty)
    }

    /// Refresh rates the display supports (empty when the runtime cannot tell).
    pub fn supported_refresh_rates(&self) -> Vec<f32> {
        self.openxr_context
            .refresh_rate_manager()
            .map(|m| m.supported_rates().to_vec())
            .unwrap_or_default()
    }

    /// Everything the host is told about this headset.
    pub fn headset_status(&self) -> HeadsetStatus {
        let mut screens = vec![convert::placement_of(0, &self.screen_transform)];
        let mut showing = Vec::new();
        if self.decoder.stats().frames_rendered > 0 {
            showing.push(0);
        }
        for (&id, stream) in &self.extra_streams {
            screens.push(convert::placement_of(u32::from(id), &stream.transform));
            if stream.shown {
                showing.push(u32::from(id));
            }
        }
        // Places saved for screens whose stream has not started.
        for (&id, transform) in &self.active_layout.screens {
            if !screens.iter().any(|s| s.display_id == id) {
                screens.push(convert::placement_of(id, transform));
            }
        }
        screens.sort_by_key(|s| s.display_id);

        let hud = self.hud.snapshot();
        let decoder = self.decoder.stats();
        HeadsetStatus {
            device: self.device_name.clone(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            settings: convert::headset_settings_of(
                self.settings_panel.settings(),
                convert::curve_of(&self.screen_transform),
            ),
            screens,
            showing,
            refresh_rates_hz: self.supported_refresh_rates(),
            stats: HeadsetStats {
                render_fps: hud.fps,
                motion_to_photon_ms: hud.motion_to_photon_ms,
                decode_latency_us: decoder.last_latency_us,
                frames_decoded: decoder.frames_decoded,
                packets_received: hud.packets_received,
                packets_lost: hud.packets_lost,
                fec_recovered: hud.fec_recovered,
                received_kbps: self.hud.received_kbps(),
            },
        }
    }

    /// Carries out one request from the host. Every request is answered by the next status.
    pub fn apply_host_command(&mut self, command: HostCommand) {
        match command {
            HostCommand::ReportStatus => {}
            HostCommand::SetLayout(screens) => self.apply_host_layout(&screens),
            HostCommand::SetSettings(settings) => self.apply_host_settings(&settings),
        }
        self.host_status_dirty = true;
    }

    /// Where display `id` is now, or would start.
    fn screen_for(&self, id: u32) -> ScreenTransform {
        if id == 0 {
            return self.screen_transform.clone();
        }
        if let Some(stream) = u16::try_from(id)
            .ok()
            .and_then(|id| self.extra_streams.get(&id))
        {
            return stream.transform.clone();
        }
        if let Some(saved) = self.active_layout.get(id) {
            return saved.clone();
        }
        match u16::try_from(id) {
            Ok(id) => self.screen_transform.beside(extra_screen_slot(id)),
            Err(_) => ScreenTransform::for_display(id),
        }
    }

    fn apply_host_layout(&mut self, screens: &[ScreenPlacement]) {
        let mut moved = 0;
        for placement in screens {
            if placement.display_id > MAX_LAYOUT_DISPLAY_ID || !placement.is_valid() {
                tracing::warn!(
                    display_id = placement.display_id,
                    "ignoring a screen placement from the host"
                );
                continue;
            }
            let base = self.screen_for(placement.display_id);
            let transform = convert::transform_from_placement(placement, &base);
            self.apply_screen_transform(placement.display_id, transform);
            moved += 1;
        }
        tracing::info!(screens = moved, "screens placed by the host");
        self.save_active_layout();
    }

    fn apply_host_settings(&mut self, wanted: &HeadsetSettings) {
        let before = self.settings_panel.settings().clone();
        let next =
            convert::apply_headset_settings(&before, wanted, &self.supported_refresh_rates());
        if next.refresh_rate != before.refresh_rate {
            self.set_target_refresh_rate(next.refresh_rate);
        }
        if next.passthrough != before.passthrough {
            if let Err(e) = self.set_passthrough_enabled(next.passthrough) {
                tracing::warn!(error = ?e, "could not change passthrough");
            }
        }
        self.config.enable_hud = next.hud_enabled;
        // The codec is only a preference here: the decoder follows what the host sends, and
        // switching its MIME type under a running stream would stop the video.
        if wanted.curve != convert::curve_of(&self.screen_transform) {
            self.bend_all_screens(wanted.curve);
        }
        self.settings_panel.replace_settings(next);
        self.persist_settings();
        tracing::info!(settings = ?wanted, "settings changed by the host");
    }

    /// Gives every screen (shown or only placed) the same curve.
    fn bend_all_screens(&mut self, curve: ScreenCurve) {
        let mut ids: Vec<u32> = std::iter::once(0)
            .chain(self.extra_streams.keys().map(|&id| u32::from(id)))
            .chain(self.active_layout.screens.keys().copied())
            .collect();
        ids.sort_unstable();
        ids.dedup();
        for id in ids {
            let mut transform = self.screen_for(id);
            convert::apply_curve(&mut transform, curve);
            self.apply_screen_transform(id, transform);
        }
        self.save_active_layout();
    }

    fn save_active_layout(&self) {
        if let Some(store) = &self.layout_store {
            if let Err(e) = store.save(&self.active_layout) {
                tracing::warn!(error = ?e, "could not save the screen layout");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ClientConfig, DisplayMode};
    use std::time::{Duration, Instant};

    fn runtime() -> QuestClientRuntime {
        QuestClientRuntime::new(ClientConfig::default()).unwrap()
    }

    fn video_packet(stream_id: u16) -> linux_quest_protocol::packet::Packet {
        use linux_quest_protocol::packet::{Packet, PacketHeader, PacketType};
        use linux_quest_protocol::video::{VideoChunk, VideoChunkMeta, VideoCodec};
        let meta = VideoChunkMeta {
            frame_id: 1,
            chunk_index: 0,
            total_chunks: 1,
            codec: VideoCodec::Av1,
            is_keyframe: true,
            is_intra_refresh: false,
            width: 1280,
            height: 720,
            fps: 60,
            pts_us: 16_666,
        };
        let bytes = VideoChunk::new(meta, bytes::Bytes::from_static(&[0x0A, 0x02, 0x11, 0x22]))
            .serialize()
            .unwrap();
        let header = PacketHeader::new(PacketType::VideoFrameChunk, stream_id, 1, 16_666, &bytes);
        Packet::new(header, bytes)
    }

    #[test]
    fn the_status_lists_the_screens_and_settings() {
        let mut rt = runtime();
        rt.set_device_name("Quest 3");
        let arrival = Instant::now();
        rt.ingest_packet(video_packet(0), arrival);
        rt.ingest_packet(video_packet(2), arrival);
        rt.step_frame_at(arrival + Duration::from_millis(10))
            .unwrap();

        let status = rt.headset_status();
        assert_eq!(status.device, "Quest 3");
        let ids: Vec<u32> = status.screens.iter().map(|s| s.display_id).collect();
        assert_eq!(ids, vec![0, 2]);
        assert!(status.showing.contains(&0));
        assert!(
            status.screen(2).unwrap().position[0] < 0.0,
            "stream 2 starts on the left"
        );
        assert_eq!(
            status.settings.refresh_rate_hz,
            ClientConfig::default().target_refresh_rate
        );
        assert!(status.settings.curve.is_curved());
    }

    #[test]
    fn a_layout_from_the_host_moves_shown_and_future_screens_and_is_reported() {
        let mut rt = runtime();
        assert!(!rt.take_host_status_dirty());
        let primary =
            ScreenPlacement::facing_user(0, 0.2, 0.0, 1.5, 1.3, 2.0, 1.125, ScreenCurve::Flat);
        // Display 3 has no stream yet; its place is kept for when it starts.
        let later = ScreenPlacement::facing_user(
            3,
            -0.9,
            0.0,
            1.4,
            1.4,
            1.6,
            0.9,
            ScreenCurve::Curved { radius_m: 2.0 },
        );
        rt.apply_host_command(HostCommand::SetLayout(vec![primary, later]));
        assert!(rt.take_host_status_dirty());

        let tf = rt.screen_transform();
        assert_eq!(tf.display_mode, DisplayMode::FlatQuad);
        assert!((tf.yaw - 0.2).abs() < 1e-3 && (tf.distance - 1.5).abs() < 1e-3);
        assert!((tf.quad_size.0 - 2.0).abs() < 1e-3);

        let reported = rt.headset_status();
        let three = reported.screen(3).expect("display 3 is reported");
        assert!((three.yaw_rad() + 0.9).abs() < 1e-3);

        // When stream 3 starts it appears where the host put it.
        let arrival = Instant::now();
        rt.ingest_packet(video_packet(3), arrival);
        let placed = rt.extra_screen_transform(3).unwrap();
        assert!((placed.yaw + 0.9).abs() < 1e-3);
    }

    #[test]
    fn invalid_or_far_off_display_ids_are_ignored() {
        let mut rt = runtime();
        let before = rt.headset_status().screens;
        let mut bad = before[0];
        bad.width_m = -1.0;
        let mut far = before[0];
        far.display_id = MAX_LAYOUT_DISPLAY_ID + 1;
        rt.apply_host_command(HostCommand::SetLayout(vec![bad, far]));
        assert_eq!(rt.headset_status().screens, before);
    }

    #[test]
    fn settings_from_the_host_change_the_headset_and_are_reported_back() {
        let mut rt = runtime();
        let mut wanted = rt.headset_status().settings;
        wanted.hud = false;
        wanted.refresh_rate_hz = 120.0;
        wanted.bitrate_mbps = 50;
        wanted.adaptive_bitrate = false;
        wanted.curve = ScreenCurve::Flat;
        rt.apply_host_command(HostCommand::SetSettings(wanted));

        let status = rt.headset_status();
        assert!(!status.settings.hud);
        assert_eq!(status.settings.refresh_rate_hz, 120.0);
        assert_eq!(status.settings.bitrate_mbps, 50);
        assert!(!status.settings.adaptive_bitrate);
        assert_eq!(status.settings.curve, ScreenCurve::Flat);
        assert!(status.screens.iter().all(|s| s.curve == ScreenCurve::Flat));
        assert!(!rt.config.enable_hud);
        assert_eq!(
            rt.openxr_context
                .refresh_rate_manager()
                .unwrap()
                .current_rate(),
            120.0
        );

        // Bending again keeps each screen's size.
        let width = status.screens[0].width_m;
        wanted.curve = ScreenCurve::Curved { radius_m: 2.5 };
        rt.apply_host_command(HostCommand::SetSettings(wanted));
        let bent = rt.headset_status();
        assert_eq!(bent.settings.curve, ScreenCurve::Curved { radius_m: 2.5 });
        assert!((bent.screens[0].width_m - width).abs() < 1e-3);
    }
}

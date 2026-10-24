//! In-headset settings panel: row model, pointer/controller navigation and RGBA rendering.

use std::net::SocketAddr;

use linux_quest_protocol::MouseButton;

use super::*;
use crate::hud::font;

/// One editable row of the panel, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelRow {
    Host,
    Transport,
    Codec,
    BitrateMode,
    Bitrate,
    RefreshRate,
    Monitors,
    Resolution,
    Layout,
    Passthrough,
    Audio,
    Hud,
    PrimaryButton,
}

impl PanelRow {
    pub const ALL: [PanelRow; 13] = [
        Self::Host,
        Self::Transport,
        Self::Codec,
        Self::BitrateMode,
        Self::Bitrate,
        Self::RefreshRate,
        Self::Monitors,
        Self::Resolution,
        Self::Layout,
        Self::Passthrough,
        Self::Audio,
        Self::Hud,
        Self::PrimaryButton,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Host => "Host",
            Self::Transport => "Transport",
            Self::Codec => "Codec",
            Self::BitrateMode => "Bitrate mode",
            Self::Bitrate => "Bitrate",
            Self::RefreshRate => "Refresh rate",
            Self::Monitors => "Monitors",
            Self::Resolution => "Resolution",
            Self::Layout => "Layout",
            Self::Passthrough => "Passthrough",
            Self::Audio => "Audio",
            Self::Hud => "Stats HUD",
            Self::PrimaryButton => "Primary click",
        }
    }
}

fn step<T: Copy + PartialEq>(options: &[T], current: T, dir: i32) -> T {
    let len = options.len() as i32;
    let idx = options.iter().position(|o| *o == current).unwrap_or(0) as i32;
    options[(idx + dir).rem_euclid(len) as usize]
}

/// The settings panel state. The caller feeds it input and runs the returned actions.
#[derive(Debug, Clone)]
pub struct SettingsPanel {
    settings: ClientSettings,
    hosts: Vec<SocketAddr>,
    visible: bool,
    selected: usize,
    dirty: bool,
    width: u32,
    height: u32,
}

impl SettingsPanel {
    pub fn new(settings: ClientSettings, width: u32, height: u32) -> Self {
        Self {
            settings: settings.sanitized(),
            hosts: Vec::new(),
            visible: false,
            selected: 0,
            dirty: false,
            width,
            height,
        }
    }

    pub fn settings(&self) -> &ClientSettings {
        &self.settings
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn toggle(&mut self) -> bool {
        self.visible = !self.visible;
        self.visible
    }

    pub fn selected_row(&self) -> PanelRow {
        PanelRow::ALL[self.selected]
    }

    /// Hosts the Host row cycles through (typically the discovery results).
    pub fn set_hosts(&mut self, hosts: Vec<SocketAddr>) {
        self.hosts = hosts;
    }

    /// Returns true once after a change that should be persisted.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Moves the selection by `dir` rows (wrapping).
    pub fn navigate(&mut self, dir: i32) {
        let len = PanelRow::ALL.len() as i32;
        self.selected = (self.selected as i32 + dir).rem_euclid(len) as usize;
    }

    /// Selects the row under panel UV coordinates (`v` = 0 at the top). Returns the row hit.
    pub fn row_at(&self, u: f32, v: f32) -> Option<PanelRow> {
        if !(0.0..=1.0).contains(&u) || !(0.0..=1.0).contains(&v) {
            return None;
        }
        let (top, row_h) = self.row_metrics();
        let y = v * self.height as f32 - top as f32;
        if y < 0.0 {
            return None;
        }
        PanelRow::ALL.get((y / row_h as f32) as usize).copied()
    }

    /// Pointer click at panel UV: the left half of a row steps back, the right half forward.
    pub fn click_at(&mut self, u: f32, v: f32) -> Vec<SettingsAction> {
        if !self.visible {
            return Vec::new();
        }
        let Some(row) = self.row_at(u, v) else {
            return Vec::new();
        };
        self.selected = PanelRow::ALL.iter().position(|r| *r == row).unwrap_or(0);
        self.adjust(if u < 0.5 { -1 } else { 1 })
    }

    /// Steps the selected row's value by `dir` (-1 or 1) and returns the follow-up actions.
    pub fn adjust(&mut self, dir: i32) -> Vec<SettingsAction> {
        if !self.visible {
            return Vec::new();
        }
        let dir = dir.signum();
        let before = self.settings.clone();
        let s = &mut self.settings;
        let mut actions = Vec::new();
        match PanelRow::ALL[self.selected] {
            PanelRow::Host => {
                if !self.hosts.is_empty() {
                    let current = s.host.unwrap_or(self.hosts[0]);
                    s.host = Some(step(&self.hosts, current, dir));
                }
            }
            PanelRow::Transport => {
                s.transport = step(
                    &[
                        TransportMode::Auto,
                        TransportMode::Udp,
                        TransportMode::AdbReverseTcp,
                    ],
                    s.transport,
                    dir,
                )
            }
            PanelRow::Codec => {
                s.codec = step(
                    &[ClientVideoCodec::Av1, ClientVideoCodec::Hevc],
                    s.codec,
                    dir,
                )
            }
            PanelRow::BitrateMode => {
                s.bitrate_mode = step(
                    &[BitrateMode::Adaptive, BitrateMode::Fixed],
                    s.bitrate_mode,
                    dir,
                )
            }
            PanelRow::Bitrate => {
                s.bitrate_mbps = (s.bitrate_mbps as i32 + dir * BITRATE_STEP_MBPS as i32)
                    .clamp(MIN_BITRATE_MBPS as i32, MAX_BITRATE_MBPS as i32)
                    as u32
            }
            PanelRow::RefreshRate => s.refresh_rate = step(&REFRESH_RATES, s.refresh_rate, dir),
            PanelRow::Monitors => {
                s.monitor_count = (s.monitor_count as i32 + dir)
                    .clamp(MIN_MONITORS as i32, MAX_MONITORS as i32)
                    as u32
            }
            PanelRow::Resolution => {
                (s.monitor_width, s.monitor_height) =
                    step(&RESOLUTIONS, (s.monitor_width, s.monitor_height), dir)
            }
            PanelRow::Layout => {
                s.layout = step(
                    &[
                        LayoutPreset::Horizontal,
                        LayoutPreset::Vertical,
                        LayoutPreset::Grid,
                    ],
                    s.layout,
                    dir,
                )
            }
            PanelRow::Passthrough => s.passthrough = !s.passthrough,
            PanelRow::Audio => s.audio_enabled = !s.audio_enabled,
            PanelRow::Hud => s.hud_enabled = !s.hud_enabled,
            PanelRow::PrimaryButton => {
                s.pointer_bindings.primary_button = step(
                    &[MouseButton::Left, MouseButton::Right, MouseButton::Middle],
                    s.pointer_bindings.primary_button,
                    dir,
                );
                s.pointer_bindings.secondary_button =
                    if s.pointer_bindings.primary_button == MouseButton::Left {
                        MouseButton::Right
                    } else {
                        MouseButton::Left
                    };
            }
        }
        if *s == before {
            return actions;
        }
        if s.host != before.host || s.transport != before.transport || s.codec != before.codec {
            actions.push(SettingsAction::Reconnect);
        }
        if (s.monitor_count, s.monitor_width, s.monitor_height, s.layout)
            != (
                before.monitor_count,
                before.monitor_width,
                before.monitor_height,
                before.layout,
            )
        {
            actions.push(SettingsAction::Display(s.monitor_request()));
        }
        if (s.bitrate_mode, s.bitrate_mbps) != (before.bitrate_mode, before.bitrate_mbps) {
            actions.push(SettingsAction::Control(s.bitrate_request()));
        }
        if s.refresh_rate != before.refresh_rate {
            actions.push(SettingsAction::SetRefreshRate(s.refresh_rate));
        }
        if s.passthrough != before.passthrough {
            actions.push(SettingsAction::SetPassthrough(s.passthrough));
        }
        if s.audio_enabled != before.audio_enabled {
            actions.push(SettingsAction::SetAudio(s.audio_enabled));
        }
        if s.hud_enabled != before.hud_enabled {
            actions.push(SettingsAction::SetHud(s.hud_enabled));
        }
        self.dirty = true;
        actions
    }

    /// Replaces the settings, for example with ones the host sent. They are sanitized, and
    /// marked for saving when they differ.
    pub fn replace_settings(&mut self, settings: ClientSettings) {
        let settings = settings.sanitized();
        if settings != self.settings {
            self.settings = settings;
            self.dirty = true;
        }
    }

    /// Reflects a HUD toggle made by the controller shortcut so the panel stays in sync.
    pub fn sync_hud(&mut self, enabled: bool) {
        if self.settings.hud_enabled != enabled {
            self.settings.hud_enabled = enabled;
            self.dirty = true;
        }
    }

    fn value_text(&self, row: PanelRow) -> String {
        let s = &self.settings;
        let on_off = |b: bool| if b { "On" } else { "Off" }.to_string();
        match row {
            PanelRow::Host => s
                .host
                .map_or_else(|| "(auto)".to_string(), |h| h.to_string()),
            PanelRow::Transport => match s.transport {
                TransportMode::Auto => "Auto (USB, else Wi-Fi)",
                TransportMode::Udp => "Wi-Fi (UDP)",
                TransportMode::AdbReverseTcp => "USB (ADB)",
            }
            .to_string(),
            PanelRow::Codec => match s.codec {
                ClientVideoCodec::Av1 => "AV1",
                ClientVideoCodec::Hevc => "HEVC",
            }
            .to_string(),
            PanelRow::BitrateMode => match s.bitrate_mode {
                BitrateMode::Adaptive => "Adaptive",
                BitrateMode::Fixed => "Fixed",
            }
            .to_string(),
            PanelRow::Bitrate => format!("{} Mbps", s.bitrate_mbps),
            PanelRow::RefreshRate => format!("{:.0} Hz", s.refresh_rate),
            PanelRow::Monitors => s.monitor_count.to_string(),
            PanelRow::Resolution => format!("{}x{}", s.monitor_width, s.monitor_height),
            PanelRow::Layout => format!("{:?}", s.layout),
            PanelRow::Passthrough => on_off(s.passthrough),
            PanelRow::Audio => on_off(s.audio_enabled),
            PanelRow::Hud => on_off(s.hud_enabled),
            PanelRow::PrimaryButton => format!("{:?}", s.pointer_bindings.primary_button),
        }
    }

    /// Text lines of the panel, one per row, with `>` marking the selection.
    pub fn lines(&self) -> Vec<String> {
        PanelRow::ALL
            .iter()
            .enumerate()
            .map(|(i, row)| {
                format!(
                    "{} {}: < {} >",
                    if i == self.selected { ">" } else { " " },
                    row.label(),
                    self.value_text(*row)
                )
            })
            .collect()
    }

    fn row_metrics(&self) -> (u32, u32) {
        let top = (self.height / 16).max(2) + 8;
        (top, (self.height - top) / PanelRow::ALL.len() as u32)
    }

    /// Renders the panel into an RGBA buffer of `width * height * 4` bytes.
    pub fn render_to_rgba(&self) -> Vec<u8> {
        let (w, h) = (self.width, self.height);
        let mut buf = vec![0u8; (w * h * 4) as usize];
        for px in buf.as_chunks_mut::<4>().0 {
            *px = [16, 18, 24, 230];
        }
        let (top, row_h) = self.row_metrics();
        for y in 0..top.min(h) {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                buf[i..i + 4].copy_from_slice(&[0, 220, 255, 255]);
            }
        }
        let scale = (row_h / 9).max(1);
        for (i, line) in self.lines().iter().enumerate() {
            let y0 = top + i as u32 * row_h;
            if i == self.selected {
                for y in y0..(y0 + row_h).min(h) {
                    for x in 0..w {
                        let p = ((y * w + x) * 4) as usize;
                        buf[p..p + 4].copy_from_slice(&[40, 60, 90, 255]);
                    }
                }
            }
            font::draw_text_rgba(
                &mut buf,
                w,
                h,
                12,
                y0 + 1,
                scale,
                line,
                [255, 255, 255, 255],
            );
        }
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panel() -> SettingsPanel {
        let mut p = SettingsPanel::new(ClientSettings::default(), 640, 520);
        p.toggle();
        p
    }

    fn select(p: &mut SettingsPanel, row: PanelRow) {
        while p.selected_row() != row {
            p.navigate(1);
        }
    }

    #[test]
    fn hidden_panel_ignores_input() {
        let mut p = SettingsPanel::new(ClientSettings::default(), 640, 520);
        assert!(p.adjust(1).is_empty());
        assert!(p.click_at(0.9, 0.2).is_empty());
        assert!(!p.take_dirty());
    }

    #[test]
    fn adding_a_monitor_and_switching_layout_emit_display_requests() {
        let mut p = panel();
        select(&mut p, PanelRow::Monitors);
        let actions = p.adjust(1);
        assert_eq!(p.settings().monitor_count, 2);
        assert_eq!(
            actions,
            vec![SettingsAction::Display(p.settings().monitor_request())]
        );
        select(&mut p, PanelRow::Layout);
        let actions = p.adjust(1);
        match &actions[..] {
            [SettingsAction::Display(DisplayConfigMessage::SetMonitorCount {
                count: 2,
                layout_mode: 1,
                ..
            })] => {}
            other => panic!("unexpected actions {other:?}"),
        }
        assert!(p.take_dirty());
        assert!(!p.take_dirty());
    }

    #[test]
    fn monitor_count_is_clamped_without_emitting_actions() {
        let mut p = panel();
        select(&mut p, PanelRow::Monitors);
        assert!(p.adjust(-1).is_empty());
        assert_eq!(p.settings().monitor_count, 1);
        p.adjust(1);
        p.adjust(1);
        assert!(p.adjust(1).is_empty());
        assert_eq!(p.settings().monitor_count, MAX_MONITORS);
    }

    #[test]
    fn transport_codec_and_host_changes_request_reconnect() {
        let mut p = panel();
        let hosts: Vec<SocketAddr> = vec![
            "192.168.1.20:48444".parse().unwrap(),
            "192.168.1.21:48444".parse().unwrap(),
        ];
        p.set_hosts(hosts.clone());
        select(&mut p, PanelRow::Host);
        assert_eq!(p.adjust(1), vec![SettingsAction::Reconnect]);
        assert_eq!(p.settings().host, Some(hosts[1]));
        select(&mut p, PanelRow::Transport);
        assert_eq!(p.adjust(1), vec![SettingsAction::Reconnect]);
        assert_eq!(p.settings().transport, TransportMode::Udp);
        select(&mut p, PanelRow::Codec);
        assert_eq!(p.adjust(1), vec![SettingsAction::Reconnect]);
        assert_eq!(p.settings().codec, ClientVideoCodec::Hevc);
    }

    #[test]
    fn local_rows_emit_matching_actions() {
        let mut p = panel();
        select(&mut p, PanelRow::RefreshRate);
        assert_eq!(p.adjust(1), vec![SettingsAction::SetRefreshRate(120.0)]);
        select(&mut p, PanelRow::Passthrough);
        assert_eq!(p.adjust(1), vec![SettingsAction::SetPassthrough(false)]);
        select(&mut p, PanelRow::Audio);
        assert_eq!(p.adjust(1), vec![SettingsAction::SetAudio(false)]);
        select(&mut p, PanelRow::Hud);
        assert_eq!(p.adjust(1), vec![SettingsAction::SetHud(false)]);
        select(&mut p, PanelRow::BitrateMode);
        let actions = p.adjust(1);
        assert!(matches!(&actions[..], [SettingsAction::Control(_)]));
        select(&mut p, PanelRow::PrimaryButton);
        p.adjust(1);
        let b = p.settings().pointer_bindings;
        assert_eq!(b.primary_button, MouseButton::Right);
        assert_eq!(b.secondary_button, MouseButton::Left);
    }

    #[test]
    fn pointer_click_selects_row_and_halves_step_in_opposite_directions() {
        let mut p = panel();
        let (top, row_h) = p.row_metrics();
        let v = (top + 6 * row_h + row_h / 2) as f32 / p.height as f32;
        assert_eq!(p.row_at(0.5, v), Some(PanelRow::Monitors));
        p.click_at(0.9, v);
        assert_eq!(p.settings().monitor_count, 2);
        p.click_at(0.1, v);
        assert_eq!(p.settings().monitor_count, 1);
        assert_eq!(p.row_at(0.5, 0.0), None);
        assert_eq!(p.row_at(1.5, 0.5), None);
    }

    #[test]
    fn render_marks_selection_and_matches_buffer_size() {
        let mut p = panel();
        let rgba = p.render_to_rgba();
        assert_eq!(rgba.len(), 640 * 520 * 4);
        assert!(rgba.as_chunks::<4>().0.contains(&[40, 60, 90, 255]));
        assert!(p.lines()[0].starts_with('>'));
        p.navigate(-1);
        assert!(p.lines()[PanelRow::ALL.len() - 1].starts_with('>'));
    }

    #[test]
    fn sync_hud_marks_dirty_only_on_change() {
        let mut p = panel();
        p.sync_hud(true);
        assert!(!p.take_dirty());
        p.sync_hud(false);
        assert!(p.take_dirty());
    }
}

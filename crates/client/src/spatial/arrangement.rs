//! Multi-panel arrangement: layout presets, per-panel editing with collision-free spacing,
//! focus/raise, "recenter all", per-host persistence and mapping back to the host display order.
//!
//! Everything here is pure (no OpenXR, no I/O except the JSON store), so it is unit testable
//! without a headset.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use linux_quest_protocol::DisplayInfo;
use serde::{Deserialize, Serialize};

use crate::config::DisplayMode;
use crate::error::ClientResult;
use crate::spatial::limits::*;
use crate::spatial::ray::Ray;
use crate::spatial::transform::ScreenTransform;

/// Minimum angular gap kept between neighbouring panels (about 1.1 degrees).
pub const PANEL_GAP_RAD: f32 = 0.02;

/// How much closer to the user the focused panel is drawn (raise).
pub const FOCUS_RAISE_M: f32 = 0.1;

/// Width of a panel in meters in the presets; the height follows the monitor aspect ratio.
const PRESET_PANEL_WIDTH_M: f32 = 1.6;

/// Maximum relaxation passes of the overlap resolver.
const RESOLVE_PASSES: usize = 32;

/// Built-in arrangements for a set of panels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutPreset {
    /// Flat panels next to each other, left to right in display order.
    SideBySide,
    /// Curved panels wrapping around the user in an arc.
    Arc,
    /// Flat panels stacked vertically, first display on top.
    Stacked,
    /// Mirror the monitor positions the host reports.
    MirrorHost,
}

/// A position for a display in the host desktop, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostPlacement {
    pub display_id: u16,
    pub x: i32,
    pub y: i32,
}

/// Stable key for a host and its monitor set, so a layout is restored only when the same
/// monitors (ids and resolutions) are connected again. Host positions are excluded on purpose:
/// they change when the layout is synced back.
pub fn host_arrangement_key(host: &str, displays: &[DisplayInfo]) -> String {
    let mut parts: Vec<String> = displays
        .iter()
        .map(|d| format!("{}:{}x{}", d.display_id, d.width, d.height))
        .collect();
    parts.sort();
    format!("{}|{}", host, parts.join(","))
}

/// Half extents (yaw, tilt) in radians that a panel covers as seen from the user.
fn angular_half_extent(t: &ScreenTransform) -> (f32, f32) {
    let (w, h) = t.size_m();
    let d = t.distance.max(MIN_DISTANCE_M);
    ((w * 0.5 / d).atan(), (h * 0.5 / d).atan())
}

fn panel_size_m(d: &DisplayInfo) -> (f32, f32) {
    let aspect = if d.height == 0 {
        16.0 / 9.0
    } else {
        d.width as f32 / d.height as f32
    };
    let aspect = clamp_aspect_ratio(aspect);
    let w = PRESET_PANEL_WIDTH_M;
    (w, w / aspect)
}

fn make_panel(d: &DisplayInfo, mode: DisplayMode) -> ScreenTransform {
    let mut t = ScreenTransform::default();
    let (w, h) = panel_size_m(d);
    t.display_mode = mode;
    t.aspect_ratio = clamp_aspect_ratio(w / h);
    t.set_quad_size(w, h);
    // Curved panels keep the same physical width as the flat ones.
    t.set_curve(DEFAULT_CURVE_RADIUS_M, w / DEFAULT_CURVE_RADIUS_M);
    t
}

/// Computes the transforms of every display for a preset. Panels are placed on a ring around
/// the user, then pushed apart if the ergonomic clamping made any of them overlap.
pub fn layout_for_preset(
    preset: LayoutPreset,
    displays: &[DisplayInfo],
) -> BTreeMap<u32, ScreenTransform> {
    let mut out = BTreeMap::new();
    if displays.is_empty() {
        return out;
    }
    let mode = match preset {
        LayoutPreset::Arc => DisplayMode::CurvedCylinder,
        _ => DisplayMode::FlatQuad,
    };
    let mut panels: Vec<(u32, ScreenTransform)> = displays
        .iter()
        .map(|d| (u32::from(d.display_id), make_panel(d, mode)))
        .collect();

    match preset {
        LayoutPreset::SideBySide | LayoutPreset::Arc => {
            let gap = if preset == LayoutPreset::Arc {
                PANEL_GAP_RAD * 0.5
            } else {
                PANEL_GAP_RAD
            };
            let widths: Vec<f32> = panels
                .iter()
                .map(|(_, t)| 2.0 * angular_half_extent(t).0)
                .collect();
            let total: f32 = widths.iter().sum::<f32>() + gap * (widths.len() - 1) as f32;
            // Left to right means decreasing yaw (positive yaw is to the left).
            let mut edge = total * 0.5;
            for ((_, t), w) in panels.iter_mut().zip(&widths) {
                t.place_at(edge - w * 0.5, 0.0, DEFAULT_DISTANCE_M);
                edge -= w + gap;
            }
        }
        LayoutPreset::Stacked => {
            let heights: Vec<f32> = panels
                .iter()
                .map(|(_, t)| 2.0 * angular_half_extent(t).1)
                .collect();
            let total: f32 =
                heights.iter().sum::<f32>() + PANEL_GAP_RAD * (heights.len() - 1) as f32;
            // First display on top: positive tilt is upwards.
            let mut edge = total * 0.5;
            for ((_, t), h) in panels.iter_mut().zip(&heights) {
                t.place_at(0.0, edge - h * 0.5, DEFAULT_DISTANCE_M);
                edge -= h + PANEL_GAP_RAD;
            }
        }
        LayoutPreset::MirrorHost => {
            // Angular size of one host pixel, taken from the first panel.
            let first = &displays[0];
            let (half_w, half_h) = angular_half_extent(&panels[0].1);
            let k = 2.0 * half_w / first.width.max(1) as f32;
            let kv = 2.0 * half_h / first.height.max(1) as f32;
            let centre = |d: &DisplayInfo| {
                (
                    d.x as f32 + d.width as f32 * 0.5,
                    d.y as f32 + d.height as f32 * 0.5,
                )
            };
            let (min_x, max_x, min_y, max_y) = displays.iter().fold(
                (f32::MAX, f32::MIN, f32::MAX, f32::MIN),
                |(a, b, c, e), d| {
                    (
                        a.min(d.x as f32),
                        b.max((d.x + d.width as i32) as f32),
                        c.min(d.y as f32),
                        e.max((d.y + d.height as i32) as f32),
                    )
                },
            );
            let (mx, my) = ((min_x + max_x) * 0.5, (min_y + max_y) * 0.5);
            for ((_, t), d) in panels.iter_mut().zip(displays) {
                let (cx, cy) = centre(d);
                t.place_at(-(cx - mx) * k, -(cy - my) * kv, DEFAULT_DISTANCE_M);
            }
        }
    }

    for (id, t) in panels {
        out.insert(id, t);
    }
    resolve_overlaps(&mut out, None);
    out
}

fn overlap(a: &ScreenTransform, b: &ScreenTransform) -> Option<(f32, f32)> {
    let (ah, av) = angular_half_extent(a);
    let (bh, bv) = angular_half_extent(b);
    let ox = ah + bh + PANEL_GAP_RAD - (a.yaw - b.yaw).abs();
    let oy = av + bv + PANEL_GAP_RAD - (a.tilt - b.tilt).abs();
    (ox > 1e-4 && oy > 1e-4).then_some((ox, oy))
}

/// Pushes overlapping panels apart along the axis of least penetration until no two panels
/// overlap (keeping at least [`PANEL_GAP_RAD`] between them). The `pinned` panel (the one the
/// user is moving) stays put while the others make room. Returns false if the ergonomic
/// limits left some pair still overlapping.
pub fn resolve_overlaps(screens: &mut BTreeMap<u32, ScreenTransform>, pinned: Option<u32>) -> bool {
    let ids: Vec<u32> = screens.keys().copied().collect();
    for _ in 0..RESOLVE_PASSES {
        let mut moved = false;
        for (i, &ia) in ids.iter().enumerate() {
            for &ib in &ids[i + 1..] {
                let Some((ox, oy)) = overlap(&screens[&ia], &screens[&ib]) else {
                    continue;
                };
                moved = true;
                let (a, b) = (screens[&ia].clone(), screens[&ib].clone());
                // Push along the axis of least penetration, away from each other.
                let (dyaw, dtilt) = if ox <= oy {
                    let s = if a.yaw >= b.yaw { 1.0 } else { -1.0 };
                    (s * ox, 0.0)
                } else {
                    let s = if a.tilt >= b.tilt { 1.0 } else { -1.0 };
                    (0.0, s * oy)
                };
                let (wa, wb) = match (Some(ia) == pinned, Some(ib) == pinned) {
                    (true, _) => (0.0, 1.0),
                    (_, true) => (1.0, 0.0),
                    _ => (0.5, 0.5),
                };
                let ta = screens.get_mut(&ia).unwrap();
                let (d, tl) = (ta.distance, ta.tilt + dtilt * wa);
                ta.place_at(a.yaw + dyaw * wa, tl, d);
                let tb = screens.get_mut(&ib).unwrap();
                let (d, tl) = (tb.distance, b.tilt - dtilt * wb);
                tb.place_at(b.yaw - dyaw * wb, tl, d);
            }
        }
        if !moved {
            return true;
        }
    }
    ids.iter().enumerate().all(|(i, a)| {
        ids[i + 1..]
            .iter()
            .all(|b| overlap(&screens[a], &screens[b]).is_none())
    })
}

/// The arrangement of one host's monitor set: what is persisted and restored on reconnect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PanelArrangement {
    /// Active preset, or `None` once a panel was moved by hand.
    pub preset: Option<LayoutPreset>,
    /// Apply in-headset rearrangements back to the host display layout.
    #[serde(default)]
    pub host_sync: bool,
    pub screens: BTreeMap<u32, ScreenTransform>,
    /// Panel the user is aiming at; raised towards the user. Not persisted.
    #[serde(skip)]
    pub focused: Option<u32>,
}

impl PanelArrangement {
    /// Default arrangement for a monitor set: one curved panel, or an arc for several.
    pub fn new(displays: &[DisplayInfo]) -> Self {
        let preset = LayoutPreset::Arc;
        Self {
            preset: Some(preset),
            host_sync: false,
            screens: layout_for_preset(preset, displays),
            focused: None,
        }
    }

    /// Switches to a preset, discarding manual placement.
    pub fn apply_preset(&mut self, preset: LayoutPreset, displays: &[DisplayInfo]) {
        self.screens = layout_for_preset(preset, displays);
        self.preset = Some(preset);
    }

    /// Re-applies the current preset (or the arc after manual edits) centred on the direction
    /// the user is facing (`head_yaw`, radians, positive = left).
    pub fn recenter_all(&mut self, displays: &[DisplayInfo], head_yaw: f32) {
        let preset = self.preset.unwrap_or(LayoutPreset::Arc);
        self.apply_preset(preset, displays);
        for t in self.screens.values_mut() {
            let (yaw, tilt, d) = (t.yaw + head_yaw, t.tilt, t.distance);
            t.place_at(yaw, tilt, d);
        }
    }

    /// Edits one panel (move, resize, curve, ...). The result is clamped, the other panels
    /// make room, and the arrangement becomes custom. Returns false for an unknown panel.
    pub fn edit_panel(&mut self, id: u32, edit: impl FnOnce(&mut ScreenTransform)) -> bool {
        let Some(t) = self.screens.get_mut(&id) else {
            return false;
        };
        edit(t);
        t.clamp();
        self.preset = None;
        resolve_overlaps(&mut self.screens, Some(id));
        true
    }

    /// Focuses the panel hit by `ray` (if any) and returns its id.
    pub fn focus_from_ray(&mut self, ray: &Ray) -> Option<u32> {
        self.focused =
            ScreenTransform::find_closest_hit(ray, self.screens.iter().map(|(i, t)| (*i, t)))
                .map(|(id, _)| id);
        self.focused
    }

    /// The transform to render a panel with: the focused panel is raised towards the user.
    pub fn render_transform(&self, id: u32) -> Option<ScreenTransform> {
        let mut t = self.screens.get(&id)?.clone();
        if self.focused == Some(id) {
            let (yaw, tilt, d) = (t.yaw, t.tilt, t.distance - FOCUS_RAISE_M);
            t.place_at(yaw, tilt, d);
        }
        Some(t)
    }

    /// Host desktop positions that match the panel adjacency in the headset: panels are
    /// grouped into rows from top to bottom, ordered left to right inside a row, and packed
    /// edge to edge, so crossing a monitor edge on the host lands on the neighbouring panel.
    /// Returns `None` when sync is off or the host already has this arrangement.
    pub fn host_sync_placements(&self, displays: &[DisplayInfo]) -> Option<Vec<HostPlacement>> {
        if !self.host_sync {
            return None;
        }
        let placements = self.host_placements(displays);
        let unchanged = placements.iter().all(|p| {
            displays
                .iter()
                .any(|d| d.display_id == p.display_id && d.x == p.x && d.y == p.y)
        });
        (!unchanged).then_some(placements)
    }

    fn host_placements(&self, displays: &[DisplayInfo]) -> Vec<HostPlacement> {
        let mut items: Vec<(&DisplayInfo, &ScreenTransform)> = displays
            .iter()
            .filter_map(|d| self.screens.get(&u32::from(d.display_id)).map(|t| (d, t)))
            .collect();
        // Top row first; panels whose vertical centres are within half a panel share a row.
        items.sort_by(|a, b| b.1.tilt.total_cmp(&a.1.tilt));
        let mut rows: Vec<Vec<(&DisplayInfo, &ScreenTransform)>> = Vec::new();
        for item in items {
            let half = angular_half_extent(item.1).1;
            match rows.last_mut() {
                Some(row) if (row[0].1.tilt - item.1.tilt).abs() < half => row.push(item),
                _ => rows.push(vec![item]),
            }
        }
        let mut out = Vec::new();
        let mut y = 0i32;
        for mut row in rows {
            // Left to right is decreasing yaw.
            row.sort_by(|a, b| b.1.yaw.total_cmp(&a.1.yaw));
            let mut x = 0i32;
            let mut row_h = 0;
            for (d, _) in row {
                out.push(HostPlacement {
                    display_id: d.display_id,
                    x,
                    y,
                });
                x += d.width as i32;
                row_h = row_h.max(d.height as i32);
            }
            y += row_h;
        }
        out.sort_by_key(|p| p.display_id);
        out
    }
}

/// Arrangements for all known hosts and monitor sets.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ArrangementBook {
    pub version: u32,
    pub entries: BTreeMap<String, PanelArrangement>,
}

impl ArrangementBook {
    pub const CURRENT_VERSION: u32 = 1;

    /// Restores the arrangement saved for this host and monitor set, or builds the default.
    pub fn restore(&self, host: &str, displays: &[DisplayInfo]) -> PanelArrangement {
        self.entries
            .get(&host_arrangement_key(host, displays))
            .cloned()
            .unwrap_or_else(|| PanelArrangement::new(displays))
    }

    /// Remembers an arrangement for this host and monitor set.
    pub fn remember(&mut self, host: &str, displays: &[DisplayInfo], a: &PanelArrangement) {
        self.version = Self::CURRENT_VERSION;
        self.entries
            .insert(host_arrangement_key(host, displays), a.clone());
    }
}

/// JSON file holding the [`ArrangementBook`].
#[derive(Debug, Clone)]
pub struct ArrangementBookStore {
    path: PathBuf,
}

impl ArrangementBookStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Atomically writes the book (temp file + rename).
    pub fn save(&self, book: &ArrangementBook) -> ClientResult<()> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let json = serde_json::to_string_pretty(book)?;
        let tmp = self
            .path
            .with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&tmp, json.as_bytes())?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Loads the book; a missing, corrupt or newer-version file yields an empty book
    /// (a corrupt file is kept as a `.bak` copy).
    pub fn load_or_default(&self) -> ArrangementBook {
        let Ok(content) = std::fs::read_to_string(&self.path) else {
            return ArrangementBook::default();
        };
        match serde_json::from_str::<ArrangementBook>(&content) {
            Ok(book) if book.version <= ArrangementBook::CURRENT_VERSION => book,
            _ => {
                tracing::warn!(path = ?self.path, "Arrangement file invalid; using defaults");
                let _ = std::fs::copy(&self.path, self.path.with_extension("corrupt.bak"));
                ArrangementBook::default()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::pose::Vec3;

    fn disp(id: u16, x: i32, y: i32) -> DisplayInfo {
        DisplayInfo {
            display_id: id,
            name: format!("D{id}"),
            width: 1920,
            height: 1080,
            refresh_rate: 60,
            dpi: 96,
            is_virtual: true,
            x,
            y,
        }
    }

    fn three() -> Vec<DisplayInfo> {
        vec![disp(0, 0, 0), disp(1, 1920, 0), disp(2, 3840, 0)]
    }

    fn no_overlaps(m: &BTreeMap<u32, ScreenTransform>) -> bool {
        let v: Vec<_> = m.values().collect();
        v.iter()
            .enumerate()
            .all(|(i, a)| v[i + 1..].iter().all(|b| overlap(a, b).is_none()))
    }

    #[test]
    fn side_by_side_orders_left_to_right_without_overlap() {
        let m = layout_for_preset(LayoutPreset::SideBySide, &three());
        assert!(m[&0].yaw > m[&1].yaw && m[&1].yaw > m[&2].yaw);
        assert!(m.values().all(|t| t.display_mode == DisplayMode::FlatQuad));
        assert!(no_overlaps(&m));
        assert!((m[&0].yaw + m[&2].yaw).abs() < 1e-4, "centred");
    }

    #[test]
    fn arc_is_curved_and_stacked_is_vertical() {
        let arc = layout_for_preset(LayoutPreset::Arc, &three());
        assert!(arc
            .values()
            .all(|t| t.display_mode == DisplayMode::CurvedCylinder));
        assert!(no_overlaps(&arc));

        let st = layout_for_preset(LayoutPreset::Stacked, &three());
        assert!(st[&0].tilt > st[&1].tilt && st[&1].tilt > st[&2].tilt);
        assert!(st.values().all(|t| t.yaw.abs() < 1e-4));
        assert!(no_overlaps(&st));
    }

    #[test]
    fn mirror_host_follows_reported_positions() {
        // L-shaped: 0 and 1 side by side, 2 below 0.
        let d = vec![disp(0, 0, 0), disp(1, 1920, 0), disp(2, 0, 1080)];
        let m = layout_for_preset(LayoutPreset::MirrorHost, &d);
        assert!(m[&0].yaw > m[&1].yaw);
        assert!(m[&2].tilt < m[&0].tilt);
        assert!(
            (m[&2].yaw - m[&0].yaw).abs() < 2.0 * PANEL_GAP_RAD,
            "only gap nudges"
        );
        assert!(no_overlaps(&m));
    }

    #[test]
    fn edit_clamps_and_makes_room_for_pinned_panel() {
        let d = three();
        let mut a = PanelArrangement::new(&d);
        let other = a.screens[&1].yaw;
        // Drag panel 0 right on top of panel 1.
        assert!(a.edit_panel(0, |t| t.place_at(other, 0.0, 1.2)));
        assert_eq!(a.preset, None);
        assert!(
            (a.screens[&0].yaw - other).abs() < 1e-4,
            "pinned panel stays"
        );
        assert!(no_overlaps(&a.screens));
        // Ergonomic clamping still holds.
        assert!(a.edit_panel(2, |t| t.place_at(10.0, 10.0, 100.0)));
        let t = &a.screens[&2];
        assert!(t.yaw <= MAX_YAW_RAD && t.tilt <= MAX_TILT_RAD && t.distance <= MAX_DISTANCE_M);
        assert!(!a.edit_panel(9, |_| {}));
    }

    #[test]
    fn panels_resize_and_curve_independently() {
        let mut a = PanelArrangement::new(&three());
        let before = a.screens[&1].clone();
        a.edit_panel(0, |t| t.set_curve(1.0, 1.0));
        assert_eq!(a.screens[&0].curve_radius, 1.0);
        assert_eq!(a.screens[&1].curve_radius, before.curve_radius);
        assert!(no_overlaps(&a.screens));
    }

    #[test]
    fn recenter_all_follows_head_yaw() {
        let d = three();
        let mut a = PanelArrangement::new(&d);
        a.edit_panel(0, |t| t.place_at(1.0, 0.5, 2.0));
        a.recenter_all(&d, 0.3);
        let mean: f32 = a.screens.values().map(|t| t.yaw).sum::<f32>() / 3.0;
        assert!((mean - 0.3).abs() < 1e-3);
        assert_eq!(a.preset, Some(LayoutPreset::Arc));
        assert!(no_overlaps(&a.screens));
    }

    #[test]
    fn focus_picks_aimed_panel_and_raises_it() {
        let d = three();
        let mut a = PanelArrangement::new(&d);
        a.apply_preset(LayoutPreset::SideBySide, &d);
        let target = a.screens[&2].pose().position;
        let ray = Ray::new(
            Vec3::new(0.0, DEFAULT_HEIGHT_M, 0.0),
            target - Vec3::new(0.0, DEFAULT_HEIGHT_M, 0.0),
        );
        assert_eq!(a.focus_from_ray(&ray), Some(2));
        let raised = a.render_transform(2).unwrap();
        assert!((raised.distance - (a.screens[&2].distance - FOCUS_RAISE_M)).abs() < 1e-4);
        assert_eq!(a.render_transform(1).unwrap(), a.screens[&1]);
        let away = Ray::new(Vec3::new(0.0, 1.4, 0.0), Vec3::new(0.0, 1.0, 0.0));
        assert_eq!(a.focus_from_ray(&away), None);
    }

    #[test]
    fn host_sync_matches_headset_adjacency() {
        let d = three();
        let mut a = PanelArrangement::new(&d);
        a.apply_preset(LayoutPreset::SideBySide, &d);
        assert!(a.host_sync_placements(&d).is_none(), "off by default");
        a.host_sync = true;
        assert!(a.host_sync_placements(&d).is_none(), "already matches");

        // Swap panels 0 and 2 in the headset: host order becomes 2, 1, 0.
        let (y0, y2) = (a.screens[&0].yaw, a.screens[&2].yaw);
        a.screens.get_mut(&0).unwrap().place_at(y2, 0.0, 1.2);
        a.screens.get_mut(&2).unwrap().place_at(y0, 0.0, 1.2);
        let p = a.host_sync_placements(&d).unwrap();
        let x = |id| p.iter().find(|p| p.display_id == id).unwrap().x;
        assert_eq!((x(2), x(1), x(0)), (0, 1920, 3840));
        assert!(p.iter().all(|p| p.y == 0));
    }

    #[test]
    fn host_sync_handles_rows() {
        let d = three();
        let mut a = PanelArrangement::new(&d);
        a.host_sync = true;
        a.apply_preset(LayoutPreset::Stacked, &d);
        let p = a.host_sync_placements(&d).unwrap();
        let at = |id| *p.iter().find(|p| p.display_id == id).unwrap();
        assert_eq!((at(0).x, at(0).y), (0, 0));
        assert_eq!((at(1).x, at(1).y), (0, 1080));
        assert_eq!((at(2).x, at(2).y), (0, 2160));
    }

    #[test]
    fn key_is_per_host_and_monitor_set_but_ignores_positions() {
        let d = three();
        let mut moved = d.clone();
        moved[1].x = 0;
        assert_eq!(
            host_arrangement_key("a", &d),
            host_arrangement_key("a", &moved)
        );
        assert_ne!(host_arrangement_key("a", &d), host_arrangement_key("b", &d));
        assert_ne!(
            host_arrangement_key("a", &d),
            host_arrangement_key("a", &d[..2])
        );
    }

    #[test]
    fn layouts_survive_restart_per_host() {
        let dir = std::env::temp_dir().join(format!("lq_arr_{}", std::process::id()));
        let path = dir.join("arrangements.json");
        let d = three();

        let store = ArrangementBookStore::new(&path);
        let mut book = store.load_or_default();
        let mut a = book.restore("hostA", &d);
        a.apply_preset(LayoutPreset::Stacked, &d);
        a.edit_panel(1, |t| t.place_at(0.4, 0.0, 1.5));
        a.host_sync = true;
        book.remember("hostA", &d, &a);
        store.save(&book).unwrap();

        // "Restart": fresh store and book.
        let book2 = ArrangementBookStore::new(&path).load_or_default();
        let restored = book2.restore("hostA", &d);
        assert_eq!(restored.screens, a.screens);
        assert_eq!(restored.preset, None);
        assert!(restored.host_sync);
        // Another host / monitor set gets the default instead.
        assert_eq!(book2.restore("hostB", &d).preset, Some(LayoutPreset::Arc));
        assert_eq!(book2.restore("hostA", &d[..2]).screens.len(), 2);

        // Corrupt file falls back to an empty book.
        std::fs::write(&path, b"{ nope").unwrap();
        assert!(ArrangementBookStore::new(&path)
            .load_or_default()
            .entries
            .is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

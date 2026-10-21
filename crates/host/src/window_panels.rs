//! Per-window panels: enumerate host application windows, track which ones are streamed as
//! their own panel, and map pointer input on a panel back onto the window's host position.
//!
//! Window panels reuse `DisplayInfo` with a stream id from `WINDOW_STREAM_ID_BASE`, and the
//! window's origin as `x`/`y`, so the client treats them like monitors.

use std::collections::BTreeMap;
use std::process::Command;

use linux_quest_protocol::{DisplayInfo, InputEvent, WindowInfo, WINDOW_STREAM_ID_BASE};
use thiserror::Error;

/// Upper bound on simultaneous window panels (the client decoder budget is shared with monitors).
pub const MAX_WINDOW_PANELS: usize = 3;

/// Normalised pointer range used by `InputEvent::MouseMoveAbsolute`.
const ABS_MAX: f64 = u16::MAX as f64;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum WindowError {
    #[error("window enumeration failed: {0}")]
    Enumerate(String),
    #[error("window {0:#x} is not available")]
    UnknownWindow(u64),
    #[error("at most {MAX_WINDOW_PANELS} window panels are supported (requested {0})")]
    TooMany(usize),
    #[error("failed to focus window: {0}")]
    Focus(String),
}

/// Where windows come from: the X11 window manager, or a mock in tests.
pub trait WindowSource: Send {
    fn list(&mut self) -> Result<Vec<WindowInfo>, WindowError>;
    fn focus(&mut self, window_id: u64) -> Result<(), WindowError>;
}

impl<T: WindowSource + ?Sized> WindowSource for Box<T> {
    fn list(&mut self) -> Result<Vec<WindowInfo>, WindowError> {
        (**self).list()
    }

    fn focus(&mut self, window_id: u64) -> Result<(), WindowError> {
        (**self).focus(window_id)
    }
}

/// X11 enumeration through `wmctrl`. Wayland sessions use the ScreenCast portal's window
/// source type instead (see `linux_quest_capture::screencast::portal::negotiate_window`).
#[derive(Debug, Default)]
pub struct X11WindowSource;

impl WindowSource for X11WindowSource {
    fn list(&mut self) -> Result<Vec<WindowInfo>, WindowError> {
        let out = Command::new("wmctrl")
            .arg("-lGx")
            .output()
            .map_err(|e| WindowError::Enumerate(format!("wmctrl: {e}")))?;
        if !out.status.success() {
            return Err(WindowError::Enumerate("wmctrl exited with error".into()));
        }
        Ok(parse_wmctrl(&String::from_utf8_lossy(&out.stdout)))
    }

    fn focus(&mut self, window_id: u64) -> Result<(), WindowError> {
        let status = Command::new("wmctrl")
            .args(["-i", "-a", &format!("{window_id:#x}")])
            .status()
            .map_err(|e| WindowError::Focus(e.to_string()))?;
        status
            .success()
            .then_some(())
            .ok_or_else(|| WindowError::Focus("wmctrl exited with error".into()))
    }
}

/// Parses `wmctrl -lGx`: `id desktop x y w h class host title...`. Sticky shell surfaces
/// (desktop -1, e.g. panels) and degenerate sizes are skipped.
pub fn parse_wmctrl(output: &str) -> Vec<WindowInfo> {
    output
        .lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let id = u64::from_str_radix(it.next()?.trim_start_matches("0x"), 16).ok()?;
            let desktop: i32 = it.next()?.parse().ok()?;
            let x: i32 = it.next()?.parse().ok()?;
            let y: i32 = it.next()?.parse().ok()?;
            let width: u32 = it.next()?.parse().ok()?;
            let height: u32 = it.next()?.parse().ok()?;
            let class = it.next()?;
            let _host = it.next()?;
            let title = it.collect::<Vec<_>>().join(" ");
            if desktop < 0 || width == 0 || height == 0 {
                return None;
            }
            let app_name = class.rsplit('.').next().unwrap_or(class).to_string();
            Some(WindowInfo {
                window_id: id,
                title,
                app_name,
                width,
                height,
                x,
                y,
            })
        })
        .collect()
}

/// Tracks the windows streamed as panels.
pub struct WindowPanelManager<S: WindowSource> {
    source: S,
    /// Panel stream id -> host window id.
    panels: BTreeMap<u16, u64>,
    desktop: (u32, u32),
    hovered: Option<u64>,
    focused: Option<u64>,
}

impl<S: WindowSource> WindowPanelManager<S> {
    /// `desktop` is the size of the host desktop the absolute pointer range covers.
    pub fn new(source: S, desktop: (u32, u32)) -> Self {
        Self {
            source,
            panels: BTreeMap::new(),
            desktop,
            hovered: None,
            focused: None,
        }
    }

    pub fn list_windows(&mut self) -> Result<Vec<WindowInfo>, WindowError> {
        self.source.list()
    }

    /// Replaces the set of panels. Windows that stay keep their stream id. Atomic: on error the
    /// previous set is kept.
    pub fn set_panels(&mut self, window_ids: &[u64]) -> Result<Vec<DisplayInfo>, WindowError> {
        let mut unique: Vec<u64> = Vec::new();
        for id in window_ids {
            if !unique.contains(id) {
                unique.push(*id);
            }
        }
        if unique.len() > MAX_WINDOW_PANELS {
            return Err(WindowError::TooMany(unique.len()));
        }
        let available = self.source.list()?;
        if let Some(missing) = unique
            .iter()
            .find(|id| !available.iter().any(|w| w.window_id == **id))
        {
            return Err(WindowError::UnknownWindow(*missing));
        }
        let mut next: BTreeMap<u16, u64> = self
            .panels
            .iter()
            .filter(|(_, w)| unique.contains(w))
            .map(|(s, w)| (*s, *w))
            .collect();
        for id in unique {
            if next.values().any(|w| *w == id) {
                continue;
            }
            let stream = (WINDOW_STREAM_ID_BASE..)
                .find(|s| !next.contains_key(s))
                .expect("stream id space is not exhausted");
            next.insert(stream, id);
        }
        self.panels = next;
        Ok(self.describe(&available))
    }

    /// Re-reads window geometry. Panels whose window closed are dropped; returns their stream
    /// ids and the current panel descriptors.
    pub fn refresh(&mut self) -> Result<(Vec<u16>, Vec<DisplayInfo>), WindowError> {
        let available = self.source.list()?;
        let gone: Vec<u16> = self
            .panels
            .iter()
            .filter(|(_, w)| !available.iter().any(|a| a.window_id == **w))
            .map(|(s, _)| *s)
            .collect();
        for s in &gone {
            self.panels.remove(s);
        }
        Ok((gone, self.describe(&available)))
    }

    pub fn panel_count(&self) -> usize {
        self.panels.len()
    }

    fn describe(&self, available: &[WindowInfo]) -> Vec<DisplayInfo> {
        self.panels
            .iter()
            .filter_map(|(stream, id)| {
                let w = available.iter().find(|w| w.window_id == *id)?;
                Some(DisplayInfo {
                    display_id: *stream,
                    name: if w.title.is_empty() {
                        w.app_name.clone()
                    } else {
                        w.title.clone()
                    },
                    width: w.width,
                    height: w.height,
                    refresh_rate: 60,
                    dpi: 96,
                    is_virtual: false,
                    x: w.x,
                    y: w.y,
                })
            })
            .collect()
    }

    /// Maps input aimed at a window panel onto the host desktop, focusing the window before
    /// the first click that lands on it. Events for other displays pass through unchanged.
    pub fn map_input(&mut self, event: InputEvent) -> InputEvent {
        match event {
            InputEvent::MouseMoveAbsolute { x, y, display_id }
                if self.panels.contains_key(&display_id) =>
            {
                let window_id = self.panels[&display_id];
                let Some(w) = self
                    .source
                    .list()
                    .ok()
                    .and_then(|l| l.into_iter().find(|w| w.window_id == window_id))
                else {
                    return InputEvent::MouseMoveAbsolute { x, y, display_id };
                };
                self.hovered = Some(window_id);
                let px = f64::from(w.x)
                    + f64::from(x.min(u16::MAX as u32)) / ABS_MAX * f64::from(w.width);
                let py = f64::from(w.y)
                    + f64::from(y.min(u16::MAX as u32)) / ABS_MAX * f64::from(w.height);
                let norm = |p: f64, size: u32| {
                    (p / f64::from(size.max(1)) * ABS_MAX)
                        .round()
                        .clamp(0.0, ABS_MAX) as u32
                };
                InputEvent::MouseMoveAbsolute {
                    x: norm(px, self.desktop.0),
                    y: norm(py, self.desktop.1),
                    display_id: 0,
                }
            }
            InputEvent::MouseMoveAbsolute { .. } => {
                self.hovered = None;
                event
            }
            InputEvent::MouseButton {
                state: linux_quest_protocol::ElementState::Pressed,
                ..
            } => {
                if let Some(id) = self.hovered {
                    if self.focused != Some(id) && self.source.focus(id).is_ok() {
                        self.focused = Some(id);
                    }
                }
                event
            }
            other => other,
        }
    }
}

#[cfg(test)]
pub mod mock {
    use super::*;

    #[derive(Default)]
    pub struct MockWindowSource {
        pub windows: Vec<WindowInfo>,
        pub focused: Vec<u64>,
    }

    impl WindowSource for MockWindowSource {
        fn list(&mut self) -> Result<Vec<WindowInfo>, WindowError> {
            Ok(self.windows.clone())
        }
        fn focus(&mut self, window_id: u64) -> Result<(), WindowError> {
            self.focused.push(window_id);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mock::MockWindowSource;
    use super::*;
    use linux_quest_protocol::{ElementState, MouseButton};

    fn win(id: u64, x: i32, y: i32, w: u32, h: u32) -> WindowInfo {
        WindowInfo {
            window_id: id,
            title: format!("w{id}"),
            app_name: "app".into(),
            width: w,
            height: h,
            x,
            y,
        }
    }

    fn manager() -> WindowPanelManager<MockWindowSource> {
        let src = MockWindowSource {
            windows: vec![
                win(1, 0, 0, 1000, 500),
                win(2, 1000, 500, 1000, 500),
                win(3, 0, 0, 10, 10),
                win(4, 0, 0, 10, 10),
            ],
            focused: vec![],
        };
        WindowPanelManager::new(src, (2000, 1000))
    }

    #[test]
    fn parses_wmctrl_output() {
        let out = "0x04a00007  0 100 50   1280 720  code.Code  host Editor - main.rs\n\
                   0x00000010 -1 0 0 1920 30 panel.Panel host Panel\n\
                   garbage\n";
        let w = parse_wmctrl(out);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].window_id, 0x04a00007);
        assert_eq!(
            (w[0].x, w[0].y, w[0].width, w[0].height),
            (100, 50, 1280, 720)
        );
        assert_eq!(w[0].app_name, "Code");
        assert_eq!(w[0].title, "Editor - main.rs");
    }

    #[test]
    fn two_windows_become_panels_with_stable_ids() {
        let mut m = manager();
        let panels = m.set_panels(&[1, 2]).unwrap();
        assert_eq!(panels.len(), 2);
        assert_eq!(panels[0].display_id, WINDOW_STREAM_ID_BASE);
        assert_eq!((panels[1].x, panels[1].y), (1000, 500));
        // Dropping window 1 keeps window 2's stream id; a new window gets a free one.
        let panels = m.set_panels(&[2, 3]).unwrap();
        let ids: Vec<u16> = panels.iter().map(|p| p.display_id).collect();
        assert_eq!(ids, vec![WINDOW_STREAM_ID_BASE, WINDOW_STREAM_ID_BASE + 1]);
        assert_eq!(panels[1].name, "w2");
    }

    #[test]
    fn set_panels_rejects_unknown_and_too_many_atomically() {
        let mut m = manager();
        m.set_panels(&[1]).unwrap();
        assert_eq!(m.set_panels(&[1, 99]), Err(WindowError::UnknownWindow(99)));
        assert_eq!(m.set_panels(&[1, 2, 3, 4]), Err(WindowError::TooMany(4)));
        assert_eq!(m.panel_count(), 1);
    }

    #[test]
    fn closed_windows_are_dropped_on_refresh() {
        let mut m = manager();
        m.set_panels(&[1, 2]).unwrap();
        m.source.windows.retain(|w| w.window_id != 1);
        m.source.windows[0].x = 5;
        let (gone, panels) = m.refresh().unwrap();
        assert_eq!(gone, vec![WINDOW_STREAM_ID_BASE]);
        assert_eq!(panels.len(), 1);
        m.source.windows.retain(|w| w.window_id != 2);
        assert_eq!(m.refresh().unwrap().0.len(), 1);
        assert_eq!(m.panel_count(), 0);
    }

    #[test]
    fn pointer_on_panel_maps_to_window_position() {
        let mut m = manager();
        m.set_panels(&[1, 2]).unwrap();
        // Centre of window 2 (1000,500 1000x500) is desktop (1500,750) of 2000x1000.
        let mapped = m.map_input(InputEvent::MouseMoveAbsolute {
            x: 32768,
            y: 32768,
            display_id: WINDOW_STREAM_ID_BASE + 1,
        });
        let InputEvent::MouseMoveAbsolute { x, y, display_id } = mapped else {
            panic!("unexpected {mapped:?}");
        };
        assert_eq!(display_id, 0);
        assert!((i64::from(x) - 49151).abs() <= 2, "x={x}");
        assert!((i64::from(y) - 49151).abs() <= 2, "y={y}");
    }

    #[test]
    fn non_panel_input_passes_through() {
        let mut m = manager();
        m.set_panels(&[1]).unwrap();
        let ev = InputEvent::MouseMoveAbsolute {
            x: 5,
            y: 6,
            display_id: 1,
        };
        assert_eq!(m.map_input(ev.clone()), ev);
    }

    #[test]
    fn click_focuses_the_hovered_window_once() {
        let mut m = manager();
        m.set_panels(&[1, 2]).unwrap();
        let click = InputEvent::MouseButton {
            button: MouseButton::Left,
            state: ElementState::Pressed,
        };
        m.map_input(click.clone()); // nothing hovered yet
        assert!(m.source.focused.is_empty());
        m.map_input(InputEvent::MouseMoveAbsolute {
            x: 1,
            y: 1,
            display_id: WINDOW_STREAM_ID_BASE + 1,
        });
        m.map_input(click.clone());
        m.map_input(click.clone());
        assert_eq!(m.source.focused, vec![2]);
        m.map_input(InputEvent::MouseMoveAbsolute {
            x: 1,
            y: 1,
            display_id: WINDOW_STREAM_ID_BASE,
        });
        m.map_input(click);
        assert_eq!(m.source.focused, vec![2, 1]);
    }
}

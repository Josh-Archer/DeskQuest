//! Input backend metadata and session based XTEST implementation.

use std::collections::HashSet;
use std::process::Command;

use linux_quest_protocol::{ElementState, InputEvent, MouseButton};

use crate::{InputBackend, InputError, InputInjector};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Portal,
    Wlroots,
    XTest,
    Uinput,
    Mock,
}

impl BackendKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Portal => "RemoteDesktop portal (libei)",
            Self::Wlroots => "wlroots virtual input",
            Self::XTest => "X11 XTEST",
            Self::Uinput => "uinput",
            Self::Mock => "mock",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendDiagnostic {
    pub active: BackendKind,
    pub reason: String,
    pub skipped: Vec<(BackendKind, String)>,
}

/// XTEST injector using the widely available `xdotool` X11 client. X keycodes are resolved
/// by XKB on the host, so the active host layout is honored. The protocol's Linux evdev
/// keycodes are translated to X keycodes by adding the standard evdev offset of eight.
pub struct XTestInjector {
    held_keys: HashSet<u32>,
    held_buttons: HashSet<MouseButton>,
}

impl XTestInjector {
    pub fn new() -> Result<Self, InputError> {
        if std::env::var_os("DISPLAY").is_none() {
            return Err(InputError::EmitFailed("DISPLAY is not set".into()));
        }
        let this = Self {
            held_keys: HashSet::new(),
            held_buttons: HashSet::new(),
        };
        this.run(&["getdisplaygeometry"])?;
        Ok(this)
    }

    fn run(&self, args: &[&str]) -> Result<(), InputError> {
        let output = Command::new("xdotool")
            .args(args)
            .output()
            .map_err(|e| InputError::EmitFailed(format!("cannot run xdotool: {e}")))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(InputError::EmitFailed(format!(
                "xdotool {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }

    fn button_number(button: MouseButton) -> u8 {
        match button {
            MouseButton::Left => 1,
            MouseButton::Middle => 2,
            MouseButton::Right => 3,
            MouseButton::Side => 8,
            MouseButton::Extra => 9,
            MouseButton::Back => 8,
            MouseButton::Forward => 9,
            MouseButton::Task => 10,
        }
    }
}

impl InputInjector for XTestInjector {
    fn inject_event(&mut self, event: InputEvent) -> Result<(), InputError> {
        match event {
            InputEvent::MouseMoveRelative { dx, dy } => {
                self.run(&["mousemove_relative", &dx.to_string(), &dy.to_string()])
            }
            InputEvent::MouseMoveAbsolute { x, y, .. } => {
                self.run(&["mousemove", &x.to_string(), &y.to_string()])
            }
            InputEvent::MouseButton { button, state } => {
                let cmd = if state == ElementState::Pressed {
                    self.held_buttons.insert(button);
                    " mousedown"
                } else {
                    self.held_buttons.remove(&button);
                    " mouseup"
                };
                self.run(&[cmd.trim(), &Self::button_number(button).to_string()])
            }
            InputEvent::MouseScroll { delta_x, delta_y }
            | InputEvent::MouseScrollHiRes { delta_x, delta_y } => {
                let (x, y) = (delta_x.round() as i32, delta_y.round() as i32);
                for (button, count) in [
                    (if y < 0 { 4 } else { 5 }, y.abs()),
                    (if x < 0 { 6 } else { 7 }, x.abs()),
                ] {
                    for _ in 0..count {
                        self.run(&["click", &button.to_string()])?;
                    }
                }
                Ok(())
            }
            InputEvent::KeyboardKey { keycode, state, .. } => {
                let x_keycode = keycode.saturating_add(8).to_string();
                let command = if state == ElementState::Pressed {
                    self.held_keys.insert(keycode);
                    "keydown"
                } else {
                    self.held_keys.remove(&keycode);
                    "keyup"
                };
                self.run(&[command, &format!("keycode:{x_keycode}")])
            }
            InputEvent::ClipboardSync { .. }
            | InputEvent::KeyframeRequest
            | InputEvent::KeyframeRequestFor { .. } => Ok(()),
        }
    }

    fn release_all(&mut self) -> Result<(), InputError> {
        let mut first_error = None;
        for key in self.held_keys.drain() {
            if let Err(e) = Command::new("xdotool")
                .args(["keyup", &format!("keycode:{}", key.saturating_add(8))])
                .status()
            {
                first_error.get_or_insert_with(|| InputError::EmitFailed(e.to_string()));
            }
        }
        for button in self.held_buttons.drain() {
            let result = Command::new("xdotool")
                .args(["mouseup", &Self::button_number(button).to_string()])
                .status();
            if let Err(e) = result {
                first_error.get_or_insert_with(|| InputError::EmitFailed(e.to_string()));
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl InputBackend for XTestInjector {
    fn backend_kind(&self) -> BackendKind {
        BackendKind::XTest
    }
    fn diagnostic(&self) -> BackendDiagnostic {
        BackendDiagnostic {
            active: BackendKind::XTest,
            reason: "X11 DISPLAY and xdotool are available".into(),
            skipped: Vec::new(),
        }
    }
}

impl Drop for XTestInjector {
    fn drop(&mut self) {
        let _ = self.release_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x11_button_numbers_match_x11_conventions() {
        assert_eq!(XTestInjector::button_number(MouseButton::Left), 1);
        assert_eq!(XTestInjector::button_number(MouseButton::Middle), 2);
        assert_eq!(XTestInjector::button_number(MouseButton::Right), 3);
        assert_eq!(XTestInjector::button_number(MouseButton::Back), 8);
        assert_eq!(XTestInjector::button_number(MouseButton::Forward), 9);
    }

    #[test]
    fn backend_names_are_suitable_for_diagnostics() {
        assert_eq!(BackendKind::XTest.name(), "X11 XTEST");
        assert_eq!(BackendKind::Portal.name(), "RemoteDesktop portal (libei)");
    }
}

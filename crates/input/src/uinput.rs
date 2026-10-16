//! Linux uinput virtual input injector.
//!
//! Creates virtual mouse, absolute pointer, and keyboard devices via `/dev/uinput`
//! and injects input events with correct evdev synchronizations (`SYN_REPORT`).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use input_linux::{
    sys, AbsoluteAxis, AbsoluteInfo, AbsoluteInfoSetup, EventKind, InputId, Key, RelativeAxis,
};
use linux_quest_protocol::{ElementState, InputEvent, MouseButton};

use crate::device::{
    make_abs_event, make_key_event, make_rel_event, make_syn_report, MockDeviceOpener,
    MockDeviceState, RealDeviceOpener, UinputDeviceFile, UinputDeviceOpener,
};
use crate::error::InputError;
use crate::InputInjector;

/// Configuration for Linux virtual uinput devices.
#[derive(Debug, Clone)]
pub struct UinputConfig {
    pub mouse_name: String,
    pub absolute_name: String,
    pub keyboard_name: String,
    pub vendor_id: u16,
    pub mouse_product_id: u16,
    pub abs_product_id: u16,
    pub keyboard_product_id: u16,
    pub version: u16,
    pub abs_min_x: i32,
    pub abs_max_x: i32,
    pub abs_min_y: i32,
    pub abs_max_y: i32,
}

impl Default for UinputConfig {
    fn default() -> Self {
        Self {
            mouse_name: "Linux Quest Virtual Mouse".into(),
            absolute_name: "Linux Quest Virtual Absolute Pointer".into(),
            keyboard_name: "Linux Quest Virtual Keyboard".into(),
            vendor_id: 0x1234,
            mouse_product_id: 0x0001,
            abs_product_id: 0x0002,
            keyboard_product_id: 0x0003,
            version: 1,
            abs_min_x: 0,
            abs_max_x: 65535,
            abs_min_y: 0,
            abs_max_y: 65535,
        }
    }
}

/// Maps protocol `MouseButton` to Linux evdev button keycode.
pub fn mouse_button_to_evdev(button: MouseButton) -> u16 {
    match button {
        MouseButton::Left => sys::BTN_LEFT as u16,
        MouseButton::Right => sys::BTN_RIGHT as u16,
        MouseButton::Middle => sys::BTN_MIDDLE as u16,
        MouseButton::Side => sys::BTN_SIDE as u16,
        MouseButton::Extra => sys::BTN_EXTRA as u16,
        MouseButton::Back => sys::BTN_BACK as u16,
        MouseButton::Forward => sys::BTN_FORWARD as u16,
        MouseButton::Task => sys::BTN_TASK as u16,
    }
}

/// Linux kernel virtual input injector using `/dev/uinput`.
pub struct UinputInjector {
    mouse: Box<dyn UinputDeviceFile>,
    absolute: Box<dyn UinputDeviceFile>,
    keyboard: Box<dyn UinputDeviceFile>,
    held_keys: HashSet<u32>,
    held_buttons: HashSet<MouseButton>,
}

/// Mock virtual input devices and injector for testing.
pub struct MockUinputDevices {
    pub injector: UinputInjector,
    pub mouse: Arc<Mutex<MockDeviceState>>,
    pub absolute: Arc<Mutex<MockDeviceState>>,
    pub keyboard: Arc<Mutex<MockDeviceState>>,
}

impl UinputInjector {
    /// Opens virtual uinput devices at `/dev/uinput` using the default configuration.
    pub fn open() -> Result<Self, InputError> {
        let opener = RealDeviceOpener::default();
        let config = UinputConfig::default();
        Self::new_with_opener(&opener, &config)
    }

    /// Opens virtual uinput devices at a custom device path.
    pub fn open_path(path: impl Into<PathBuf>) -> Result<Self, InputError> {
        let opener = RealDeviceOpener::new(path);
        let config = UinputConfig::default();
        Self::new_with_opener(&opener, &config)
    }

    /// Creates virtual input devices using the provided `UinputDeviceOpener`.
    pub fn new_with_opener(
        opener: &dyn UinputDeviceOpener,
        config: &UinputConfig,
    ) -> Result<Self, InputError> {
        // 1. Virtual Mouse Device
        let mut mouse = opener.open_device()?;
        setup_mouse(&mut *mouse, config)?;

        // 2. Virtual Absolute Pointer Device
        let mut absolute = opener.open_device()?;
        setup_absolute(&mut *absolute, config)?;

        // 3. Virtual Keyboard Device
        let mut keyboard = opener.open_device()?;
        setup_keyboard(&mut *keyboard, config)?;

        Ok(Self {
            mouse,
            absolute,
            keyboard,
            held_keys: HashSet::new(),
            held_buttons: HashSet::new(),
        })
    }

    /// Constructs an injector from pre-opened or mock device files.
    pub fn from_device_files(
        mouse: Box<dyn UinputDeviceFile>,
        absolute: Box<dyn UinputDeviceFile>,
        keyboard: Box<dyn UinputDeviceFile>,
    ) -> Self {
        Self {
            mouse,
            absolute,
            keyboard,
            held_keys: HashSet::new(),
            held_buttons: HashSet::new(),
        }
    }

    /// Creates a mock `UinputInjector` for unit tests, returning the injector and inspection handles
    /// for mouse, absolute pointer, and keyboard devices.
    pub fn new_mock() -> MockUinputDevices {
        let opener = MockDeviceOpener::new();
        let config = UinputConfig::default();
        let injector = Self::new_with_opener(&opener, &config)
            .expect("Mock injector creation should never fail");
        let devices = opener.devices();
        MockUinputDevices {
            injector,
            mouse: devices[0].clone(),
            absolute: devices[1].clone(),
            keyboard: devices[2].clone(),
        }
    }

    /// Set of currently held keyboard keys.
    pub fn held_keys(&self) -> &HashSet<u32> {
        &self.held_keys
    }

    /// Set of currently held mouse buttons.
    pub fn held_buttons(&self) -> &HashSet<MouseButton> {
        &self.held_buttons
    }

    /// Injects an event into the appropriate virtual device.
    pub fn inject(&mut self, event: InputEvent) -> Result<(), InputError> {
        match event {
            InputEvent::MouseMoveRelative { dx, dy } => {
                let events = [
                    make_rel_event(sys::REL_X as u16, dx),
                    make_rel_event(sys::REL_Y as u16, dy),
                    make_syn_report(),
                ];
                self.mouse.write_events(&events)?;
            }
            InputEvent::MouseMoveAbsolute {
                x,
                y,
                display_id: _,
            } => {
                let events = [
                    make_abs_event(sys::ABS_X as u16, x as i32),
                    make_abs_event(sys::ABS_Y as u16, y as i32),
                    make_syn_report(),
                ];
                self.absolute.write_events(&events)?;
            }
            InputEvent::MouseButton { button, state } => {
                let code = mouse_button_to_evdev(button);
                let value = match state {
                    ElementState::Pressed => {
                        self.held_buttons.insert(button);
                        1
                    }
                    ElementState::Released => {
                        self.held_buttons.remove(&button);
                        0
                    }
                };
                let events = [make_key_event(code, value), make_syn_report()];
                self.mouse.write_events(&events)?;
            }
            InputEvent::MouseScroll { delta_x, delta_y }
            | InputEvent::MouseScrollHiRes { delta_x, delta_y } => {
                let mut events = Vec::with_capacity(5);
                if delta_y.abs() > 0.0001 {
                    let hires_y = (delta_y * 120.0).round() as i32;
                    events.push(make_rel_event(sys::REL_WHEEL_HI_RES as u16, hires_y));
                    let clicks_y = delta_y.trunc() as i32;
                    if clicks_y != 0 {
                        events.push(make_rel_event(sys::REL_WHEEL as u16, clicks_y));
                    }
                }
                if delta_x.abs() > 0.0001 {
                    let hires_x = (delta_x * 120.0).round() as i32;
                    events.push(make_rel_event(sys::REL_HWHEEL_HI_RES as u16, hires_x));
                    let clicks_x = delta_x.trunc() as i32;
                    if clicks_x != 0 {
                        events.push(make_rel_event(sys::REL_HWHEEL as u16, clicks_x));
                    }
                }
                if !events.is_empty() {
                    events.push(make_syn_report());
                    self.mouse.write_events(&events)?;
                }
            }
            InputEvent::KeyboardKey {
                keycode,
                state,
                modifiers: _,
            } => {
                let value = match state {
                    ElementState::Pressed => {
                        self.held_keys.insert(keycode);
                        1
                    }
                    ElementState::Released => {
                        self.held_keys.remove(&keycode);
                        0
                    }
                };
                let events = [make_key_event(keycode as u16, value), make_syn_report()];
                self.keyboard.write_events(&events)?;
            }
            InputEvent::ClipboardSync { .. } => {
                // Clipboard sync is handled out-of-band by compositor/clipboard protocols.
                tracing::debug!("Clipboard sync received on uinput injector; ignoring for evdev");
            }
        }
        Ok(())
    }

    /// Releases all currently held keys and mouse buttons, emitting appropriate release events
    /// and SYN_REPORT to avoid stuck keys across client disconnection or drop.
    pub fn release_all(&mut self) -> Result<(), InputError> {
        if !self.held_keys.is_empty() {
            let mut events = Vec::with_capacity(self.held_keys.len() + 1);
            for &keycode in &self.held_keys {
                events.push(make_key_event(keycode as u16, 0));
            }
            events.push(make_syn_report());
            self.keyboard.write_events(&events)?;
            self.held_keys.clear();
        }

        if !self.held_buttons.is_empty() {
            let mut events = Vec::with_capacity(self.held_buttons.len() + 1);
            for &button in &self.held_buttons {
                let code = mouse_button_to_evdev(button);
                events.push(make_key_event(code, 0));
            }
            events.push(make_syn_report());
            self.mouse.write_events(&events)?;
            self.held_buttons.clear();
        }

        Ok(())
    }
}

impl InputInjector for UinputInjector {
    fn inject_event(&mut self, event: InputEvent) -> Result<(), InputError> {
        self.inject(event)
    }

    fn release_all(&mut self) -> Result<(), InputError> {
        self.release_all()
    }
}

impl Drop for UinputInjector {
    fn drop(&mut self) {
        if let Err(e) = self.release_all() {
            tracing::warn!(error = ?e, "Failed to release held keys/buttons on uinput injector drop");
        }
    }
}

fn setup_mouse(device: &mut dyn UinputDeviceFile, config: &UinputConfig) -> Result<(), InputError> {
    // 1. Enable event kinds
    device.set_evbit(EventKind::Synchronize)?;
    device.set_evbit(EventKind::Relative)?;
    device.set_evbit(EventKind::Key)?;

    // 2. Enable relative axes: X, Y, wheels and high-resolution variants
    device.set_relbit(RelativeAxis::X)?;
    device.set_relbit(RelativeAxis::Y)?;
    device.set_relbit(RelativeAxis::Wheel)?;
    device.set_relbit(RelativeAxis::HorizontalWheel)?;
    device.set_relbit(RelativeAxis::WheelHiRes)?;
    device.set_relbit(RelativeAxis::HorizontalWheelHiRes)?;

    // 3. Enable mouse buttons: Left, Right, Middle, Side, Extra, Forward, Back, Task
    device.set_keybit(Key::ButtonLeft)?;
    device.set_keybit(Key::ButtonRight)?;
    device.set_keybit(Key::ButtonMiddle)?;
    device.set_keybit(Key::ButtonSide)?;
    device.set_keybit(Key::ButtonExtra)?;
    device.set_keybit(Key::ButtonForward)?;
    device.set_keybit(Key::ButtonBack)?;
    device.set_keybit(Key::ButtonTask)?;

    // 4. Create device
    let id = InputId {
        bustype: sys::BUS_USB,
        vendor: config.vendor_id,
        product: config.mouse_product_id,
        version: config.version,
    };
    device.create_device(&id, config.mouse_name.as_bytes(), 0, &[])?;
    Ok(())
}

fn setup_absolute(
    device: &mut dyn UinputDeviceFile,
    config: &UinputConfig,
) -> Result<(), InputError> {
    // 1. Enable event kinds
    device.set_evbit(EventKind::Synchronize)?;
    device.set_evbit(EventKind::Absolute)?;
    device.set_evbit(EventKind::Key)?;

    // 2. Enable absolute axes
    device.set_absbit(AbsoluteAxis::X)?;
    device.set_absbit(AbsoluteAxis::Y)?;

    // 3. Enable buttons
    device.set_keybit(Key::ButtonLeft)?;
    device.set_keybit(Key::ButtonRight)?;
    device.set_keybit(Key::ButtonMiddle)?;

    // 4. Abs setup info
    let abs_x = AbsoluteInfoSetup {
        axis: AbsoluteAxis::X,
        info: AbsoluteInfo {
            value: 0,
            minimum: config.abs_min_x,
            maximum: config.abs_max_x,
            fuzz: 0,
            flat: 0,
            resolution: 0,
        },
    };
    let abs_y = AbsoluteInfoSetup {
        axis: AbsoluteAxis::Y,
        info: AbsoluteInfo {
            value: 0,
            minimum: config.abs_min_y,
            maximum: config.abs_max_y,
            fuzz: 0,
            flat: 0,
            resolution: 0,
        },
    };

    let id = InputId {
        bustype: sys::BUS_USB,
        vendor: config.vendor_id,
        product: config.abs_product_id,
        version: config.version,
    };
    device.create_device(&id, config.absolute_name.as_bytes(), 0, &[abs_x, abs_y])?;
    Ok(())
}

fn setup_keyboard(
    device: &mut dyn UinputDeviceFile,
    config: &UinputConfig,
) -> Result<(), InputError> {
    // 1. Enable event kinds
    device.set_evbit(EventKind::Synchronize)?;
    device.set_evbit(EventKind::Key)?;
    device.set_evbit(EventKind::Autorepeat)?; // EV_REP

    // 2. Enable all standard keyboard keys
    for key in Key::iter() {
        if key.is_key() {
            let _ = device.set_keybit(key);
        }
    }

    let id = InputId {
        bustype: sys::BUS_USB,
        vendor: config.vendor_id,
        product: config.keyboard_product_id,
        version: config.version,
    };
    device.create_device(&id, config.keyboard_name.as_bytes(), 0, &[])?;
    Ok(())
}

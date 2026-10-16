//! Linux input injection crate for linux-quest.
//!
//! Provides kernel-level input redirection via `/dev/uinput`, supporting virtual mouse,
//! absolute pointer, and keyboard devices.

pub mod auto;
pub mod device;
pub mod error;
pub mod uinput;

pub use auto::{create_auto_injector, AutoInputInjector};
pub use device::{
    make_abs_event, make_key_event, make_rel_event, make_syn_report, MockDeviceOpener,
    MockDeviceState, MockUinputDeviceFile, RawInputEvent, RealDeviceOpener, RealUinputDeviceFile,
    UinputDeviceFile, UinputDeviceOpener, DEFAULT_UINPUT_PATH,
};
pub use error::InputError;
use linux_quest_protocol::InputEvent;
pub use uinput::{mouse_button_to_evdev, MockUinputDevices, UinputConfig, UinputInjector};

/// Trait defining the input injection interface for Linux Quest.
pub trait InputInjector: Send + Sync {
    /// Injects an input event into the system.
    fn inject_event(&mut self, event: InputEvent) -> Result<(), InputError>;

    /// Releases all currently held keyboard keys and mouse buttons.
    fn release_all(&mut self) -> Result<(), InputError> {
        Ok(())
    }
}

impl<T: ?Sized + InputInjector> InputInjector for Box<T> {
    fn inject_event(&mut self, event: InputEvent) -> Result<(), InputError> {
        (**self).inject_event(event)
    }

    fn release_all(&mut self) -> Result<(), InputError> {
        (**self).release_all()
    }
}

/// In-memory mock injector for testing remote input processing.
#[derive(Debug, Default, Clone)]
pub struct MockInputInjector {
    pub received_events: Vec<InputEvent>,
}

impl MockInputInjector {
    pub fn new() -> Self {
        Self::default()
    }
}

impl InputInjector for MockInputInjector {
    fn inject_event(&mut self, event: InputEvent) -> Result<(), InputError> {
        tracing::debug!(event = ?event, "MockInputInjector received event");
        self.received_events.push(event);
        Ok(())
    }

    fn release_all(&mut self) -> Result<(), InputError> {
        tracing::debug!("MockInputInjector release_all");
        Ok(())
    }
}

/// Simple logging injector that logs all injected events.
#[derive(Debug, Default, Clone)]
pub struct LoggingInputInjector {
    pub event_count: usize,
}

impl LoggingInputInjector {
    pub fn new() -> Self {
        Self::default()
    }
}

impl InputInjector for LoggingInputInjector {
    fn inject_event(&mut self, event: InputEvent) -> Result<(), InputError> {
        self.event_count += 1;
        tracing::info!(
            count = self.event_count,
            event = ?event,
            "LoggingInputInjector: injected input event"
        );
        Ok(())
    }

    fn release_all(&mut self) -> Result<(), InputError> {
        tracing::info!("LoggingInputInjector: released all held keys and buttons");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use input_linux::{
        sys, AbsoluteAxis, AbsoluteInfoSetup, EventKind, InputId, Key, RelativeAxis,
    };
    use linux_quest_protocol::{ElementState, KeyModifiers, MouseButton};
    use std::path::PathBuf;

    #[test]
    fn test_mock_input_injector() {
        let mut injector = MockInputInjector::new();
        let ev1 = InputEvent::MouseMoveRelative { dx: 10, dy: -5 };
        let ev2 = InputEvent::MouseButton {
            button: MouseButton::Left,
            state: ElementState::Pressed,
        };

        injector.inject_event(ev1.clone()).unwrap();
        injector.inject_event(ev2.clone()).unwrap();

        assert_eq!(injector.received_events.len(), 2);
        assert_eq!(injector.received_events[0], ev1);
        assert_eq!(injector.received_events[1], ev2);
    }

    #[test]
    fn test_logging_input_injector() {
        let mut injector = LoggingInputInjector::new();
        let ev = InputEvent::MouseMoveRelative { dx: 1, dy: 1 };
        assert_eq!(injector.event_count, 0);
        injector.inject_event(ev).unwrap();
        assert_eq!(injector.event_count, 1);
        injector.release_all().unwrap();
    }

    #[test]
    fn test_uinput_device_creation_capabilities() {
        let MockUinputDevices {
            injector,
            mouse: mouse_state,
            absolute: abs_state,
            keyboard: kbd_state,
        } = UinputInjector::new_mock();

        // 1. Verify Mouse Capabilities
        let m = mouse_state.lock().unwrap();
        assert!(m.is_created);
        assert!(m.has_evbit(EventKind::Synchronize));
        assert!(m.has_evbit(EventKind::Relative));
        assert!(m.has_evbit(EventKind::Key));
        assert!(m.has_relbit(RelativeAxis::X));
        assert!(m.has_relbit(RelativeAxis::Y));
        assert!(m.has_relbit(RelativeAxis::Wheel));
        assert!(m.has_relbit(RelativeAxis::HorizontalWheel));
        assert!(m.has_relbit(RelativeAxis::WheelHiRes));
        assert!(m.has_relbit(RelativeAxis::HorizontalWheelHiRes));
        assert!(m.has_keybit(Key::ButtonLeft));
        assert!(m.has_keybit(Key::ButtonRight));
        assert!(m.has_keybit(Key::ButtonMiddle));
        assert!(m.has_keybit(Key::ButtonSide));
        assert!(m.has_keybit(Key::ButtonExtra));
        assert_eq!(m.name, b"Linux Quest Virtual Mouse");

        // 2. Verify Absolute Pointer Capabilities
        let a = abs_state.lock().unwrap();
        assert!(a.is_created);
        assert!(a.has_evbit(EventKind::Synchronize));
        assert!(a.has_evbit(EventKind::Absolute));
        assert!(a.has_evbit(EventKind::Key));
        assert!(a.has_absbit(AbsoluteAxis::X));
        assert!(a.has_absbit(AbsoluteAxis::Y));
        assert!(a.has_keybit(Key::ButtonLeft));
        assert_eq!(a.abs_setup.len(), 2);
        assert_eq!(a.abs_setup[0].axis, AbsoluteAxis::X);
        assert_eq!(a.abs_setup[0].info.maximum, 65535);
        assert_eq!(a.abs_setup[1].axis, AbsoluteAxis::Y);
        assert_eq!(a.abs_setup[1].info.maximum, 65535);
        assert_eq!(a.name, b"Linux Quest Virtual Absolute Pointer");

        // 3. Verify Keyboard Capabilities
        let k = kbd_state.lock().unwrap();
        assert!(k.is_created);
        assert!(k.has_evbit(EventKind::Synchronize));
        assert!(k.has_evbit(EventKind::Key));
        assert!(k.has_evbit(EventKind::Autorepeat));
        assert!(k.has_keybit(Key::A));
        assert!(k.has_keybit(Key::Esc));
        assert!(k.has_keybit(Key::Enter));
        assert!(k.has_keybit(Key::LeftShift));
        assert_eq!(k.name, b"Linux Quest Virtual Keyboard");

        drop(injector);
    }

    #[test]
    fn test_uinput_mouse_relative_injection() {
        let MockUinputDevices {
            mut injector,
            mouse: mouse_state,
            ..
        } = UinputInjector::new_mock();

        injector
            .inject_event(InputEvent::MouseMoveRelative { dx: 15, dy: -25 })
            .unwrap();

        let m = mouse_state.lock().unwrap();
        let evs = m.written_events();
        assert_eq!(evs.len(), 3);

        assert_eq!(evs[0].type_, sys::EV_REL as u16);
        assert_eq!(evs[0].code, sys::REL_X as u16);
        assert_eq!(evs[0].value, 15);

        assert_eq!(evs[1].type_, sys::EV_REL as u16);
        assert_eq!(evs[1].code, sys::REL_Y as u16);
        assert_eq!(evs[1].value, -25);

        assert_eq!(evs[2].type_, sys::EV_SYN as u16);
        assert_eq!(evs[2].code, sys::SYN_REPORT as u16);
        assert_eq!(evs[2].value, 0);
    }

    #[test]
    fn test_uinput_mouse_absolute_injection() {
        let MockUinputDevices {
            mut injector,
            absolute: abs_state,
            ..
        } = UinputInjector::new_mock();

        injector
            .inject_event(InputEvent::MouseMoveAbsolute {
                x: 1920,
                y: 1080,
                display_id: 1,
            })
            .unwrap();

        let a = abs_state.lock().unwrap();
        let evs = a.written_events();
        assert_eq!(evs.len(), 3);

        assert_eq!(evs[0].type_, sys::EV_ABS as u16);
        assert_eq!(evs[0].code, sys::ABS_X as u16);
        assert_eq!(evs[0].value, 1920);

        assert_eq!(evs[1].type_, sys::EV_ABS as u16);
        assert_eq!(evs[1].code, sys::ABS_Y as u16);
        assert_eq!(evs[1].value, 1080);

        assert_eq!(evs[2].type_, sys::EV_SYN as u16);
        assert_eq!(evs[2].code, sys::SYN_REPORT as u16);
        assert_eq!(evs[2].value, 0);
    }

    #[test]
    fn test_uinput_mouse_buttons_injection() {
        let MockUinputDevices {
            mut injector,
            mouse: mouse_state,
            ..
        } = UinputInjector::new_mock();

        let buttons = [
            (MouseButton::Left, sys::BTN_LEFT as u16),
            (MouseButton::Right, sys::BTN_RIGHT as u16),
            (MouseButton::Middle, sys::BTN_MIDDLE as u16),
            (MouseButton::Side, sys::BTN_SIDE as u16),
            (MouseButton::Extra, sys::BTN_EXTRA as u16),
            (MouseButton::Back, sys::BTN_BACK as u16),
            (MouseButton::Forward, sys::BTN_FORWARD as u16),
            (MouseButton::Task, sys::BTN_TASK as u16),
        ];

        for (btn, expected_code) in buttons {
            // Press
            injector
                .inject_event(InputEvent::MouseButton {
                    button: btn,
                    state: ElementState::Pressed,
                })
                .unwrap();
            assert!(injector.held_buttons().contains(&btn));

            // Release
            injector
                .inject_event(InputEvent::MouseButton {
                    button: btn,
                    state: ElementState::Released,
                })
                .unwrap();
            assert!(!injector.held_buttons().contains(&btn));

            let m = mouse_state.lock().unwrap();
            let evs = m.written_events();
            let last_4 = &evs[evs.len() - 4..];

            // Press event
            assert_eq!(last_4[0].type_, sys::EV_KEY as u16);
            assert_eq!(last_4[0].code, expected_code);
            assert_eq!(last_4[0].value, 1);
            assert_eq!(last_4[1].type_, sys::EV_SYN as u16);
            assert_eq!(last_4[1].code, sys::SYN_REPORT as u16);

            // Release event
            assert_eq!(last_4[2].type_, sys::EV_KEY as u16);
            assert_eq!(last_4[2].code, expected_code);
            assert_eq!(last_4[2].value, 0);
            assert_eq!(last_4[3].type_, sys::EV_SYN as u16);
            assert_eq!(last_4[3].code, sys::SYN_REPORT as u16);
        }
    }

    #[test]
    fn test_uinput_mouse_scroll_discrete_and_hires() {
        let MockUinputDevices {
            mut injector,
            mouse: mouse_state,
            ..
        } = UinputInjector::new_mock();

        // 1. Discrete vertical scroll notch (1.0)
        injector
            .inject_event(InputEvent::MouseScroll {
                delta_x: 0.0,
                delta_y: 1.0,
            })
            .unwrap();

        {
            let m = mouse_state.lock().unwrap();
            let evs = m.written_events();
            assert_eq!(evs.len(), 3);
            assert_eq!(evs[0].type_, sys::EV_REL as u16);
            assert_eq!(evs[0].code, sys::REL_WHEEL_HI_RES as u16);
            assert_eq!(evs[0].value, 120);

            assert_eq!(evs[1].type_, sys::EV_REL as u16);
            assert_eq!(evs[1].code, sys::REL_WHEEL as u16);
            assert_eq!(evs[1].value, 1);

            assert_eq!(evs[2].type_, sys::EV_SYN as u16);
            assert_eq!(evs[2].code, sys::SYN_REPORT as u16);
        }

        // 2. High-res fractional scroll (0.5 y, -0.25 x)
        injector
            .inject_event(InputEvent::MouseScrollHiRes {
                delta_x: -0.25,
                delta_y: 0.5,
            })
            .unwrap();

        {
            let m = mouse_state.lock().unwrap();
            let evs = m.written_events();
            // Events 3, 4, 5:
            // hires_y: 60 (clicks_y: 0, no integer tick)
            // hires_x: -30 (clicks_x: 0, no integer tick)
            // syn_report
            assert_eq!(evs.len(), 6);
            assert_eq!(evs[3].code, sys::REL_WHEEL_HI_RES as u16);
            assert_eq!(evs[3].value, 60);

            assert_eq!(evs[4].code, sys::REL_HWHEEL_HI_RES as u16);
            assert_eq!(evs[4].value, -30);

            assert_eq!(evs[5].code, sys::SYN_REPORT as u16);
        }
    }

    #[test]
    fn test_uinput_keyboard_injection() {
        let MockUinputDevices {
            mut injector,
            keyboard: kbd_state,
            ..
        } = UinputInjector::new_mock();

        // Key Press
        injector
            .inject_event(InputEvent::KeyboardKey {
                keycode: 30, // KEY_A
                state: ElementState::Pressed,
                modifiers: KeyModifiers::empty(),
            })
            .unwrap();

        assert!(injector.held_keys().contains(&30));

        {
            let k = kbd_state.lock().unwrap();
            let evs = k.written_events();
            assert_eq!(evs.len(), 2);
            assert_eq!(evs[0].type_, sys::EV_KEY as u16);
            assert_eq!(evs[0].code, 30);
            assert_eq!(evs[0].value, 1);
            assert_eq!(evs[1].type_, sys::EV_SYN as u16);
            assert_eq!(evs[1].code, sys::SYN_REPORT as u16);
        }

        // Key Release
        injector
            .inject_event(InputEvent::KeyboardKey {
                keycode: 30,
                state: ElementState::Released,
                modifiers: KeyModifiers::empty(),
            })
            .unwrap();

        assert!(!injector.held_keys().contains(&30));

        {
            let k = kbd_state.lock().unwrap();
            let evs = k.written_events();
            assert_eq!(evs.len(), 4);
            assert_eq!(evs[2].type_, sys::EV_KEY as u16);
            assert_eq!(evs[2].code, 30);
            assert_eq!(evs[2].value, 0);
            assert_eq!(evs[3].type_, sys::EV_SYN as u16);
            assert_eq!(evs[3].code, sys::SYN_REPORT as u16);
        }
    }

    #[test]
    fn test_uinput_release_all_on_drop_and_disconnect() {
        let MockUinputDevices {
            mut injector,
            mouse: mouse_state,
            keyboard: kbd_state,
            ..
        } = UinputInjector::new_mock();

        // Press 2 keys and 2 mouse buttons
        injector
            .inject_event(InputEvent::KeyboardKey {
                keycode: 30,
                state: ElementState::Pressed,
                modifiers: KeyModifiers::empty(),
            })
            .unwrap();
        injector
            .inject_event(InputEvent::KeyboardKey {
                keycode: 42,
                state: ElementState::Pressed,
                modifiers: KeyModifiers::empty(),
            })
            .unwrap();
        injector
            .inject_event(InputEvent::MouseButton {
                button: MouseButton::Left,
                state: ElementState::Pressed,
            })
            .unwrap();
        injector
            .inject_event(InputEvent::MouseButton {
                button: MouseButton::Right,
                state: ElementState::Pressed,
            })
            .unwrap();

        assert_eq!(injector.held_keys().len(), 2);
        assert_eq!(injector.held_buttons().len(), 2);

        // Explicit release_all (as invoked on disconnect)
        injector.release_all().unwrap();

        assert!(injector.held_keys().is_empty());
        assert!(injector.held_buttons().is_empty());

        // Verify keyboard received release events
        {
            let k = kbd_state.lock().unwrap();
            let evs = k.written_events();
            // 4 events from original presses (key + syn each), plus releases
            let releases = &evs[4..];
            assert!(releases.iter().any(|e| e.code == 30 && e.value == 0));
            assert!(releases.iter().any(|e| e.code == 42 && e.value == 0));
            assert_eq!(releases.last().unwrap().code, sys::SYN_REPORT as u16);
        }

        // Verify mouse received release events
        {
            let m = mouse_state.lock().unwrap();
            let evs = m.written_events();
            let releases = &evs[4..];
            assert!(releases
                .iter()
                .any(|e| e.code == sys::BTN_LEFT as u16 && e.value == 0));
            assert!(releases
                .iter()
                .any(|e| e.code == sys::BTN_RIGHT as u16 && e.value == 0));
            assert_eq!(releases.last().unwrap().code, sys::SYN_REPORT as u16);
        }

        // Calling release_all again is idempotent
        injector.release_all().unwrap();

        // Now test drop release
        injector
            .inject_event(InputEvent::KeyboardKey {
                keycode: 1, // ESC
                state: ElementState::Pressed,
                modifiers: KeyModifiers::empty(),
            })
            .unwrap();
        assert_eq!(injector.held_keys().len(), 1);

        drop(injector);

        // Verify ESC was released on drop
        let k = kbd_state.lock().unwrap();
        let evs = k.written_events();
        assert!(evs.iter().any(|e| e.code == 1 && e.value == 0));
    }

    #[test]
    fn test_uinput_errors_missing_and_permission_denied() {
        // Missing device
        let missing_path = PathBuf::from("/tmp/non_existent_uinput_test_path_12345");
        let err = RealDeviceOpener::new(&missing_path)
            .open_device()
            .unwrap_err();
        match err {
            InputError::UinputNotFound(p) => {
                assert_eq!(p, missing_path);
                assert!(p.to_string_lossy().contains("non_existent"));
            }
            other => panic!("Expected UinputNotFound, got {:?}", other),
        }

        // Permission denied simulation via from_open_io
        let io_err = std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Permission denied (os error 13)",
        );
        let classified = InputError::from_open_io(io_err, &PathBuf::from("/dev/uinput"));
        match classified {
            InputError::UinputPermissionDenied(ref p) => {
                assert_eq!(p, &PathBuf::from("/dev/uinput"));
                let msg = classified.to_string();
                assert!(msg.contains("input"));
                assert!(msg.contains("99-uinput.rules"));
            }
            other => panic!("Expected UinputPermissionDenied, got {:?}", other),
        }
    }

    #[test]
    fn test_auto_input_injector_fallback() {
        // When /dev/uinput is unavailable, AutoInputInjector creates a MockInputInjector fallback
        let mut auto = AutoInputInjector::new();
        assert!(!auto.is_uinput());

        let ev = InputEvent::MouseMoveRelative { dx: 10, dy: -2 };
        auto.inject_event(ev.clone()).unwrap();
        auto.release_all().unwrap();
    }

    #[test]
    fn test_auto_input_injector_with_mock_opener() {
        let opener = MockDeviceOpener::new();
        let config = UinputConfig::default();
        let mut auto = AutoInputInjector::new_with_opener(&opener, &config);
        assert!(auto.is_uinput());

        let ev = InputEvent::MouseMoveRelative { dx: 10, dy: -2 };
        auto.inject_event(ev.clone()).unwrap();
        assert_eq!(opener.devices().len(), 3);
    }

    #[test]
    fn test_clipboard_sync_graceful_handling() {
        let MockUinputDevices { mut injector, .. } = UinputInjector::new_mock();
        let ev = InputEvent::ClipboardSync {
            mime_type: "text/plain".into(),
            content: b"hello".to_vec(),
        };
        assert!(injector.inject_event(ev).is_ok());
    }

    struct FailingOpener;
    impl UinputDeviceOpener for FailingOpener {
        fn open_device(&self) -> Result<Box<dyn UinputDeviceFile>, InputError> {
            Err(InputError::DeviceOpenFailed("Simulated error".into()))
        }
    }

    #[test]
    fn test_auto_input_injector_demotes_on_opener_error() {
        let opener = FailingOpener;
        let config = UinputConfig::default();
        let mut auto = AutoInputInjector::new_with_opener(&opener, &config);
        assert!(!auto.is_uinput());
        assert!(auto
            .inject_event(InputEvent::MouseMoveRelative { dx: 1, dy: 1 })
            .is_ok());
    }

    #[derive(Debug)]
    struct FailingDevice {
        fail_evbit: bool,
    }
    impl UinputDeviceFile for FailingDevice {
        fn set_evbit(&mut self, _kind: EventKind) -> Result<(), InputError> {
            if self.fail_evbit {
                Err(InputError::DeviceSetupFailed("evbit failed".into()))
            } else {
                Ok(())
            }
        }
        fn set_keybit(&mut self, _key: Key) -> Result<(), InputError> {
            Ok(())
        }
        fn set_relbit(&mut self, _axis: RelativeAxis) -> Result<(), InputError> {
            Ok(())
        }
        fn set_absbit(&mut self, _axis: AbsoluteAxis) -> Result<(), InputError> {
            Ok(())
        }
        fn create_device(
            &mut self,
            _id: &InputId,
            _name: &[u8],
            _ff: u32,
            _abs: &[AbsoluteInfoSetup],
        ) -> Result<(), InputError> {
            Ok(())
        }
        fn write_events(&mut self, _events: &[RawInputEvent]) -> Result<usize, InputError> {
            Ok(0)
        }
        fn destroy_device(&mut self) -> Result<(), InputError> {
            Ok(())
        }
    }

    struct FailingDeviceOpener;
    impl UinputDeviceOpener for FailingDeviceOpener {
        fn open_device(&self) -> Result<Box<dyn UinputDeviceFile>, InputError> {
            Ok(Box::new(FailingDevice { fail_evbit: true }))
        }
    }

    #[test]
    fn test_uinput_setup_failure_propagates_error() {
        let opener = FailingDeviceOpener;
        let config = UinputConfig::default();
        let res = UinputInjector::new_with_opener(&opener, &config);
        assert!(res.is_err());
    }
}

//! Main client input capture coordinator mapping Android and Quest controller events.

use linux_quest_protocol::{ElementState, InputEvent, MouseButton};

use super::coalescer::MouseCoalescer;
use super::keycodes::{android_keycode_to_evdev, android_meta_to_modifiers};
use super::source::{
    AndroidKeyEvent, AndroidMotionEvent, CapturedEvent, ControllerInputEvent, InputSource,
    AKEY_EVENT_ACTION_DOWN, AMOTION_EVENT_ACTION_BUTTON_PRESS, AMOTION_EVENT_ACTION_BUTTON_RELEASE,
    AMOTION_EVENT_ACTION_DOWN, AMOTION_EVENT_ACTION_HOVER_MOVE, AMOTION_EVENT_ACTION_MOVE,
    AMOTION_EVENT_ACTION_SCROLL, AMOTION_EVENT_ACTION_UP, AMOTION_EVENT_BUTTON_BACK,
    AMOTION_EVENT_BUTTON_FORWARD, AMOTION_EVENT_BUTTON_PRIMARY, AMOTION_EVENT_BUTTON_SECONDARY,
    AMOTION_EVENT_BUTTON_TERTIARY,
};

/// Coordinates input capture, mapping, and coalescing for the Quest client.
pub struct ClientInputCapture<S: InputSource> {
    source: S,
    coalescer: MouseCoalescer,
    pending_discrete: Vec<InputEvent>,
    last_button_state: i32,
    last_controller_trigger: bool,
    last_controller_grip: bool,
    last_controller_thumbstick_click: bool,
    controller_sensitivity: f32,
    controller_deadzone: f32,
}

impl<S: InputSource> ClientInputCapture<S> {
    pub fn new(source: S, max_frequency_hz: u32) -> Self {
        Self {
            source,
            coalescer: MouseCoalescer::new(max_frequency_hz),
            pending_discrete: Vec::new(),
            last_button_state: 0,
            last_controller_trigger: false,
            last_controller_grip: false,
            last_controller_thumbstick_click: false,
            controller_sensitivity: 25.0, // Default controller cursor speed
            controller_deadzone: 0.08,    // 8% deadzone
        }
    }

    pub fn source_mut(&mut self) -> &mut S {
        &mut self.source
    }

    pub fn coalescer_mut(&mut self) -> &mut MouseCoalescer {
        &mut self.coalescer
    }

    pub fn set_controller_sensitivity(&mut self, sensitivity: f32) {
        self.controller_sensitivity = sensitivity;
    }

    pub fn set_controller_deadzone(&mut self, deadzone: f32) {
        self.controller_deadzone = deadzone;
    }

    /// Poll all pending events from the input source and translate them into protocol events.
    pub fn poll_and_process(&mut self) {
        let events = self.source.poll_events();
        for event in events {
            match event {
                CapturedEvent::Key(key_event) => self.process_key_event(&key_event),
                CapturedEvent::Motion(motion_event) => self.process_motion_event(&motion_event),
                CapturedEvent::Controller(ctrl_event) => self.process_controller_event(&ctrl_event),
                CapturedEvent::Protocol(proto_event) => self.pending_discrete.push(proto_event),
            }
        }
    }

    /// Process an Android KeyEvent and convert to evdev keycode + modifier flags.
    fn process_key_event(&mut self, key: &AndroidKeyEvent) {
        if let Some(evdev_keycode) = android_keycode_to_evdev(key.keycode) {
            let state = if key.action == AKEY_EVENT_ACTION_DOWN {
                ElementState::Pressed
            } else {
                ElementState::Released
            };
            let modifiers = android_meta_to_modifiers(key.meta_state);
            self.pending_discrete.push(InputEvent::KeyboardKey {
                keycode: evdev_keycode,
                state,
                modifiers,
            });
        }
    }

    /// Process an Android MotionEvent from Bluetooth mouse or touchpad.
    fn process_motion_event(&mut self, motion: &AndroidMotionEvent) {
        // 1. Check for discrete mouse button bit transitions
        let diff = motion.button_state ^ self.last_button_state;
        if diff != 0 {
            let button_checks = [
                (AMOTION_EVENT_BUTTON_PRIMARY, MouseButton::Left),
                (AMOTION_EVENT_BUTTON_SECONDARY, MouseButton::Right),
                (AMOTION_EVENT_BUTTON_TERTIARY, MouseButton::Middle),
                (AMOTION_EVENT_BUTTON_BACK, MouseButton::Back),
                (AMOTION_EVENT_BUTTON_FORWARD, MouseButton::Forward),
            ];

            for (mask, button) in button_checks {
                if (diff & mask) != 0 {
                    let state = if (motion.button_state & mask) != 0 {
                        ElementState::Pressed
                    } else {
                        ElementState::Released
                    };
                    self.pending_discrete
                        .push(InputEvent::MouseButton { button, state });
                }
            }
            self.last_button_state = motion.button_state;
        } else if motion.action == AMOTION_EVENT_ACTION_BUTTON_PRESS
            || motion.action == AMOTION_EVENT_ACTION_BUTTON_RELEASE
        {
            let state = if motion.action == AMOTION_EVENT_ACTION_BUTTON_PRESS {
                ElementState::Pressed
            } else {
                ElementState::Released
            };
            let btn = match motion.action_button {
                AMOTION_EVENT_BUTTON_PRIMARY => Some(MouseButton::Left),
                AMOTION_EVENT_BUTTON_SECONDARY => Some(MouseButton::Right),
                AMOTION_EVENT_BUTTON_TERTIARY => Some(MouseButton::Middle),
                AMOTION_EVENT_BUTTON_BACK => Some(MouseButton::Back),
                AMOTION_EVENT_BUTTON_FORWARD => Some(MouseButton::Forward),
                _ => None,
            };
            if let Some(button) = btn {
                self.pending_discrete
                    .push(InputEvent::MouseButton { button, state });
            }
        } else if motion.action == AMOTION_EVENT_ACTION_DOWN && motion.button_state == 0 {
            // Touch / primary pointer tap down
            self.pending_discrete.push(InputEvent::MouseButton {
                button: MouseButton::Left,
                state: ElementState::Pressed,
            });
        } else if motion.action == AMOTION_EVENT_ACTION_UP && motion.button_state == 0 {
            // Touch / primary pointer tap release
            self.pending_discrete.push(InputEvent::MouseButton {
                button: MouseButton::Left,
                state: ElementState::Released,
            });
        }

        // 2. Relative or absolute mouse motion
        if motion.relative_x.abs() > 0.001 || motion.relative_y.abs() > 0.001 {
            self.coalescer
                .accumulate_relative(motion.relative_x, motion.relative_y);
        } else if diff == 0
            && (motion.action == AMOTION_EVENT_ACTION_MOVE
                || motion.action == AMOTION_EVENT_ACTION_HOVER_MOVE)
            && (motion.x > 0.0 || motion.y > 0.0)
        {
            self.coalescer
                .accumulate_absolute(motion.x as u32, motion.y as u32, 0);
        }

        // 3. High-resolution scroll
        if motion.action == AMOTION_EVENT_ACTION_SCROLL
            || motion.vscroll.abs() > 0.001
            || motion.hscroll.abs() > 0.001
        {
            self.coalescer
                .accumulate_scroll(motion.hscroll, motion.vscroll, true);
        }
    }

    /// Process Quest VR controller inputs as virtual mouse movements and clicks.
    fn process_controller_event(&mut self, ctrl: &ControllerInputEvent) {
        // Controller pointer absolute position (e.g. raycast intersection on virtual screen)
        if let (Some(px), Some(py)) = (ctrl.pointer_x, ctrl.pointer_y) {
            self.coalescer
                .accumulate_absolute(px as u32, py as u32, ctrl.display_id);
        } else {
            // Controller thumbstick-as-mouse relative motion
            let mut sx = ctrl.thumbstick_x;
            let mut sy = ctrl.thumbstick_y;

            if sx.abs() < self.controller_deadzone {
                sx = 0.0;
            }
            if sy.abs() < self.controller_deadzone {
                sy = 0.0;
            }

            if sx != 0.0 || sy != 0.0 {
                // Invert sy so pushing stick forward moves cursor up
                let dx = sx * self.controller_sensitivity;
                let dy = -sy * self.controller_sensitivity;
                self.coalescer.accumulate_relative(dx, dy);
            }
        }

        // Controller trigger as mouse Left Button
        if ctrl.trigger_pressed != self.last_controller_trigger {
            let state = if ctrl.trigger_pressed {
                ElementState::Pressed
            } else {
                ElementState::Released
            };
            self.pending_discrete.push(InputEvent::MouseButton {
                button: MouseButton::Left,
                state,
            });
            self.last_controller_trigger = ctrl.trigger_pressed;
        }

        // Controller grip as mouse Right Button
        if ctrl.grip_pressed != self.last_controller_grip {
            let state = if ctrl.grip_pressed {
                ElementState::Pressed
            } else {
                ElementState::Released
            };
            self.pending_discrete.push(InputEvent::MouseButton {
                button: MouseButton::Right,
                state,
            });
            self.last_controller_grip = ctrl.grip_pressed;
        }

        // Controller thumbstick click as mouse Middle Button
        if ctrl.thumbstick_click != self.last_controller_thumbstick_click {
            let state = if ctrl.thumbstick_click {
                ElementState::Pressed
            } else {
                ElementState::Released
            };
            self.pending_discrete.push(InputEvent::MouseButton {
                button: MouseButton::Middle,
                state,
            });
            self.last_controller_thumbstick_click = ctrl.thumbstick_click;
        }
    }

    /// Flush all discrete events and coalesced motion for the current frame.
    pub fn flush_frame(&mut self, timestamp_us: u64) -> Vec<InputEvent> {
        let mut events = std::mem::take(&mut self.pending_discrete);
        let coalesced_events = self.coalescer.flush(timestamp_us, true);
        events.extend(coalesced_events);
        events
    }

    /// Step the capture module for one frame: polls, processes, and flushes events.
    pub fn step_frame(&mut self, timestamp_us: u64) -> Vec<InputEvent> {
        self.poll_and_process();
        self.flush_frame(timestamp_us)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::keycodes::{AKEYCODE_A, AKEYCODE_ENTER, AMETA_SHIFT_ON, KEY_A, KEY_ENTER};
    use crate::input::source::{
        MockInputSource, AMOTION_EVENT_BUTTON_PRIMARY, AMOTION_EVENT_BUTTON_SECONDARY,
    };

    #[test]
    fn test_capture_keyboard_mapping() {
        let mut source = MockInputSource::new();
        source.push_key(AKEY_EVENT_ACTION_DOWN, AKEYCODE_A, AMETA_SHIFT_ON, 1000);
        source.push_key(1, AKEYCODE_ENTER, 0, 2000);

        let mut capture = ClientInputCapture::new(source, 120);
        let events = capture.step_frame(10_000);

        assert_eq!(events.len(), 2);
        match &events[0] {
            InputEvent::KeyboardKey {
                keycode,
                state,
                modifiers,
            } => {
                assert_eq!(*keycode, KEY_A);
                assert_eq!(*state, ElementState::Pressed);
                assert!(modifiers.is_shift());
            }
            other => panic!("Unexpected event: {:?}", other),
        }
        match &events[1] {
            InputEvent::KeyboardKey { keycode, state, .. } => {
                assert_eq!(*keycode, KEY_ENTER);
                assert_eq!(*state, ElementState::Released);
            }
            other => panic!("Unexpected event: {:?}", other),
        }
    }

    #[test]
    fn test_capture_mouse_motion_coalescing() {
        let mut source = MockInputSource::new();
        // Three rapid mouse movements in one frame
        source.push_motion(AndroidMotionEvent {
            relative_x: 10.0,
            relative_y: -5.0,
            ..Default::default()
        });
        source.push_motion(AndroidMotionEvent {
            relative_x: 20.0,
            relative_y: 15.0,
            ..Default::default()
        });
        source.push_motion(AndroidMotionEvent {
            relative_x: -5.0,
            relative_y: -2.0,
            ..Default::default()
        });

        let mut capture = ClientInputCapture::new(source, 120);
        let events = capture.step_frame(10_000);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0], InputEvent::MouseMoveRelative { dx: 25, dy: 8 });
    }

    #[test]
    fn test_capture_mouse_buttons() {
        let mut source = MockInputSource::new();
        // Mouse Left Press via button_state
        source.push_motion(AndroidMotionEvent {
            button_state: AMOTION_EVENT_BUTTON_PRIMARY,
            ..Default::default()
        });
        // Mouse Right Press added to button_state
        source.push_motion(AndroidMotionEvent {
            button_state: AMOTION_EVENT_BUTTON_PRIMARY | AMOTION_EVENT_BUTTON_SECONDARY,
            ..Default::default()
        });
        // Mouse Left Release
        source.push_motion(AndroidMotionEvent {
            button_state: AMOTION_EVENT_BUTTON_SECONDARY,
            ..Default::default()
        });

        let mut capture = ClientInputCapture::new(source, 120);
        let events = capture.step_frame(10_000);

        assert_eq!(events.len(), 3);
        assert_eq!(
            events[0],
            InputEvent::MouseButton {
                button: MouseButton::Left,
                state: ElementState::Pressed
            }
        );
        assert_eq!(
            events[1],
            InputEvent::MouseButton {
                button: MouseButton::Right,
                state: ElementState::Pressed
            }
        );
        assert_eq!(
            events[2],
            InputEvent::MouseButton {
                button: MouseButton::Left,
                state: ElementState::Released
            }
        );
    }

    #[test]
    fn test_capture_controller_as_mouse() {
        let mut source = MockInputSource::new();
        source.push_controller(ControllerInputEvent {
            thumbstick_x: 0.5,
            thumbstick_y: 0.2, // Pushing forward
            trigger_pressed: true,
            grip_pressed: false,
            thumbstick_click: false,
            ..Default::default()
        });

        let mut capture = ClientInputCapture::new(source, 120);
        let events = capture.step_frame(10_000);

        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            InputEvent::MouseButton {
                button: MouseButton::Left,
                state: ElementState::Pressed
            }
        );
        match events[1] {
            InputEvent::MouseMoveRelative { dx, dy } => {
                assert!(dx > 0);
                assert!(dy < 0); // Inverted Y: forward = up = negative dy
            }
            ref other => panic!("Expected MouseMoveRelative, got {:?}", other),
        }
    }
}

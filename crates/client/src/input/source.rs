//! Abstract input sources and Android event structures for input capture.

use std::collections::VecDeque;

// Android MotionEvent actions
pub const AMOTION_EVENT_ACTION_DOWN: i32 = 0;
pub const AMOTION_EVENT_ACTION_UP: i32 = 1;
pub const AMOTION_EVENT_ACTION_MOVE: i32 = 2;
pub const AMOTION_EVENT_ACTION_HOVER_MOVE: i32 = 7;
pub const AMOTION_EVENT_ACTION_SCROLL: i32 = 8;
pub const AMOTION_EVENT_ACTION_BUTTON_PRESS: i32 = 11;
pub const AMOTION_EVENT_ACTION_BUTTON_RELEASE: i32 = 12;

// Android MotionEvent button bitmasks
pub const AMOTION_EVENT_BUTTON_PRIMARY: i32 = 1; // Left mouse button
pub const AMOTION_EVENT_BUTTON_SECONDARY: i32 = 2; // Right mouse button
pub const AMOTION_EVENT_BUTTON_TERTIARY: i32 = 4; // Middle mouse button
pub const AMOTION_EVENT_BUTTON_BACK: i32 = 8;
pub const AMOTION_EVENT_BUTTON_FORWARD: i32 = 16;

// Android KeyEvent actions
pub const AKEY_EVENT_ACTION_DOWN: i32 = 0;
pub const AKEY_EVENT_ACTION_UP: i32 = 1;

/// Android-compatible KeyEvent representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndroidKeyEvent {
    pub action: i32,
    pub keycode: i32,
    pub meta_state: i32,
    pub event_time_ns: i64,
}

impl AndroidKeyEvent {
    pub fn new(action: i32, keycode: i32, meta_state: i32, event_time_ns: i64) -> Self {
        Self {
            action,
            keycode,
            meta_state,
            event_time_ns,
        }
    }
}

/// Android-compatible MotionEvent representation for mouse, touchpad, and pointer devices.
#[derive(Debug, Clone, PartialEq)]
pub struct AndroidMotionEvent {
    pub action: i32,
    pub action_button: i32,
    pub button_state: i32,
    pub x: f32,
    pub y: f32,
    pub relative_x: f32,
    pub relative_y: f32,
    pub vscroll: f32,
    pub hscroll: f32,
    pub meta_state: i32,
    pub source: i32,
    pub event_time_ns: i64,
}

impl Default for AndroidMotionEvent {
    fn default() -> Self {
        Self {
            action: AMOTION_EVENT_ACTION_MOVE,
            action_button: 0,
            button_state: 0,
            x: 0.0,
            y: 0.0,
            relative_x: 0.0,
            relative_y: 0.0,
            vscroll: 0.0,
            hscroll: 0.0,
            meta_state: 0,
            source: 0x2002, // SOURCE_MOUSE
            event_time_ns: 0,
        }
    }
}

/// Quest VR controller input event (for controller-as-mouse emulation).
#[derive(Debug, Clone, PartialEq)]
pub struct ControllerInputEvent {
    pub controller_id: u8, // 0 = Left, 1 = Right
    pub thumbstick_x: f32, // -1.0 to 1.0
    pub thumbstick_y: f32, // -1.0 to 1.0
    pub pointer_x: Option<f32>,
    pub pointer_y: Option<f32>,
    pub display_id: u16,
    pub trigger_pressed: bool,
    pub grip_pressed: bool,
    pub thumbstick_click: bool,
    pub timestamp_us: u64,
}

impl Default for ControllerInputEvent {
    fn default() -> Self {
        Self {
            controller_id: 1, // Right hand dominant by default
            thumbstick_x: 0.0,
            thumbstick_y: 0.0,
            pointer_x: None,
            pointer_y: None,
            display_id: 0,
            trigger_pressed: false,
            grip_pressed: false,
            thumbstick_click: false,
            timestamp_us: 0,
        }
    }
}

/// Generic captured input event from any device.
#[derive(Debug, Clone, PartialEq)]
pub enum CapturedEvent {
    Key(AndroidKeyEvent),
    Motion(AndroidMotionEvent),
    Controller(ControllerInputEvent),
    Protocol(linux_quest_protocol::InputEvent),
}

/// Trait for polling input events from Android system or mock sources.
pub trait InputSource: Send + Sync {
    fn poll_events(&mut self) -> Vec<CapturedEvent>;
}

/// In-memory mock input source for unit and integration testing without Android hardware.
#[derive(Default)]
pub struct MockInputSource {
    events: VecDeque<CapturedEvent>,
}

impl MockInputSource {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, event: CapturedEvent) {
        self.events.push_back(event);
    }

    pub fn push_key(&mut self, action: i32, keycode: i32, meta_state: i32, event_time_ns: i64) {
        self.push(CapturedEvent::Key(AndroidKeyEvent::new(
            action,
            keycode,
            meta_state,
            event_time_ns,
        )));
    }

    pub fn push_motion(&mut self, event: AndroidMotionEvent) {
        self.push(CapturedEvent::Motion(event));
    }

    pub fn push_controller(&mut self, event: ControllerInputEvent) {
        self.push(CapturedEvent::Controller(event));
    }
}

impl InputSource for MockInputSource {
    fn poll_events(&mut self) -> Vec<CapturedEvent> {
        self.events.drain(..).collect()
    }
}

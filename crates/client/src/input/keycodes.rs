//! Android KeyEvent keycode and meta state to Linux evdev keycode mapping.

use linux_quest_protocol::KeyModifiers;

// Android Keycodes (from android.view.KeyEvent / ndk keycodes.h)
pub const AKEYCODE_UNKNOWN: i32 = 0;
pub const AKEYCODE_HOME: i32 = 3;
pub const AKEYCODE_BACK: i32 = 4;
pub const AKEYCODE_0: i32 = 7;
pub const AKEYCODE_1: i32 = 8;
pub const AKEYCODE_2: i32 = 9;
pub const AKEYCODE_3: i32 = 10;
pub const AKEYCODE_4: i32 = 11;
pub const AKEYCODE_5: i32 = 12;
pub const AKEYCODE_6: i32 = 13;
pub const AKEYCODE_7: i32 = 14;
pub const AKEYCODE_8: i32 = 15;
pub const AKEYCODE_9: i32 = 16;
pub const AKEYCODE_STAR: i32 = 17;
pub const AKEYCODE_POUND: i32 = 18;
pub const AKEYCODE_DPAD_UP: i32 = 19;
pub const AKEYCODE_DPAD_DOWN: i32 = 20;
pub const AKEYCODE_DPAD_LEFT: i32 = 21;
pub const AKEYCODE_DPAD_RIGHT: i32 = 22;
pub const AKEYCODE_DPAD_CENTER: i32 = 23;
pub const AKEYCODE_VOLUME_UP: i32 = 24;
pub const AKEYCODE_VOLUME_DOWN: i32 = 25;
pub const AKEYCODE_POWER: i32 = 26;
pub const AKEYCODE_CAMERA: i32 = 27;
pub const AKEYCODE_CLEAR: i32 = 28;
pub const AKEYCODE_A: i32 = 29;
pub const AKEYCODE_B: i32 = 30;
pub const AKEYCODE_C: i32 = 31;
pub const AKEYCODE_D: i32 = 32;
pub const AKEYCODE_E: i32 = 33;
pub const AKEYCODE_F: i32 = 34;
pub const AKEYCODE_G: i32 = 35;
pub const AKEYCODE_H: i32 = 36;
pub const AKEYCODE_I: i32 = 37;
pub const AKEYCODE_J: i32 = 38;
pub const AKEYCODE_K: i32 = 39;
pub const AKEYCODE_L: i32 = 40;
pub const AKEYCODE_M: i32 = 41;
pub const AKEYCODE_N: i32 = 42;
pub const AKEYCODE_O: i32 = 43;
pub const AKEYCODE_P: i32 = 44;
pub const AKEYCODE_Q: i32 = 45;
pub const AKEYCODE_R: i32 = 46;
pub const AKEYCODE_S: i32 = 47;
pub const AKEYCODE_T: i32 = 48;
pub const AKEYCODE_U: i32 = 49;
pub const AKEYCODE_V: i32 = 50;
pub const AKEYCODE_W: i32 = 51;
pub const AKEYCODE_X: i32 = 52;
pub const AKEYCODE_Y: i32 = 53;
pub const AKEYCODE_Z: i32 = 54;
pub const AKEYCODE_COMMA: i32 = 55;
pub const AKEYCODE_PERIOD: i32 = 56;
pub const AKEYCODE_ALT_LEFT: i32 = 57;
pub const AKEYCODE_ALT_RIGHT: i32 = 58;
pub const AKEYCODE_SHIFT_LEFT: i32 = 59;
pub const AKEYCODE_SHIFT_RIGHT: i32 = 60;
pub const AKEYCODE_TAB: i32 = 61;
pub const AKEYCODE_SPACE: i32 = 62;
pub const AKEYCODE_ENTER: i32 = 66;
pub const AKEYCODE_DEL: i32 = 67; // Backspace
pub const AKEYCODE_GRAVE: i32 = 68;
pub const AKEYCODE_MINUS: i32 = 69;
pub const AKEYCODE_EQUALS: i32 = 70;
pub const AKEYCODE_LEFT_BRACKET: i32 = 71;
pub const AKEYCODE_RIGHT_BRACKET: i32 = 72;
pub const AKEYCODE_BACKSLASH: i32 = 73;
pub const AKEYCODE_SEMICOLON: i32 = 74;
pub const AKEYCODE_APOSTROPHE: i32 = 75;
pub const AKEYCODE_SLASH: i32 = 76;
pub const AKEYCODE_AT: i32 = 77;
pub const AKEYCODE_PAGE_UP: i32 = 92;
pub const AKEYCODE_PAGE_DOWN: i32 = 93;
pub const AKEYCODE_ESCAPE: i32 = 111;
pub const AKEYCODE_FORWARD_DEL: i32 = 112; // Delete
pub const AKEYCODE_CTRL_LEFT: i32 = 113;
pub const AKEYCODE_CTRL_RIGHT: i32 = 114;
pub const AKEYCODE_CAPS_LOCK: i32 = 115;
pub const AKEYCODE_SCROLL_LOCK: i32 = 116;
pub const AKEYCODE_META_LEFT: i32 = 117;
pub const AKEYCODE_META_RIGHT: i32 = 118;
pub const AKEYCODE_FUNCTION: i32 = 119;
pub const AKEYCODE_SYSRQ: i32 = 120; // PrintScreen
pub const AKEYCODE_BREAK: i32 = 121; // Pause
pub const AKEYCODE_MOVE_HOME: i32 = 122;
pub const AKEYCODE_MOVE_END: i32 = 123;
pub const AKEYCODE_INSERT: i32 = 124;
pub const AKEYCODE_FORWARD: i32 = 125;
pub const AKEYCODE_F1: i32 = 131;
pub const AKEYCODE_F2: i32 = 132;
pub const AKEYCODE_F3: i32 = 133;
pub const AKEYCODE_F4: i32 = 134;
pub const AKEYCODE_F5: i32 = 135;
pub const AKEYCODE_F6: i32 = 136;
pub const AKEYCODE_F7: i32 = 137;
pub const AKEYCODE_F8: i32 = 138;
pub const AKEYCODE_F9: i32 = 139;
pub const AKEYCODE_F10: i32 = 140;
pub const AKEYCODE_F11: i32 = 141;
pub const AKEYCODE_F12: i32 = 142;
pub const AKEYCODE_NUM_LOCK: i32 = 143;
pub const AKEYCODE_NUMPAD_0: i32 = 144;
pub const AKEYCODE_NUMPAD_1: i32 = 145;
pub const AKEYCODE_NUMPAD_2: i32 = 146;
pub const AKEYCODE_NUMPAD_3: i32 = 147;
pub const AKEYCODE_NUMPAD_4: i32 = 148;
pub const AKEYCODE_NUMPAD_5: i32 = 149;
pub const AKEYCODE_NUMPAD_6: i32 = 150;
pub const AKEYCODE_NUMPAD_7: i32 = 151;
pub const AKEYCODE_NUMPAD_8: i32 = 152;
pub const AKEYCODE_NUMPAD_9: i32 = 153;
pub const AKEYCODE_NUMPAD_DIVIDE: i32 = 154;
pub const AKEYCODE_NUMPAD_MULTIPLY: i32 = 155;
pub const AKEYCODE_NUMPAD_SUBTRACT: i32 = 156;
pub const AKEYCODE_NUMPAD_ADD: i32 = 157;
pub const AKEYCODE_NUMPAD_DOT: i32 = 158;
pub const AKEYCODE_NUMPAD_COMMA: i32 = 159;
pub const AKEYCODE_NUMPAD_ENTER: i32 = 160;
pub const AKEYCODE_NUMPAD_EQUALS: i32 = 161;

// Android Meta States
pub const AMETA_SHIFT_ON: i32 = 0x01;
pub const AMETA_ALT_ON: i32 = 0x02;
pub const AMETA_ALT_LEFT_ON: i32 = 0x10;
pub const AMETA_ALT_RIGHT_ON: i32 = 0x20;
pub const AMETA_SHIFT_LEFT_ON: i32 = 0x40;
pub const AMETA_SHIFT_RIGHT_ON: i32 = 0x80;
pub const AMETA_CTRL_ON: i32 = 0x1000;
pub const AMETA_CTRL_LEFT_ON: i32 = 0x2000;
pub const AMETA_CTRL_RIGHT_ON: i32 = 0x4000;
pub const AMETA_META_ON: i32 = 0x10000;
pub const AMETA_META_LEFT_ON: i32 = 0x20000;
pub const AMETA_META_RIGHT_ON: i32 = 0x40000;
pub const AMETA_CAPS_LOCK_ON: i32 = 0x100000;
pub const AMETA_NUM_LOCK_ON: i32 = 0x200000;
pub const AMETA_SCROLL_LOCK_ON: i32 = 0x400000;

// Linux evdev keycodes (from <linux/input-event-codes.h>)
pub const KEY_ESC: u32 = 1;
pub const KEY_1: u32 = 2;
pub const KEY_2: u32 = 3;
pub const KEY_3: u32 = 4;
pub const KEY_4: u32 = 5;
pub const KEY_5: u32 = 6;
pub const KEY_6: u32 = 7;
pub const KEY_7: u32 = 8;
pub const KEY_8: u32 = 9;
pub const KEY_9: u32 = 10;
pub const KEY_0: u32 = 11;
pub const KEY_MINUS: u32 = 12;
pub const KEY_EQUAL: u32 = 13;
pub const KEY_BACKSPACE: u32 = 14;
pub const KEY_TAB: u32 = 15;
pub const KEY_Q: u32 = 16;
pub const KEY_W: u32 = 17;
pub const KEY_E: u32 = 18;
pub const KEY_R: u32 = 19;
pub const KEY_T: u32 = 20;
pub const KEY_Y: u32 = 21;
pub const KEY_U: u32 = 22;
pub const KEY_I: u32 = 23;
pub const KEY_O: u32 = 24;
pub const KEY_P: u32 = 25;
pub const KEY_LEFTBRACE: u32 = 26;
pub const KEY_RIGHTBRACE: u32 = 27;
pub const KEY_ENTER: u32 = 28;
pub const KEY_LEFTCTRL: u32 = 29;
pub const KEY_A: u32 = 30;
pub const KEY_S: u32 = 31;
pub const KEY_D: u32 = 32;
pub const KEY_F: u32 = 33;
pub const KEY_G: u32 = 34;
pub const KEY_H: u32 = 35;
pub const KEY_J: u32 = 36;
pub const KEY_K: u32 = 37;
pub const KEY_L: u32 = 38;
pub const KEY_SEMICOLON: u32 = 39;
pub const KEY_APOSTROPHE: u32 = 40;
pub const KEY_GRAVE: u32 = 41;
pub const KEY_LEFTSHIFT: u32 = 42;
pub const KEY_BACKSLASH: u32 = 43;
pub const KEY_Z: u32 = 44;
pub const KEY_X: u32 = 45;
pub const KEY_C: u32 = 46;
pub const KEY_V: u32 = 47;
pub const KEY_B: u32 = 48;
pub const KEY_N: u32 = 49;
pub const KEY_M: u32 = 50;
pub const KEY_COMMA: u32 = 51;
pub const KEY_DOT: u32 = 52;
pub const KEY_SLASH: u32 = 53;
pub const KEY_RIGHTSHIFT: u32 = 54;
pub const KEY_KPASTERISK: u32 = 55;
pub const KEY_LEFTALT: u32 = 56;
pub const KEY_SPACE: u32 = 57;
pub const KEY_CAPSLOCK: u32 = 58;
pub const KEY_F1: u32 = 59;
pub const KEY_F2: u32 = 60;
pub const KEY_F3: u32 = 61;
pub const KEY_F4: u32 = 62;
pub const KEY_F5: u32 = 63;
pub const KEY_F6: u32 = 64;
pub const KEY_F7: u32 = 65;
pub const KEY_F8: u32 = 66;
pub const KEY_F9: u32 = 67;
pub const KEY_F10: u32 = 68;
pub const KEY_NUMLOCK: u32 = 69;
pub const KEY_SCROLLLOCK: u32 = 70;
pub const KEY_KP7: u32 = 71;
pub const KEY_KP8: u32 = 72;
pub const KEY_KP9: u32 = 73;
pub const KEY_KPMINUS: u32 = 74;
pub const KEY_KP4: u32 = 75;
pub const KEY_KP5: u32 = 76;
pub const KEY_KP6: u32 = 77;
pub const KEY_KPPLUS: u32 = 78;
pub const KEY_KP1: u32 = 79;
pub const KEY_KP2: u32 = 80;
pub const KEY_KP3: u32 = 81;
pub const KEY_KP0: u32 = 82;
pub const KEY_KPDOT: u32 = 83;
pub const KEY_F11: u32 = 87;
pub const KEY_F12: u32 = 88;
pub const KEY_KPENTER: u32 = 96;
pub const KEY_RIGHTCTRL: u32 = 97;
pub const KEY_KPSLASH: u32 = 98;
pub const KEY_SYSRQ: u32 = 99;
pub const KEY_RIGHTALT: u32 = 100;
pub const KEY_HOME: u32 = 102;
pub const KEY_UP: u32 = 103;
pub const KEY_PAGEUP: u32 = 104;
pub const KEY_LEFT: u32 = 105;
pub const KEY_RIGHT: u32 = 106;
pub const KEY_END: u32 = 107;
pub const KEY_DOWN: u32 = 108;
pub const KEY_PAGEDOWN: u32 = 109;
pub const KEY_INSERT: u32 = 110;
pub const KEY_DELETE: u32 = 111;
pub const KEY_VOLUMEDOWN: u32 = 114;
pub const KEY_VOLUMEUP: u32 = 115;
pub const KEY_POWER: u32 = 116;
pub const KEY_PAUSE: u32 = 119;
pub const KEY_LEFTMETA: u32 = 125;
pub const KEY_RIGHTMETA: u32 = 126;
pub const KEY_FORWARD: u32 = 159;

/// Map an Android `KeyEvent` keycode to standard Linux evdev keycode.
pub fn android_keycode_to_evdev(android_keycode: i32) -> Option<u32> {
    match android_keycode {
        AKEYCODE_A => Some(KEY_A),
        AKEYCODE_B => Some(KEY_B),
        AKEYCODE_C => Some(KEY_C),
        AKEYCODE_D => Some(KEY_D),
        AKEYCODE_E => Some(KEY_E),
        AKEYCODE_F => Some(KEY_F),
        AKEYCODE_G => Some(KEY_G),
        AKEYCODE_H => Some(KEY_H),
        AKEYCODE_I => Some(KEY_I),
        AKEYCODE_J => Some(KEY_J),
        AKEYCODE_K => Some(KEY_K),
        AKEYCODE_L => Some(KEY_L),
        AKEYCODE_M => Some(KEY_M),
        AKEYCODE_N => Some(KEY_N),
        AKEYCODE_O => Some(KEY_O),
        AKEYCODE_P => Some(KEY_P),
        AKEYCODE_Q => Some(KEY_Q),
        AKEYCODE_R => Some(KEY_R),
        AKEYCODE_S => Some(KEY_S),
        AKEYCODE_T => Some(KEY_T),
        AKEYCODE_U => Some(KEY_U),
        AKEYCODE_V => Some(KEY_V),
        AKEYCODE_W => Some(KEY_W),
        AKEYCODE_X => Some(KEY_X),
        AKEYCODE_Y => Some(KEY_Y),
        AKEYCODE_Z => Some(KEY_Z),

        AKEYCODE_0 => Some(KEY_0),
        AKEYCODE_1 => Some(KEY_1),
        AKEYCODE_2 => Some(KEY_2),
        AKEYCODE_3 => Some(KEY_3),
        AKEYCODE_4 => Some(KEY_4),
        AKEYCODE_5 => Some(KEY_5),
        AKEYCODE_6 => Some(KEY_6),
        AKEYCODE_7 => Some(KEY_7),
        AKEYCODE_8 => Some(KEY_8),
        AKEYCODE_9 => Some(KEY_9),

        AKEYCODE_COMMA => Some(KEY_COMMA),
        AKEYCODE_PERIOD => Some(KEY_DOT),
        AKEYCODE_MINUS => Some(KEY_MINUS),
        AKEYCODE_EQUALS => Some(KEY_EQUAL),
        AKEYCODE_LEFT_BRACKET => Some(KEY_LEFTBRACE),
        AKEYCODE_RIGHT_BRACKET => Some(KEY_RIGHTBRACE),
        AKEYCODE_BACKSLASH => Some(KEY_BACKSLASH),
        AKEYCODE_SEMICOLON => Some(KEY_SEMICOLON),
        AKEYCODE_APOSTROPHE => Some(KEY_APOSTROPHE),
        AKEYCODE_SLASH => Some(KEY_SLASH),
        AKEYCODE_GRAVE => Some(KEY_GRAVE),

        AKEYCODE_ALT_LEFT => Some(KEY_LEFTALT),
        AKEYCODE_ALT_RIGHT => Some(KEY_RIGHTALT),
        AKEYCODE_SHIFT_LEFT => Some(KEY_LEFTSHIFT),
        AKEYCODE_SHIFT_RIGHT => Some(KEY_RIGHTSHIFT),
        AKEYCODE_CTRL_LEFT => Some(KEY_LEFTCTRL),
        AKEYCODE_CTRL_RIGHT => Some(KEY_RIGHTCTRL),
        AKEYCODE_META_LEFT => Some(KEY_LEFTMETA),
        AKEYCODE_META_RIGHT => Some(KEY_RIGHTMETA),
        AKEYCODE_CAPS_LOCK => Some(KEY_CAPSLOCK),
        AKEYCODE_SCROLL_LOCK => Some(KEY_SCROLLLOCK),
        AKEYCODE_NUM_LOCK => Some(KEY_NUMLOCK),

        AKEYCODE_TAB => Some(KEY_TAB),
        AKEYCODE_SPACE => Some(KEY_SPACE),
        AKEYCODE_ENTER | AKEYCODE_DPAD_CENTER => Some(KEY_ENTER),
        AKEYCODE_DEL => Some(KEY_BACKSPACE),
        AKEYCODE_FORWARD_DEL => Some(KEY_DELETE),
        AKEYCODE_ESCAPE | AKEYCODE_BACK => Some(KEY_ESC),
        AKEYCODE_INSERT => Some(KEY_INSERT),
        AKEYCODE_MOVE_HOME | AKEYCODE_HOME => Some(KEY_HOME),
        AKEYCODE_MOVE_END => Some(KEY_END),
        AKEYCODE_PAGE_UP => Some(KEY_PAGEUP),
        AKEYCODE_PAGE_DOWN => Some(KEY_PAGEDOWN),

        AKEYCODE_DPAD_UP => Some(KEY_UP),
        AKEYCODE_DPAD_DOWN => Some(KEY_DOWN),
        AKEYCODE_DPAD_LEFT => Some(KEY_LEFT),
        AKEYCODE_DPAD_RIGHT => Some(KEY_RIGHT),

        AKEYCODE_F1 => Some(KEY_F1),
        AKEYCODE_F2 => Some(KEY_F2),
        AKEYCODE_F3 => Some(KEY_F3),
        AKEYCODE_F4 => Some(KEY_F4),
        AKEYCODE_F5 => Some(KEY_F5),
        AKEYCODE_F6 => Some(KEY_F6),
        AKEYCODE_F7 => Some(KEY_F7),
        AKEYCODE_F8 => Some(KEY_F8),
        AKEYCODE_F9 => Some(KEY_F9),
        AKEYCODE_F10 => Some(KEY_F10),
        AKEYCODE_F11 => Some(KEY_F11),
        AKEYCODE_F12 => Some(KEY_F12),

        AKEYCODE_NUMPAD_0 => Some(KEY_KP0),
        AKEYCODE_NUMPAD_1 => Some(KEY_KP1),
        AKEYCODE_NUMPAD_2 => Some(KEY_KP2),
        AKEYCODE_NUMPAD_3 => Some(KEY_KP3),
        AKEYCODE_NUMPAD_4 => Some(KEY_KP4),
        AKEYCODE_NUMPAD_5 => Some(KEY_KP5),
        AKEYCODE_NUMPAD_6 => Some(KEY_KP6),
        AKEYCODE_NUMPAD_7 => Some(KEY_KP7),
        AKEYCODE_NUMPAD_8 => Some(KEY_KP8),
        AKEYCODE_NUMPAD_9 => Some(KEY_KP9),
        AKEYCODE_NUMPAD_DIVIDE => Some(KEY_KPSLASH),
        AKEYCODE_NUMPAD_MULTIPLY | AKEYCODE_STAR => Some(KEY_KPASTERISK),
        AKEYCODE_NUMPAD_SUBTRACT => Some(KEY_KPMINUS),
        AKEYCODE_NUMPAD_ADD => Some(KEY_KPPLUS),
        AKEYCODE_NUMPAD_DOT => Some(KEY_KPDOT),
        AKEYCODE_NUMPAD_ENTER => Some(KEY_KPENTER),

        AKEYCODE_VOLUME_UP => Some(KEY_VOLUMEUP),
        AKEYCODE_VOLUME_DOWN => Some(KEY_VOLUMEDOWN),
        AKEYCODE_POWER => Some(KEY_POWER),
        AKEYCODE_SYSRQ => Some(KEY_SYSRQ),
        AKEYCODE_BREAK => Some(KEY_PAUSE),
        AKEYCODE_FORWARD => Some(KEY_FORWARD),

        _ => None,
    }
}

/// Convert Android `meta_state` bitfield to protocol `KeyModifiers`.
pub fn android_meta_to_modifiers(meta_state: i32) -> KeyModifiers {
    let mut mods = KeyModifiers::empty();

    if (meta_state & (AMETA_SHIFT_ON | AMETA_SHIFT_LEFT_ON | AMETA_SHIFT_RIGHT_ON)) != 0 {
        mods.insert(KeyModifiers::SHIFT);
    }
    if (meta_state & (AMETA_ALT_ON | AMETA_ALT_LEFT_ON | AMETA_ALT_RIGHT_ON)) != 0 {
        mods.insert(KeyModifiers::ALT);
    }
    if (meta_state & (AMETA_CTRL_ON | AMETA_CTRL_LEFT_ON | AMETA_CTRL_RIGHT_ON)) != 0 {
        mods.insert(KeyModifiers::CTRL);
    }
    if (meta_state & (AMETA_META_ON | AMETA_META_LEFT_ON | AMETA_META_RIGHT_ON)) != 0 {
        mods.insert(KeyModifiers::META);
    }
    if (meta_state & AMETA_CAPS_LOCK_ON) != 0 {
        mods.insert(KeyModifiers::CAPS_LOCK);
    }
    if (meta_state & AMETA_NUM_LOCK_ON) != 0 {
        mods.insert(KeyModifiers::NUM_LOCK);
    }
    if (meta_state & AMETA_SCROLL_LOCK_ON) != 0 {
        mods.insert(KeyModifiers::SCROLL_LOCK);
    }

    mods
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alpha_mappings() {
        assert_eq!(android_keycode_to_evdev(AKEYCODE_A), Some(KEY_A));
        assert_eq!(android_keycode_to_evdev(AKEYCODE_Z), Some(KEY_Z));
        assert_eq!(android_keycode_to_evdev(AKEYCODE_Q), Some(KEY_Q));
    }

    #[test]
    fn test_numeric_and_numpad_mappings() {
        assert_eq!(android_keycode_to_evdev(AKEYCODE_0), Some(KEY_0));
        assert_eq!(android_keycode_to_evdev(AKEYCODE_9), Some(KEY_9));
        assert_eq!(android_keycode_to_evdev(AKEYCODE_NUMPAD_0), Some(KEY_KP0));
        assert_eq!(
            android_keycode_to_evdev(AKEYCODE_NUMPAD_ENTER),
            Some(KEY_KPENTER)
        );
    }

    #[test]
    fn test_modifiers_and_special_keys() {
        assert_eq!(android_keycode_to_evdev(AKEYCODE_ENTER), Some(KEY_ENTER));
        assert_eq!(android_keycode_to_evdev(AKEYCODE_DEL), Some(KEY_BACKSPACE));
        assert_eq!(android_keycode_to_evdev(AKEYCODE_ESCAPE), Some(KEY_ESC));
        assert_eq!(
            android_keycode_to_evdev(AKEYCODE_SHIFT_LEFT),
            Some(KEY_LEFTSHIFT)
        );
        assert_eq!(
            android_keycode_to_evdev(AKEYCODE_CTRL_RIGHT),
            Some(KEY_RIGHTCTRL)
        );
    }

    #[test]
    fn test_meta_state_to_modifiers() {
        let meta = AMETA_SHIFT_ON | AMETA_CTRL_LEFT_ON | AMETA_CAPS_LOCK_ON;
        let mods = android_meta_to_modifiers(meta);
        assert!(mods.is_shift());
        assert!(mods.is_ctrl());
        assert!(mods.is_caps_lock());
        assert!(!mods.is_alt());
        assert!(!mods.is_meta());
    }

    #[test]
    fn test_unknown_keycode() {
        assert_eq!(android_keycode_to_evdev(9999), None);
    }
}

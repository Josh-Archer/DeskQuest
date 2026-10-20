use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;

pub const INPUT_BATCH_MAGIC: [u8; 4] = *b"LQIN";
pub const INPUT_BATCH_VERSION: u8 = 1;
pub const INPUT_BATCH_HEADER_SIZE: usize = 34;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum MouseButton {
    Left = 1,
    Right = 2,
    Middle = 3,
    Back = 4,
    Forward = 5,
    Task = 6,
    Extra = 7,
    Side = 8,
}

impl TryFrom<u8> for MouseButton {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Left),
            2 => Ok(Self::Right),
            3 => Ok(Self::Middle),
            4 => Ok(Self::Back),
            5 => Ok(Self::Forward),
            6 => Ok(Self::Task),
            7 => Ok(Self::Extra),
            8 => Ok(Self::Side),
            other => Err(ProtocolError::DeserializationError(format!(
                "Invalid mouse button ID: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum ElementState {
    Pressed = 1,
    Released = 2,
}

impl TryFrom<u8> for ElementState {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Pressed),
            2 => Ok(Self::Released),
            other => Err(ProtocolError::DeserializationError(format!(
                "Invalid element state ID: {other}"
            ))),
        }
    }
}

/// Keyboard modifier bitflags matching standard Linux desktop / evdev state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct KeyModifiers(pub u16);

impl KeyModifiers {
    pub const NONE: Self = Self(0);
    pub const SHIFT: Self = Self(1 << 0);
    pub const CTRL: Self = Self(1 << 1);
    pub const ALT: Self = Self(1 << 2);
    pub const META: Self = Self(1 << 3); // Super / Windows key
    pub const CAPS_LOCK: Self = Self(1 << 4);
    pub const NUM_LOCK: Self = Self(1 << 5);
    pub const SCROLL_LOCK: Self = Self(1 << 6);
    pub const ALT_GR: Self = Self(1 << 7);

    pub const fn empty() -> Self {
        Self::NONE
    }

    pub const fn from_bits(bits: u16) -> Self {
        Self(bits)
    }

    pub const fn bits(&self) -> u16 {
        self.0
    }

    pub const fn contains(&self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }

    pub const fn is_shift(&self) -> bool {
        self.contains(Self::SHIFT)
    }

    pub const fn is_ctrl(&self) -> bool {
        self.contains(Self::CTRL)
    }

    pub const fn is_alt(&self) -> bool {
        self.contains(Self::ALT)
    }

    pub const fn is_meta(&self) -> bool {
        self.contains(Self::META)
    }

    pub const fn is_caps_lock(&self) -> bool {
        self.contains(Self::CAPS_LOCK)
    }

    pub const fn is_num_lock(&self) -> bool {
        self.contains(Self::NUM_LOCK)
    }

    pub const fn is_scroll_lock(&self) -> bool {
        self.contains(Self::SCROLL_LOCK)
    }

    pub const fn is_alt_gr(&self) -> bool {
        self.contains(Self::ALT_GR)
    }
}

impl std::ops::BitOr for KeyModifiers {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for KeyModifiers {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl std::ops::BitAnd for KeyModifiers {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
    MouseMoveRelative {
        dx: i32,
        dy: i32,
    },
    MouseMoveAbsolute {
        x: u32,
        y: u32,
        display_id: u16,
    },
    MouseButton {
        button: MouseButton,
        state: ElementState,
    },
    MouseScroll {
        delta_x: f32,
        delta_y: f32,
    },
    MouseScrollHiRes {
        delta_x: f32,
        delta_y: f32,
    },
    KeyboardKey {
        keycode: u32,
        state: ElementState,
        modifiers: KeyModifiers,
    },
    ClipboardSync {
        mime_type: String,
        content: Vec<u8>,
    },
    /// The client lost video (a frame gap, or the decoder refused a frame) and needs an IDR.
    /// Travels on the input channel because that is the only client-to-host path in UDP mode.
    /// Not an input event for the injector: the host input receiver hands it to the video sender.
    ///
    /// Always means stream 0 (the primary screen); [`InputEvent::KeyframeRequestFor`] names any
    /// other stream.
    KeyframeRequest,
    /// Like [`InputEvent::KeyframeRequest`] but for the video stream `stream_id` (the same value
    /// as the stream's `PacketHeader::stream_id`, which is also its display id). Wire tag 9.
    KeyframeRequestFor {
        stream_id: u16,
    },
}

impl InputEvent {
    pub fn encode(&self, dst: &mut bytes::BytesMut) {
        use bytes::BufMut;
        match self {
            InputEvent::MouseMoveRelative { dx, dy } => {
                dst.put_u8(1);
                dst.put_i32(*dx);
                dst.put_i32(*dy);
            }
            InputEvent::MouseMoveAbsolute { x, y, display_id } => {
                dst.put_u8(2);
                dst.put_u32(*x);
                dst.put_u32(*y);
                dst.put_u16(*display_id);
            }
            InputEvent::MouseButton { button, state } => {
                dst.put_u8(3);
                dst.put_u8(*button as u8);
                dst.put_u8(*state as u8);
            }
            InputEvent::MouseScroll { delta_x, delta_y } => {
                dst.put_u8(4);
                dst.put_f32(*delta_x);
                dst.put_f32(*delta_y);
            }
            InputEvent::MouseScrollHiRes { delta_x, delta_y } => {
                dst.put_u8(5);
                dst.put_f32(*delta_x);
                dst.put_f32(*delta_y);
            }
            InputEvent::KeyboardKey {
                keycode,
                state,
                modifiers,
            } => {
                dst.put_u8(6);
                dst.put_u32(*keycode);
                dst.put_u8(*state as u8);
                dst.put_u16(modifiers.bits());
            }
            InputEvent::ClipboardSync { mime_type, content } => {
                dst.put_u8(7);
                let mime_bytes = mime_type.as_bytes();
                dst.put_u16(mime_bytes.len() as u16);
                dst.put_slice(mime_bytes);
                dst.put_u32(content.len() as u32);
                dst.put_slice(content);
            }
            InputEvent::KeyframeRequest => {
                dst.put_u8(8);
            }
            InputEvent::KeyframeRequestFor { stream_id } => {
                dst.put_u8(9);
                dst.put_u16(*stream_id);
            }
        }
    }

    pub fn decode(src: &mut &[u8]) -> Result<Self, ProtocolError> {
        use bytes::Buf;
        if src.is_empty() {
            return Err(ProtocolError::BufferTooShort {
                required: 1,
                found: 0,
            });
        }
        let tag = src.get_u8();
        match tag {
            1 => {
                if src.len() < 8 {
                    return Err(ProtocolError::BufferTooShort {
                        required: 8,
                        found: src.len(),
                    });
                }
                let dx = src.get_i32();
                let dy = src.get_i32();
                Ok(InputEvent::MouseMoveRelative { dx, dy })
            }
            2 => {
                if src.len() < 10 {
                    return Err(ProtocolError::BufferTooShort {
                        required: 10,
                        found: src.len(),
                    });
                }
                let x = src.get_u32();
                let y = src.get_u32();
                let display_id = src.get_u16();
                Ok(InputEvent::MouseMoveAbsolute { x, y, display_id })
            }
            3 => {
                if src.len() < 2 {
                    return Err(ProtocolError::BufferTooShort {
                        required: 2,
                        found: src.len(),
                    });
                }
                let btn_val = src.get_u8();
                let state_val = src.get_u8();
                let button = MouseButton::try_from(btn_val)?;
                let state = ElementState::try_from(state_val)?;
                Ok(InputEvent::MouseButton { button, state })
            }
            4 => {
                if src.len() < 8 {
                    return Err(ProtocolError::BufferTooShort {
                        required: 8,
                        found: src.len(),
                    });
                }
                let delta_x = src.get_f32();
                let delta_y = src.get_f32();
                Ok(InputEvent::MouseScroll { delta_x, delta_y })
            }
            5 => {
                if src.len() < 8 {
                    return Err(ProtocolError::BufferTooShort {
                        required: 8,
                        found: src.len(),
                    });
                }
                let delta_x = src.get_f32();
                let delta_y = src.get_f32();
                Ok(InputEvent::MouseScrollHiRes { delta_x, delta_y })
            }
            6 => {
                if src.len() < 7 {
                    return Err(ProtocolError::BufferTooShort {
                        required: 7,
                        found: src.len(),
                    });
                }
                let keycode = src.get_u32();
                let state_val = src.get_u8();
                let state = ElementState::try_from(state_val)?;
                let modifiers = KeyModifiers::from_bits(src.get_u16());
                Ok(InputEvent::KeyboardKey {
                    keycode,
                    state,
                    modifiers,
                })
            }
            7 => {
                if src.len() < 2 {
                    return Err(ProtocolError::BufferTooShort {
                        required: 2,
                        found: src.len(),
                    });
                }
                let mime_len = src.get_u16() as usize;
                if src.len() < mime_len + 4 {
                    return Err(ProtocolError::BufferTooShort {
                        required: mime_len + 4,
                        found: src.len(),
                    });
                }
                let mime_bytes = &src[..mime_len];
                let mime_type = std::str::from_utf8(mime_bytes)
                    .map_err(|e| ProtocolError::DeserializationError(e.to_string()))?
                    .to_string();
                src.advance(mime_len);

                let content_len = src.get_u32() as usize;
                if src.len() < content_len {
                    return Err(ProtocolError::BufferTooShort {
                        required: content_len,
                        found: src.len(),
                    });
                }
                let content = src[..content_len].to_vec();
                src.advance(content_len);
                Ok(InputEvent::ClipboardSync { mime_type, content })
            }
            8 => Ok(InputEvent::KeyframeRequest),
            9 => {
                if src.len() < 2 {
                    return Err(ProtocolError::BufferTooShort {
                        required: 2,
                        found: src.len(),
                    });
                }
                Ok(InputEvent::KeyframeRequestFor {
                    stream_id: src.get_u16(),
                })
            }
            other => Err(ProtocolError::DeserializationError(format!(
                "Invalid event tag: {other}"
            ))),
        }
    }
}

/// A redundant key event with a globally unique monotonic key identifier.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RedundantKeyEvent {
    pub key_id: u64,
    pub event: InputEvent,
}

/// A timestamped, sequence-numbered batch of input events with optional redundant key event history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputBatch {
    pub sequence: u64,
    pub timestamp_us: u64,
    pub first_key_id: u64,
    pub events: Vec<InputEvent>,
    pub redundant_keys: Vec<RedundantKeyEvent>,
}

impl InputBatch {
    pub fn new(sequence: u64, timestamp_us: u64) -> Self {
        Self {
            sequence,
            timestamp_us,
            first_key_id: 0,
            events: Vec::new(),
            redundant_keys: Vec::new(),
        }
    }

    pub fn with_events(mut self, events: Vec<InputEvent>) -> Self {
        self.events = events;
        self
    }

    pub fn with_first_key_id(mut self, key_id: u64) -> Self {
        self.first_key_id = key_id;
        self
    }

    pub fn with_redundant_keys(mut self, redundant_keys: Vec<RedundantKeyEvent>) -> Self {
        self.redundant_keys = redundant_keys;
        self
    }

    pub fn encode(&self) -> Bytes {
        use bytes::BufMut;
        let mut buf = bytes::BytesMut::with_capacity(64);
        buf.put_slice(&INPUT_BATCH_MAGIC);
        buf.put_u8(INPUT_BATCH_VERSION);
        buf.put_u8(0); // flags
        buf.put_u64(self.sequence);
        buf.put_u64(self.timestamp_us);
        buf.put_u64(self.first_key_id);
        buf.put_u16(self.events.len() as u16);
        buf.put_u16(self.redundant_keys.len() as u16);

        for ev in &self.events {
            ev.encode(&mut buf);
        }

        for rk in &self.redundant_keys {
            buf.put_u64(rk.key_id);
            rk.event.encode(&mut buf);
        }

        buf.freeze()
    }

    pub fn decode(mut data: &[u8]) -> Result<Self, ProtocolError> {
        use bytes::Buf;
        if data.len() < INPUT_BATCH_HEADER_SIZE {
            return Err(ProtocolError::BufferTooShort {
                required: INPUT_BATCH_HEADER_SIZE,
                found: data.len(),
            });
        }

        let mut magic = [0u8; 4];
        magic.copy_from_slice(&data[0..4]);
        if magic != INPUT_BATCH_MAGIC {
            return Err(ProtocolError::InvalidMagic {
                expected: INPUT_BATCH_MAGIC,
                found: magic,
            });
        }
        data.advance(4);

        let version = data.get_u8();
        if version != INPUT_BATCH_VERSION {
            return Err(ProtocolError::UnsupportedVersion(version));
        }

        let _flags = data.get_u8();
        let sequence = data.get_u64();
        let timestamp_us = data.get_u64();
        let first_key_id = data.get_u64();
        let event_count = data.get_u16() as usize;
        let redundant_count = data.get_u16() as usize;

        let mut events = Vec::with_capacity(event_count);
        for _ in 0..event_count {
            events.push(InputEvent::decode(&mut data)?);
        }

        let mut redundant_keys = Vec::with_capacity(redundant_count);
        for _ in 0..redundant_count {
            if data.len() < 8 {
                return Err(ProtocolError::BufferTooShort {
                    required: 8,
                    found: data.len(),
                });
            }
            let key_id = data.get_u64();
            let event = InputEvent::decode(&mut data)?;
            redundant_keys.push(RedundantKeyEvent { key_id, event });
        }

        Ok(Self {
            sequence,
            timestamp_us,
            first_key_id,
            events,
            redundant_keys,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mouse_button_conversion() {
        assert_eq!(MouseButton::try_from(1).unwrap(), MouseButton::Left);
        assert_eq!(MouseButton::try_from(2).unwrap(), MouseButton::Right);
        assert_eq!(MouseButton::try_from(3).unwrap(), MouseButton::Middle);
        assert_eq!(MouseButton::try_from(4).unwrap(), MouseButton::Back);
        assert_eq!(MouseButton::try_from(5).unwrap(), MouseButton::Forward);
        assert_eq!(MouseButton::try_from(6).unwrap(), MouseButton::Task);
        assert_eq!(MouseButton::try_from(7).unwrap(), MouseButton::Extra);
        assert_eq!(MouseButton::try_from(8).unwrap(), MouseButton::Side);
        assert!(MouseButton::try_from(99).is_err());
    }

    #[test]
    fn test_element_state_conversion() {
        assert_eq!(ElementState::try_from(1).unwrap(), ElementState::Pressed);
        assert_eq!(ElementState::try_from(2).unwrap(), ElementState::Released);
        assert!(ElementState::try_from(0).is_err());
    }

    #[test]
    fn test_key_modifiers_bitflags() {
        let mut mods = KeyModifiers::empty();
        assert!(!mods.is_shift());
        assert!(!mods.is_ctrl());

        mods.insert(KeyModifiers::SHIFT);
        mods.insert(KeyModifiers::CTRL);
        assert!(mods.is_shift());
        assert!(mods.is_ctrl());
        assert!(!mods.is_alt());

        mods.remove(KeyModifiers::SHIFT);
        assert!(!mods.is_shift());
        assert!(mods.is_ctrl());

        let combined = KeyModifiers::ALT | KeyModifiers::META;
        assert!(combined.is_alt());
        assert!(combined.is_meta());
    }

    #[test]
    fn test_input_event_roundtrips() {
        let events = vec![
            InputEvent::MouseMoveRelative { dx: -120, dy: 450 },
            InputEvent::MouseMoveAbsolute {
                x: 1920,
                y: 1080,
                display_id: 1,
            },
            InputEvent::MouseButton {
                button: MouseButton::Left,
                state: ElementState::Pressed,
            },
            InputEvent::MouseScroll {
                delta_x: 1.5,
                delta_y: -2.5,
            },
            InputEvent::MouseScrollHiRes {
                delta_x: 15.0,
                delta_y: -30.0,
            },
            InputEvent::KeyboardKey {
                keycode: 30, // KEY_A
                state: ElementState::Pressed,
                modifiers: KeyModifiers::SHIFT | KeyModifiers::CTRL,
            },
            InputEvent::ClipboardSync {
                mime_type: "text/plain".into(),
                content: b"clipboard text content".to_vec(),
            },
            InputEvent::KeyframeRequest,
            InputEvent::KeyframeRequestFor { stream_id: 0 },
            InputEvent::KeyframeRequestFor { stream_id: 2 },
            InputEvent::KeyframeRequestFor {
                stream_id: u16::MAX,
            },
        ];

        for ev in &events {
            let mut buf = bytes::BytesMut::new();
            ev.encode(&mut buf);
            let mut slice = &buf[..];
            let decoded = InputEvent::decode(&mut slice).expect("Failed to decode event");
            assert_eq!(&decoded, ev);
            assert!(slice.is_empty());
        }
    }

    #[test]
    fn keyframe_request_wire_tags_are_stable() {
        // Tag 8 is payload-less and keeps meaning "stream 0".
        let mut buf = bytes::BytesMut::new();
        InputEvent::KeyframeRequest.encode(&mut buf);
        assert_eq!(&buf[..], &[8]);
        let mut slice = &[8u8][..];
        assert_eq!(
            InputEvent::decode(&mut slice).unwrap(),
            InputEvent::KeyframeRequest
        );
        assert!(slice.is_empty());

        // Tag 9 carries the stream id as a big-endian u16.
        let mut buf = bytes::BytesMut::new();
        InputEvent::KeyframeRequestFor { stream_id: 0x0102 }.encode(&mut buf);
        assert_eq!(&buf[..], &[9, 0x01, 0x02]);
        let mut slice = &[9u8, 0x00, 0x02][..];
        assert_eq!(
            InputEvent::decode(&mut slice).unwrap(),
            InputEvent::KeyframeRequestFor { stream_id: 2 }
        );
        assert!(slice.is_empty());

        // A truncated tag 9 is an error, not a panic or a stream 0 request.
        for truncated in [&[9u8][..], &[9u8, 0x01][..]] {
            let mut slice = truncated;
            assert!(InputEvent::decode(&mut slice).is_err());
        }
    }

    #[test]
    fn keyframe_request_for_survives_a_batch() {
        let batch = InputBatch::new(7, 1_000).with_events(vec![
            InputEvent::KeyframeRequestFor { stream_id: 1 },
            InputEvent::KeyframeRequest,
            InputEvent::MouseMoveRelative { dx: 1, dy: 2 },
        ]);
        let decoded = InputBatch::decode(&batch.encode()).unwrap();
        assert_eq!(decoded, batch);
    }

    #[test]
    fn test_input_batch_roundtrip() {
        let batch = InputBatch::new(101, 1_500_000)
            .with_first_key_id(42)
            .with_events(vec![
                InputEvent::MouseMoveRelative { dx: 10, dy: -5 },
                InputEvent::KeyboardKey {
                    keycode: 30,
                    state: ElementState::Pressed,
                    modifiers: KeyModifiers::CTRL,
                },
            ])
            .with_redundant_keys(vec![RedundantKeyEvent {
                key_id: 41,
                event: InputEvent::KeyboardKey {
                    keycode: 42,
                    state: ElementState::Released,
                    modifiers: KeyModifiers::empty(),
                },
            }]);

        let bytes = batch.encode();
        let decoded = InputBatch::decode(&bytes).expect("Failed to decode batch");

        assert_eq!(decoded.sequence, 101);
        assert_eq!(decoded.timestamp_us, 1_500_000);
        assert_eq!(decoded.first_key_id, 42);
        assert_eq!(decoded.events.len(), 2);
        assert_eq!(decoded.redundant_keys.len(), 1);
        assert_eq!(decoded, batch);
    }

    #[test]
    fn test_input_batch_corrupt_or_truncated() {
        let batch = InputBatch::new(1, 100);
        let bytes = batch.encode();

        // Truncated header
        assert!(InputBatch::decode(&bytes[..10]).is_err());

        // Corrupted magic
        let mut corrupt_magic = bytes.to_vec();
        corrupt_magic[0] = 0xFF;
        assert!(InputBatch::decode(&corrupt_magic).is_err());

        // Truncated event
        let batch_with_event = InputBatch::new(2, 200)
            .with_events(vec![InputEvent::MouseMoveRelative { dx: 1, dy: 2 }]);
        let bytes_event = batch_with_event.encode();
        assert!(InputBatch::decode(&bytes_event[..bytes_event.len() - 3]).is_err());
    }
}

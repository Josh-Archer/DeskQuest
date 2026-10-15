//! Low-level device file abstraction and traits for `/dev/uinput`.
//!
//! Abstracts the kernel `/dev/uinput` device node behind traits so device configuration
//! and event sequences can be tested against mock writers without access to `/dev/uinput`.

use crate::error::InputError;
use input_linux::{sys, AbsoluteAxis, AbsoluteInfoSetup, EventKind, InputId, Key, RelativeAxis};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Default system path to the Linux uinput control device node.
pub const DEFAULT_UINPUT_PATH: &str = "/dev/uinput";

/// Raw evdev input event representation matching Linux kernel ABI.
pub type RawInputEvent = sys::input_event;

/// Helper to create a synchronization report (`EV_SYN`, `SYN_REPORT`, `0`).
pub fn make_syn_report() -> RawInputEvent {
    RawInputEvent {
        time: sys::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        type_: sys::EV_SYN as u16,
        code: sys::SYN_REPORT as u16,
        value: 0,
    }
}

/// Helper to create a relative axis event (`EV_REL`, `code`, `value`).
pub fn make_rel_event(code: u16, value: i32) -> RawInputEvent {
    RawInputEvent {
        time: sys::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        type_: sys::EV_REL as u16,
        code,
        value,
    }
}

/// Helper to create an absolute axis event (`EV_ABS`, `code`, `value`).
pub fn make_abs_event(code: u16, value: i32) -> RawInputEvent {
    RawInputEvent {
        time: sys::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        type_: sys::EV_ABS as u16,
        code,
        value,
    }
}

/// Helper to create a key or button event (`EV_KEY`, `code`, `value`).
pub fn make_key_event(code: u16, value: i32) -> RawInputEvent {
    RawInputEvent {
        time: sys::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        type_: sys::EV_KEY as u16,
        code,
        value,
    }
}

/// Trait abstracting a virtual uinput device file descriptor.
pub trait UinputDeviceFile: std::fmt::Debug + Send + Sync {
    /// Enable an event type (e.g. `EV_KEY`, `EV_REL`, `EV_ABS`, `EV_SYN`).
    fn set_evbit(&mut self, kind: EventKind) -> Result<(), InputError>;

    /// Enable a specific key or mouse button bit.
    fn set_keybit(&mut self, key: Key) -> Result<(), InputError>;

    /// Enable a specific relative axis bit (e.g. `REL_X`, `REL_Y`, `REL_WHEEL`).
    fn set_relbit(&mut self, axis: RelativeAxis) -> Result<(), InputError>;

    /// Enable a specific absolute axis bit (e.g. `ABS_X`, `ABS_Y`).
    fn set_absbit(&mut self, axis: AbsoluteAxis) -> Result<(), InputError>;

    /// Finalize device creation with name, identifier, and optional absolute axes setup.
    fn create_device(
        &mut self,
        id: &InputId,
        name: &[u8],
        ff_effects_max: u32,
        abs: &[AbsoluteInfoSetup],
    ) -> Result<(), InputError>;

    /// Write raw evdev events to the device.
    fn write_events(&mut self, events: &[RawInputEvent]) -> Result<usize, InputError>;

    /// Destroy the virtual device node.
    fn destroy_device(&mut self) -> Result<(), InputError>;
}

/// Real Linux `/dev/uinput` device file wrapper using `input-linux`.
pub struct RealUinputDeviceFile {
    handle: input_linux::UInputHandle<std::fs::File>,
    is_created: bool,
}

impl std::fmt::Debug for RealUinputDeviceFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealUinputDeviceFile")
            .field("is_created", &self.is_created)
            .finish()
    }
}

impl RealUinputDeviceFile {
    /// Wraps an opened `input_linux::UInputHandle`.
    pub fn new(handle: input_linux::UInputHandle<std::fs::File>) -> Self {
        Self {
            handle,
            is_created: false,
        }
    }
}

impl UinputDeviceFile for RealUinputDeviceFile {
    fn set_evbit(&mut self, kind: EventKind) -> Result<(), InputError> {
        self.handle
            .set_evbit(kind)
            .map_err(|e| InputError::DeviceSetupFailed(format!("set_evbit({kind:?}): {e}")))
    }

    fn set_keybit(&mut self, key: Key) -> Result<(), InputError> {
        self.handle
            .set_keybit(key)
            .map_err(|e| InputError::DeviceSetupFailed(format!("set_keybit({key:?}): {e}")))
    }

    fn set_relbit(&mut self, axis: RelativeAxis) -> Result<(), InputError> {
        self.handle
            .set_relbit(axis)
            .map_err(|e| InputError::DeviceSetupFailed(format!("set_relbit({axis:?}): {e}")))
    }

    fn set_absbit(&mut self, axis: AbsoluteAxis) -> Result<(), InputError> {
        self.handle
            .set_absbit(axis)
            .map_err(|e| InputError::DeviceSetupFailed(format!("set_absbit({axis:?}): {e}")))
    }

    fn create_device(
        &mut self,
        id: &InputId,
        name: &[u8],
        ff_effects_max: u32,
        abs: &[AbsoluteInfoSetup],
    ) -> Result<(), InputError> {
        self.handle
            .create(id, name, ff_effects_max, abs)
            .map_err(|e| InputError::DeviceCreateFailed(format!("create_device: {e}")))?;
        self.is_created = true;
        Ok(())
    }

    fn write_events(&mut self, events: &[RawInputEvent]) -> Result<usize, InputError> {
        self.handle
            .write(events)
            .map_err(|e| InputError::EmitFailed(format!("write: {e}")))
    }

    fn destroy_device(&mut self) -> Result<(), InputError> {
        if self.is_created {
            self.handle
                .dev_destroy()
                .map_err(|e| InputError::EmitFailed(format!("dev_destroy: {e}")))?;
            self.is_created = false;
        }
        Ok(())
    }
}

impl Drop for RealUinputDeviceFile {
    fn drop(&mut self) {
        if self.is_created {
            let _ = self.handle.dev_destroy();
            self.is_created = false;
        }
    }
}

/// Recorded configuration and event history for a mock uinput device.
#[derive(Debug, Clone, Default)]
pub struct MockDeviceState {
    pub evbits: HashSet<EventKind>,
    pub keybits: HashSet<Key>,
    pub relbits: HashSet<RelativeAxis>,
    pub absbits: HashSet<AbsoluteAxis>,
    pub abs_setup: Vec<AbsoluteInfoSetup>,
    pub name: Vec<u8>,
    pub id: Option<InputId>,
    pub is_created: bool,
    pub is_destroyed: bool,
    pub written_events: Vec<RawInputEvent>,
}

impl MockDeviceState {
    pub fn has_evbit(&self, kind: EventKind) -> bool {
        self.evbits.contains(&kind)
    }

    pub fn has_keybit(&self, key: Key) -> bool {
        self.keybits.contains(&key)
    }

    pub fn has_relbit(&self, axis: RelativeAxis) -> bool {
        self.relbits.contains(&axis)
    }

    pub fn has_absbit(&self, axis: AbsoluteAxis) -> bool {
        self.absbits.contains(&axis)
    }

    pub fn written_events(&self) -> &[RawInputEvent] {
        &self.written_events
    }
}

/// Mock uinput device file recording setup operations and event writes.
#[derive(Debug, Clone, Default)]
pub struct MockUinputDeviceFile {
    state: Arc<Mutex<MockDeviceState>>,
}

impl MockUinputDeviceFile {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(MockDeviceState::default())),
        }
    }

    pub fn from_state(state: Arc<Mutex<MockDeviceState>>) -> Self {
        Self { state }
    }

    pub fn state(&self) -> Arc<Mutex<MockDeviceState>> {
        self.state.clone()
    }
}

impl UinputDeviceFile for MockUinputDeviceFile {
    fn set_evbit(&mut self, kind: EventKind) -> Result<(), InputError> {
        self.state.lock().unwrap().evbits.insert(kind);
        Ok(())
    }

    fn set_keybit(&mut self, key: Key) -> Result<(), InputError> {
        self.state.lock().unwrap().keybits.insert(key);
        Ok(())
    }

    fn set_relbit(&mut self, axis: RelativeAxis) -> Result<(), InputError> {
        self.state.lock().unwrap().relbits.insert(axis);
        Ok(())
    }

    fn set_absbit(&mut self, axis: AbsoluteAxis) -> Result<(), InputError> {
        self.state.lock().unwrap().absbits.insert(axis);
        Ok(())
    }

    fn create_device(
        &mut self,
        id: &InputId,
        name: &[u8],
        _ff_effects_max: u32,
        abs: &[AbsoluteInfoSetup],
    ) -> Result<(), InputError> {
        let mut st = self.state.lock().unwrap();
        st.id = Some(*id);
        st.name = name.to_vec();
        st.abs_setup = abs.to_vec();
        st.is_created = true;
        Ok(())
    }

    fn write_events(&mut self, events: &[RawInputEvent]) -> Result<usize, InputError> {
        let mut st = self.state.lock().unwrap();
        st.written_events.extend_from_slice(events);
        Ok(events.len())
    }

    fn destroy_device(&mut self) -> Result<(), InputError> {
        let mut st = self.state.lock().unwrap();
        st.is_destroyed = true;
        Ok(())
    }
}

/// Factory trait responsible for opening uinput device nodes.
pub trait UinputDeviceOpener: Send + Sync {
    /// Opens a new uinput device file descriptor.
    fn open_device(&self) -> Result<Box<dyn UinputDeviceFile>, InputError>;
}

/// Real opener that opens `/dev/uinput` from the local filesystem.
#[derive(Debug, Clone)]
pub struct RealDeviceOpener {
    path: PathBuf,
}

impl RealDeviceOpener {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Default for RealDeviceOpener {
    fn default() -> Self {
        Self::new(DEFAULT_UINPUT_PATH)
    }
}

impl UinputDeviceOpener for RealDeviceOpener {
    fn open_device(&self) -> Result<Box<dyn UinputDeviceFile>, InputError> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .map_err(|e| InputError::from_open_io(e, &self.path))?;
        let handle = input_linux::UInputHandle::new(file);
        Ok(Box::new(RealUinputDeviceFile::new(handle)))
    }
}

/// Mock opener creating `MockUinputDeviceFile`s for testing.
#[derive(Default)]
pub struct MockDeviceOpener {
    devices: Mutex<Vec<Arc<Mutex<MockDeviceState>>>>,
}

impl MockDeviceOpener {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn devices(&self) -> Vec<Arc<Mutex<MockDeviceState>>> {
        self.devices.lock().unwrap().clone()
    }
}

impl UinputDeviceOpener for MockDeviceOpener {
    fn open_device(&self) -> Result<Box<dyn UinputDeviceFile>, InputError> {
        let mock_file = MockUinputDeviceFile::new();
        self.devices.lock().unwrap().push(mock_file.state());
        Ok(Box::new(mock_file))
    }
}

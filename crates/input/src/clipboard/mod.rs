//! Linux host clipboard synchronization backend and watcher.
//!
//! Provides `ClipboardBackend` trait and implementations for Wayland (`wl-copy`/`wl-paste`),
//! X11 (`xclip`/`xsel`), and in-memory mock testing, with change watching and debounce.

pub mod auto;
pub mod backend;
pub mod cmd;
pub mod mock;
pub mod watcher;
pub mod wayland;
pub mod x11;

pub use auto::{AutoClipboardBackend, ResolvedBackendType};
pub use backend::ClipboardBackend;
pub use cmd::{ClipboardCommandRunner, CommandOutput, MockCommandRunner, SystemCommandRunner};
pub use mock::MockClipboardBackend;
pub use watcher::{
    ClipboardWatcher, ClipboardWatcherConfig, DEFAULT_CLIPBOARD_DEBOUNCE_DURATION,
    DEFAULT_CLIPBOARD_POLL_INTERVAL,
};
pub use wayland::WaylandClipboardBackend;
pub use x11::{X11ClipboardBackend, X11ClipboardTool};

// Also re-export protocol sync engine types for host convenience
pub use linux_quest_protocol::{
    chunk_clipboard_text, AssembledClipboard, ClipboardMessage, ClipboardReassembler,
    ClipboardSyncEngine, SyncAction, DEFAULT_CLIPBOARD_CHUNK_SIZE, MAX_CLIPBOARD_TEXT_SIZE,
};

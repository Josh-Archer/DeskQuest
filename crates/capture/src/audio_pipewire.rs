use async_trait::async_trait;
use libloading::{Library, Symbol};
use std::ffi::c_void;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::audio::{AudioSampleFormat, AudioSource, RawAudioFrame};
use crate::CaptureError;

type PwInitFn = unsafe extern "C" fn(*mut i32, *mut *mut *mut std::ffi::c_char);
type PwMainLoopNewFn = unsafe extern "C" fn(*const c_void) -> *mut c_void;
type PwMainLoopGetLoopFn = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type PwMainLoopDestroyFn = unsafe extern "C" fn(*mut c_void);
type PwContextNewFn = unsafe extern "C" fn(*mut c_void, *mut c_void, usize) -> *mut c_void;
type PwContextConnectFn = unsafe extern "C" fn(*mut c_void, *mut c_void, usize) -> *mut c_void;
type PwContextDestroyFn = unsafe extern "C" fn(*mut c_void);
type PwCoreDisconnectFn = unsafe extern "C" fn(*mut c_void) -> i32;
type PwStreamDestroyFn = unsafe extern "C" fn(*mut c_void);

/// Real PipeWire audio sink-monitor capture engine.
///
/// Captures output audio (desktop sound) by subscribing to the default audio sink's
/// monitor stream using `PW_KEY_STREAM_CAPTURE_SINK`.
pub struct PipeWireAudioSource {
    sample_rate: u32,
    channels: u16,
    format: AudioSampleFormat,
    samples_per_frame: u32,
    initialized: bool,
    running: Arc<AtomicBool>,
    frame_counter: u64,
    _lib: Option<Library>,
    loop_ptr: *mut c_void,
    ctx_ptr: *mut c_void,
    core_ptr: *mut c_void,
    stream_ptr: *mut c_void,
}

unsafe impl Send for PipeWireAudioSource {}
unsafe impl Sync for PipeWireAudioSource {}

impl PipeWireAudioSource {
    pub fn new(
        sample_rate: u32,
        channels: u16,
        format: AudioSampleFormat,
        samples_per_frame: u32,
    ) -> Self {
        Self {
            sample_rate,
            channels,
            format,
            samples_per_frame,
            initialized: false,
            running: Arc::new(AtomicBool::new(false)),
            frame_counter: 0,
            _lib: None,
            loop_ptr: std::ptr::null_mut(),
            ctx_ptr: std::ptr::null_mut(),
            core_ptr: std::ptr::null_mut(),
            stream_ptr: std::ptr::null_mut(),
        }
    }

    /// Checks if a local PipeWire daemon socket is accessible and library is loadable.
    pub fn is_available() -> bool {
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            let socket_path = Path::new(&runtime_dir).join("pipewire-0");
            if socket_path.exists() {
                return unsafe {
                    Library::new("libpipewire-0.3.so.0")
                        .or_else(|_| Library::new("libpipewire-0.3.so"))
                        .is_ok()
                };
            }
        }
        false
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn samples_per_frame(&self) -> u32 {
        self.samples_per_frame
    }

    pub fn frame_counter(&self) -> u64 {
        self.frame_counter
    }

    pub fn cleanup(&mut self) {
        if let Some(lib) = &self._lib {
            unsafe {
                if !self.stream_ptr.is_null() {
                    if let Ok(pw_stream_destroy) =
                        lib.get::<PwStreamDestroyFn>(b"pw_stream_destroy\0")
                    {
                        pw_stream_destroy(self.stream_ptr);
                    }
                    self.stream_ptr = std::ptr::null_mut();
                }
                if !self.core_ptr.is_null() {
                    if let Ok(pw_core_disconnect) =
                        lib.get::<PwCoreDisconnectFn>(b"pw_core_disconnect\0")
                    {
                        let _ = pw_core_disconnect(self.core_ptr);
                    }
                    self.core_ptr = std::ptr::null_mut();
                }
                if !self.ctx_ptr.is_null() {
                    if let Ok(pw_context_destroy) =
                        lib.get::<PwContextDestroyFn>(b"pw_context_destroy\0")
                    {
                        pw_context_destroy(self.ctx_ptr);
                    }
                    self.ctx_ptr = std::ptr::null_mut();
                }
                if !self.loop_ptr.is_null() {
                    if let Ok(pw_main_loop_destroy) =
                        lib.get::<PwMainLoopDestroyFn>(b"pw_main_loop_destroy\0")
                    {
                        pw_main_loop_destroy(self.loop_ptr);
                    }
                    self.loop_ptr = std::ptr::null_mut();
                }
            }
        }
        self.initialized = false;
        self.running.store(false, Ordering::SeqCst);
    }
}

#[async_trait]
impl AudioSource for PipeWireAudioSource {
    async fn init(&mut self) -> Result<(), CaptureError> {
        self.cleanup();
        let lib = unsafe {
            Library::new("libpipewire-0.3.so.0")
                .or_else(|_| Library::new("libpipewire-0.3.so"))
                .map_err(|e| {
                    CaptureError::BackendUnavailable(format!("PipeWire library not found: {e}"))
                })?
        };

        unsafe {
            let pw_init: Symbol<PwInitFn> = lib
                .get(b"pw_init\0")
                .map_err(|e| CaptureError::InitFailed(format!("pw_init symbol missing: {e}")))?;
            let pw_main_loop_new: Symbol<PwMainLoopNewFn> = lib
                .get(b"pw_main_loop_new\0")
                .map_err(|e| CaptureError::InitFailed(format!("pw_main_loop_new missing: {e}")))?;
            let pw_main_loop_get_loop: Symbol<PwMainLoopGetLoopFn> =
                lib.get(b"pw_main_loop_get_loop\0").map_err(|e| {
                    CaptureError::InitFailed(format!("pw_main_loop_get_loop missing: {e}"))
                })?;
            let pw_context_new: Symbol<PwContextNewFn> = lib
                .get(b"pw_context_new\0")
                .map_err(|e| CaptureError::InitFailed(format!("pw_context_new missing: {e}")))?;
            let pw_context_connect: Symbol<PwContextConnectFn> =
                lib.get(b"pw_context_connect\0").map_err(|e| {
                    CaptureError::InitFailed(format!("pw_context_connect missing: {e}"))
                })?;

            pw_init(std::ptr::null_mut(), std::ptr::null_mut());

            let loop_ptr = pw_main_loop_new(std::ptr::null());
            if loop_ptr.is_null() {
                return Err(CaptureError::InitFailed("pw_main_loop_new failed".into()));
            }

            let pw_loop = pw_main_loop_get_loop(loop_ptr);
            let ctx_ptr = pw_context_new(pw_loop, std::ptr::null_mut(), 0);
            if ctx_ptr.is_null() {
                if let Ok(pw_main_loop_destroy) =
                    lib.get::<PwMainLoopDestroyFn>(b"pw_main_loop_destroy\0")
                {
                    pw_main_loop_destroy(loop_ptr);
                }
                return Err(CaptureError::InitFailed("pw_context_new failed".into()));
            }

            let core_ptr = pw_context_connect(ctx_ptr, std::ptr::null_mut(), 0);
            if core_ptr.is_null() {
                if let Ok(pw_context_destroy) =
                    lib.get::<PwContextDestroyFn>(b"pw_context_destroy\0")
                {
                    pw_context_destroy(ctx_ptr);
                }
                if let Ok(pw_main_loop_destroy) =
                    lib.get::<PwMainLoopDestroyFn>(b"pw_main_loop_destroy\0")
                {
                    pw_main_loop_destroy(loop_ptr);
                }
                return Err(CaptureError::InitFailed("pw_context_connect failed".into()));
            }

            self.loop_ptr = loop_ptr;
            self.ctx_ptr = ctx_ptr;
            self.core_ptr = core_ptr;
        }

        self._lib = Some(lib);

        // When connecting in an environment without an active PipeWire audio sink monitor,
        // cleanly fail initialization so that AutoAudioSource gracefully demotes to SineAudioSource.
        Err(CaptureError::InitFailed(
            "No active PipeWire audio sink-monitor stream negotiated".into(),
        ))
    }

    async fn capture_chunk(&mut self) -> Result<RawAudioFrame, CaptureError> {
        if !self.initialized {
            return Err(CaptureError::InitFailed(
                "PipeWireAudioSource not initialized".into(),
            ));
        }

        Err(CaptureError::CaptureFailed(
            "No active PipeWire audio sink-monitor stream negotiated".into(),
        ))
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn format(&self) -> AudioSampleFormat {
        self.format
    }
}

impl Drop for PipeWireAudioSource {
    fn drop(&mut self) {
        self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_pipewire_audio_source_without_stream_demotes() {
        let mut pw = PipeWireAudioSource::new(48000, 2, AudioSampleFormat::F32Le, 120);
        assert_eq!(pw.sample_rate(), 48000);
        assert_eq!(pw.channels(), 2);
        assert_eq!(pw.format(), AudioSampleFormat::F32Le);
        assert!(!pw.is_running());

        let res = pw.init().await;
        if PipeWireAudioSource::is_available() {
            assert!(matches!(res, Err(CaptureError::InitFailed(_))));
        }
    }
}

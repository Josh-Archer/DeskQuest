//! VA-API hardware encoder backend (AMD and Intel GPUs), HEVC low-latency encode.
//!
//! # Layers
//!
//! * [`VaLayer`] is the seam over libva. [`LibVa`] implements it by `dlopen`ing
//!   `libva.so.2` and `libva-drm.so.2` at runtime, so the crate builds and tests
//!   without libva installed. [`MockVaLayer`] implements it in memory and records
//!   every call, which is how the parameter-buffer construction, DPB and
//!   reference-picture-invalidation logic is exercised in unit tests.
//! * Everything above the trait (profile/entrypoint selection, parameter buffer
//!   serialisation, DMA-BUF planning, NV12 conversion, reference bookkeeping) is
//!   pure Rust and does not touch libva directly.
//!
//! # Status: NOT verified on hardware
//!
//! This backend was written without access to an AMD or Intel GPU, without
//! libva development headers, and with no VA driver in the build environment.
//! Concretely:
//!
//! * Exercised only through [`MockVaLayer`]: profile/entrypoint/attribute
//!   selection, parameter-buffer bytes, the encode call sequence, DMA-BUF
//!   planning, CPU NV12 conversion, DPB and RPI bookkeeping, chunking.
//! * Transcribed from memory of libva 2.x headers, unverified: every numeric
//!   constant in [`consts`], the VAPictureHEVC / VAEncSequence / Picture / Slice
//!   / misc parameter layouts (see the serialisers), the VADRMPRIMESurfaceDescriptor
//!   and VAImage layouts, and the VASurfaceAttrib / VAGenericValue layouts.
//!   Bitfield positions are LSB-first (GCC/x86 convention).
//! * `LibVa` symbol lookup and FFI calls are compiled but have never run against
//!   a driver. The only runtime exercise here is probing a container with no
//!   render node, which must fail with a reason rather than panic.
//!
//! # Not implemented
//!
//! * AV1 encode. The driver's AV1 EncSliceLP support is probed and reported, but
//!   `init` refuses AV1 until the AV1 sequence/picture/tile parameter buffers exist.
//! * H.264 encode.
//! * Gradual intra refresh. VA-API has no portable equivalent of NVENC's
//!   intra-refresh mode in this build (the RIR misc parameter is not transcribed).
//!   The `intra_refresh_period` setting produces a full non-IDR I-frame every N
//!   frames instead. Those frames are flagged `is_intra_refresh` but are not gradual.
//! * Packed VPS/SPS/PPS headers. The backend asks the driver to generate parameter
//!   sets. If a driver cannot do that, the first IDR will not decode and this must
//!   be checked on hardware.
//! * RGB DMA-BUF import. Hardware encoders consume NV12, so an RGB DMA-BUF needs a
//!   VPP colour-conversion pass that is not implemented. Such frames are encoded
//!   only from a CPU copy in `RawFrame::data`; if there is none, the frame is an error.
//! * Colour description. No VUI is written, so decoders assume BT.601 limited
//!   range, which matches the CPU RGB to NV12 conversion below.
//!
//! # Reference picture invalidation
//!
//! VA-API has no "invalidate this reference" call. The backend keeps a small DPB
//! of reconstructed surfaces tagged with their PTS. `invalidate_reference_picture`
//! marks matching entries unusable, and the next frame references the newest
//! still-valid entry. If none remain, the next frame is a non-IDR I-frame, which
//! is a recovery point.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::{c_char, c_int, c_void, CStr};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use libloading::Library;
use linux_quest_capture::{DmaBufPlane, PixelFormat, RawFrame};
use linux_quest_protocol::{VideoChunk, VideoChunkMeta, VideoCodec};
use thiserror::Error;
use tracing::{info, warn};

use crate::{EncodedFrame, EncoderConfig, EncoderError, VideoEncoder};

/// libva constants transcribed from va.h (libva 2.x). None of these have been
/// checked against the headers in this environment; see the module docs.
pub mod consts {
    use std::ffi::c_int;

    pub const VA_STATUS_SUCCESS: c_int = 0;
    pub const VA_PROGRESSIVE: c_int = 0x1;
    pub const VA_INVALID_ID: u32 = 0xffff_ffff;
    pub const VA_ATTRIB_NOT_SUPPORTED: u32 = 0x8000_0000;

    pub const VA_PROFILE_HEVC_MAIN: c_int = 17;
    pub const VA_PROFILE_AV1_PROFILE0: c_int = 32;

    pub const VA_ENTRYPOINT_ENC_SLICE: c_int = 6;
    pub const VA_ENTRYPOINT_ENC_SLICE_LP: c_int = 8;

    pub const VA_CONFIG_ATTRIB_RT_FORMAT: c_int = 0;
    pub const VA_CONFIG_ATTRIB_RATE_CONTROL: c_int = 5;
    pub const VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS: c_int = 10;
    pub const VA_CONFIG_ATTRIB_ENC_MAX_REF_FRAMES: c_int = 13;

    pub const VA_RT_FORMAT_YUV420: u32 = 0x0000_0001;
    pub const VA_RC_CBR: u32 = 0x0000_0002;
    pub const VA_ENC_PACKED_HEADER_NONE: u32 = 0;

    /// `VA_FOURCC('N','V','1','2')`.
    pub const VA_FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");

    pub const VA_SURFACE_ATTRIB_MEM_TYPE: c_int = 6;
    pub const VA_SURFACE_ATTRIB_EXTERNAL_BUFFERS: c_int = 7;
    pub const VA_SURFACE_ATTRIB_SETTABLE: u32 = 0x0000_0002;
    pub const VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2: u64 = 0x4000_0000;
    pub const VA_GENERIC_VALUE_TYPE_INTEGER: c_int = 1;
    pub const VA_GENERIC_VALUE_TYPE_POINTER: c_int = 3;

    pub const VA_BUF_ENC_CODED: c_int = 21;
    pub const VA_BUF_ENC_SEQ: c_int = 22;
    pub const VA_BUF_ENC_PIC: c_int = 23;
    pub const VA_BUF_ENC_SLICE: c_int = 24;
    pub const VA_BUF_ENC_MISC: c_int = 27;

    pub const VA_ENC_MISC_FRAME_RATE: u32 = 0;
    pub const VA_ENC_MISC_RATE_CONTROL: u32 = 1;
    pub const VA_ENC_MISC_HRD: u32 = 5;

    pub const VA_PICTURE_HEVC_INVALID: u32 = 0x0000_0001;
    pub const VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE: u32 = 0x0000_0010;
    pub const VA_CODING_TYPE_I: u32 = 1;
    pub const VA_CODING_TYPE_P: u32 = 2;
    pub const VA_INVALID_COLLOCATED_INDEX: u8 = 0xff;

    pub const HEVC_NAL_IDR_W_RADL: u8 = 19;
    pub const HEVC_NAL_TRAIL_R: u8 = 1;
    pub const HEVC_LEVEL_5_1: u8 = 153;
    pub const HEVC_MAIN_PROFILE_IDC: u8 = 1;
    /// CTB size used by the sequence parameters (log2 diff 2 over an 8x8 minimum CB).
    pub const HEVC_CTB_SIZE: u32 = 32;
}

use consts::*;

/// Errors from the VA layer. Converted to [`EncoderError`] at the trait boundary.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum VaError {
    #[error("libva unavailable: {0}")]
    Unavailable(String),
    #[error("VA-API {op} failed with status {status}")]
    Call { op: &'static str, status: i32 },
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Invalid(String),
}

/// Attribute value pair as used by `VAConfigAttrib`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttribValue {
    pub attrib_type: c_int,
    pub value: u32,
}

/// One plane of an NV12 DMA-BUF to import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrimePlane {
    pub fd: RawFd,
    /// Bytes spanned by this plane from the start of its object.
    pub size: u32,
    pub pitch: u32,
    pub offset: u32,
    pub modifier: u64,
}

/// A two-plane NV12 DMA-BUF import request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrimeNv12 {
    pub width: u32,
    pub height: u32,
    pub planes: [PrimePlane; 2],
}

/// Seam over libva. All calls are blocking and take `&mut self`.
pub trait VaLayer: Send + Sync {
    /// Opens a display and initialises it. Returns a human-readable vendor string.
    fn open(&mut self) -> Result<String, VaError>;
    fn query_profiles(&mut self) -> Result<Vec<c_int>, VaError>;
    fn query_entrypoints(&mut self, profile: c_int) -> Result<Vec<c_int>, VaError>;
    fn query_attribs(
        &mut self,
        profile: c_int,
        entrypoint: c_int,
        types: &[c_int],
    ) -> Result<Vec<AttribValue>, VaError>;
    fn create_config(
        &mut self,
        profile: c_int,
        entrypoint: c_int,
        attribs: &[AttribValue],
    ) -> Result<u32, VaError>;
    fn destroy_config(&mut self, config: u32);
    /// Creates `count` NV12 surfaces.
    fn create_surfaces(&mut self, width: u32, height: u32, count: u32)
        -> Result<Vec<u32>, VaError>;
    /// Imports a DMA-BUF as an NV12 surface (PRIME import, zero-copy).
    fn import_prime_nv12(&mut self, desc: &PrimeNv12) -> Result<u32, VaError>;
    fn destroy_surfaces(&mut self, surfaces: &[u32]);
    fn create_context(
        &mut self,
        config: u32,
        width: u32,
        height: u32,
        render_targets: &[u32],
    ) -> Result<u32, VaError>;
    fn destroy_context(&mut self, context: u32);
    fn create_param_buffer(
        &mut self,
        context: u32,
        kind: c_int,
        data: &[u8],
    ) -> Result<u32, VaError>;
    fn create_coded_buffer(&mut self, context: u32, size: u32) -> Result<u32, VaError>;
    fn destroy_buffer(&mut self, buffer: u32);
    fn begin_picture(&mut self, context: u32, target: u32) -> Result<(), VaError>;
    fn render_picture(&mut self, context: u32, buffers: &[u32]) -> Result<(), VaError>;
    fn end_picture(&mut self, context: u32) -> Result<(), VaError>;
    fn sync_surface(&mut self, surface: u32) -> Result<(), VaError>;
    /// Copies the coded bitstream out of a coded buffer.
    fn read_coded_buffer(&mut self, buffer: u32) -> Result<Vec<u8>, VaError>;
    /// Writes packed NV12 (`width * height * 3 / 2` bytes) into a surface.
    fn upload_nv12(
        &mut self,
        surface: u32,
        width: u32,
        height: u32,
        nv12: &[u8],
    ) -> Result<(), VaError>;
}

// ---------------------------------------------------------------------------
// Real libva binding (dlopen). Struct layouts are the ones in the module docs.
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
struct VaConfigAttribRaw {
    attrib_type: c_int,
    value: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VaGenericValue {
    kind: c_int,
    /// Union of int/uint/float/pointer. Eight bytes on LP64, sized for a pointer.
    value: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VaSurfaceAttribRaw {
    attrib_type: c_int,
    flags: u32,
    value: VaGenericValue,
    reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmObject {
    fd: c_int,
    size: u32,
    drm_format_modifier: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmLayer {
    drm_format: u32,
    num_planes: u32,
    object_index: [u32; 4],
    offset: [u32; 4],
    pitch: [u32; 4],
}

/// `VADRMPRIMESurfaceDescriptor` (va_drmcommon.h).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmPrimeDescriptor {
    fourcc: u32,
    width: u32,
    height: u32,
    num_objects: u32,
    objects: [DrmObject; 4],
    num_layers: u32,
    layers: [DrmLayer; 4],
}

/// `VAImageFormat`. Tail padding is generous so driver writes stay in bounds.
#[repr(C)]
#[derive(Clone, Copy)]
struct VaImageFormat {
    fourcc: u32,
    byte_order: u32,
    bits_per_pixel: u32,
    depth: u32,
    red_mask: u32,
    green_mask: u32,
    blue_mask: u32,
    alpha_mask: u32,
    reserved: [u32; 4],
}

/// `VAImage`. Over-sized tail reserve so a driver writing its own (possibly
/// larger) layout cannot overrun this buffer.
#[repr(C)]
#[derive(Clone, Copy)]
struct VaImage {
    image_id: u32,
    format: VaImageFormat,
    buf: u32,
    width: u16,
    height: u16,
    data_size: u32,
    num_planes: u32,
    pitches: [u32; 3],
    offsets: [u32; 3],
    num_palette_entries: c_int,
    entry_bytes: c_int,
    component_order: [i8; 4],
    reserved: [u32; 32],
}

/// `VACodedBufferSegment`.
#[repr(C)]
struct VaCodedSegment {
    size: u32,
    bit_offset: u32,
    status: u32,
    reserved: u32,
    buf: *mut c_void,
    next: *mut c_void,
}

type GetDisplayDrmFn = unsafe extern "C" fn(c_int) -> *mut c_void;
type InitializeFn = unsafe extern "C" fn(*mut c_void, *mut c_int, *mut c_int) -> c_int;
type TerminateFn = unsafe extern "C" fn(*mut c_void) -> c_int;
type VendorStringFn = unsafe extern "C" fn(*mut c_void) -> *const c_char;
type MaxFn = unsafe extern "C" fn(*mut c_void) -> c_int;
type QueryProfilesFn = unsafe extern "C" fn(*mut c_void, *mut c_int, *mut c_int) -> c_int;
type QueryEntrypointsFn = unsafe extern "C" fn(*mut c_void, c_int, *mut c_int, *mut c_int) -> c_int;
type GetConfigAttribsFn =
    unsafe extern "C" fn(*mut c_void, c_int, c_int, *mut VaConfigAttribRaw, c_int) -> c_int;
type CreateConfigFn = unsafe extern "C" fn(
    *mut c_void,
    c_int,
    c_int,
    *mut VaConfigAttribRaw,
    c_int,
    *mut u32,
) -> c_int;
type CreateSurfacesFn = unsafe extern "C" fn(
    *mut c_void,
    u32,
    u32,
    u32,
    *mut u32,
    u32,
    *mut VaSurfaceAttribRaw,
    u32,
) -> c_int;
type DestroySurfacesFn = unsafe extern "C" fn(*mut c_void, *mut u32, c_int) -> c_int;
type CreateContextFn =
    unsafe extern "C" fn(*mut c_void, u32, c_int, c_int, c_int, *mut u32, c_int, *mut u32) -> c_int;
type CreateBufferFn =
    unsafe extern "C" fn(*mut c_void, u32, c_int, u32, u32, *mut c_void, *mut u32) -> c_int;
type DestroyIdFn = unsafe extern "C" fn(*mut c_void, u32) -> c_int;
type BeginPictureFn = unsafe extern "C" fn(*mut c_void, u32, u32) -> c_int;
type RenderPictureFn = unsafe extern "C" fn(*mut c_void, u32, *mut u32, c_int) -> c_int;
type MapBufferFn = unsafe extern "C" fn(*mut c_void, u32, *mut *mut c_void) -> c_int;
type DeriveImageFn = unsafe extern "C" fn(*mut c_void, u32, *mut VaImage) -> c_int;

struct VaFns {
    get_display_drm: GetDisplayDrmFn,
    initialize: InitializeFn,
    terminate: TerminateFn,
    vendor_string: VendorStringFn,
    max_profiles: MaxFn,
    max_entrypoints: MaxFn,
    query_profiles: QueryProfilesFn,
    query_entrypoints: QueryEntrypointsFn,
    get_config_attribs: GetConfigAttribsFn,
    create_config: CreateConfigFn,
    destroy_config: DestroyIdFn,
    create_surfaces: CreateSurfacesFn,
    destroy_surfaces: DestroySurfacesFn,
    create_context: CreateContextFn,
    destroy_context: DestroyIdFn,
    create_buffer: CreateBufferFn,
    destroy_buffer: DestroyIdFn,
    begin_picture: BeginPictureFn,
    render_picture: RenderPictureFn,
    end_picture: DestroyIdFn,
    sync_surface: DestroyIdFn,
    map_buffer: MapBufferFn,
    unmap_buffer: DestroyIdFn,
    derive_image: DeriveImageFn,
    destroy_image: DestroyIdFn,
}

/// Looks up a symbol and copies out its function pointer.
///
/// # Safety
/// `T` must be the exact C signature of the named symbol.
unsafe fn sym<T: Copy>(lib: &Library, name: &'static [u8]) -> Result<T, VaError> {
    lib.get::<T>(name)
        .map(|s| *s)
        .map_err(|e| VaError::Unavailable(format!("{}: {e}", String::from_utf8_lossy(name))))
}

struct VaLibs {
    _va: Library,
    _drm: Library,
    fns: VaFns,
}

impl VaLibs {
    fn load(va_names: &[&str], drm_names: &[&str]) -> Result<Self, VaError> {
        let open_first = |names: &[&str]| -> Result<Library, VaError> {
            let mut errors = Vec::new();
            for name in names {
                match unsafe { Library::new(name) } {
                    Ok(lib) => return Ok(lib),
                    Err(e) => errors.push(format!("{name}: {e}")),
                }
            }
            Err(VaError::Unavailable(errors.join("; ")))
        };
        let va = open_first(va_names)?;
        let drm = open_first(drm_names)?;
        // SAFETY: every type below is the C signature of the named libva symbol.
        let fns = unsafe {
            VaFns {
                get_display_drm: sym(&drm, b"vaGetDisplayDRM\0")?,
                initialize: sym(&va, b"vaInitialize\0")?,
                terminate: sym(&va, b"vaTerminate\0")?,
                vendor_string: sym(&va, b"vaQueryVendorString\0")?,
                max_profiles: sym(&va, b"vaMaxNumProfiles\0")?,
                max_entrypoints: sym(&va, b"vaMaxNumEntrypoints\0")?,
                query_profiles: sym(&va, b"vaQueryConfigProfiles\0")?,
                query_entrypoints: sym(&va, b"vaQueryConfigEntrypoints\0")?,
                get_config_attribs: sym(&va, b"vaGetConfigAttributes\0")?,
                create_config: sym(&va, b"vaCreateConfig\0")?,
                destroy_config: sym(&va, b"vaDestroyConfig\0")?,
                create_surfaces: sym(&va, b"vaCreateSurfaces\0")?,
                destroy_surfaces: sym(&va, b"vaDestroySurfaces\0")?,
                create_context: sym(&va, b"vaCreateContext\0")?,
                destroy_context: sym(&va, b"vaDestroyContext\0")?,
                create_buffer: sym(&va, b"vaCreateBuffer\0")?,
                destroy_buffer: sym(&va, b"vaDestroyBuffer\0")?,
                begin_picture: sym(&va, b"vaBeginPicture\0")?,
                render_picture: sym(&va, b"vaRenderPicture\0")?,
                end_picture: sym(&va, b"vaEndPicture\0")?,
                sync_surface: sym(&va, b"vaSyncSurface\0")?,
                map_buffer: sym(&va, b"vaMapBuffer\0")?,
                unmap_buffer: sym(&va, b"vaUnmapBuffer\0")?,
                derive_image: sym(&va, b"vaDeriveImage\0")?,
                destroy_image: sym(&va, b"vaDestroyImage\0")?,
            }
        };
        Ok(Self {
            _va: va,
            _drm: drm,
            fns,
        })
    }
}

/// Real libva layer. Loads libraries on the first `open`, not at construction.
pub struct LibVa {
    va_names: Vec<String>,
    drm_names: Vec<String>,
    libs: Option<VaLibs>,
    display: *mut c_void,
    drm_fd: Option<OwnedFd>,
}

// SAFETY: the VADisplay is only touched through `&mut self` methods, and the
// trait requires `Send + Sync` only so the encoder can live in async tasks.
unsafe impl Send for LibVa {}
unsafe impl Sync for LibVa {}

impl Default for LibVa {
    fn default() -> Self {
        Self::new()
    }
}

impl LibVa {
    pub fn new() -> Self {
        Self::with_library_names(
            &["libva.so.2", "libva.so"],
            &["libva-drm.so.2", "libva-drm.so"],
        )
    }

    pub fn with_library_names(va: &[&str], drm: &[&str]) -> Self {
        Self {
            va_names: va.iter().map(|s| s.to_string()).collect(),
            drm_names: drm.iter().map(|s| s.to_string()).collect(),
            libs: None,
            display: std::ptr::null_mut(),
            drm_fd: None,
        }
    }

    fn fns(&self) -> Result<&VaFns, VaError> {
        self.libs
            .as_ref()
            .map(|l| &l.fns)
            .ok_or_else(|| VaError::Invalid("libva is not open".into()))
    }

    fn display(&self) -> Result<*mut c_void, VaError> {
        if self.display.is_null() {
            Err(VaError::Invalid("VA display is not open".into()))
        } else {
            Ok(self.display)
        }
    }
}

fn check(op: &'static str, status: c_int) -> Result<(), VaError> {
    if status == VA_STATUS_SUCCESS {
        Ok(())
    } else {
        Err(VaError::Call { op, status })
    }
}

fn render_node_candidates() -> Result<Vec<String>, VaError> {
    if let Ok(path) = std::env::var(VAAPI_DEVICE_ENV) {
        if !path.is_empty() {
            return Ok(vec![path]);
        }
    }
    let entries = std::fs::read_dir("/dev/dri")
        .map_err(|e| VaError::Unavailable(format!("cannot read /dev/dri: {e}")))?;
    let mut nodes: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("renderD"))
        .map(|n| format!("/dev/dri/{n}"))
        .collect();
    nodes.sort();
    if nodes.is_empty() {
        return Err(VaError::Unavailable(
            "no DRM render node under /dev/dri".into(),
        ));
    }
    Ok(nodes)
}

/// Environment variable that pins the DRM render node, e.g. `/dev/dri/renderD129`.
pub const VAAPI_DEVICE_ENV: &str = "LINUX_QUEST_VAAPI_DEVICE";

impl VaLayer for LibVa {
    fn open(&mut self) -> Result<String, VaError> {
        // Re-opening (a second init) must not leak the previous display.
        if let Some(libs) = &self.libs {
            if !self.display.is_null() {
                // SAFETY: display was initialised by vaInitialize and is terminated once.
                unsafe {
                    (libs.fns.terminate)(self.display);
                }
                self.display = std::ptr::null_mut();
            }
        }
        self.drm_fd = None;
        if self.libs.is_none() {
            self.libs = Some(VaLibs::load(
                &self.va_names.iter().map(String::as_str).collect::<Vec<_>>(),
                &self
                    .drm_names
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
            )?);
        }
        let mut failures = Vec::new();
        for path in render_node_candidates()? {
            let file = match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
            {
                Ok(f) => f,
                Err(e) => {
                    failures.push(format!("{path}: open failed: {e}"));
                    continue;
                }
            };
            let fd: OwnedFd = file.into();
            let fns = self.fns()?;
            // SAFETY: fd is a valid open render node owned by `fd` for this call.
            let dpy = unsafe { (fns.get_display_drm)(fd.as_raw_fd()) };
            if dpy.is_null() {
                failures.push(format!("{path}: vaGetDisplayDRM returned NULL"));
                continue;
            }
            let (mut major, mut minor) = (0, 0);
            // SAFETY: dpy came from vaGetDisplayDRM; out-pointers are valid locals.
            let status = unsafe { (fns.initialize)(dpy, &mut major, &mut minor) };
            if status != VA_STATUS_SUCCESS {
                failures.push(format!("{path}: vaInitialize status {status}"));
                continue;
            }
            // SAFETY: the vendor string is a static NUL-terminated string owned by libva.
            let vendor = unsafe {
                let p = (fns.vendor_string)(dpy);
                if p.is_null() {
                    "unknown vendor".to_string()
                } else {
                    CStr::from_ptr(p).to_string_lossy().into_owned()
                }
            };
            self.display = dpy;
            self.drm_fd = Some(fd);
            return Ok(format!("{vendor} via {path} (VA-API {major}.{minor})"));
        }
        Err(VaError::Unavailable(format!(
            "no usable DRM render node: {}",
            failures.join("; ")
        )))
    }

    fn query_profiles(&mut self) -> Result<Vec<c_int>, VaError> {
        let fns = self.fns()?;
        let dpy = self.display()?;
        // SAFETY: dpy is an initialised display; the buffer holds the max count
        // libva reports for this display.
        unsafe {
            let max = (fns.max_profiles)(dpy).max(1) as usize;
            let mut buf = vec![0 as c_int; max];
            let mut n: c_int = 0;
            check(
                "vaQueryConfigProfiles",
                (fns.query_profiles)(dpy, buf.as_mut_ptr(), &mut n),
            )?;
            buf.truncate(n.max(0) as usize);
            Ok(buf)
        }
    }

    fn query_entrypoints(&mut self, profile: c_int) -> Result<Vec<c_int>, VaError> {
        let fns = self.fns()?;
        let dpy = self.display()?;
        // SAFETY: as in query_profiles.
        unsafe {
            let max = (fns.max_entrypoints)(dpy).max(1) as usize;
            let mut buf = vec![0 as c_int; max];
            let mut n: c_int = 0;
            check(
                "vaQueryConfigEntrypoints",
                (fns.query_entrypoints)(dpy, profile, buf.as_mut_ptr(), &mut n),
            )?;
            buf.truncate(n.max(0) as usize);
            Ok(buf)
        }
    }

    fn query_attribs(
        &mut self,
        profile: c_int,
        entrypoint: c_int,
        types: &[c_int],
    ) -> Result<Vec<AttribValue>, VaError> {
        let fns = self.fns()?;
        let dpy = self.display()?;
        let mut raw: Vec<VaConfigAttribRaw> = types
            .iter()
            .map(|&attrib_type| VaConfigAttribRaw {
                attrib_type,
                value: 0,
            })
            .collect();
        // SAFETY: raw has exactly `types.len()` writable entries.
        check("vaGetConfigAttributes", unsafe {
            (fns.get_config_attribs)(
                dpy,
                profile,
                entrypoint,
                raw.as_mut_ptr(),
                raw.len() as c_int,
            )
        })?;
        Ok(raw
            .into_iter()
            .map(|a| AttribValue {
                attrib_type: a.attrib_type,
                value: a.value,
            })
            .collect())
    }

    fn create_config(
        &mut self,
        profile: c_int,
        entrypoint: c_int,
        attribs: &[AttribValue],
    ) -> Result<u32, VaError> {
        let fns = self.fns()?;
        let dpy = self.display()?;
        let mut raw: Vec<VaConfigAttribRaw> = attribs
            .iter()
            .map(|a| VaConfigAttribRaw {
                attrib_type: a.attrib_type,
                value: a.value,
            })
            .collect();
        let mut id = VA_INVALID_ID;
        // SAFETY: raw and id are valid for the duration of the call.
        check("vaCreateConfig", unsafe {
            (fns.create_config)(
                dpy,
                profile,
                entrypoint,
                raw.as_mut_ptr(),
                raw.len() as c_int,
                &mut id,
            )
        })?;
        Ok(id)
    }

    fn destroy_config(&mut self, config: u32) {
        if let (Ok(fns), Ok(dpy)) = (self.fns(), self.display()) {
            // SAFETY: config was created on this display.
            unsafe {
                (fns.destroy_config)(dpy, config);
            }
        }
    }

    fn create_surfaces(
        &mut self,
        width: u32,
        height: u32,
        count: u32,
    ) -> Result<Vec<u32>, VaError> {
        let fns = self.fns()?;
        let dpy = self.display()?;
        let mut ids = vec![VA_INVALID_ID; count as usize];
        // SAFETY: ids has `count` slots; no attribute list is passed.
        check("vaCreateSurfaces", unsafe {
            (fns.create_surfaces)(
                dpy,
                VA_RT_FORMAT_YUV420,
                width,
                height,
                ids.as_mut_ptr(),
                count,
                std::ptr::null_mut(),
                0,
            )
        })?;
        Ok(ids)
    }

    fn import_prime_nv12(&mut self, desc: &PrimeNv12) -> Result<u32, VaError> {
        let fns = self.fns()?;
        let dpy = self.display()?;
        let mut prime = DrmPrimeDescriptor {
            fourcc: VA_FOURCC_NV12,
            width: desc.width,
            height: desc.height,
            num_objects: 2,
            num_layers: 1,
            ..Default::default()
        };
        for (i, p) in desc.planes.iter().enumerate() {
            prime.objects[i] = DrmObject {
                fd: p.fd,
                size: p.size,
                drm_format_modifier: p.modifier,
            };
        }
        prime.layers[0] = DrmLayer {
            drm_format: VA_FOURCC_NV12,
            num_planes: 2,
            object_index: [0, 1, 0, 0],
            offset: [desc.planes[0].offset, desc.planes[1].offset, 0, 0],
            pitch: [desc.planes[0].pitch, desc.planes[1].pitch, 0, 0],
        };
        let mut attribs = [
            VaSurfaceAttribRaw {
                attrib_type: VA_SURFACE_ATTRIB_MEM_TYPE,
                flags: VA_SURFACE_ATTRIB_SETTABLE,
                value: VaGenericValue {
                    kind: VA_GENERIC_VALUE_TYPE_INTEGER,
                    value: VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
                },
                reserved: [0; 4],
            },
            VaSurfaceAttribRaw {
                attrib_type: VA_SURFACE_ATTRIB_EXTERNAL_BUFFERS,
                flags: VA_SURFACE_ATTRIB_SETTABLE,
                value: VaGenericValue {
                    kind: VA_GENERIC_VALUE_TYPE_POINTER,
                    value: &mut prime as *mut DrmPrimeDescriptor as u64,
                },
                reserved: [0; 4],
            },
        ];
        let mut id = VA_INVALID_ID;
        // SAFETY: prime and attribs outlive the call; libva copies the descriptor
        // during vaCreateSurfaces and does not retain the pointer.
        check("vaCreateSurfaces(DRM_PRIME_2)", unsafe {
            (fns.create_surfaces)(
                dpy,
                VA_RT_FORMAT_YUV420,
                desc.width,
                desc.height,
                &mut id,
                1,
                attribs.as_mut_ptr(),
                attribs.len() as u32,
            )
        })?;
        Ok(id)
    }

    fn destroy_surfaces(&mut self, surfaces: &[u32]) {
        if let (Ok(fns), Ok(dpy)) = (self.fns(), self.display()) {
            let mut ids = surfaces.to_vec();
            if !ids.is_empty() {
                // SAFETY: ids are surfaces created on this display.
                unsafe {
                    (fns.destroy_surfaces)(dpy, ids.as_mut_ptr(), ids.len() as c_int);
                }
            }
        }
    }

    fn create_context(
        &mut self,
        config: u32,
        width: u32,
        height: u32,
        render_targets: &[u32],
    ) -> Result<u32, VaError> {
        let fns = self.fns()?;
        let dpy = self.display()?;
        let mut targets = render_targets.to_vec();
        let mut ctx = VA_INVALID_ID;
        // SAFETY: targets holds the surfaces the context will render into.
        check("vaCreateContext", unsafe {
            (fns.create_context)(
                dpy,
                config,
                width as c_int,
                height as c_int,
                VA_PROGRESSIVE,
                targets.as_mut_ptr(),
                targets.len() as c_int,
                &mut ctx,
            )
        })?;
        Ok(ctx)
    }

    fn destroy_context(&mut self, context: u32) {
        if let (Ok(fns), Ok(dpy)) = (self.fns(), self.display()) {
            // SAFETY: context was created on this display.
            unsafe {
                (fns.destroy_context)(dpy, context);
            }
        }
    }

    fn create_param_buffer(
        &mut self,
        context: u32,
        kind: c_int,
        data: &[u8],
    ) -> Result<u32, VaError> {
        let fns = self.fns()?;
        let dpy = self.display()?;
        let mut buf = VA_INVALID_ID;
        // SAFETY: data is read-only to libva; vaCreateBuffer copies it.
        check("vaCreateBuffer(param)", unsafe {
            (fns.create_buffer)(
                dpy,
                context,
                kind,
                data.len() as u32,
                1,
                data.as_ptr() as *mut c_void,
                &mut buf,
            )
        })?;
        Ok(buf)
    }

    fn create_coded_buffer(&mut self, context: u32, size: u32) -> Result<u32, VaError> {
        let fns = self.fns()?;
        let dpy = self.display()?;
        let mut buf = VA_INVALID_ID;
        // SAFETY: NULL data is the documented way to create a coded buffer.
        check("vaCreateBuffer(coded)", unsafe {
            (fns.create_buffer)(
                dpy,
                context,
                VA_BUF_ENC_CODED,
                size,
                1,
                std::ptr::null_mut(),
                &mut buf,
            )
        })?;
        Ok(buf)
    }

    fn destroy_buffer(&mut self, buffer: u32) {
        if let (Ok(fns), Ok(dpy)) = (self.fns(), self.display()) {
            // SAFETY: buffer was created on this display.
            unsafe {
                (fns.destroy_buffer)(dpy, buffer);
            }
        }
    }

    fn begin_picture(&mut self, context: u32, target: u32) -> Result<(), VaError> {
        let fns = self.fns()?;
        let dpy = self.display()?;
        // SAFETY: context and target were created on this display.
        check("vaBeginPicture", unsafe {
            (fns.begin_picture)(dpy, context, target)
        })
    }

    fn render_picture(&mut self, context: u32, buffers: &[u32]) -> Result<(), VaError> {
        let fns = self.fns()?;
        let dpy = self.display()?;
        let mut ids = buffers.to_vec();
        // SAFETY: ids are buffers created on this display.
        check("vaRenderPicture", unsafe {
            (fns.render_picture)(dpy, context, ids.as_mut_ptr(), ids.len() as c_int)
        })
    }

    fn end_picture(&mut self, context: u32) -> Result<(), VaError> {
        let fns = self.fns()?;
        let dpy = self.display()?;
        // SAFETY: context is live on this display.
        check("vaEndPicture", unsafe { (fns.end_picture)(dpy, context) })
    }

    fn sync_surface(&mut self, surface: u32) -> Result<(), VaError> {
        let fns = self.fns()?;
        let dpy = self.display()?;
        // SAFETY: surface is live on this display.
        check("vaSyncSurface", unsafe { (fns.sync_surface)(dpy, surface) })
    }

    fn read_coded_buffer(&mut self, buffer: u32) -> Result<Vec<u8>, VaError> {
        let fns = self.fns()?;
        let dpy = self.display()?;
        let mut ptr: *mut c_void = std::ptr::null_mut();
        // SAFETY: vaMapBuffer on a coded buffer yields a VACodedBufferSegment list
        // that stays valid until vaUnmapBuffer. Every segment is read before unmap.
        unsafe {
            check(
                "vaMapBuffer(coded)",
                (fns.map_buffer)(dpy, buffer, &mut ptr),
            )?;
            let mut out = Vec::new();
            let mut seg = ptr as *const VaCodedSegment;
            while !seg.is_null() {
                let s = &*seg;
                if !s.buf.is_null() && s.size > 0 {
                    out.extend_from_slice(std::slice::from_raw_parts(
                        s.buf as *const u8,
                        s.size as usize,
                    ));
                }
                seg = s.next as *const VaCodedSegment;
            }
            check("vaUnmapBuffer", (fns.unmap_buffer)(dpy, buffer))?;
            Ok(out)
        }
    }

    fn upload_nv12(
        &mut self,
        surface: u32,
        width: u32,
        height: u32,
        nv12: &[u8],
    ) -> Result<(), VaError> {
        let fns = self.fns()?;
        let dpy = self.display()?;
        let (w, h) = (width as usize, height as usize);
        if nv12.len() < w * h * 3 / 2 {
            return Err(VaError::Invalid("NV12 buffer too small for upload".into()));
        }
        let mut image: VaImage = unsafe { std::mem::zeroed() };
        // SAFETY: image is a writable VAImage-sized local.
        check("vaDeriveImage", unsafe {
            (fns.derive_image)(dpy, surface, &mut image)
        })?;
        let result = (|| -> Result<(), VaError> {
            if image.format.fourcc != VA_FOURCC_NV12 || image.num_planes < 2 {
                return Err(VaError::Unsupported(
                    "vaDeriveImage did not return a two-plane NV12 image".into(),
                ));
            }
            if (image.width as usize) < w || (image.height as usize) < h {
                return Err(VaError::Invalid("derived image smaller than frame".into()));
            }
            let mut base: *mut c_void = std::ptr::null_mut();
            // SAFETY: image.buf is a live buffer owned by the derived image.
            check("vaMapBuffer(image)", unsafe {
                (fns.map_buffer)(dpy, image.buf, &mut base)
            })?;
            let base = base as *mut u8;
            let (y_pitch, uv_pitch) = (image.pitches[0] as usize, image.pitches[1] as usize);
            // SAFETY: each row write is bounded by the pitch and plane height, which
            // libva reports for this image; the source rows are bounded by `nv12`.
            unsafe {
                for row in 0..h {
                    let dst = base.add(image.offsets[0] as usize + row * y_pitch);
                    std::ptr::copy_nonoverlapping(nv12.as_ptr().add(row * w), dst, w);
                }
                let uv_src = &nv12[w * h..];
                for row in 0..h / 2 {
                    let dst = base.add(image.offsets[1] as usize + row * uv_pitch);
                    std::ptr::copy_nonoverlapping(uv_src.as_ptr().add(row * w), dst, w);
                }
            }
            // SAFETY: unmapping the buffer mapped above.
            check("vaUnmapBuffer(image)", unsafe {
                (fns.unmap_buffer)(dpy, image.buf)
            })
        })();
        // SAFETY: the derived image was created on this display.
        unsafe {
            (fns.destroy_image)(dpy, image.image_id);
        }
        result
    }
}

impl Drop for LibVa {
    fn drop(&mut self) {
        if let Some(libs) = &self.libs {
            if !self.display.is_null() {
                // SAFETY: display was initialised by vaInitialize in open().
                unsafe {
                    (libs.fns.terminate)(self.display);
                }
            }
        }
        self.display = std::ptr::null_mut();
    }
}

// ---------------------------------------------------------------------------
// Capability probing and profile/entrypoint selection (pure, over VaLayer).
// ---------------------------------------------------------------------------

/// Result of selecting an encode profile and entrypoint for a codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodeSelection {
    pub profile: c_int,
    pub entrypoint: c_int,
    /// Packed-header capability bits reported by the driver.
    pub packed_headers: u32,
}

/// Outcome of probing a live (or mock) VA display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    pub vendor: String,
    pub selection: EncodeSelection,
}

fn codec_name(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::Av1 => "AV1",
        VideoCodec::Hevc => "HEVC",
        VideoCodec::H264 => "H.264",
    }
}

/// Entrypoints to try, in preference order. LP (VDEnc / low-power) comes first
/// because it is the low-latency path on Intel and VCN-based AMD hardware.
fn preferred_entrypoints(codec: VideoCodec) -> &'static [c_int] {
    match codec {
        VideoCodec::Hevc => &[VA_ENTRYPOINT_ENC_SLICE_LP, VA_ENTRYPOINT_ENC_SLICE],
        VideoCodec::Av1 => &[VA_ENTRYPOINT_ENC_SLICE_LP],
        VideoCodec::H264 => &[],
    }
}

/// Selects the profile and entrypoint for `codec` and checks that the driver
/// can do what this backend needs: NV12 render targets and CBR rate control.
pub fn select_encode(
    layer: &mut dyn VaLayer,
    codec: VideoCodec,
) -> Result<EncodeSelection, VaError> {
    let profile = match codec {
        VideoCodec::Hevc => VA_PROFILE_HEVC_MAIN,
        VideoCodec::Av1 => VA_PROFILE_AV1_PROFILE0,
        VideoCodec::H264 => {
            return Err(VaError::Unsupported(
                "H.264 VA-API encode is not implemented by this backend".into(),
            ))
        }
    };
    let profiles = layer.query_profiles()?;
    if !profiles.contains(&profile) {
        return Err(VaError::Unsupported(format!(
            "driver does not offer {} profile (id {profile}); offers {profiles:?}",
            codec_name(codec)
        )));
    }
    let entrypoints = layer.query_entrypoints(profile)?;
    let entrypoint = preferred_entrypoints(codec)
        .iter()
        .copied()
        .find(|e| entrypoints.contains(e))
        .ok_or_else(|| {
            VaError::Unsupported(format!(
                "no usable encode entrypoint for {}: driver offers {entrypoints:?}",
                codec_name(codec)
            ))
        })?;
    let attribs = layer.query_attribs(
        profile,
        entrypoint,
        &[
            VA_CONFIG_ATTRIB_RT_FORMAT,
            VA_CONFIG_ATTRIB_RATE_CONTROL,
            VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS,
            VA_CONFIG_ATTRIB_ENC_MAX_REF_FRAMES,
        ],
    )?;
    let get = |ty: c_int| {
        attribs
            .iter()
            .find(|a| a.attrib_type == ty)
            .map(|a| a.value)
            .filter(|v| *v != VA_ATTRIB_NOT_SUPPORTED)
    };
    let rt = get(VA_CONFIG_ATTRIB_RT_FORMAT).unwrap_or(0);
    if rt & VA_RT_FORMAT_YUV420 == 0 {
        return Err(VaError::Unsupported(
            "driver does not offer 4:2:0 render targets".into(),
        ));
    }
    let rc = get(VA_CONFIG_ATTRIB_RATE_CONTROL).unwrap_or(0);
    if rc & VA_RC_CBR == 0 {
        return Err(VaError::Unsupported(
            "driver does not offer constant-bitrate rate control".into(),
        ));
    }
    Ok(EncodeSelection {
        profile,
        entrypoint,
        packed_headers: get(VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS)
            .unwrap_or(VA_ENC_PACKED_HEADER_NONE),
    })
}

/// Probes a layer for `codec` and reports the vendor and the selected mode.
pub fn probe_with(layer: &mut dyn VaLayer, codec: VideoCodec) -> Result<ProbeReport, VaError> {
    let vendor = layer.open()?;
    let selection = select_encode(layer, codec)?;
    Ok(ProbeReport { vendor, selection })
}

/// Probes the real libva installation for `codec`.
pub fn probe(codec: VideoCodec) -> Result<ProbeReport, VaError> {
    let mut layer = LibVa::new();
    probe_with(&mut layer, codec)
}

// ---------------------------------------------------------------------------
// Parameter buffer serialisation.
// ---------------------------------------------------------------------------

/// Little-endian byte writer for parameter buffers.
#[derive(Default)]
struct Le {
    buf: Vec<u8>,
}

impl Le {
    fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }
    fn i8(&mut self, v: i8) -> &mut Self {
        self.buf.push(v as u8);
        self
    }
    fn u16(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn i32(&mut self, v: i32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn zeros(&mut self, n: usize) -> &mut Self {
        self.buf.resize(self.buf.len() + n, 0);
        self
    }
    fn picture(&mut self, p: &VaPictureHevc) -> &mut Self {
        self.u32(p.picture_id).i32(p.pic_order_cnt).u32(p.flags)
    }
}

/// `VAPictureHEVC`: a reference picture entry (12 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaPictureHevc {
    pub picture_id: u32,
    pub pic_order_cnt: i32,
    pub flags: u32,
}

impl VaPictureHevc {
    pub const fn invalid() -> Self {
        Self {
            picture_id: VA_INVALID_ID,
            pic_order_cnt: 0,
            flags: VA_PICTURE_HEVC_INVALID,
        }
    }
}

/// `VAEncSequenceParameterBufferHEVC`, serialised field by field.
/// Layout is transcribed from memory; see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HevcSeqParams {
    pub intra_period: u32,
    pub intra_idr_period: u32,
    pub ip_period: u32,
    pub bits_per_second: u32,
    pub width: u16,
    pub height: u16,
}

/// `seq_fields` bits: chroma_format_idc=1 (bits 0-1), sample_adaptive_offset (bit 12),
/// low_delay_seq (bit 16). Everything else is zero.
const HEVC_SEQ_FIELDS: u32 = 1 | (1 << 12) | (1 << 16);

impl HevcSeqParams {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = Le::default();
        w.u8(HEVC_MAIN_PROFILE_IDC)
            .u8(HEVC_LEVEL_5_1)
            .u8(0)
            .u8(0)
            .u32(self.intra_period)
            .u32(self.intra_idr_period)
            .u32(self.ip_period)
            .u32(self.bits_per_second)
            .u16(self.width)
            .u16(self.height)
            .u32(HEVC_SEQ_FIELDS)
            .u8(0) // log2_min_luma_coding_block_size_minus3 (8x8)
            .u8(2) // log2_diff_max_min_luma_coding_block_size (CTB 32)
            .u8(0) // log2_min_transform_block_size_minus2 (4x4)
            .u8(3) // log2_diff_max_min_transform_block_size (TB 32)
            .u8(2) // max_transform_hierarchy_depth_inter
            .u8(2); // max_transform_hierarchy_depth_intra
        w.buf
    }
}

/// `VAEncPictureParameterBufferHEVC`, serialised field by field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HevcPicParams {
    pub decoded_curr_pic: VaPictureHevc,
    pub reference_frames: [VaPictureHevc; 15],
    pub coded_buf: u32,
    pub pic_init_qp: i8,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub nal_unit_type: u8,
    pub idr: bool,
    pub coding_type: u32,
    pub reference_pic: bool,
}

impl HevcPicParams {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = Le::default();
        w.picture(&self.decoded_curr_pic);
        for r in &self.reference_frames {
            w.picture(r);
        }
        w.u32(self.coded_buf)
            .u8(VA_INVALID_COLLOCATED_INDEX)
            .u8(0) // last_picture
            .i8(self.pic_init_qp)
            .u8(0) // diff_cu_qp_delta_depth
            .i8(0) // pps_cb_qp_offset
            .i8(0) // pps_cr_qp_offset
            .u8(0) // num_tile_columns_minus1
            .u8(0) // num_tile_rows_minus1
            .zeros(19 * 2) // column_width_minus1[19]
            .zeros(21 * 2) // row_height_minus1[21]
            .u8(0) // log2_parallel_merge_level_minus2
            .u8(self.num_ref_idx_l0_default_active_minus1)
            .u8(0) // num_ref_idx_l1_default_active_minus1
            .u8(self.nal_unit_type);
        let pic_fields = (self.idr as u32)
            | ((self.coding_type & 0x3) << 1)
            | ((self.reference_pic as u32) << 3);
        w.u32(pic_fields);
        w.buf
    }
}

/// `VAEncSliceParameterBufferHEVC`, serialised field by field. One slice per picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HevcSliceParams {
    pub num_ctu_in_slice: u32,
    /// 0 = B, 1 = P, 2 = I (HEVC slice_type values).
    pub slice_type: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub ref_pic_list0: [VaPictureHevc; 15],
}

impl HevcSliceParams {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = Le::default();
        w.u32(0) // slice_segment_address
            .u32(self.num_ctu_in_slice)
            .u8(self.slice_type)
            .u8(0) // slice_pic_parameter_set_id
            .u8(self.num_ref_idx_l0_active_minus1)
            .u8(0); // num_ref_idx_l1_active_minus1
        for r in &self.ref_pic_list0 {
            w.picture(r);
        }
        for _ in 0..15 {
            w.picture(&VaPictureHevc::invalid());
        }
        w.u8(0) // luma_log2_weight_denom
            .i8(0) // delta_chroma_log2_weight_denom
            .zeros(2 * (15 + 15 + 30 + 30)) // weight tables, disabled (zero)
            .u8(5) // max_num_merge_cand
            .i8(0) // slice_qp_delta
            .i8(0) // slice_cb_qp_offset
            .i8(0) // slice_cr_qp_offset
            .i8(0) // slice_beta_offset_div2
            .i8(0) // slice_tc_offset_div2
            .u32(1); // slice_fields: last_slice_of_pic
        w.buf
    }
}

/// Builds a misc parameter buffer: a 32-bit type followed by its payload words.
fn misc_buffer(kind: u32, payload: &[u32]) -> Vec<u8> {
    let mut w = Le::default();
    w.u32(kind);
    for v in payload {
        w.u32(*v);
    }
    w.buf
}

/// Rate-control misc parameter for CBR with `bitrate_kbps` at `fps`.
pub fn rate_control_misc(bitrate_kbps: u32, fps: u32) -> Vec<u8> {
    let bits = bitrate_kbps.saturating_mul(1000);
    let fps = fps.max(1);
    let window_ms = (1000 / fps).max(1);
    misc_buffer(
        VA_ENC_MISC_RATE_CONTROL,
        &[
            bits,      // bits_per_second
            100,       // target_percentage: 100 means constant bitrate
            window_ms, // window_size
            0,         // initial_qp (driver default)
            0,         // min_qp
            0,         // basic_unit_size
            0,         // rc_flags (no reset)
            0,         // ICQ_quality_factor
            51,        // max_qp
            0,         // quality_factor
            0,         // target_frame_size
        ],
    )
}

/// Frame-rate misc parameter (fps as a plain integer).
pub fn frame_rate_misc(fps: u32) -> Vec<u8> {
    misc_buffer(VA_ENC_MISC_FRAME_RATE, &[fps.max(1), 0])
}

/// HRD misc parameter: one frame of VBV buffering, matching the NVENC backend.
pub fn hrd_misc(bitrate_kbps: u32, fps: u32) -> Vec<u8> {
    let buffer = bitrate_kbps.saturating_mul(1000) / fps.max(1);
    misc_buffer(VA_ENC_MISC_HRD, &[buffer, buffer])
}

// ---------------------------------------------------------------------------
// Input planning and CPU NV12 conversion.
// ---------------------------------------------------------------------------

/// Returns a PRIME import request if the frame is a two-plane NV12 DMA-BUF.
pub fn prime_nv12_descriptor(frame: &RawFrame) -> Option<PrimeNv12> {
    if frame.format != PixelFormat::Nv12 {
        return None;
    }
    let planes = frame.dma_buf.as_ref()?;
    if planes.len() != 2 || !planes.iter().all(DmaBufPlane::is_valid) {
        return None;
    }
    let (w, h) = (frame.width, frame.height);
    if w == 0 || h == 0 || !w.is_multiple_of(2) || !h.is_multiple_of(2) {
        return None;
    }
    let make = |p: &DmaBufPlane, rows: u32| PrimePlane {
        fd: p.raw_fd(),
        size: p.offset.saturating_add(p.stride.saturating_mul(rows)),
        pitch: p.stride,
        offset: p.offset,
        modifier: p.modifier,
    };
    Some(PrimeNv12 {
        width: w,
        height: h,
        planes: [make(&planes[0], h), make(&planes[1], h / 2)],
    })
}

/// Packs a frame into NV12 on the CPU. NV12 frames are copied; BGRA and RGBA
/// frames are converted to BT.601 limited range.
pub fn nv12_from_frame(frame: &RawFrame) -> Result<Vec<u8>, VaError> {
    let (w, h) = (frame.width as usize, frame.height as usize);
    if w == 0 || h == 0 || !w.is_multiple_of(2) || !h.is_multiple_of(2) {
        return Err(VaError::Invalid(format!(
            "NV12 requires even non-zero dimensions, got {w}x{h}"
        )));
    }
    let stride = frame.stride as usize;
    if frame.data.is_empty() {
        return Err(VaError::Invalid(
            "frame has no CPU-readable data (DMA-BUF only); RGB DMA-BUF import needs VPP \
             colour conversion, which this backend does not implement"
                .into(),
        ));
    }
    let data = &frame.data[..];
    match frame.format {
        PixelFormat::Nv12 => {
            if stride < w || data.len() < stride * h * 3 / 2 {
                return Err(VaError::Invalid("NV12 frame data is too small".into()));
            }
            let mut out = Vec::with_capacity(w * h * 3 / 2);
            for row in 0..h {
                out.extend_from_slice(&data[row * stride..row * stride + w]);
            }
            for row in 0..h / 2 {
                let start = stride * h + row * stride;
                out.extend_from_slice(&data[start..start + w]);
            }
            Ok(out)
        }
        PixelFormat::Bgra8 | PixelFormat::Rgba8 => {
            if stride < w * 4 || data.len() < stride * h {
                return Err(VaError::Invalid("RGB frame data is too small".into()));
            }
            let (ri, bi) = if frame.format == PixelFormat::Rgba8 {
                (0, 2)
            } else {
                (2, 0)
            };
            let px = |x: usize, y: usize| -> (i32, i32, i32) {
                let o = y * stride + x * 4;
                (data[o + ri] as i32, data[o + 1] as i32, data[o + bi] as i32)
            };
            let mut out = vec![0u8; w * h * 3 / 2];
            for y in 0..h {
                for x in 0..w {
                    let (r, g, b) = px(x, y);
                    out[y * w + x] = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
                }
            }
            let mut chroma = Vec::with_capacity(w * h / 2);
            for cy in 0..h / 2 {
                for cx in 0..w / 2 {
                    let (mut sr, mut sg, mut sb) = (0, 0, 0);
                    for dy in 0..2 {
                        for dx in 0..2 {
                            let (r, g, b) = px(cx * 2 + dx, cy * 2 + dy);
                            sr += r;
                            sg += g;
                            sb += b;
                        }
                    }
                    let (r, g, b) = ((sr + 2) >> 2, (sg + 2) >> 2, (sb + 2) >> 2);
                    let cb = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
                    let cr = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
                    chroma.push(cb);
                    chroma.push(cr);
                }
            }
            out[w * h..].copy_from_slice(&chroma);
            Ok(out)
        }
    }
}

// ---------------------------------------------------------------------------
// Reference bookkeeping.
// ---------------------------------------------------------------------------

/// Reference picture slots kept by the encoder.
pub const DPB_CAPACITY: usize = 4;
/// Reconstruction pool size. One more than the DPB, so a free target always exists.
pub const RECON_POOL_SIZE: usize = DPB_CAPACITY + 1;
/// Number of references each inter frame uses. Low latency means one.
pub const LOW_LATENCY_REFS: usize = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DpbEntry {
    pub surface: u32,
    pub pts_us: u64,
    pub poc: i32,
    pub valid: bool,
}

#[derive(Debug, Default)]
pub struct Dpb {
    entries: VecDeque<DpbEntry>,
}

impl Dpb {
    pub fn push(&mut self, entry: DpbEntry) {
        self.entries.push_back(entry);
        while self.entries.len() > DPB_CAPACITY {
            self.entries.pop_front();
        }
    }

    /// Marks entries with this PTS unusable. Returns true if any matched.
    pub fn invalidate(&mut self, pts_us: u64) -> bool {
        let mut hit = false;
        for e in self.entries.iter_mut().filter(|e| e.pts_us == pts_us) {
            e.valid = false;
            hit = true;
        }
        hit
    }

    /// Newest-first valid entries, at most `n`.
    pub fn valid_refs(&self, n: usize) -> Vec<DpbEntry> {
        self.entries
            .iter()
            .rev()
            .filter(|e| e.valid)
            .take(n)
            .copied()
            .collect()
    }

    pub fn holds(&self, surface: u32) -> bool {
        self.entries.iter().any(|e| e.surface == surface)
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Encoder.
// ---------------------------------------------------------------------------

/// Session objects created on the VA display.
#[derive(Default)]
struct Session {
    config: EncoderConfig,
    selection: Option<EncodeSelection>,
    config_id: Option<u32>,
    context: Option<u32>,
    coded: Option<u32>,
    recon: Vec<u32>,
    input: Vec<u32>,
    next_input: usize,
    dpb: Dpb,
    frame_counter: u64,
    idr_base: u64,
    sequence_started: bool,
    cpu_fallback_logged: bool,
}

impl Session {
    fn destroy(&mut self, layer: &mut dyn VaLayer) {
        let mut surfaces = std::mem::take(&mut self.recon);
        surfaces.append(&mut self.input);
        if !surfaces.is_empty() {
            layer.destroy_surfaces(&surfaces);
        }
        if let Some(c) = self.coded.take() {
            layer.destroy_buffer(c);
        }
        if let Some(ctx) = self.context.take() {
            layer.destroy_context(ctx);
        }
        if let Some(cfg) = self.config_id.take() {
            layer.destroy_config(cfg);
        }
        self.dpb.clear();
    }
}

/// Picture kind for a frame. Determines slice type, NAL type and output flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PictureKind {
    /// IDR. Starts a sequence and resets the DPB.
    Idr,
    /// Non-IDR I-frame used as a recovery point when no valid reference remains.
    Recovery,
    /// Periodic full I-frame, see the module docs on intra refresh.
    IntraRefresh,
    /// P-frame referencing the newest valid DPB entry.
    Inter,
}

impl PictureKind {
    fn is_intra(self) -> bool {
        !matches!(self, PictureKind::Inter)
    }
    fn picture_type(self) -> u32 {
        match self {
            PictureKind::Idr => 3,
            PictureKind::Recovery => 2,
            PictureKind::IntraRefresh => 6,
            PictureKind::Inter => 0,
        }
    }
}

/// VA-API HEVC encoder. Uses the real libva when constructed with [`VaapiEncoder::new`].
pub struct VaapiEncoder {
    layer: Box<dyn VaLayer>,
    config: EncoderConfig,
    session: Option<Session>,
    force_keyframe: bool,
    last_rpi_frame: Option<u64>,
    vendor: Option<String>,
}

impl VaapiEncoder {
    /// Encoder over the real libva, loaded lazily on `init`.
    pub fn new(config: EncoderConfig) -> Self {
        Self::with_layer(config, Box::new(LibVa::new()))
    }

    /// Encoder over an arbitrary layer (used by tests with [`MockVaLayer`]).
    pub fn with_layer(config: EncoderConfig, layer: Box<dyn VaLayer>) -> Self {
        Self {
            layer,
            config,
            session: None,
            force_keyframe: true,
            last_rpi_frame: None,
            vendor: None,
        }
    }

    pub fn vendor(&self) -> Option<&str> {
        self.vendor.as_deref()
    }

    pub fn last_rpi_frame(&self) -> Option<u64> {
        self.last_rpi_frame
    }

    fn teardown(&mut self) {
        if let Some(mut s) = self.session.take() {
            s.destroy(self.layer.as_mut());
        }
    }
}

fn to_init(e: VaError) -> EncoderError {
    EncoderError::InitFailed(format!("VA-API: {e}"))
}

fn to_encode(e: VaError) -> EncoderError {
    EncoderError::EncodeFailed(format!("VA-API: {e}"))
}

/// Creates every session object. On error the caller destroys the partial session.
fn build_session(
    layer: &mut dyn VaLayer,
    s: &mut Session,
    selection: EncodeSelection,
) -> Result<(), VaError> {
    let attribs = [
        AttribValue {
            attrib_type: VA_CONFIG_ATTRIB_RT_FORMAT,
            value: VA_RT_FORMAT_YUV420,
        },
        AttribValue {
            attrib_type: VA_CONFIG_ATTRIB_RATE_CONTROL,
            value: VA_RC_CBR,
        },
        AttribValue {
            attrib_type: VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS,
            value: VA_ENC_PACKED_HEADER_NONE,
        },
    ];
    let (w, h) = (s.config.width, s.config.height);
    let config_id = layer.create_config(selection.profile, selection.entrypoint, &attribs)?;
    s.config_id = Some(config_id);
    let recon = layer.create_surfaces(w, h, RECON_POOL_SIZE as u32)?;
    s.recon = recon.clone();
    s.input = layer.create_surfaces(w, h, 2)?;
    let ctx = layer.create_context(config_id, w, h, &recon)?;
    s.context = Some(ctx);
    let coded_size = (w as u64 * h as u64 * 3 / 2).clamp(64 * 1024, u32::MAX as u64) as u32;
    s.coded = Some(layer.create_coded_buffer(ctx, coded_size)?);
    s.selection = Some(selection);
    Ok(())
}

fn warn_cpu_once(s: &mut Session, why: String) {
    if !s.cpu_fallback_logged {
        warn!("VA-API input: {why}. Using the CPU NV12 upload path for this stream.");
        s.cpu_fallback_logged = true;
    }
}

/// Resolves the input surface for a frame. Returns `(surface, imported)`, where
/// `imported` is set when the surface was created for this frame and must be destroyed.
fn resolve_input(
    layer: &mut dyn VaLayer,
    s: &mut Session,
    frame: &RawFrame,
) -> Result<(u32, Option<u32>), EncoderError> {
    if let Some(desc) = prime_nv12_descriptor(frame) {
        match layer.import_prime_nv12(&desc) {
            Ok(surface) => return Ok((surface, Some(surface))),
            Err(e) => {
                if frame.data.is_empty() {
                    return Err(EncoderError::EncodeFailed(format!(
                        "VA-API DMA-BUF import failed and the frame has no CPU copy: {e}"
                    )));
                }
                warn_cpu_once(s, format!("DMA-BUF PRIME import failed ({e})"));
            }
        }
    } else if frame.dma_buf.is_some() {
        warn_cpu_once(
            s,
            "DMA-BUF frame is not two-plane NV12, so zero-copy import is not possible".into(),
        );
    }
    let nv12 = nv12_from_frame(frame).map_err(to_encode)?;
    if s.input.is_empty() {
        return Err(EncoderError::EncodeFailed(
            "VA-API input surfaces are not allocated".into(),
        ));
    }
    let surface = s.input[s.next_input % s.input.len()];
    s.next_input = s.next_input.wrapping_add(1);
    layer
        .upload_nv12(surface, frame.width, frame.height, &nv12)
        .map_err(to_encode)?;
    Ok((surface, None))
}

/// Submits one picture to the driver and returns the coded bitstream.
fn submit_picture(
    layer: &mut dyn VaLayer,
    s: &Session,
    temp: &mut Vec<u32>,
    input: u32,
    kind: PictureKind,
    pic: &HevcPicParams,
    slice: &HevcSliceParams,
) -> Result<Vec<u8>, VaError> {
    let context = s
        .context
        .ok_or_else(|| VaError::Invalid("no VA context".into()))?;
    let cfg = &s.config;
    let mut make = |layer: &mut dyn VaLayer, ty: c_int, data: &[u8]| -> Result<(), VaError> {
        let id = layer.create_param_buffer(context, ty, data)?;
        temp.push(id);
        Ok(())
    };
    if kind == PictureKind::Idr {
        let seq = HevcSeqParams {
            intra_period: if cfg.intra_refresh_period > 0 {
                cfg.intra_refresh_period
            } else {
                u32::MAX
            },
            intra_idr_period: u32::MAX,
            ip_period: 1,
            bits_per_second: cfg.bitrate_kbps.saturating_mul(1000),
            width: cfg.width as u16,
            height: cfg.height as u16,
        };
        make(layer, VA_BUF_ENC_SEQ, &seq.to_bytes())?;
        make(
            layer,
            VA_BUF_ENC_MISC,
            &rate_control_misc(cfg.bitrate_kbps, cfg.fps),
        )?;
        make(layer, VA_BUF_ENC_MISC, &frame_rate_misc(cfg.fps))?;
        make(layer, VA_BUF_ENC_MISC, &hrd_misc(cfg.bitrate_kbps, cfg.fps))?;
    }
    make(layer, VA_BUF_ENC_PIC, &pic.to_bytes())?;
    make(layer, VA_BUF_ENC_SLICE, &slice.to_bytes())?;
    let buffers = temp.clone();

    layer.begin_picture(context, input)?;
    let render = layer.render_picture(context, &buffers);
    let end = layer.end_picture(context);
    render?;
    end?;
    layer.sync_surface(input)?;
    let coded = s
        .coded
        .ok_or_else(|| VaError::Invalid("no coded buffer".into()))?;
    layer.read_coded_buffer(coded)
}

fn encode_on_session(
    layer: &mut dyn VaLayer,
    s: &mut Session,
    frame: &RawFrame,
    force_idr: bool,
) -> Result<(PictureKind, u64, Vec<u8>), EncoderError> {
    s.frame_counter += 1;
    let frame_index = s.frame_counter;
    let period = s.config.intra_refresh_period as u64;
    let kind = if force_idr || !s.sequence_started {
        PictureKind::Idr
    } else if s.dpb.valid_refs(LOW_LATENCY_REFS).is_empty() {
        PictureKind::Recovery
    } else if period > 0 && frame_index.is_multiple_of(period) {
        PictureKind::IntraRefresh
    } else {
        PictureKind::Inter
    };
    if kind == PictureKind::Idr {
        s.dpb.clear();
        s.idr_base = frame_index;
    }
    let refs = if kind == PictureKind::Inter {
        s.dpb.valid_refs(LOW_LATENCY_REFS)
    } else {
        Vec::new()
    };
    if s.selection.is_none() {
        return Err(EncoderError::EncodeFailed(
            "VA-API session not configured".into(),
        ));
    }

    let (input, imported) = resolve_input(layer, s, frame)?;
    let result = encode_with_input(layer, s, kind, &refs, input);
    if let Some(surface) = imported {
        layer.destroy_surfaces(&[surface]);
    }
    let (recon, payload) = result?;
    if kind == PictureKind::Idr {
        s.sequence_started = true;
    }
    let poc = (frame_index - s.idr_base) as i32;
    s.dpb.push(DpbEntry {
        surface: recon,
        pts_us: frame.pts_us,
        poc,
        valid: true,
    });
    Ok((kind, frame_index, payload))
}

fn encode_with_input(
    layer: &mut dyn VaLayer,
    s: &mut Session,
    kind: PictureKind,
    refs: &[DpbEntry],
    input: u32,
) -> Result<(u32, Vec<u8>), EncoderError> {
    let recon = s
        .recon
        .iter()
        .copied()
        .find(|surf| !s.dpb.holds(*surf))
        .ok_or_else(|| EncoderError::EncodeFailed("no free reconstruction surface".into()))?;
    let mut reference_frames = [VaPictureHevc::invalid(); 15];
    for (slot, e) in reference_frames.iter_mut().zip(refs) {
        *slot = VaPictureHevc {
            picture_id: e.surface,
            pic_order_cnt: e.poc,
            flags: VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE,
        };
    }
    let coded_buf = s
        .coded
        .ok_or_else(|| EncoderError::EncodeFailed("no coded buffer".into()))?;
    let poc = (s.frame_counter - s.idr_base) as i32;
    let idr = kind == PictureKind::Idr;
    let pic = HevcPicParams {
        decoded_curr_pic: VaPictureHevc {
            picture_id: recon,
            pic_order_cnt: poc,
            flags: 0,
        },
        reference_frames,
        coded_buf,
        pic_init_qp: 26,
        num_ref_idx_l0_default_active_minus1: refs.len().max(1).saturating_sub(1) as u8,
        nal_unit_type: if idr {
            HEVC_NAL_IDR_W_RADL
        } else {
            HEVC_NAL_TRAIL_R
        },
        idr,
        coding_type: if kind.is_intra() {
            VA_CODING_TYPE_I
        } else {
            VA_CODING_TYPE_P
        },
        reference_pic: true,
    };
    let ctb = HEVC_CTB_SIZE;
    let slice = HevcSliceParams {
        num_ctu_in_slice: s.config.width.div_ceil(ctb) * s.config.height.div_ceil(ctb),
        slice_type: match kind {
            PictureKind::Inter => 1,
            _ => 2,
        },
        num_ref_idx_l0_active_minus1: pic.num_ref_idx_l0_default_active_minus1,
        ref_pic_list0: reference_frames,
    };
    let mut temp = Vec::new();
    let payload = submit_picture(layer, s, &mut temp, input, kind, &pic, &slice);
    for id in temp {
        layer.destroy_buffer(id);
    }
    Ok((recon, payload.map_err(to_encode)?))
}

#[async_trait]
impl VideoEncoder for VaapiEncoder {
    async fn init(&mut self, config: EncoderConfig) -> Result<(), EncoderError> {
        self.teardown();
        self.config = config.clone();
        self.force_keyframe = true;
        self.last_rpi_frame = None;
        if config.codec != VideoCodec::Hevc {
            return Err(EncoderError::UnsupportedCodec(config.codec));
        }
        if config.width == 0
            || config.height == 0
            || !config.width.is_multiple_of(2)
            || !config.height.is_multiple_of(2)
        {
            return Err(EncoderError::InitFailed(format!(
                "VA-API NV12 needs even non-zero dimensions, got {}x{}",
                config.width, config.height
            )));
        }
        let vendor = self.layer.open().map_err(to_init)?;
        let selection = select_encode(self.layer.as_mut(), config.codec).map_err(to_init)?;
        let mut session = Session {
            config: config.clone(),
            ..Default::default()
        };
        if let Err(e) = build_session(self.layer.as_mut(), &mut session, selection) {
            session.destroy(self.layer.as_mut());
            return Err(to_init(e));
        }
        if selection.packed_headers != VA_ENC_PACKED_HEADER_NONE {
            info!(
                "VA-API driver reports packed-header support ({:#x}); parameter sets are left to the driver",
                selection.packed_headers
            );
        }
        warn!(
            "VA-API HEVC encoder initialised on {vendor} (profile {}, entrypoint {}). \
             This backend has not been verified on hardware; check the first IDR decodes.",
            selection.profile, selection.entrypoint
        );
        self.vendor = Some(vendor);
        self.session = Some(session);
        Ok(())
    }

    async fn encode(&mut self, frame: &RawFrame) -> Result<EncodedFrame, EncoderError> {
        let start = Instant::now();
        let force_idr = self.force_keyframe;
        let codec = self.config.codec;
        let layer = self.layer.as_mut();
        let s = self
            .session
            .as_mut()
            .ok_or_else(|| EncoderError::EncodeFailed("Encoder not initialized".into()))?;
        let (kind, frame_id, payload) = encode_on_session(layer, s, frame, force_idr)?;
        self.force_keyframe = false;

        let payload = Bytes::from(payload);
        let max_chunk = self.config.max_chunk_size.max(1);
        let total_chunks = payload.len().div_ceil(max_chunk);
        let mut chunks = Vec::with_capacity(total_chunks.max(1));
        for idx in 0..total_chunks {
            let start_off = idx * max_chunk;
            let end_off = (start_off + max_chunk).min(payload.len());
            let meta = VideoChunkMeta {
                frame_id,
                chunk_index: idx as u16,
                total_chunks: total_chunks as u16,
                codec,
                is_keyframe: matches!(kind, PictureKind::Idr | PictureKind::Recovery),
                is_intra_refresh: kind == PictureKind::IntraRefresh,
                width: self.config.width,
                height: self.config.height,
                fps: self.config.fps,
                pts_us: frame.pts_us,
            };
            chunks.push(VideoChunk::new(meta, payload.slice(start_off..end_off)));
        }

        Ok(EncodedFrame {
            frame_id,
            is_keyframe: matches!(kind, PictureKind::Idr | PictureKind::Recovery),
            is_intra_refresh: kind == PictureKind::IntraRefresh,
            picture_type: kind.picture_type(),
            chunks,
            encode_duration_us: (start.elapsed().as_micros() as u32).max(1),
        })
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn invalidate_reference_picture(&mut self, pts_us: u64) {
        self.last_rpi_frame = Some(pts_us);
        if let Some(s) = self.session.as_mut() {
            s.dpb.invalidate(pts_us);
        }
    }
}

impl Drop for VaapiEncoder {
    fn drop(&mut self) {
        self.teardown();
    }
}

// ---------------------------------------------------------------------------
// Mock layer for tests and for exercising the call sequence without a driver.
// ---------------------------------------------------------------------------

/// One recorded interaction with the mock layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaCall {
    Import { surface: u32 },
    Upload { surface: u32, len: usize },
    Begin { target: u32 },
    Render { buffers: Vec<u32> },
    End,
    Sync { surface: u32 },
    ReadCoded { buffer: u32 },
    DestroySurfaces { count: usize },
    DestroyBuffer { buffer: u32 },
}

/// Scripted driver state behind [`MockVaLayer`]. Tests read `calls` and `params`.
#[derive(Debug, Default)]
pub struct MockVaState {
    pub vendor: String,
    pub open_error: Option<String>,
    pub profiles: Vec<c_int>,
    pub entrypoints: BTreeMap<c_int, Vec<c_int>>,
    pub attribs: BTreeMap<(c_int, c_int, c_int), u32>,
    pub import_error: Option<String>,
    /// Bitstream returned by `read_coded_buffer`.
    pub coded_payload: Vec<u8>,
    pub calls: Vec<VaCall>,
    /// Every parameter buffer ever created: id -> (kind, bytes).
    pub params: BTreeMap<u32, (c_int, Vec<u8>)>,
    pub rendered: Vec<Vec<u32>>,
    next_id: u32,
}

impl MockVaState {
    fn id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    /// A driver offering HEVC Main with LP and CBR, the case this backend targets.
    pub fn hevc_lp() -> Self {
        let mut s = Self {
            vendor: "mock-vendor".into(),
            profiles: vec![VA_PROFILE_HEVC_MAIN],
            coded_payload: vec![0xAB; 3000],
            ..Default::default()
        };
        s.entrypoints
            .insert(VA_PROFILE_HEVC_MAIN, vec![VA_ENTRYPOINT_ENC_SLICE_LP]);
        s.set_attribs(VA_PROFILE_HEVC_MAIN, VA_ENTRYPOINT_ENC_SLICE_LP);
        s
    }

    pub fn set_attribs(&mut self, profile: c_int, entrypoint: c_int) {
        self.attribs.insert(
            (profile, entrypoint, VA_CONFIG_ATTRIB_RT_FORMAT),
            VA_RT_FORMAT_YUV420,
        );
        self.attribs.insert(
            (profile, entrypoint, VA_CONFIG_ATTRIB_RATE_CONTROL),
            VA_RC_CBR,
        );
        self.attribs.insert(
            (profile, entrypoint, VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS),
            VA_ENC_PACKED_HEADER_NONE,
        );
        self.attribs.insert(
            (profile, entrypoint, VA_CONFIG_ATTRIB_ENC_MAX_REF_FRAMES),
            1,
        );
    }
}

/// In-memory [`VaLayer`]. Clone it to keep a handle on the shared state.
#[derive(Clone)]
pub struct MockVaLayer {
    pub state: Arc<Mutex<MockVaState>>,
}

impl MockVaLayer {
    pub fn new(state: MockVaState) -> Self {
        Self {
            state: Arc::new(Mutex::new(state)),
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut MockVaState) -> T) -> T {
        let mut guard = self.state.lock().expect("mock state poisoned");
        f(&mut guard)
    }
}

impl VaLayer for MockVaLayer {
    fn open(&mut self) -> Result<String, VaError> {
        self.with(|s| match &s.open_error {
            Some(e) => Err(VaError::Unavailable(e.clone())),
            None => Ok(s.vendor.clone()),
        })
    }

    fn query_profiles(&mut self) -> Result<Vec<c_int>, VaError> {
        Ok(self.with(|s| s.profiles.clone()))
    }

    fn query_entrypoints(&mut self, profile: c_int) -> Result<Vec<c_int>, VaError> {
        Ok(self.with(|s| s.entrypoints.get(&profile).cloned().unwrap_or_default()))
    }

    fn query_attribs(
        &mut self,
        profile: c_int,
        entrypoint: c_int,
        types: &[c_int],
    ) -> Result<Vec<AttribValue>, VaError> {
        Ok(self.with(|s| {
            types
                .iter()
                .map(|&t| AttribValue {
                    attrib_type: t,
                    value: s
                        .attribs
                        .get(&(profile, entrypoint, t))
                        .copied()
                        .unwrap_or(VA_ATTRIB_NOT_SUPPORTED),
                })
                .collect()
        }))
    }

    fn create_config(
        &mut self,
        _profile: c_int,
        _entrypoint: c_int,
        _attribs: &[AttribValue],
    ) -> Result<u32, VaError> {
        Ok(self.with(|s| s.id()))
    }

    fn destroy_config(&mut self, _config: u32) {}

    fn create_surfaces(&mut self, _w: u32, _h: u32, count: u32) -> Result<Vec<u32>, VaError> {
        Ok(self.with(|s| (0..count).map(|_| s.id()).collect()))
    }

    fn import_prime_nv12(&mut self, desc: &PrimeNv12) -> Result<u32, VaError> {
        let _ = desc;
        self.with(|s| {
            if let Some(e) = &s.import_error {
                return Err(VaError::Unsupported(e.clone()));
            }
            let surface = s.id();
            s.calls.push(VaCall::Import { surface });
            Ok(surface)
        })
    }

    fn destroy_surfaces(&mut self, surfaces: &[u32]) {
        self.with(|s| {
            s.calls.push(VaCall::DestroySurfaces {
                count: surfaces.len(),
            })
        });
    }

    fn create_context(
        &mut self,
        _config: u32,
        _w: u32,
        _h: u32,
        _targets: &[u32],
    ) -> Result<u32, VaError> {
        Ok(self.with(|s| s.id()))
    }

    fn destroy_context(&mut self, _context: u32) {}

    fn create_param_buffer(
        &mut self,
        _context: u32,
        kind: c_int,
        data: &[u8],
    ) -> Result<u32, VaError> {
        Ok(self.with(|s| {
            let id = s.id();
            s.params.insert(id, (kind, data.to_vec()));
            id
        }))
    }

    fn create_coded_buffer(&mut self, _context: u32, _size: u32) -> Result<u32, VaError> {
        Ok(self.with(|s| s.id()))
    }

    fn destroy_buffer(&mut self, buffer: u32) {
        self.with(|s| s.calls.push(VaCall::DestroyBuffer { buffer }));
    }

    fn begin_picture(&mut self, _context: u32, target: u32) -> Result<(), VaError> {
        self.with(|s| s.calls.push(VaCall::Begin { target }));
        Ok(())
    }

    fn render_picture(&mut self, _context: u32, buffers: &[u32]) -> Result<(), VaError> {
        self.with(|s| {
            s.rendered.push(buffers.to_vec());
            s.calls.push(VaCall::Render {
                buffers: buffers.to_vec(),
            });
        });
        Ok(())
    }

    fn end_picture(&mut self, _context: u32) -> Result<(), VaError> {
        self.with(|s| s.calls.push(VaCall::End));
        Ok(())
    }

    fn sync_surface(&mut self, surface: u32) -> Result<(), VaError> {
        self.with(|s| s.calls.push(VaCall::Sync { surface }));
        Ok(())
    }

    fn read_coded_buffer(&mut self, buffer: u32) -> Result<Vec<u8>, VaError> {
        Ok(self.with(|s| {
            s.calls.push(VaCall::ReadCoded { buffer });
            s.coded_payload.clone()
        }))
    }

    fn upload_nv12(&mut self, surface: u32, _w: u32, _h: u32, nv12: &[u8]) -> Result<(), VaError> {
        self.with(|s| {
            s.calls.push(VaCall::Upload {
                surface,
                len: nv12.len(),
            })
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use linux_quest_capture::DmaBufPlane;

    type Shared = Arc<Mutex<MockVaState>>;

    fn cfg(codec: VideoCodec, refresh: u32) -> EncoderConfig {
        EncoderConfig {
            codec,
            width: 1280,
            height: 720,
            fps: 60,
            bitrate_kbps: 20_000,
            intra_refresh_period: refresh,
            max_chunk_size: 1200,
        }
    }

    fn mock(state: MockVaState) -> (Shared, Box<dyn VaLayer>) {
        let layer = MockVaLayer::new(state);
        let shared = layer.state.clone();
        (shared, Box::new(layer))
    }

    fn bgra(w: u32, h: u32, pts: u64, px: [u8; 4]) -> RawFrame {
        let data: Vec<u8> = px
            .iter()
            .copied()
            .cycle()
            .take((w * h * 4) as usize)
            .collect();
        RawFrame {
            display_id: 0,
            width: w,
            height: h,
            stride: w * 4,
            format: PixelFormat::Bgra8,
            pts_us: pts,
            dma_buf: None,
            data: Bytes::from(data),
        }
    }

    fn dmabuf_nv12(w: u32, h: u32, pts: u64, data: Vec<u8>) -> RawFrame {
        RawFrame {
            display_id: 0,
            width: w,
            height: h,
            stride: w,
            format: PixelFormat::Nv12,
            pts_us: pts,
            dma_buf: Some(vec![
                DmaBufPlane::create_test_memfd(w, 0, 0),
                DmaBufPlane::create_test_memfd(w, 0, 0),
            ]),
            data: Bytes::from(data),
        }
    }

    fn render_kinds(st: &MockVaState) -> Vec<c_int> {
        st.rendered
            .last()
            .expect("a picture was rendered")
            .iter()
            .map(|id| st.params[id].0)
            .collect()
    }

    fn last_param(st: &MockVaState, kind: c_int) -> Vec<u8> {
        let ids = st.rendered.last().expect("a picture was rendered");
        let id = ids
            .iter()
            .find(|id| st.params[id].0 == kind)
            .expect("buffer kind rendered");
        st.params[id].1.clone()
    }

    fn pic_decoded_id(pic: &[u8]) -> u32 {
        u32::from_le_bytes(pic[0..4].try_into().unwrap())
    }
    fn pic_ref0_id(pic: &[u8]) -> u32 {
        u32::from_le_bytes(pic[12..16].try_into().unwrap())
    }
    fn pic_nal(pic: &[u8]) -> u8 {
        pic[pic.len() - 5]
    }
    fn pic_fields(pic: &[u8]) -> u32 {
        u32::from_le_bytes(pic[pic.len() - 4..].try_into().unwrap())
    }

    #[test]
    fn selection_prefers_lp_and_is_cbr() {
        let (_s, mut layer) = mock(MockVaState::hevc_lp());
        let report = probe_with(layer.as_mut(), VideoCodec::Hevc).expect("probe");
        assert_eq!(report.vendor, "mock-vendor");
        assert_eq!(report.selection.profile, VA_PROFILE_HEVC_MAIN);
        assert_eq!(report.selection.entrypoint, VA_ENTRYPOINT_ENC_SLICE_LP);
    }

    #[test]
    fn selection_uses_full_slice_when_lp_missing() {
        let mut st = MockVaState::hevc_lp();
        st.entrypoints
            .insert(VA_PROFILE_HEVC_MAIN, vec![VA_ENTRYPOINT_ENC_SLICE]);
        st.set_attribs(VA_PROFILE_HEVC_MAIN, VA_ENTRYPOINT_ENC_SLICE);
        let (_s, mut layer) = mock(st);
        let sel = select_encode(layer.as_mut(), VideoCodec::Hevc).expect("select");
        assert_eq!(sel.entrypoint, VA_ENTRYPOINT_ENC_SLICE);
    }

    #[test]
    fn selection_rejects_driver_without_cbr() {
        let mut st = MockVaState::hevc_lp();
        st.attribs.insert(
            (
                VA_PROFILE_HEVC_MAIN,
                VA_ENTRYPOINT_ENC_SLICE_LP,
                VA_CONFIG_ATTRIB_RATE_CONTROL,
            ),
            0x1, // CQP only
        );
        let (_s, mut layer) = mock(st);
        let err = select_encode(layer.as_mut(), VideoCodec::Hevc).unwrap_err();
        assert!(err.to_string().contains("constant-bitrate"), "{err}");
    }

    #[test]
    fn selection_rejects_missing_profile_and_entrypoint() {
        let mut st = MockVaState::hevc_lp();
        st.profiles.clear();
        let (_s, mut layer) = mock(st);
        assert!(matches!(
            select_encode(layer.as_mut(), VideoCodec::Hevc),
            Err(VaError::Unsupported(_))
        ));

        let mut st = MockVaState::hevc_lp();
        st.entrypoints
            .insert(VA_PROFILE_AV1_PROFILE0, vec![VA_ENTRYPOINT_ENC_SLICE]);
        st.profiles.push(VA_PROFILE_AV1_PROFILE0);
        let (_s, mut layer) = mock(st);
        let err = select_encode(layer.as_mut(), VideoCodec::Av1).unwrap_err();
        assert!(
            err.to_string().contains("no usable encode entrypoint"),
            "{err}"
        );
    }

    #[test]
    fn h264_is_not_implemented() {
        let (_s, mut layer) = mock(MockVaState::hevc_lp());
        assert!(matches!(
            select_encode(layer.as_mut(), VideoCodec::H264),
            Err(VaError::Unsupported(_))
        ));
    }

    #[tokio::test]
    async fn init_reports_open_failure_reason() {
        let mut st = MockVaState::hevc_lp();
        st.open_error = Some("no DRM render node under /dev/dri".into());
        let (_s, layer) = mock(st);
        let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 0), layer);
        let err = enc.init(cfg(VideoCodec::Hevc, 0)).await.unwrap_err();
        match err {
            EncoderError::InitFailed(msg) => assert!(msg.contains("no DRM render node"), "{msg}"),
            other => panic!("unexpected error {other:?}"),
        }
    }

    #[tokio::test]
    async fn init_rejects_av1_and_odd_dimensions() {
        let (_s, layer) = mock(MockVaState::hevc_lp());
        let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Av1, 0), layer);
        assert!(matches!(
            enc.init(cfg(VideoCodec::Av1, 0)).await,
            Err(EncoderError::UnsupportedCodec(VideoCodec::Av1))
        ));

        let (_s, layer) = mock(MockVaState::hevc_lp());
        let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 0), layer);
        let mut odd = cfg(VideoCodec::Hevc, 0);
        odd.width = 1281;
        assert!(matches!(
            enc.init(odd).await,
            Err(EncoderError::InitFailed(_))
        ));
    }

    #[tokio::test]
    async fn encode_before_init_is_an_error() {
        let (_s, layer) = mock(MockVaState::hevc_lp());
        let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 0), layer);
        let frame = bgra(64, 64, 1, [0, 0, 0, 255]);
        assert!(matches!(
            enc.encode(&frame).await,
            Err(EncoderError::EncodeFailed(_))
        ));
    }

    #[tokio::test]
    async fn idr_frame_submits_sequence_rc_and_picture_buffers() {
        let (st, layer) = mock(MockVaState::hevc_lp());
        let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 0), layer);
        enc.init(cfg(VideoCodec::Hevc, 0)).await.unwrap();
        let out = enc
            .encode(&bgra(1280, 720, 1000, [255, 255, 255, 255]))
            .await
            .unwrap();

        assert_eq!(out.picture_type, 3);
        assert!(out.is_keyframe && !out.is_intra_refresh);
        assert_eq!(out.frame_id, 1);
        assert_eq!(out.chunks.len(), 3, "3000 bytes at 1200 per chunk");
        assert_eq!(out.chunks[0].meta.total_chunks, 3);
        assert_eq!(out.chunks[2].payload.len(), 600);

        let st = st.lock().unwrap();
        assert_eq!(
            render_kinds(&st),
            vec![
                VA_BUF_ENC_SEQ,
                VA_BUF_ENC_MISC,
                VA_BUF_ENC_MISC,
                VA_BUF_ENC_MISC,
                VA_BUF_ENC_PIC,
                VA_BUF_ENC_SLICE
            ]
        );
        let rc = last_param(&st, VA_BUF_ENC_MISC);
        assert_eq!(
            u32::from_le_bytes(rc[0..4].try_into().unwrap()),
            VA_ENC_MISC_RATE_CONTROL
        );
        assert_eq!(u32::from_le_bytes(rc[4..8].try_into().unwrap()), 20_000_000);
        let pic = last_param(&st, VA_BUF_ENC_PIC);
        assert_eq!(pic_fields(&pic) & 1, 1, "idr_pic_flag");
        assert_eq!(pic_nal(&pic), HEVC_NAL_IDR_W_RADL);
        // CPU path: the BGRA frame was converted and uploaded before encoding.
        assert!(st
            .calls
            .iter()
            .any(|c| matches!(c, VaCall::Upload { len, .. } if *len == 1280 * 720 * 3 / 2)));
    }

    #[tokio::test]
    async fn inter_frame_references_previous_reconstruction() {
        let (st, layer) = mock(MockVaState::hevc_lp());
        let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 0), layer);
        enc.init(cfg(VideoCodec::Hevc, 0)).await.unwrap();
        let f = bgra(64, 64, 1, [1, 2, 3, 255]);
        enc.encode(&f).await.unwrap();
        let first = pic_decoded_id(&last_param(&st.lock().unwrap(), VA_BUF_ENC_PIC));

        let out = enc
            .encode(&bgra(64, 64, 2000, [1, 2, 3, 255]))
            .await
            .unwrap();
        assert_eq!(out.picture_type, 0);
        assert!(!out.is_keyframe);
        let st = st.lock().unwrap();
        let pic = last_param(&st, VA_BUF_ENC_PIC);
        let second = pic_decoded_id(&pic);
        assert_ne!(first, second, "recon surfaces must not alias the reference");
        assert_eq!(pic_ref0_id(&pic), first);
        assert_eq!(pic_fields(&pic) & 1, 0, "not an IDR");
        let slice = last_param(&st, VA_BUF_ENC_SLICE);
        assert_eq!(slice[8], 1, "P slice");
        assert!(
            !render_kinds(&st).contains(&VA_BUF_ENC_SEQ),
            "sequence parameters are sent only at IDR"
        );
    }

    #[tokio::test]
    async fn request_keyframe_forces_idr() {
        let (_st, layer) = mock(MockVaState::hevc_lp());
        let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 0), layer);
        enc.init(cfg(VideoCodec::Hevc, 0)).await.unwrap();
        enc.encode(&bgra(64, 64, 1, [0; 4])).await.unwrap();
        enc.encode(&bgra(64, 64, 2, [0; 4])).await.unwrap();
        enc.request_keyframe();
        let out = enc.encode(&bgra(64, 64, 3, [0; 4])).await.unwrap();
        assert_eq!(out.picture_type, 3);
        assert!(out.is_keyframe);
    }

    #[tokio::test]
    async fn periodic_intra_refresh_every_period() {
        let (_st, layer) = mock(MockVaState::hevc_lp());
        let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 3), layer);
        enc.init(cfg(VideoCodec::Hevc, 3)).await.unwrap();
        let mut types = Vec::new();
        for i in 1..=6u64 {
            let out = enc.encode(&bgra(64, 64, i, [0; 4])).await.unwrap();
            types.push((out.picture_type, out.is_intra_refresh, out.is_keyframe));
        }
        assert_eq!(types[0], (3, false, true), "IDR first");
        assert_eq!(types[1], (0, false, false));
        assert_eq!(
            types[2],
            (6, true, false),
            "frame 3 is a periodic intra frame"
        );
        assert_eq!(types[5], (6, true, false), "frame 6 too");
    }

    #[tokio::test]
    async fn rpi_invalidation_falls_back_to_older_reference() {
        let (st, layer) = mock(MockVaState::hevc_lp());
        let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 0), layer);
        enc.init(cfg(VideoCodec::Hevc, 0)).await.unwrap();
        enc.encode(&bgra(64, 64, 1000, [0; 4])).await.unwrap();
        let a = pic_decoded_id(&last_param(&st.lock().unwrap(), VA_BUF_ENC_PIC));
        enc.encode(&bgra(64, 64, 2000, [0; 4])).await.unwrap();
        let b = pic_decoded_id(&last_param(&st.lock().unwrap(), VA_BUF_ENC_PIC));
        enc.encode(&bgra(64, 64, 3000, [0; 4])).await.unwrap();
        assert_eq!(
            pic_ref0_id(&last_param(&st.lock().unwrap(), VA_BUF_ENC_PIC)),
            b
        );

        enc.invalidate_reference_picture(3000);
        assert_eq!(enc.last_rpi_frame(), Some(3000));
        let out = enc.encode(&bgra(64, 64, 4000, [0; 4])).await.unwrap();
        assert_eq!(
            out.picture_type, 0,
            "still inter: an older reference is valid"
        );
        let pic = last_param(&st.lock().unwrap(), VA_BUF_ENC_PIC);
        assert_eq!(
            pic_ref0_id(&pic),
            b,
            "newest valid reference is the 2000us frame"
        );
        assert_ne!(pic_ref0_id(&pic), a);
    }

    #[tokio::test]
    async fn rpi_with_no_valid_reference_codes_recovery_iframe() {
        let (st, layer) = mock(MockVaState::hevc_lp());
        let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 0), layer);
        enc.init(cfg(VideoCodec::Hevc, 0)).await.unwrap();
        enc.encode(&bgra(64, 64, 1000, [0; 4])).await.unwrap();
        enc.invalidate_reference_picture(1000);
        let out = enc.encode(&bgra(64, 64, 2000, [0; 4])).await.unwrap();
        assert_eq!(out.picture_type, 2, "non-IDR I-frame recovery point");
        assert!(out.is_keyframe);
        let pic = last_param(&st.lock().unwrap(), VA_BUF_ENC_PIC);
        assert_eq!(pic_fields(&pic) & 1, 0);
        assert_eq!(pic_nal(&pic), HEVC_NAL_TRAIL_R);
    }

    #[tokio::test]
    async fn nv12_dmabuf_imports_zero_copy() {
        let (st, layer) = mock(MockVaState::hevc_lp());
        let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 0), layer);
        enc.init(cfg(VideoCodec::Hevc, 0)).await.unwrap();
        let frame = dmabuf_nv12(64, 64, 1, Vec::new());
        enc.encode(&frame).await.unwrap();
        let st = st.lock().unwrap();
        let import = st
            .calls
            .iter()
            .find_map(|c| match c {
                VaCall::Import { surface } => Some(*surface),
                _ => None,
            })
            .expect("PRIME import happened");
        assert!(!st.calls.iter().any(|c| matches!(c, VaCall::Upload { .. })));
        assert!(st.calls.contains(&VaCall::Begin { target: import }));
        assert!(st.calls.contains(&VaCall::DestroySurfaces { count: 1 }));
    }

    #[tokio::test]
    async fn dmabuf_import_failure_falls_back_to_cpu_copy() {
        let mut state = MockVaState::hevc_lp();
        state.import_error = Some("driver refused PRIME import".into());
        let (st, layer) = mock(state);
        let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 0), layer);
        enc.init(cfg(VideoCodec::Hevc, 0)).await.unwrap();
        let frame = dmabuf_nv12(64, 64, 1, vec![16; 64 * 64 * 3 / 2]);
        enc.encode(&frame).await.expect("CPU fallback encodes");
        assert!(st
            .lock()
            .unwrap()
            .calls
            .iter()
            .any(|c| matches!(c, VaCall::Upload { .. })));
    }

    #[tokio::test]
    async fn dmabuf_import_failure_without_cpu_copy_is_error() {
        let mut state = MockVaState::hevc_lp();
        state.import_error = Some("refused".into());
        let (_st, layer) = mock(state);
        let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 0), layer);
        enc.init(cfg(VideoCodec::Hevc, 0)).await.unwrap();
        let err = enc
            .encode(&dmabuf_nv12(64, 64, 1, Vec::new()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no CPU copy"), "{err}");
    }

    #[tokio::test]
    async fn rgb_dmabuf_without_cpu_copy_explains_missing_vpp() {
        let (_st, layer) = mock(MockVaState::hevc_lp());
        let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 0), layer);
        enc.init(cfg(VideoCodec::Hevc, 0)).await.unwrap();
        let mut frame = bgra(64, 64, 1, [0; 4]);
        frame.data = Bytes::new();
        frame.dma_buf = Some(vec![DmaBufPlane::create_test_memfd(256, 0, 0)]);
        let err = enc.encode(&frame).await.unwrap_err();
        assert!(err.to_string().contains("VPP colour conversion"), "{err}");
    }

    #[test]
    fn bgra_white_and_black_convert_to_limited_range() {
        let white = nv12_from_frame(&bgra(4, 4, 0, [255, 255, 255, 255])).unwrap();
        assert!(white[..16].iter().all(|&y| y == 235));
        assert!(white[16..].iter().all(|&c| c == 128));
        let black = nv12_from_frame(&bgra(4, 4, 0, [0, 0, 0, 255])).unwrap();
        assert!(black[..16].iter().all(|&y| y == 16));
        assert!(black[16..].iter().all(|&c| c == 128));
    }

    #[test]
    fn red_has_high_v_and_low_u() {
        // BGRA byte order: B, G, R, A.
        let red = nv12_from_frame(&bgra(2, 2, 0, [0, 0, 255, 255])).unwrap();
        let (u, v) = (red[4], red[5]);
        assert!(v > 200, "V {v} should be high for red");
        assert!(u < 128, "U {u} should be low for red");
    }

    #[test]
    fn nv12_frame_copies_planes_respecting_stride() {
        let (w, h, stride) = (4usize, 2usize, 8usize);
        let mut data = vec![0u8; stride * h * 3 / 2];
        for row in 0..h {
            for x in 0..w {
                data[row * stride + x] = 1;
            }
        }
        for row in 0..h / 2 {
            for x in 0..w {
                data[stride * h + row * stride + x] = 2;
            }
        }
        let frame = RawFrame {
            display_id: 0,
            width: w as u32,
            height: h as u32,
            stride: stride as u32,
            format: PixelFormat::Nv12,
            pts_us: 0,
            dma_buf: None,
            data: Bytes::from(data),
        };
        let out = nv12_from_frame(&frame).unwrap();
        assert_eq!(out.len(), w * h * 3 / 2);
        assert!(out[..w * h].iter().all(|&y| y == 1));
        assert!(out[w * h..].iter().all(|&c| c == 2));
    }

    #[test]
    fn prime_descriptor_requires_two_planes_of_nv12() {
        let good = dmabuf_nv12(64, 64, 0, Vec::new());
        assert!(prime_nv12_descriptor(&good).is_some());
        let mut one_plane = dmabuf_nv12(64, 64, 0, Vec::new());
        one_plane.dma_buf.as_mut().unwrap().truncate(1);
        assert!(prime_nv12_descriptor(&one_plane).is_none());
        let mut bgra_dma = bgra(64, 64, 0, [0; 4]);
        bgra_dma.dma_buf = Some(vec![
            DmaBufPlane::create_test_memfd(256, 0, 0),
            DmaBufPlane::create_test_memfd(256, 0, 0),
        ]);
        assert!(prime_nv12_descriptor(&bgra_dma).is_none());
    }

    #[test]
    fn hevc_sequence_and_rc_buffers_serialise_expected_fields() {
        let seq = HevcSeqParams {
            intra_period: 30,
            intra_idr_period: u32::MAX,
            ip_period: 1,
            bits_per_second: 20_000_000,
            width: 1280,
            height: 720,
        }
        .to_bytes();
        assert_eq!(seq[0], HEVC_MAIN_PROFILE_IDC);
        assert_eq!(u32::from_le_bytes(seq[4..8].try_into().unwrap()), 30);
        assert_eq!(
            u32::from_le_bytes(seq[16..20].try_into().unwrap()),
            20_000_000
        );
        assert_eq!(u16::from_le_bytes(seq[20..22].try_into().unwrap()), 1280);
        assert_eq!(u16::from_le_bytes(seq[22..24].try_into().unwrap()), 720);

        let hrd = hrd_misc(20_000, 60);
        assert_eq!(
            u32::from_le_bytes(hrd[0..4].try_into().unwrap()),
            VA_ENC_MISC_HRD
        );
        assert_eq!(u32::from_le_bytes(hrd[4..8].try_into().unwrap()), 333_333);
    }

    #[test]
    fn dpb_caps_entries_and_invalidates_by_pts() {
        let mut dpb = Dpb::default();
        for i in 0..(DPB_CAPACITY as u64 + 2) {
            dpb.push(DpbEntry {
                surface: i as u32 + 1,
                pts_us: i,
                poc: i as i32,
                valid: true,
            });
        }
        assert_eq!(dpb.len(), DPB_CAPACITY);
        assert!(!dpb.holds(1), "oldest evicted");
        assert!(dpb.invalidate(5));
        assert!(!dpb.invalidate(999));
        let refs = dpb.valid_refs(LOW_LATENCY_REFS);
        assert_eq!(
            refs[0].pts_us, 4,
            "entries 2..=5 kept; 5 invalidated; newest valid is 4"
        );
        dpb.clear();
        assert!(dpb.is_empty());
    }

    #[test]
    fn libva_missing_library_is_a_reason_not_a_panic() {
        let mut layer = LibVa::with_library_names(
            &["libdefinitely-not-installed-va.so.9"],
            &["libdefinitely-not-installed-va-drm.so.9"],
        );
        let err = layer.open().unwrap_err();
        assert!(matches!(err, VaError::Unavailable(_)), "{err:?}");
    }

    #[test]
    fn real_probe_never_panics() {
        // Passes on machines with and without a VA-API driver; only the reason is checked.
        match probe(VideoCodec::Hevc) {
            Ok(report) => assert!(!report.vendor.is_empty()),
            Err(e) => assert!(!e.to_string().is_empty()),
        }
    }
}

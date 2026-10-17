use async_trait::async_trait;
use bytes::Bytes;
use libloading::{Library, Symbol};
use linux_quest_capture::{PixelFormat, RawFrame};
use linux_quest_protocol::{VideoChunk, VideoChunkMeta, VideoCodec};
use std::ffi::c_void;
use std::time::Instant;
use tracing::info;

use crate::{EncodedFrame, EncoderConfig, EncoderError, VideoEncoder};

/// Standard NVIDIA NVENC GUID definition matching nvEncodeAPI.h.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NvEncGuid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

pub const NV_ENC_CODEC_AV1_GUID: NvEncGuid = NvEncGuid {
    data1: 0x0a352289,
    data2: 0x0aa7,
    data3: 0x4759,
    data4: [0x86, 0x2d, 0x5d, 0x15, 0xcd, 0x16, 0xd2, 0x54],
};

pub const NV_ENC_CODEC_HEVC_GUID: NvEncGuid = NvEncGuid {
    data1: 0x790cdc88,
    data2: 0x4522,
    data3: 0x4d7b,
    data4: [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03],
};

pub const NV_ENC_CODEC_H264_GUID: NvEncGuid = NvEncGuid {
    data1: 0x6bc82762,
    data2: 0x4e63,
    data3: 0x4ca4,
    data4: [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
};

pub const NV_ENC_PRESET_P1_GUID: NvEncGuid = NvEncGuid {
    data1: 0xfc0a8d3e,
    data2: 0x45f8,
    data3: 0x4cf8,
    data4: [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf],
};

pub const NV_ENC_SUCCESS: u32 = 0;
pub const NVENCAPI_MAJOR_VERSION: u32 = 12;
pub const NVENCAPI_MINOR_VERSION: u32 = 1;
pub const NVENCAPI_VERSION: u32 = NVENCAPI_MAJOR_VERSION | (NVENCAPI_MINOR_VERSION << 24);

pub const fn nv_enc_struct_version(ver: u32) -> u32 {
    NVENCAPI_VERSION | (ver << 16) | (0x7 << 28)
}

pub const NV_ENC_DEVICE_TYPE_CUDA: u32 = 1;
pub const NV_ENC_BUFFER_FORMAT_NV12: u32 = 0x00000001;
pub const NV_ENC_BUFFER_FORMAT_ARGB: u32 = 0x01000000;
pub const NV_ENC_BUFFER_FORMAT_ABGR: u32 = 0x10000000;
pub const NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY: u32 = 3;
pub const NV_ENC_PARAMS_RC_CBR: u32 = 2;
pub const NVENC_INFINITE_GOPLENGTH: u32 = 0xffffffff;
pub const NV_ENC_PIC_STRUCT_FRAME: u32 = 1;
pub const NV_ENC_PIC_FLAG_FORCEINTRA: u32 = 0x00000001;
pub const NV_ENC_PIC_FLAG_FORCEIDR: u32 = 0x00000002;
pub const NV_ENC_PIC_FLAG_OUTPUT_SPSPPS: u32 = 0x00000004;

pub const NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER: u32 = nv_enc_struct_version(1);
pub const NV_ENC_INITIALIZE_PARAMS_VER: u32 = nv_enc_struct_version(6) | (1 << 31);
pub const NV_ENC_PRESET_CONFIG_VER: u32 = nv_enc_struct_version(4) | (1 << 31);
pub const NV_ENC_CONFIG_VER: u32 = nv_enc_struct_version(8) | (1 << 31);
pub const NV_ENC_CREATE_INPUT_BUFFER_VER: u32 = nv_enc_struct_version(1);
pub const NV_ENC_CREATE_BITSTREAM_BUFFER_VER: u32 = nv_enc_struct_version(1);
pub const NV_ENC_LOCK_INPUT_BUFFER_VER: u32 = nv_enc_struct_version(1);
pub const NV_ENC_PIC_PARAMS_VER: u32 = nv_enc_struct_version(6) | (1 << 31);
pub const NV_ENC_LOCK_BITSTREAM_VER: u32 = nv_enc_struct_version(1) | (1 << 31);

/// Full NV_ENCODE_API_FUNCTION_LIST layout matching NVIDIA Video Codec SDK 12.1/13 ABI.
/// Total size: exactly 2552 bytes (318 pointers/slots).
#[repr(C)]
pub struct NvEncodeApiFunctionList {
    pub version: u32,
    pub reserved: u32,
    pub nv_enc_open_encode_session: *mut c_void,
    pub nv_enc_get_encode_guid_count: *mut c_void,
    pub nv_enc_get_encode_profile_guid_count: *mut c_void,
    pub nv_enc_get_encode_profile_guids: *mut c_void,
    pub nv_enc_get_encode_guids: *mut c_void,
    pub nv_enc_get_input_format_count: *mut c_void,
    pub nv_enc_get_input_formats: *mut c_void,
    pub nv_enc_get_encode_caps: *mut c_void,
    pub nv_enc_get_encode_preset_count: *mut c_void,
    pub nv_enc_get_encode_preset_guids: *mut c_void,
    pub nv_enc_get_encode_preset_config: *mut c_void,
    pub nv_enc_initialize_encoder: *mut c_void,
    pub nv_enc_create_input_buffer: *mut c_void,
    pub nv_enc_destroy_input_buffer: *mut c_void,
    pub nv_enc_create_bitstream_buffer: *mut c_void,
    pub nv_enc_destroy_bitstream_buffer: *mut c_void,
    pub nv_enc_encode_picture: *mut c_void,
    pub nv_enc_lock_bitstream: *mut c_void,
    pub nv_enc_unlock_bitstream: *mut c_void,
    pub nv_enc_lock_input_buffer: *mut c_void,
    pub nv_enc_unlock_input_buffer: *mut c_void,
    pub nv_enc_get_encode_stats: *mut c_void,
    pub nv_enc_get_sequence_params: *mut c_void,
    pub nv_enc_register_async_event: *mut c_void,
    pub nv_enc_unregister_async_event: *mut c_void,
    pub nv_enc_map_input_resource: *mut c_void,
    pub nv_enc_unmap_input_resource: *mut c_void,
    pub nv_enc_destroy_encoder: *mut c_void,
    pub nv_enc_invalidate_ref_frames: *mut c_void,
    pub nv_enc_open_encode_session_ex: *mut c_void,
    pub nv_enc_register_resource: *mut c_void,
    pub nv_enc_unregister_resource: *mut c_void,
    pub nv_enc_reconfigure_encoder: *mut c_void,
    pub reserved1: *mut c_void,
    pub nv_enc_create_mv_buffer: *mut c_void,
    pub nv_enc_destroy_mv_buffer: *mut c_void,
    pub nv_enc_run_motion_estimation_only: *mut c_void,
    pub nv_enc_get_last_error_string: *mut c_void,
    pub nv_enc_set_io_cuda_streams: *mut c_void,
    pub nv_enc_get_encode_preset_config_ex: *mut c_void,
    pub nv_enc_get_sequence_param_ex: *mut c_void,
    pub nv_enc_restore_encoder_state: *mut c_void,
    pub nv_enc_lookahead_picture: *mut c_void,
    pub reserved2: [*mut c_void; 275],
}

unsafe impl Send for NvEncodeApiFunctionList {}
unsafe impl Sync for NvEncodeApiFunctionList {}

const _: () = assert!(std::mem::size_of::<NvEncodeApiFunctionList>() == 2552);

#[repr(C)]
pub struct NvEncOpenEncodeSessionExParams {
    pub version: u32,
    pub device_type: u32,
    pub device: *mut c_void,
    pub reserved: *mut c_void,
    pub api_version: u32,
    pub reserved1: [u32; 253],
    pub reserved2: [*mut c_void; 64],
}
const _: () = assert!(std::mem::size_of::<NvEncOpenEncodeSessionExParams>() == 1552);

#[repr(C)]
pub struct NvEncRcParams {
    pub version: u32,
    pub rate_control_mode: u32,
    pub const_qp: [u32; 3],
    pub average_bit_rate: u32,
    pub max_bit_rate: u32,
    pub vbv_buffer_size: u32,
    pub vbv_initial_delay: u32,
    pub bitfields: u32,
    pub reserved: [u8; 88],
}
const _: () = assert!(std::mem::size_of::<NvEncRcParams>() == 128);

#[repr(C)]
pub struct NvEncCodecConfig {
    pub raw: [u8; 1792],
}
const _: () = assert!(std::mem::size_of::<NvEncCodecConfig>() == 1792);

#[repr(C)]
pub struct NvEncConfig {
    pub version: u32,
    pub profile_guid: NvEncGuid,
    pub gop_length: u32,
    pub frame_interval_p: i32,
    pub mono_chrome_encoding: u32,
    pub frame_field_mode: u32,
    pub mv_precision: u32,
    pub rc_params: NvEncRcParams,
    pub encode_codec_config: NvEncCodecConfig,
    pub reserved: [u8; 1624],
}
const _: () = assert!(std::mem::size_of::<NvEncConfig>() == 3584);

#[repr(C)]
pub struct NvEncPresetConfig {
    pub version: u32,
    pub reserved_pad: u32,
    pub preset_cfg: NvEncConfig,
    pub reserved: [u8; 1536],
}
const _: () = assert!(std::mem::size_of::<NvEncPresetConfig>() == 5128);

#[repr(C)]
pub struct NvEncInitializeParams {
    pub version: u32,
    pub encode_guid: NvEncGuid,
    pub preset_guid: NvEncGuid,
    pub encode_width: u32,
    pub encode_height: u32,
    pub dar_width: u32,
    pub dar_height: u32,
    pub frame_rate_num: u32,
    pub frame_rate_den: u32,
    pub enable_encode_async: u32,
    pub enable_ptd: u32,
    pub bitfields: u32,
    pub priv_data_size: u32,
    pub priv_data: *mut c_void,
    pub encode_config: *mut NvEncConfig,
    pub max_encode_width: u32,
    pub max_encode_height: u32,
    pub max_me_hint_counts_per_block: [u32; 8],
    pub tuning_info: u32,
    pub buffer_format: u32,
    pub num_state_buffers: u32,
    pub output_stats_level: u32,
    pub reserved: [u8; 1656],
}
const _: () = assert!(std::mem::size_of::<NvEncInitializeParams>() == 1808);

#[repr(C)]
pub struct NvEncCreateInputBuffer {
    pub version: u32,
    pub width: u32,
    pub height: u32,
    pub memory_heap: u32,
    pub buffer_fmt: u32,
    pub reserved: u32,
    pub input_buffer: *mut c_void,
    pub p_sys_mem_buffer: *mut c_void,
    pub reserved_bytes: [u8; 736],
}
const _: () = assert!(std::mem::size_of::<NvEncCreateInputBuffer>() == 776);

#[repr(C)]
pub struct NvEncCreateBitstreamBuffer {
    pub version: u32,
    pub size: u32,
    pub memory_heap: u32,
    pub reserved: u32,
    pub bitstream_buffer: *mut c_void,
    pub bitstream_buffer_ptr: *mut c_void,
    pub reserved_bytes: [u8; 744],
}
const _: () = assert!(std::mem::size_of::<NvEncCreateBitstreamBuffer>() == 776);

#[repr(C)]
pub struct NvEncLockInputBuffer {
    pub version: u32,
    pub do_not_wait: u32,
    pub input_buffer: *mut c_void,
    pub buffer_data_ptr: *mut c_void,
    pub pitch: u32,
    pub reserved: [u8; 1516],
}
const _: () = assert!(std::mem::size_of::<NvEncLockInputBuffer>() == 1544);

#[repr(C)]
pub struct NvEncPicParams {
    pub version: u32,
    pub input_width: u32,
    pub input_height: u32,
    pub input_pitch: u32,
    pub encode_pic_flags: u32,
    pub frame_idx: u32,
    pub input_time_stamp: u64,
    pub input_duration: u64,
    pub input_buffer: *mut c_void,
    pub output_bitstream: *mut c_void,
    pub completion_event: *mut c_void,
    pub buffer_fmt: u32,
    pub picture_struct: u32,
    pub picture_type: u32,
    pub reserved: [u8; 3284],
}
const _: () = assert!(std::mem::size_of::<NvEncPicParams>() == 3360);

#[repr(C)]
pub struct NvEncLockBitstream {
    pub version: u32,
    pub do_not_wait: u32,
    pub output_bitstream: *mut c_void,
    pub slice_offsets: *mut c_void,
    pub frame_idx: u32,
    pub hw_encode_status: u32,
    pub num_slices: u32,
    pub bitstream_size_in_bytes: u32,
    pub output_time_stamp: u64,
    pub output_duration: u64,
    pub bitstream_buffer_ptr: *mut c_void,
    pub picture_type: u32,
    pub picture_struct: u32,
    pub frame_avg_qp: u32,
    pub frame_satd: u32,
    pub reserved: [u8; 1472],
}
const _: () = assert!(std::mem::size_of::<NvEncLockBitstream>() == 1552);

type NvEncodeApiCreateInstanceFn = unsafe extern "C" fn(*mut NvEncodeApiFunctionList) -> u32;
type NvEncodeApiGetMaxSupportedVersionFn = unsafe extern "C" fn(*mut u32) -> u32;

type CuInitFn = unsafe extern "C" fn(u32) -> i32;
type CuDeviceGetFn = unsafe extern "C" fn(*mut i32, i32) -> i32;
type CuCtxCreateFn = unsafe extern "C" fn(*mut *mut c_void, u32, i32) -> i32;
type CuCtxDestroyFn = unsafe extern "C" fn(*mut c_void) -> i32;
type CuCtxSetCurrentFn = unsafe extern "C" fn(*mut c_void) -> i32;

/// Ultra-low latency hardware encoder leveraging NVIDIA NVENC (RTX 40/50 series dual NVENC).
pub struct NvencEncoder {
    config: EncoderConfig,
    frame_counter: u64,
    force_keyframe: bool,
    last_rpi_frame: Option<u64>,
    initialized: bool,
    _cuda_lib: Option<Library>,
    _nvenc_lib: Option<Library>,
    fn_list: Option<Box<NvEncodeApiFunctionList>>,
    cuda_ctx: *mut c_void,
    encoder: *mut c_void,
    input_buffer: *mut c_void,
    input_pitch: u32,
    bitstream_buffer: *mut c_void,
}

unsafe impl Send for NvencEncoder {}
unsafe impl Sync for NvencEncoder {}

impl NvencEncoder {
    pub fn new(config: EncoderConfig) -> Self {
        Self {
            config,
            frame_counter: 0,
            force_keyframe: true,
            last_rpi_frame: None,
            initialized: false,
            _cuda_lib: None,
            _nvenc_lib: None,
            fn_list: None,
            cuda_ctx: std::ptr::null_mut(),
            encoder: std::ptr::null_mut(),
            input_buffer: std::ptr::null_mut(),
            input_pitch: 0,
            bitstream_buffer: std::ptr::null_mut(),
        }
    }

    /// Releases all GPU encoder and CUDA resources cleanly.
    pub fn cleanup(&mut self) {
        if let Some(fn_list) = &self.fn_list {
            unsafe {
                if !self.input_buffer.is_null() && !fn_list.nv_enc_destroy_input_buffer.is_null() {
                    let destroy_in: unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32 =
                        std::mem::transmute(fn_list.nv_enc_destroy_input_buffer);
                    let _ = destroy_in(self.encoder, self.input_buffer);
                    self.input_buffer = std::ptr::null_mut();
                }
                if !self.bitstream_buffer.is_null()
                    && !fn_list.nv_enc_destroy_bitstream_buffer.is_null()
                {
                    let destroy_bs: unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32 =
                        std::mem::transmute(fn_list.nv_enc_destroy_bitstream_buffer);
                    let _ = destroy_bs(self.encoder, self.bitstream_buffer);
                    self.bitstream_buffer = std::ptr::null_mut();
                }
                if !self.encoder.is_null() && !fn_list.nv_enc_destroy_encoder.is_null() {
                    let destroy_enc: unsafe extern "C" fn(*mut c_void) -> u32 =
                        std::mem::transmute(fn_list.nv_enc_destroy_encoder);
                    let _ = destroy_enc(self.encoder);
                    self.encoder = std::ptr::null_mut();
                }
            }
        }
        if !self.cuda_ctx.is_null() {
            if let Some(cuda_lib) = &self._cuda_lib {
                unsafe {
                    if let Ok(cu_ctx_destroy) = cuda_lib
                        .get::<CuCtxDestroyFn>(b"cuCtxDestroy_v2\0")
                        .or_else(|_| cuda_lib.get::<CuCtxDestroyFn>(b"cuCtxDestroy\0"))
                    {
                        let _ = cu_ctx_destroy(self.cuda_ctx);
                    }
                }
            }
            self.cuda_ctx = std::ptr::null_mut();
        }
        self.initialized = false;
    }

    /// Checks whether the NVENC runtime is loadable and new enough, reporting why not.
    pub fn probe() -> Result<(), String> {
        unsafe {
            let lib = Library::new("libnvidia-encode.so.1")
                .or_else(|_| Library::new("libnvidia-encode.so"))
                .map_err(|e| format!("libnvidia-encode not loadable: {e}"))?;
            let get_max_ver = lib
                .get::<NvEncodeApiGetMaxSupportedVersionFn>(b"NvEncodeAPIGetMaxSupportedVersion\0")
                .map_err(|e| format!("NvEncodeAPIGetMaxSupportedVersion missing: {e}"))?;
            let mut max_version = 0u32;
            let status = get_max_ver(&mut max_version);
            if status != NV_ENC_SUCCESS {
                return Err(format!(
                    "NvEncodeAPIGetMaxSupportedVersion failed: {status}"
                ));
            }
            if max_version < ((12 << 4) | 1) {
                return Err(format!(
                    "driver NVENC API {max_version:#x} is older than required 12.1"
                ));
            }
            Ok(())
        }
    }

    /// Checks if NVIDIA NVENC library is dynamically loadable and can report supported version.
    pub fn is_available() -> bool {
        Self::probe().is_ok()
    }

    pub fn codec_guid(&self) -> NvEncGuid {
        match self.config.codec {
            VideoCodec::Av1 => NV_ENC_CODEC_AV1_GUID,
            VideoCodec::Hevc => NV_ENC_CODEC_HEVC_GUID,
            VideoCodec::H264 => NV_ENC_CODEC_H264_GUID,
        }
    }
}

#[async_trait]
impl VideoEncoder for NvencEncoder {
    async fn init(&mut self, config: EncoderConfig) -> Result<(), EncoderError> {
        self.cleanup();

        let cuda_lib = unsafe {
            Library::new("libcuda.so.1")
                .or_else(|_| Library::new("libcuda.so"))
                .map_err(|e| EncoderError::InitFailed(format!("Failed to load libcuda: {e}")))?
        };

        let mut cuda_ctx = std::ptr::null_mut();
        unsafe {
            let cu_init: Symbol<CuInitFn> = cuda_lib
                .get(b"cuInit\0")
                .map_err(|e| EncoderError::InitFailed(format!("cuInit lookup failed: {e}")))?;
            let cu_device_get: Symbol<CuDeviceGetFn> = cuda_lib
                .get(b"cuDeviceGet\0")
                .map_err(|e| EncoderError::InitFailed(format!("cuDeviceGet lookup failed: {e}")))?;
            let cu_ctx_create: Symbol<CuCtxCreateFn> = cuda_lib
                .get(b"cuCtxCreate_v2\0")
                .or_else(|_| cuda_lib.get(b"cuCtxCreate\0"))
                .map_err(|e| EncoderError::InitFailed(format!("cuCtxCreate lookup failed: {e}")))?;

            let res = cu_init(0);
            if res != 0 {
                return Err(EncoderError::InitFailed(format!(
                    "cuInit failed with code {res}"
                )));
            }

            let mut dev = 0i32;
            let res = cu_device_get(&mut dev, 0);
            if res != 0 {
                return Err(EncoderError::InitFailed(format!(
                    "cuDeviceGet failed with code {res}"
                )));
            }

            let res = cu_ctx_create(&mut cuda_ctx, 0, dev);
            if res != 0 || cuda_ctx.is_null() {
                return Err(EncoderError::InitFailed(format!(
                    "cuCtxCreate failed with code {res}"
                )));
            }
        }
        self._cuda_lib = Some(cuda_lib);
        self.cuda_ctx = cuda_ctx;

        let nvenc_lib = match unsafe {
            Library::new("libnvidia-encode.so.1").or_else(|_| Library::new("libnvidia-encode.so"))
        } {
            Ok(lib) => lib,
            Err(e) => {
                self.cleanup();
                return Err(EncoderError::InitFailed(format!(
                    "Failed to load libnvidia-encode: {e}"
                )));
            }
        };

        let mut fn_list = Box::new(unsafe { std::mem::zeroed::<NvEncodeApiFunctionList>() });
        let mut encoder = std::ptr::null_mut();
        let input_buffer;
        let bitstream_buffer;
        let input_pitch;

        unsafe {
            let get_max_ver: Symbol<NvEncodeApiGetMaxSupportedVersionFn> =
                match nvenc_lib.get(b"NvEncodeAPIGetMaxSupportedVersion\0") {
                    Ok(s) => s,
                    Err(e) => {
                        self.cleanup();
                        return Err(EncoderError::InitFailed(format!(
                            "NvEncodeAPIGetMaxSupportedVersion lookup: {e}"
                        )));
                    }
                };
            let mut max_version = 0u32;
            let status = get_max_ver(&mut max_version);
            if status != NV_ENC_SUCCESS || max_version < ((12 << 4) | 1) {
                self.cleanup();
                return Err(EncoderError::InitFailed(format!(
                    "Unsupported NVENC version: {max_version:#x}"
                )));
            }

            let create_instance: Symbol<NvEncodeApiCreateInstanceFn> =
                match nvenc_lib.get(b"NvEncodeAPICreateInstance\0") {
                    Ok(s) => s,
                    Err(e) => {
                        self.cleanup();
                        return Err(EncoderError::InitFailed(format!(
                            "Symbol lookup failed: {e}"
                        )));
                    }
                };

            fn_list.version = nv_enc_struct_version(2);
            let status = create_instance(fn_list.as_mut());
            if status != NV_ENC_SUCCESS {
                self.cleanup();
                return Err(EncoderError::InitFailed(format!(
                    "NvEncodeAPICreateInstance failed with code {status}"
                )));
            }

            // Open Encode Session
            let mut open_params: NvEncOpenEncodeSessionExParams = std::mem::zeroed();
            open_params.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
            open_params.device_type = NV_ENC_DEVICE_TYPE_CUDA;
            open_params.device = self.cuda_ctx;
            open_params.api_version = NVENCAPI_VERSION;

            let open_session_ex: unsafe extern "C" fn(
                *mut NvEncOpenEncodeSessionExParams,
                *mut *mut c_void,
            ) -> u32 = std::mem::transmute(fn_list.nv_enc_open_encode_session_ex);
            let status = open_session_ex(&mut open_params, &mut encoder);
            if status != NV_ENC_SUCCESS || encoder.is_null() {
                self.cleanup();
                return Err(EncoderError::InitFailed(format!(
                    "NvEncOpenEncodeSessionEx failed with code {status}"
                )));
            }

            self._nvenc_lib = Some(nvenc_lib);
            self.fn_list = Some(fn_list);
            self.encoder = encoder;

            let fn_list = self.fn_list.as_ref().unwrap();

            // Query Preset Config Ex (P1 Preset with Ultra Low Latency tuning)
            let codec_guid = match config.codec {
                VideoCodec::Av1 => NV_ENC_CODEC_AV1_GUID,
                VideoCodec::Hevc => NV_ENC_CODEC_HEVC_GUID,
                VideoCodec::H264 => NV_ENC_CODEC_H264_GUID,
            };

            let mut preset_cfg: NvEncPresetConfig = std::mem::zeroed();
            preset_cfg.version = NV_ENC_PRESET_CONFIG_VER;
            preset_cfg.preset_cfg.version = NV_ENC_CONFIG_VER;

            let get_preset_config_ex: unsafe extern "C" fn(
                *mut c_void,
                NvEncGuid,
                NvEncGuid,
                u32,
                *mut NvEncPresetConfig,
            ) -> u32 = std::mem::transmute(fn_list.nv_enc_get_encode_preset_config_ex);
            let status = get_preset_config_ex(
                self.encoder,
                codec_guid,
                NV_ENC_PRESET_P1_GUID,
                NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                &mut preset_cfg,
            );
            if status != NV_ENC_SUCCESS {
                self.cleanup();
                return Err(EncoderError::InitFailed(format!(
                    "NvEncGetEncodePresetConfigEx failed with code {status}"
                )));
            }

            // Override low-latency rate control: CBR, 1-frame VBV, zero reorder delay (SDK 12.1 bit 9), infinite GOP, intra-refresh
            preset_cfg.preset_cfg.gop_length = NVENC_INFINITE_GOPLENGTH;
            preset_cfg.preset_cfg.frame_interval_p = 1;
            preset_cfg.preset_cfg.rc_params.rate_control_mode = NV_ENC_PARAMS_RC_CBR;
            let average_bit_rate = config.bitrate_kbps * 1000;
            preset_cfg.preset_cfg.rc_params.average_bit_rate = average_bit_rate;
            let vbv_buffer_size = average_bit_rate / config.fps.max(1);
            preset_cfg.preset_cfg.rc_params.vbv_buffer_size = vbv_buffer_size;
            preset_cfg.preset_cfg.rc_params.vbv_initial_delay = vbv_buffer_size;
            preset_cfg.preset_cfg.rc_params.bitfields |= 0x200; // zeroReorderDelay = 1 (bit 9 in SDK 12.1)

            let intra_period = config.intra_refresh_period.max(1);
            let intra_cnt = (config.intra_refresh_period / 8).clamp(1, 10);

            let codec_raw = &mut preset_cfg.preset_cfg.encode_codec_config.raw;
            match config.codec {
                VideoCodec::Av1 => {
                    let mut bitfields = u32::from_ne_bytes(codec_raw[16..20].try_into().unwrap());
                    bitfields |= 0x2e0; // repeatSeqHdr(0x20) | enableIntraRefresh(0x40) | chromaFormatIDC=1(0x80) | enableBitstreamPadding(0x200)
                    codec_raw[16..20].copy_from_slice(&bitfields.to_ne_bytes());
                    codec_raw[20..24].copy_from_slice(&NVENC_INFINITE_GOPLENGTH.to_ne_bytes());
                    codec_raw[24..28].copy_from_slice(&intra_period.to_ne_bytes());
                    codec_raw[28..32].copy_from_slice(&intra_cnt.to_ne_bytes());
                    codec_raw[32..36].copy_from_slice(&1u32.to_ne_bytes()); // maxNumRefFramesInDPB = 1
                }
                VideoCodec::Hevc => {
                    let mut bitfields = u32::from_ne_bytes(codec_raw[16..20].try_into().unwrap());
                    bitfields |= 0x4380; // repeatSPSPPS(0x80) | enableIntraRefresh(0x100) | chromaFormatIDC=1(0x200) | enableFillerDataInsertion(0x4000)
                    codec_raw[16..20].copy_from_slice(&bitfields.to_ne_bytes());
                    codec_raw[20..24].copy_from_slice(&NVENC_INFINITE_GOPLENGTH.to_ne_bytes());
                    codec_raw[24..28].copy_from_slice(&intra_period.to_ne_bytes());
                    codec_raw[28..32].copy_from_slice(&intra_cnt.to_ne_bytes());
                    codec_raw[32..36].copy_from_slice(&1u32.to_ne_bytes()); // maxNumRefFramesInDPB = 1
                }
                _ => {}
            }

            // Initialize Encoder
            let mut init_params: NvEncInitializeParams = std::mem::zeroed();
            init_params.version = NV_ENC_INITIALIZE_PARAMS_VER;
            init_params.encode_guid = codec_guid;
            init_params.preset_guid = NV_ENC_PRESET_P1_GUID;
            init_params.encode_width = config.width;
            init_params.encode_height = config.height;
            init_params.dar_width = config.width;
            init_params.dar_height = config.height;
            init_params.frame_rate_num = config.fps;
            init_params.frame_rate_den = 1;
            init_params.enable_ptd = 1;
            init_params.tuning_info = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
            init_params.buffer_format = NV_ENC_BUFFER_FORMAT_ARGB;
            init_params.encode_config = &mut preset_cfg.preset_cfg;

            let init_encoder: unsafe extern "C" fn(*mut c_void, *mut NvEncInitializeParams) -> u32 =
                std::mem::transmute(fn_list.nv_enc_initialize_encoder);
            let status = init_encoder(self.encoder, &mut init_params);
            if status != NV_ENC_SUCCESS {
                self.cleanup();
                return Err(EncoderError::InitFailed(format!(
                    "NvEncInitializeEncoder failed with code {status}"
                )));
            }

            // Create Input Buffer
            let mut in_buf_params: NvEncCreateInputBuffer = std::mem::zeroed();
            in_buf_params.version = NV_ENC_CREATE_INPUT_BUFFER_VER;
            in_buf_params.width = config.width;
            in_buf_params.height = config.height;
            in_buf_params.buffer_fmt = NV_ENC_BUFFER_FORMAT_ARGB;

            let create_input_buffer: unsafe extern "C" fn(
                *mut c_void,
                *mut NvEncCreateInputBuffer,
            ) -> u32 = std::mem::transmute(fn_list.nv_enc_create_input_buffer);
            let status = create_input_buffer(self.encoder, &mut in_buf_params);
            if status != NV_ENC_SUCCESS {
                self.cleanup();
                return Err(EncoderError::InitFailed(format!(
                    "NvEncCreateInputBuffer failed with code {status}"
                )));
            }
            input_buffer = in_buf_params.input_buffer;
            self.input_buffer = input_buffer;

            // Create Bitstream Buffer
            let mut out_buf_params: NvEncCreateBitstreamBuffer = std::mem::zeroed();
            out_buf_params.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;

            let create_bitstream_buffer: unsafe extern "C" fn(
                *mut c_void,
                *mut NvEncCreateBitstreamBuffer,
            ) -> u32 = std::mem::transmute(fn_list.nv_enc_create_bitstream_buffer);
            let status = create_bitstream_buffer(self.encoder, &mut out_buf_params);
            if status != NV_ENC_SUCCESS {
                self.cleanup();
                return Err(EncoderError::InitFailed(format!(
                    "NvEncCreateBitstreamBuffer failed with code {status}"
                )));
            }
            bitstream_buffer = out_buf_params.bitstream_buffer;
            self.bitstream_buffer = bitstream_buffer;

            // Pre-calculate pitch by locking input buffer once
            let mut lock_in: NvEncLockInputBuffer = std::mem::zeroed();
            lock_in.version = NV_ENC_LOCK_INPUT_BUFFER_VER;
            lock_in.input_buffer = input_buffer;
            let lock_in_fn: unsafe extern "C" fn(*mut c_void, *mut NvEncLockInputBuffer) -> u32 =
                std::mem::transmute(fn_list.nv_enc_lock_input_buffer);
            if lock_in_fn(self.encoder, &mut lock_in) == NV_ENC_SUCCESS {
                input_pitch = lock_in.pitch;
                let unlock_in_fn: unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32 =
                    std::mem::transmute(fn_list.nv_enc_unlock_input_buffer);
                unlock_in_fn(self.encoder, input_buffer);
            } else {
                input_pitch = config.width * 4;
            }
        }

        self.config = config;
        self.input_pitch = input_pitch;
        self.initialized = true;
        self.frame_counter = 0;
        self.force_keyframe = true;

        info!(
            codec = ?self.config.codec,
            preset = "P1 (Ultra Low Latency)",
            rc = "CBR",
            bitrate_kbps = self.config.bitrate_kbps,
            width = self.config.width,
            height = self.config.height,
            fps = self.config.fps,
            "NVIDIA NVENC hardware session successfully initialized"
        );
        Ok(())
    }

    async fn encode(&mut self, frame: &RawFrame) -> Result<EncodedFrame, EncoderError> {
        if !self.initialized {
            return Err(EncoderError::EncodeFailed("Encoder not initialized".into()));
        }

        if let Some(cuda_lib) = &self._cuda_lib {
            unsafe {
                if let Ok(cu_ctx_set_current) =
                    cuda_lib.get::<CuCtxSetCurrentFn>(b"cuCtxSetCurrent\0")
                {
                    let _ = cu_ctx_set_current(self.cuda_ctx);
                }
            }
        }

        match frame.format {
            PixelFormat::Bgra8 | PixelFormat::Rgba8 => {}
            unsupported => {
                return Err(EncoderError::EncodeFailed(format!(
                    "Unsupported input pixel format {unsupported:?} for ARGB NVENC encoder"
                )));
            }
        }

        let encode_start = Instant::now();
        self.frame_counter += 1;

        let is_keyframe_req = self.force_keyframe;

        let fn_list = self.fn_list.as_ref().unwrap();
        let encoder = self.encoder;

        // Lock Input Buffer and upload frame pixels (ARGB format)
        unsafe {
            let mut lock_in: NvEncLockInputBuffer = std::mem::zeroed();
            lock_in.version = NV_ENC_LOCK_INPUT_BUFFER_VER;
            lock_in.input_buffer = self.input_buffer;

            let lock_input_buffer: unsafe extern "C" fn(
                *mut c_void,
                *mut NvEncLockInputBuffer,
            ) -> u32 = std::mem::transmute(fn_list.nv_enc_lock_input_buffer);
            let status = lock_input_buffer(encoder, &mut lock_in);
            if status != NV_ENC_SUCCESS {
                return Err(EncoderError::EncodeFailed(format!(
                    "NvEncLockInputBuffer failed: {status}"
                )));
            }

            let pitch = lock_in.pitch as usize;
            let width = self.config.width as usize;
            let height = self.config.height as usize;
            let buf_ptr = lock_in.buffer_data_ptr as *mut u8;

            if !frame.data.is_empty() {
                let src = &frame.data;
                let dst = std::slice::from_raw_parts_mut(buf_ptr, pitch * height);
                let src_stride = frame.stride as usize;
                let row_bytes = (width * 4).min(src_stride);

                if frame.format == PixelFormat::Rgba8 {
                    // Swap byte 0 (R) and byte 2 (B) for NV_ENC_BUFFER_FORMAT_ARGB (little-endian B,G,R,A)
                    for y in 0..height {
                        let src_start = y * src_stride;
                        let dst_start = y * pitch;
                        if src_start + row_bytes <= src.len() && dst_start + row_bytes <= dst.len()
                        {
                            let src_row = &src[src_start..src_start + row_bytes];
                            let dst_row = &mut dst[dst_start..dst_start + row_bytes];
                            for x in (0..row_bytes).step_by(4) {
                                if x + 3 < row_bytes {
                                    dst_row[x] = src_row[x + 2]; // dst B (byte 0) <- src B (byte 2)
                                    dst_row[x + 1] = src_row[x + 1]; // dst G (byte 1) <- src G (byte 1)
                                    dst_row[x + 2] = src_row[x]; // dst R (byte 2) <- src R (byte 0)
                                    dst_row[x + 3] = src_row[x + 3]; // dst A (byte 3) <- src A (byte 3)
                                }
                            }
                        }
                    }
                } else if src_stride == pitch && row_bytes == pitch && src.len() >= pitch * height {
                    dst[..pitch * height].copy_from_slice(&src[..pitch * height]);
                } else {
                    for y in 0..height {
                        let src_start = y * src_stride;
                        let dst_start = y * pitch;
                        if src_start + row_bytes <= src.len() && dst_start + row_bytes <= dst.len()
                        {
                            dst[dst_start..dst_start + row_bytes]
                                .copy_from_slice(&src[src_start..src_start + row_bytes]);
                        }
                    }
                }
            } else {
                let dst = std::slice::from_raw_parts_mut(buf_ptr, pitch * height);
                dst.fill(0);
            }

            let unlock_input_buffer: unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32 =
                std::mem::transmute(fn_list.nv_enc_unlock_input_buffer);
            unlock_input_buffer(encoder, self.input_buffer);

            // Encode Picture
            let mut pic: NvEncPicParams = std::mem::zeroed();
            pic.version = NV_ENC_PIC_PARAMS_VER;
            pic.input_width = self.config.width;
            pic.input_height = self.config.height;
            pic.input_pitch = lock_in.pitch;
            pic.input_buffer = self.input_buffer;
            pic.output_bitstream = self.bitstream_buffer;
            pic.buffer_fmt = NV_ENC_BUFFER_FORMAT_ARGB;
            pic.picture_struct = NV_ENC_PIC_STRUCT_FRAME;
            if is_keyframe_req {
                pic.encode_pic_flags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
            }
            pic.input_time_stamp = frame.pts_us;

            let encode_picture: unsafe extern "C" fn(*mut c_void, *mut NvEncPicParams) -> u32 =
                std::mem::transmute(fn_list.nv_enc_encode_picture);
            let status = encode_picture(encoder, &mut pic);
            if status != NV_ENC_SUCCESS {
                return Err(EncoderError::EncodeFailed(format!(
                    "NvEncEncodePicture failed: {status}"
                )));
            }

            // Lock Bitstream
            let mut lock_bs: NvEncLockBitstream = std::mem::zeroed();
            lock_bs.version = NV_ENC_LOCK_BITSTREAM_VER;
            lock_bs.output_bitstream = self.bitstream_buffer;

            let lock_bitstream: unsafe extern "C" fn(*mut c_void, *mut NvEncLockBitstream) -> u32 =
                std::mem::transmute(fn_list.nv_enc_lock_bitstream);
            let status = lock_bitstream(encoder, &mut lock_bs);
            if status != NV_ENC_SUCCESS {
                return Err(EncoderError::EncodeFailed(format!(
                    "NvEncLockBitstream failed: {status}"
                )));
            }

            let bitstream_bytes = std::slice::from_raw_parts(
                lock_bs.bitstream_buffer_ptr as *const u8,
                lock_bs.bitstream_size_in_bytes as usize,
            );
            let encoded_payload = Bytes::copy_from_slice(bitstream_bytes);

            let pic_type = lock_bs.picture_type;
            let is_keyframe = pic_type == 3 || pic_type == 2 || is_keyframe_req;
            let is_intra_refresh = pic_type == 6
                || (!is_keyframe
                    && self.config.intra_refresh_period > 0
                    && self
                        .frame_counter
                        .is_multiple_of(self.config.intra_refresh_period as u64));

            let unlock_bitstream: unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32 =
                std::mem::transmute(fn_list.nv_enc_unlock_bitstream);
            unlock_bitstream(encoder, self.bitstream_buffer);

            let encode_duration_us = encode_start.elapsed().as_micros() as u32;

            let max_chunk = self.config.max_chunk_size;
            let total_chunks = encoded_payload.len().div_ceil(max_chunk);
            let mut chunks = Vec::with_capacity(total_chunks.max(1));

            for (chunk_idx, slice) in encoded_payload.chunks(max_chunk).enumerate() {
                let meta = VideoChunkMeta {
                    frame_id: self.frame_counter,
                    chunk_index: chunk_idx as u16,
                    total_chunks: total_chunks as u16,
                    codec: self.config.codec,
                    is_keyframe,
                    is_intra_refresh,
                    width: self.config.width,
                    height: self.config.height,
                    fps: self.config.fps,
                    pts_us: frame.pts_us,
                };
                chunks.push(VideoChunk::new(meta, slice.to_vec().into()));
            }

            if is_keyframe_req {
                self.force_keyframe = false;
            }

            Ok(EncodedFrame {
                frame_id: self.frame_counter,
                is_keyframe,
                is_intra_refresh,
                picture_type: pic_type,
                chunks,
                encode_duration_us: encode_duration_us.max(1),
            })
        }
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn invalidate_reference_picture(&mut self, pts_us: u64) {
        self.last_rpi_frame = Some(pts_us);
        if let (Some(fn_list), encoder) = (&self.fn_list, self.encoder) {
            if !encoder.is_null() && !fn_list.nv_enc_invalidate_ref_frames.is_null() {
                unsafe {
                    let invalidate_fn: unsafe extern "C" fn(*mut c_void, u64) -> u32 =
                        std::mem::transmute(fn_list.nv_enc_invalidate_ref_frames);
                    let _ = invalidate_fn(encoder, pts_us);
                }
            }
        }
    }
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_quest_capture::PixelFormat;

    #[test]
    fn test_nvenc_codec_guid_resolution() {
        let enc_av1 = NvencEncoder::new(EncoderConfig {
            codec: VideoCodec::Av1,
            ..Default::default()
        });
        assert_eq!(enc_av1.codec_guid(), NV_ENC_CODEC_AV1_GUID);

        let enc_hevc = NvencEncoder::new(EncoderConfig {
            codec: VideoCodec::Hevc,
            ..Default::default()
        });
        assert_eq!(enc_hevc.codec_guid(), NV_ENC_CODEC_HEVC_GUID);

        let enc_h264 = NvencEncoder::new(EncoderConfig {
            codec: VideoCodec::H264,
            ..Default::default()
        });
        assert_eq!(enc_h264.codec_guid(), NV_ENC_CODEC_H264_GUID);
    }

    #[tokio::test]
    async fn test_nvenc_hardware_encode_when_available() {
        if !NvencEncoder::is_available() {
            println!("Skipping hardware NVENC test: NVIDIA NVENC device not available.");
            return;
        }

        let mut encoder = NvencEncoder::new(EncoderConfig {
            codec: VideoCodec::Av1,
            width: 1280,
            height: 720,
            fps: 60,
            bitrate_kbps: 60_000,
            intra_refresh_period: 60,
            max_chunk_size: 1400,
        });

        encoder
            .init(EncoderConfig {
                codec: VideoCodec::Av1,
                width: 1280,
                height: 720,
                fps: 60,
                bitrate_kbps: 60_000,
                intra_refresh_period: 60,
                max_chunk_size: 1400,
            })
            .await
            .expect("Hardware NVENC AV1 init failed on available device");

        let frame1 = RawFrame {
            display_id: 0,
            width: 1280,
            height: 720,
            stride: 1280 * 4,
            format: PixelFormat::Bgra8,
            pts_us: 1000,
            dma_buf: None,
            data: Bytes::from(vec![128u8; 1280 * 720 * 4]),
        };

        // Frame 1: initial frame must be IDR (picture_type == 3)
        let encoded1 = encoder
            .encode(&frame1)
            .await
            .expect("Encode frame 1 failed");
        assert_eq!(encoded1.frame_id, 1);
        assert!(encoded1.is_keyframe);
        assert_eq!(encoded1.picture_type, 3);
        assert!(!encoded1.chunks.is_empty());
        assert!(
            encoded1.encode_duration_us < 3000,
            "Encode duration {} us exceeds 3ms budget",
            encoded1.encode_duration_us
        );

        // Verify CBR bitstream padding (> 50,000 bytes at 60 Mbps CBR)
        let total_size: usize = encoded1.chunks.iter().map(|c| c.payload.len()).sum();
        assert!(
            total_size > 50_000,
            "CBR padding failed: total size {total_size} bytes <= 50,000"
        );

        // Verify AV1 Sequence Header or Temporal Delimiter OBU
        let first_payload = &encoded1.chunks[0].payload;
        assert!(first_payload.len() >= 4);
        let obu_type = (first_payload[0] >> 3) & 0x0f;
        assert!(
            obu_type == 1 || obu_type == 2,
            "Expected AV1 Seq Header or Temporal Delimiter, got {obu_type}"
        );

        // Frame 2: regular frame (non-keyframe)
        let frame2 = RawFrame {
            display_id: 0,
            width: 1280,
            height: 720,
            stride: 1280 * 4,
            format: PixelFormat::Bgra8,
            pts_us: 2000,
            dma_buf: None,
            data: Bytes::from(vec![130u8; 1280 * 720 * 4]),
        };
        let encoded2 = encoder
            .encode(&frame2)
            .await
            .expect("Encode frame 2 failed");
        assert_eq!(encoded2.frame_id, 2);
        assert!(!encoded2.is_keyframe);
        assert_ne!(encoded2.picture_type, 3);

        // Frame 3: explicit keyframe request
        encoder.request_keyframe();
        let frame3 = RawFrame {
            display_id: 0,
            width: 1280,
            height: 720,
            stride: 1280 * 4,
            format: PixelFormat::Bgra8,
            pts_us: 3000,
            dma_buf: None,
            data: Bytes::from(vec![132u8; 1280 * 720 * 4]),
        };
        let encoded3 = encoder
            .encode(&frame3)
            .await
            .expect("Encode frame 3 failed");
        assert_eq!(encoded3.frame_id, 3);
        assert!(encoded3.is_keyframe);
        assert_eq!(encoded3.picture_type, 3);

        // Invalidate reference picture test
        encoder.invalidate_reference_picture(3000);
        assert_eq!(encoder.last_rpi_frame, Some(3000));

        // Frame 4: RGBA8 format frame testing channel conversion to ARGB buffer with distinct channels
        let mut rgba_buf = vec![0u8; 1280 * 720 * 4];
        for i in (0..rgba_buf.len()).step_by(4) {
            rgba_buf[i] = 10; // R
            rgba_buf[i + 1] = 20; // G
            rgba_buf[i + 2] = 30; // B
            rgba_buf[i + 3] = 255; // A
        }
        let frame4 = RawFrame {
            display_id: 0,
            width: 1280,
            height: 720,
            stride: 1280 * 4,
            format: PixelFormat::Rgba8,
            pts_us: 4000,
            dma_buf: None,
            data: Bytes::from(rgba_buf),
        };
        let encoded4 = encoder
            .encode(&frame4)
            .await
            .expect("Encode frame 4 (Rgba8) failed");
        assert_eq!(encoded4.frame_id, 4);

        // Verify unsupported format (Nv12) is rejected cleanly
        let frame_unsupported = RawFrame {
            display_id: 0,
            width: 1280,
            height: 720,
            stride: 1280,
            format: PixelFormat::Nv12,
            pts_us: 5000,
            dma_buf: None,
            data: Bytes::from(vec![0u8; 1280 * 720 * 3 / 2]),
        };
        let res = encoder.encode(&frame_unsupported).await;
        assert!(matches!(res, Err(EncoderError::EncodeFailed(_))));

        // Test HEVC hardware session initialization and encode
        let mut hevc_encoder = NvencEncoder::new(EncoderConfig {
            codec: VideoCodec::Hevc,
            width: 1280,
            height: 720,
            fps: 60,
            bitrate_kbps: 60_000,
            intra_refresh_period: 60,
            max_chunk_size: 1400,
        });

        hevc_encoder
            .init(EncoderConfig {
                codec: VideoCodec::Hevc,
                width: 1280,
                height: 720,
                fps: 60,
                bitrate_kbps: 60_000,
                intra_refresh_period: 60,
                max_chunk_size: 1400,
            })
            .await
            .expect("HEVC hardware NVENC init must succeed with status 0");

        let hevc_frame = hevc_encoder
            .encode(&frame1)
            .await
            .expect("HEVC encode failed");
        assert_eq!(hevc_frame.frame_id, 1);
        assert!(hevc_frame.is_keyframe);
        assert_eq!(hevc_frame.picture_type, 3);
        assert!(hevc_frame.encode_duration_us < 3000);
    }
}

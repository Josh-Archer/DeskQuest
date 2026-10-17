//! Byte layouts of the libva parameter structures this backend fills in.
//!
//! Every size, field offset and bit position here was measured against the real
//! libva 2.20.0 headers by `scripts/libva-abi/abi_probe.c` (a C program that uses
//! `sizeof`, `offsetof` and bitfield set-and-read-back). The output is checked in as
//! `tests/fixtures/libva-2.20.0-abi.txt`, and the test module at the bottom of this
//! file compares every constant against it. A header change, or a typo in a
//! constant, fails `cargo test`.
//!
//! What the fixture does and does not cover: it proves the offsets, sizes and bit
//! positions match the headers. It does not prove that the values written into them
//! are what a driver expects. Those semantic values are marked as unverified where
//! they are set (see the `vaapi` module docs).

use std::ffi::c_int;

use super::consts::*;

/// Little-endian writer over a zeroed buffer sized to a libva struct.
pub(crate) struct Buf(Vec<u8>);

impl Buf {
    pub(crate) fn zeroed(size: usize) -> Self {
        Buf(vec![0; size])
    }
    pub(crate) fn u8(&mut self, off: usize, v: u8) -> &mut Self {
        self.0[off] = v;
        self
    }
    pub(crate) fn i8(&mut self, off: usize, v: i8) -> &mut Self {
        self.0[off] = v as u8;
        self
    }
    pub(crate) fn u16(&mut self, off: usize, v: u16) -> &mut Self {
        self.0[off..off + 2].copy_from_slice(&v.to_le_bytes());
        self
    }
    pub(crate) fn u32(&mut self, off: usize, v: u32) -> &mut Self {
        self.0[off..off + 4].copy_from_slice(&v.to_le_bytes());
        self
    }
    pub(crate) fn i32(&mut self, off: usize, v: i32) -> &mut Self {
        self.0[off..off + 4].copy_from_slice(&v.to_le_bytes());
        self
    }
    pub(crate) fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

/// `VAPictureHEVC` (28 bytes: three fields plus `va_reserved[VA_PADDING_LOW]`).
pub mod hevc_picture {
    pub const SIZE: usize = 28;
    pub const PICTURE_ID: usize = 0;
    pub const PIC_ORDER_CNT: usize = 4;
    pub const FLAGS: usize = 8;
}

/// `VAEncSequenceParameterBufferHEVC` (116 bytes).
pub mod hevc_seq {
    pub const SIZE: usize = 116;
    pub const GENERAL_PROFILE_IDC: usize = 0;
    pub const GENERAL_LEVEL_IDC: usize = 1;
    pub const GENERAL_TIER_FLAG: usize = 2;
    pub const INTRA_PERIOD: usize = 4;
    pub const INTRA_IDR_PERIOD: usize = 8;
    pub const IP_PERIOD: usize = 12;
    pub const BITS_PER_SECOND: usize = 16;
    pub const PIC_WIDTH_IN_LUMA_SAMPLES: usize = 20;
    pub const PIC_HEIGHT_IN_LUMA_SAMPLES: usize = 22;
    pub const SEQ_FIELDS: usize = 24;
    pub const LOG2_MIN_LUMA_CB_MINUS3: usize = 28;
    pub const LOG2_DIFF_MAX_MIN_LUMA_CB: usize = 29;
    pub const LOG2_MIN_TB_MINUS2: usize = 30;
    pub const LOG2_DIFF_MAX_MIN_TB: usize = 31;
    pub const MAX_TRANSFORM_HIERARCHY_DEPTH_INTER: usize = 32;
    pub const MAX_TRANSFORM_HIERARCHY_DEPTH_INTRA: usize = 33;
    pub const VUI_PARAMETERS_PRESENT_FLAG: usize = 52;
    pub const VUI_FIELDS: usize = 56;
    pub const ASPECT_RATIO_IDC: usize = 60;
    pub const VUI_NUM_UNITS_IN_TICK: usize = 72;
    pub const VUI_TIME_SCALE: usize = 76;
    /// `seq_fields` bit positions.
    pub const BIT_CHROMA_FORMAT_IDC: u32 = 0;
    pub const BIT_SAMPLE_ADAPTIVE_OFFSET: u32 = 12;
    pub const BIT_LOW_DELAY_SEQ: u32 = 16;
}

/// `VAEncPictureParameterBufferHEVC` (576 bytes).
pub mod hevc_pic {
    pub const SIZE: usize = 576;
    pub const DECODED_CURR_PIC: usize = 0;
    pub const REFERENCE_FRAMES: usize = 28;
    pub const CODED_BUF: usize = 448;
    pub const COLLOCATED_REF_PIC_INDEX: usize = 452;
    pub const LAST_PICTURE: usize = 453;
    pub const PIC_INIT_QP: usize = 454;
    pub const DIFF_CU_QP_DELTA_DEPTH: usize = 455;
    pub const PPS_CB_QP_OFFSET: usize = 456;
    pub const PPS_CR_QP_OFFSET: usize = 457;
    pub const NUM_TILE_COLUMNS_MINUS1: usize = 458;
    pub const NUM_TILE_ROWS_MINUS1: usize = 459;
    pub const COLUMN_WIDTH_MINUS1: usize = 460;
    pub const ROW_HEIGHT_MINUS1: usize = 479;
    pub const LOG2_PARALLEL_MERGE_LEVEL_MINUS2: usize = 500;
    pub const CTU_MAX_BITSIZE_ALLOWED: usize = 501;
    pub const NUM_REF_IDX_L0_DEFAULT_ACTIVE_MINUS1: usize = 502;
    pub const NUM_REF_IDX_L1_DEFAULT_ACTIVE_MINUS1: usize = 503;
    pub const SLICE_PIC_PARAMETER_SET_ID: usize = 504;
    pub const NAL_UNIT_TYPE: usize = 505;
    pub const PIC_FIELDS: usize = 508;
    pub const HIERARCHICAL_LEVEL_PLUS1: usize = 512;
    pub const SCC_FIELDS: usize = 514;
    /// `pic_fields` bit positions.
    pub const BIT_IDR_PIC_FLAG: u32 = 0;
    pub const BIT_CODING_TYPE: u32 = 1;
    pub const BIT_REFERENCE_PIC_FLAG: u32 = 4;
}

/// `VAEncSliceParameterBufferHEVC` (1076 bytes).
pub mod hevc_slice {
    pub const SIZE: usize = 1076;
    pub const SLICE_SEGMENT_ADDRESS: usize = 0;
    pub const NUM_CTU_IN_SLICE: usize = 4;
    pub const SLICE_TYPE: usize = 8;
    pub const SLICE_PIC_PARAMETER_SET_ID: usize = 9;
    pub const NUM_REF_IDX_L0_ACTIVE_MINUS1: usize = 10;
    pub const NUM_REF_IDX_L1_ACTIVE_MINUS1: usize = 11;
    pub const REF_PIC_LIST0: usize = 12;
    pub const REF_PIC_LIST1: usize = 432;
    pub const LUMA_LOG2_WEIGHT_DENOM: usize = 852;
    pub const MAX_NUM_MERGE_CAND: usize = 1034;
    pub const SLICE_QP_DELTA: usize = 1035;
    pub const SLICE_TC_OFFSET_DIV2: usize = 1039;
    pub const SLICE_FIELDS: usize = 1040;
    pub const PRED_WEIGHT_TABLE_BIT_OFFSET: usize = 1044;
    pub const PRED_WEIGHT_TABLE_BIT_LENGTH: usize = 1048;
    /// `slice_fields` bit positions.
    pub const BIT_LAST_SLICE_OF_PIC: u32 = 0;
}

/// `VAPictureH264` (36 bytes).
pub mod h264_picture {
    pub const SIZE: usize = 36;
    pub const PICTURE_ID: usize = 0;
    pub const FRAME_IDX: usize = 4;
    pub const FLAGS: usize = 8;
}

/// `VAEncSequenceParameterBufferH264` (1132 bytes).
pub mod h264_seq {
    pub const SIZE: usize = 1132;
    pub const SEQ_PARAMETER_SET_ID: usize = 0;
    pub const LEVEL_IDC: usize = 1;
    pub const INTRA_PERIOD: usize = 4;
    pub const INTRA_IDR_PERIOD: usize = 8;
    pub const IP_PERIOD: usize = 12;
    pub const BITS_PER_SECOND: usize = 16;
    pub const MAX_NUM_REF_FRAMES: usize = 20;
    pub const PICTURE_WIDTH_IN_MBS: usize = 24;
    pub const PICTURE_HEIGHT_IN_MBS: usize = 26;
    pub const SEQ_FIELDS: usize = 28;
    pub const VUI_PARAMETERS_PRESENT_FLAG: usize = 1088;
    pub const VUI_FIELDS: usize = 1092;
    pub const ASPECT_RATIO_IDC: usize = 1096;
    pub const NUM_UNITS_IN_TICK: usize = 1108;
    pub const TIME_SCALE: usize = 1112;
    /// `seq_fields` bit positions.
    pub const BIT_CHROMA_FORMAT_IDC: u32 = 0;
    pub const BIT_FRAME_MBS_ONLY_FLAG: u32 = 2;
    pub const BIT_DIRECT_8X8_INFERENCE_FLAG: u32 = 5;
    /// `vui_fields` bit positions.
    pub const VUI_BIT_TIMING_INFO_PRESENT: u32 = 1;
    pub const VUI_BIT_BITSTREAM_RESTRICTION: u32 = 2;
}

/// `VAEncPictureParameterBufferH264` (648 bytes).
pub mod h264_pic {
    pub const SIZE: usize = 648;
    pub const CURR_PIC: usize = 0;
    pub const REFERENCE_FRAMES: usize = 36;
    pub const CODED_BUF: usize = 612;
    pub const PIC_PARAMETER_SET_ID: usize = 616;
    pub const SEQ_PARAMETER_SET_ID: usize = 617;
    pub const LAST_PICTURE: usize = 618;
    pub const FRAME_NUM: usize = 620;
    pub const PIC_INIT_QP: usize = 622;
    pub const NUM_REF_IDX_L0_ACTIVE_MINUS1: usize = 623;
    pub const NUM_REF_IDX_L1_ACTIVE_MINUS1: usize = 624;
    pub const CHROMA_QP_INDEX_OFFSET: usize = 625;
    pub const SECOND_CHROMA_QP_INDEX_OFFSET: usize = 626;
    pub const PIC_FIELDS: usize = 628;
    /// `pic_fields` bit positions.
    pub const BIT_IDR_PIC_FLAG: u32 = 0;
    pub const BIT_REFERENCE_PIC_FLAG: u32 = 1;
    pub const BIT_ENTROPY_CODING_MODE_FLAG: u32 = 3;
}

/// `VAEncSliceParameterBufferH264` (3140 bytes).
pub mod h264_slice {
    pub const SIZE: usize = 3140;
    pub const MACROBLOCK_ADDRESS: usize = 0;
    pub const NUM_MACROBLOCKS: usize = 4;
    pub const SLICE_TYPE: usize = 12;
    pub const PIC_PARAMETER_SET_ID: usize = 13;
    pub const IDR_PIC_ID: usize = 14;
    pub const PIC_ORDER_CNT_LSB: usize = 16;
    pub const DIRECT_SPATIAL_MV_PRED_FLAG: usize = 32;
    pub const NUM_REF_IDX_ACTIVE_OVERRIDE_FLAG: usize = 33;
    pub const NUM_REF_IDX_L0_ACTIVE_MINUS1: usize = 34;
    pub const NUM_REF_IDX_L1_ACTIVE_MINUS1: usize = 35;
    pub const REF_PIC_LIST0: usize = 36;
    pub const REF_PIC_LIST1: usize = 1188;
    pub const CABAC_INIT_IDC: usize = 3118;
    pub const SLICE_QP_DELTA: usize = 3119;
    pub const DISABLE_DEBLOCKING_FILTER_IDC: usize = 3120;
    pub const SLICE_ALPHA_C0_OFFSET_DIV2: usize = 3121;
    pub const SLICE_BETA_OFFSET_DIV2: usize = 3122;
}

/// A `VAPictureH264` entry (36 bytes) with `picture_id` and `frame_idx` set and every
/// other field zero. The POC and reference-flag fields are left zero: whether a driver
/// needs them filled is unverified.
pub fn h264_picture_bytes(picture_id: u32, frame_idx: u32) -> [u8; h264_picture::SIZE] {
    let mut b = Buf::zeroed(h264_picture::SIZE);
    b.u32(h264_picture::PICTURE_ID, picture_id)
        .u32(h264_picture::FRAME_IDX, frame_idx);
    let mut out = [0u8; h264_picture::SIZE];
    out.copy_from_slice(&b.into_vec());
    out
}

/// `VAEncSequenceParameterBufferAV1` (88 bytes).
pub mod av1_seq {
    pub const SIZE: usize = 88;
    pub const SEQ_PROFILE: usize = 0;
    pub const SEQ_LEVEL_IDX: usize = 1;
    pub const SEQ_TIER: usize = 2;
    pub const HIERARCHICAL_FLAG: usize = 3;
    pub const INTRA_PERIOD: usize = 4;
    pub const IP_PERIOD: usize = 8;
    pub const BITS_PER_SECOND: usize = 12;
    pub const SEQ_FIELDS: usize = 16;
    pub const ORDER_HINT_BITS_MINUS_1: usize = 20;
    /// `seq_fields` bit positions.
    pub const BIT_STILL_PICTURE: u32 = 0;
    pub const BIT_USE_128X128_SUPERBLOCK: u32 = 1;
    pub const BIT_ENABLE_ORDER_HINT: u32 = 8;
    pub const BIT_BIT_DEPTH_MINUS8: u32 = 14;
    pub const BIT_SUBSAMPLING_X: u32 = 17;
    pub const BIT_SUBSAMPLING_Y: u32 = 18;
    pub const BIT_MONO_CHROME: u32 = 19;
}

/// `VAEncPictureParameterBufferAV1` (1032 bytes). Only the fields this backend sets
/// are named; everything else is zero.
pub mod av1_pic {
    pub const SIZE: usize = 1032;
    pub const FRAME_WIDTH_MINUS_1: usize = 0;
    pub const FRAME_HEIGHT_MINUS_1: usize = 2;
    pub const RECONSTRUCTED_FRAME: usize = 4;
    pub const CODED_BUF: usize = 8;
    pub const REFERENCE_FRAMES: usize = 12;
    pub const REF_FRAME_IDX: usize = 44;
    pub const HIERARCHICAL_LEVEL_PLUS1: usize = 51;
    pub const PRIMARY_REF_FRAME: usize = 52;
    pub const ORDER_HINT: usize = 53;
    pub const REFRESH_FRAME_FLAGS: usize = 54;
    pub const REF_FRAME_CTRL_L0: usize = 56;
    pub const REF_FRAME_CTRL_L1: usize = 60;
    pub const PICTURE_FLAGS: usize = 64;
    pub const SEG_ID_BLOCK_SIZE: usize = 68;
    pub const NUM_TILE_GROUPS_MINUS1: usize = 69;
    pub const TEMPORAL_ID: usize = 70;
    pub const FILTER_LEVEL: usize = 71;
    pub const FILTER_LEVEL_U: usize = 73;
    pub const FILTER_LEVEL_V: usize = 74;
    pub const INTERPOLATION_FILTER: usize = 77;
    pub const BASE_QINDEX: usize = 88;
    pub const Y_DC_DELTA_Q: usize = 89;
    pub const TILE_COLS: usize = 260;
    pub const TILE_ROWS: usize = 261;
    pub const WIDTH_IN_SBS_MINUS_1: usize = 264;
    pub const HEIGHT_IN_SBS_MINUS_1: usize = 390;
    pub const CONTEXT_UPDATE_TILE_ID: usize = 516;
    pub const BYTE_OFFSET_FRAME_HDR_OBU_SIZE: usize = 952;
    pub const SIZE_IN_BITS_FRAME_HDR_OBU: usize = 956;
    pub const BIT_OFFSET_QINDEX: usize = 932;
    /// `picture_flags` bit positions.
    pub const BIT_FRAME_TYPE: u32 = 0;
    pub const BIT_ERROR_RESILIENT_MODE: u32 = 2;
    pub const BIT_DISABLE_FRAME_END_UPDATE_CDF: u32 = 7;
    pub const BIT_REDUCED_TX_SET: u32 = 8;
    pub const BIT_ENABLE_FRAME_OBU: u32 = 9;
    pub const BIT_LONG_TERM_REFERENCE: u32 = 10;
    pub const BIT_ALLOW_INTRABC: u32 = 12;
}

/// `VAEncTileGroupBufferAV1` (20 bytes).
pub mod av1_tile_group {
    pub const SIZE: usize = 20;
    pub const TG_START: usize = 0;
    pub const TG_END: usize = 1;
}

/// Misc parameter buffers: `VAEncMiscParameterBuffer` is a 4-byte type followed by
/// the payload, so payload field offsets below are relative to the payload start.
pub mod misc {
    /// Offset of the payload inside the buffer (`VAEncMiscParameterBuffer.data`).
    pub const DATA: usize = 4;

    pub mod rate_control {
        pub const SIZE: usize = 60;
        pub const BITS_PER_SECOND: usize = 0;
        pub const TARGET_PERCENTAGE: usize = 4;
        pub const WINDOW_SIZE: usize = 8;
        pub const INITIAL_QP: usize = 12;
        pub const MIN_QP: usize = 16;
        pub const BASIC_UNIT_SIZE: usize = 20;
        pub const RC_FLAGS: usize = 24;
        pub const ICQ_QUALITY_FACTOR: usize = 28;
        pub const MAX_QP: usize = 32;
        pub const QUALITY_FACTOR: usize = 36;
        pub const TARGET_FRAME_SIZE: usize = 40;
    }

    pub mod frame_rate {
        pub const SIZE: usize = 24;
        pub const FRAMERATE: usize = 0;
    }

    pub mod hrd {
        pub const SIZE: usize = 24;
        pub const INITIAL_BUFFER_FULLNESS: usize = 0;
        pub const BUFFER_SIZE: usize = 4;
    }

    pub mod rir {
        pub const SIZE: usize = 28;
        pub const RIR_FLAGS: usize = 0;
        pub const INTRA_INSERTION_LOCATION: usize = 4;
        pub const INTRA_INSERT_SIZE: usize = 6;
        pub const QP_DELTA_FOR_INSERTED_INTRA: usize = 8;
        /// `rir_flags` bit positions.
        pub const BIT_ENABLE_RIR_COLUMN: u32 = 0;
        pub const BIT_ENABLE_RIR_ROW: u32 = 1;
    }
}

/// `VAEncPackedHeaderParameterBuffer` (28 bytes).
pub mod packed_header {
    pub const SIZE: usize = 28;
    pub const TYPE: usize = 0;
    pub const BIT_LENGTH: usize = 4;
    pub const HAS_EMULATION_BYTES: usize = 8;
}

/// `VASurfaceAttrib` (24 bytes) and its `VAGenericValue` (16 bytes).
pub mod surface_attrib {
    pub const SIZE: usize = 24;
    pub const TYPE: usize = 0;
    pub const FLAGS: usize = 4;
    pub const VALUE: usize = 8;
    pub const GENERIC_TYPE: usize = 0;
    pub const GENERIC_VALUE: usize = 8;
}

/// `VAProcPipelineParameterBuffer` (224 bytes) and `VAProcColorProperties` (8 bytes).
pub mod vpp {
    pub const PIPELINE_SIZE: usize = 224;
    pub const SURFACE: usize = 0;
    pub const SURFACE_REGION: usize = 8;
    pub const SURFACE_COLOR_STANDARD: usize = 16;
    pub const OUTPUT_REGION: usize = 24;
    pub const OUTPUT_BACKGROUND_COLOR: usize = 32;
    pub const OUTPUT_COLOR_STANDARD: usize = 36;
    pub const PIPELINE_FLAGS: usize = 40;
    pub const FILTER_FLAGS: usize = 44;
    pub const FILTERS: usize = 48;
    pub const NUM_FILTERS: usize = 56;
    pub const FORWARD_REFERENCES: usize = 64;
    pub const NUM_FORWARD_REFERENCES: usize = 72;
    pub const BACKWARD_REFERENCES: usize = 80;
    pub const NUM_BACKWARD_REFERENCES: usize = 88;
    pub const INPUT_COLOR_PROPERTIES: usize = 132;
    pub const OUTPUT_COLOR_PROPERTIES: usize = 140;

    pub const COLOR_SIZE: usize = 8;
    pub const COLOR_CHROMA_SITING: usize = 0;
    pub const COLOR_RANGE: usize = 1;
    pub const COLOR_PRIMARIES: usize = 2;
    pub const COLOR_TRANSFER: usize = 3;
    pub const COLOR_MATRIX: usize = 4;
}

/// `VADRMPRIMESurfaceDescriptor` (312 bytes). Kept as a `repr(C)` struct because it
/// is passed by pointer and has no bitfields; the test checks its layout too.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct DrmPrimeDescriptor {
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
    pub num_objects: u32,
    pub objects: [DrmObject; 4],
    pub num_layers: u32,
    pub layers: [DrmLayer; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct DrmObject {
    pub fd: c_int,
    pub size: u32,
    pub drm_format_modifier: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct DrmLayer {
    pub drm_format: u32,
    pub num_planes: u32,
    pub object_index: [u32; 4],
    pub offset: [u32; 4],
    pub pitch: [u32; 4],
}

// ---------------------------------------------------------------------------
// Typed inputs and their serialisers.
// ---------------------------------------------------------------------------

/// `VAPictureHEVC` as a Rust value. Serialised with [`hevc_picture`] offsets.
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

    fn write(&self, b: &mut Buf, off: usize) {
        b.u32(off + hevc_picture::PICTURE_ID, self.picture_id)
            .i32(off + hevc_picture::PIC_ORDER_CNT, self.pic_order_cnt)
            .u32(off + hevc_picture::FLAGS, self.flags);
    }
}

/// HEVC sequence parameters. Only the fields this backend sets are exposed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HevcSeqParams {
    pub intra_period: u32,
    pub intra_idr_period: u32,
    pub ip_period: u32,
    pub bits_per_second: u32,
    pub width: u16,
    pub height: u16,
}

impl HevcSeqParams {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = Buf::zeroed(hevc_seq::SIZE);
        b.u8(hevc_seq::GENERAL_PROFILE_IDC, HEVC_MAIN_PROFILE_IDC)
            .u8(hevc_seq::GENERAL_LEVEL_IDC, HEVC_LEVEL_5_1)
            .u8(hevc_seq::GENERAL_TIER_FLAG, 0)
            .u32(hevc_seq::INTRA_PERIOD, self.intra_period)
            .u32(hevc_seq::INTRA_IDR_PERIOD, self.intra_idr_period)
            .u32(hevc_seq::IP_PERIOD, self.ip_period)
            .u32(hevc_seq::BITS_PER_SECOND, self.bits_per_second)
            .u16(hevc_seq::PIC_WIDTH_IN_LUMA_SAMPLES, self.width)
            .u16(hevc_seq::PIC_HEIGHT_IN_LUMA_SAMPLES, self.height)
            .u32(
                hevc_seq::SEQ_FIELDS,
                (1 << hevc_seq::BIT_CHROMA_FORMAT_IDC) // 4:2:0
                    | (1 << hevc_seq::BIT_SAMPLE_ADAPTIVE_OFFSET)
                    | (1 << hevc_seq::BIT_LOW_DELAY_SEQ),
            )
            .u8(hevc_seq::LOG2_MIN_LUMA_CB_MINUS3, 0) // 8x8 minimum CB
            .u8(hevc_seq::LOG2_DIFF_MAX_MIN_LUMA_CB, 2) // 32x32 CTB
            .u8(hevc_seq::LOG2_MIN_TB_MINUS2, 0) // 4x4 minimum TB
            .u8(hevc_seq::LOG2_DIFF_MAX_MIN_TB, 3) // 32x32 maximum TB
            .u8(hevc_seq::MAX_TRANSFORM_HIERARCHY_DEPTH_INTER, 2)
            .u8(hevc_seq::MAX_TRANSFORM_HIERARCHY_DEPTH_INTRA, 2);
        b.into_vec()
    }
}

/// HEVC picture parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HevcPicParams {
    pub decoded_curr_pic: VaPictureHevc,
    pub reference_frames: [VaPictureHevc; 15],
    pub coded_buf: u32,
    pub pic_init_qp: i8,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub nal_unit_type: u8,
    pub idr: bool,
    /// Picture type as used in `pic_fields.coding_type` (1 = I, 2 = P).
    pub coding_type: u32,
    pub reference_pic: bool,
}

impl HevcPicParams {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = Buf::zeroed(hevc_pic::SIZE);
        self.decoded_curr_pic
            .write(&mut b, hevc_pic::DECODED_CURR_PIC);
        for (i, r) in self.reference_frames.iter().enumerate() {
            r.write(&mut b, hevc_pic::REFERENCE_FRAMES + i * hevc_picture::SIZE);
        }
        let pic_fields = (self.idr as u32) << hevc_pic::BIT_IDR_PIC_FLAG
            | (self.coding_type & 0x7) << hevc_pic::BIT_CODING_TYPE
            | (self.reference_pic as u32) << hevc_pic::BIT_REFERENCE_PIC_FLAG;
        b.u32(hevc_pic::CODED_BUF, self.coded_buf)
            .u8(
                hevc_pic::COLLOCATED_REF_PIC_INDEX,
                VA_INVALID_COLLOCATED_INDEX,
            )
            .u8(hevc_pic::LAST_PICTURE, 0)
            .u8(hevc_pic::PIC_INIT_QP, self.pic_init_qp as u8)
            .u8(
                hevc_pic::NUM_REF_IDX_L0_DEFAULT_ACTIVE_MINUS1,
                self.num_ref_idx_l0_default_active_minus1,
            )
            .u8(hevc_pic::NAL_UNIT_TYPE, self.nal_unit_type)
            .u32(hevc_pic::PIC_FIELDS, pic_fields)
            .u8(hevc_pic::HIERARCHICAL_LEVEL_PLUS1, 0);
        b.into_vec()
    }
}

/// HEVC slice parameters. One slice covers the picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HevcSliceParams {
    pub num_ctu_in_slice: u32,
    /// HEVC slice_type: 0 = B, 1 = P, 2 = I.
    pub slice_type: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub ref_pic_list0: [VaPictureHevc; 15],
}

impl HevcSliceParams {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = Buf::zeroed(hevc_slice::SIZE);
        b.u32(hevc_slice::SLICE_SEGMENT_ADDRESS, 0)
            .u32(hevc_slice::NUM_CTU_IN_SLICE, self.num_ctu_in_slice)
            .u8(hevc_slice::SLICE_TYPE, self.slice_type)
            .u8(hevc_slice::SLICE_PIC_PARAMETER_SET_ID, 0)
            .u8(
                hevc_slice::NUM_REF_IDX_L0_ACTIVE_MINUS1,
                self.num_ref_idx_l0_active_minus1,
            )
            .u8(hevc_slice::NUM_REF_IDX_L1_ACTIVE_MINUS1, 0)
            .u8(hevc_slice::MAX_NUM_MERGE_CAND, 5)
            .u32(
                hevc_slice::SLICE_FIELDS,
                1 << hevc_slice::BIT_LAST_SLICE_OF_PIC,
            );
        for i in 0..15 {
            let entry = self
                .ref_pic_list0
                .get(i)
                .copied()
                .unwrap_or(VaPictureHevc::invalid());
            entry.write(&mut b, hevc_slice::REF_PIC_LIST0 + i * hevc_picture::SIZE);
            VaPictureHevc::invalid()
                .write(&mut b, hevc_slice::REF_PIC_LIST1 + i * hevc_picture::SIZE);
        }
        b.into_vec()
    }
}

/// H.264 sequence parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct H264SeqParams {
    pub level_idc: u8,
    pub intra_period: u32,
    pub intra_idr_period: u32,
    pub ip_period: u32,
    pub bits_per_second: u32,
    pub max_num_ref_frames: u32,
    pub width_mbs: u16,
    pub height_mbs: u16,
}

impl H264SeqParams {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = Buf::zeroed(h264_seq::SIZE);
        b.u8(h264_seq::SEQ_PARAMETER_SET_ID, 0)
            .u8(h264_seq::LEVEL_IDC, self.level_idc)
            .u32(h264_seq::INTRA_PERIOD, self.intra_period)
            .u32(h264_seq::INTRA_IDR_PERIOD, self.intra_idr_period)
            .u32(h264_seq::IP_PERIOD, self.ip_period)
            .u32(h264_seq::BITS_PER_SECOND, self.bits_per_second)
            .u32(h264_seq::MAX_NUM_REF_FRAMES, self.max_num_ref_frames)
            .u16(h264_seq::PICTURE_WIDTH_IN_MBS, self.width_mbs)
            .u16(h264_seq::PICTURE_HEIGHT_IN_MBS, self.height_mbs)
            .u32(
                h264_seq::SEQ_FIELDS,
                (1 << h264_seq::BIT_CHROMA_FORMAT_IDC)
                    | (1 << h264_seq::BIT_FRAME_MBS_ONLY_FLAG)
                    | (1 << h264_seq::BIT_DIRECT_8X8_INFERENCE_FLAG),
            );
        b.into_vec()
    }
}

/// H.264 picture parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct H264PicParams {
    pub curr_pic: [u8; h264_picture::SIZE],
    pub reference_frames: [[u8; h264_picture::SIZE]; 16],
    pub coded_buf: u32,
    pub frame_num: u16,
    pub idr: bool,
    pub reference_pic: bool,
    pub num_ref_idx_l0_active_minus1: u8,
}

impl H264PicParams {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = Buf::zeroed(h264_pic::SIZE);
        b.0[h264_pic::CURR_PIC..h264_pic::CURR_PIC + h264_picture::SIZE]
            .copy_from_slice(&self.curr_pic);
        for (i, r) in self.reference_frames.iter().enumerate() {
            let off = h264_pic::REFERENCE_FRAMES + i * h264_picture::SIZE;
            b.0[off..off + h264_picture::SIZE].copy_from_slice(r);
        }
        let pic_fields = (self.idr as u32) << h264_pic::BIT_IDR_PIC_FLAG
            | 1u32 << h264_pic::BIT_REFERENCE_PIC_FLAG
            | 1u32 << h264_pic::BIT_ENTROPY_CODING_MODE_FLAG;
        // The picture is a reference picture; `reference_pic` only changes the
        // `reference_pic_flag` value (2-bit field), so set it explicitly.
        let pic_fields = if self.reference_pic {
            pic_fields
        } else {
            pic_fields & !(0b11 << h264_pic::BIT_REFERENCE_PIC_FLAG)
        };
        b.u32(h264_pic::CODED_BUF, self.coded_buf)
            .u8(h264_pic::PIC_PARAMETER_SET_ID, 0)
            .u8(h264_pic::SEQ_PARAMETER_SET_ID, 0)
            .u8(h264_pic::LAST_PICTURE, 0)
            .u16(h264_pic::FRAME_NUM, self.frame_num)
            .u8(h264_pic::PIC_INIT_QP, 26)
            .u8(
                h264_pic::NUM_REF_IDX_L0_ACTIVE_MINUS1,
                self.num_ref_idx_l0_active_minus1,
            )
            .u8(h264_pic::NUM_REF_IDX_L1_ACTIVE_MINUS1, 0)
            .i8(h264_pic::CHROMA_QP_INDEX_OFFSET, 0)
            .i8(h264_pic::SECOND_CHROMA_QP_INDEX_OFFSET, 0)
            .u32(h264_pic::PIC_FIELDS, pic_fields);
        b.into_vec()
    }
}

/// H.264 slice parameters covering the whole picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct H264SliceParams {
    pub num_macroblocks: u32,
    /// H.264 slice_type: 0 = P, 2 = I (1 = B is not used by this backend).
    pub slice_type: u8,
    pub idr_pic_id: u16,
    pub pic_order_cnt_lsb: u16,
    pub num_ref_idx_l0_active_minus1: u8,
    /// Reference list entries, each a VAPictureH264 (36 bytes).
    pub ref_pic_list0: Vec<[u8; h264_picture::SIZE]>,
}

impl H264SliceParams {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = Buf::zeroed(h264_slice::SIZE);
        b.u32(h264_slice::MACROBLOCK_ADDRESS, 0)
            .u32(h264_slice::NUM_MACROBLOCKS, self.num_macroblocks)
            .u8(h264_slice::SLICE_TYPE, self.slice_type)
            .u8(h264_slice::PIC_PARAMETER_SET_ID, 0)
            .u16(h264_slice::IDR_PIC_ID, self.idr_pic_id)
            .u16(h264_slice::PIC_ORDER_CNT_LSB, self.pic_order_cnt_lsb)
            .u8(h264_slice::NUM_REF_IDX_ACTIVE_OVERRIDE_FLAG, 1)
            .u8(
                h264_slice::NUM_REF_IDX_L0_ACTIVE_MINUS1,
                self.num_ref_idx_l0_active_minus1,
            )
            .u8(h264_slice::NUM_REF_IDX_L1_ACTIVE_MINUS1, 0)
            .u8(h264_slice::DIRECT_SPATIAL_MV_PRED_FLAG, 1)
            .u8(h264_slice::CABAC_INIT_IDC, 0)
            .i8(h264_slice::SLICE_QP_DELTA, 0)
            .u8(h264_slice::DISABLE_DEBLOCKING_FILTER_IDC, 0);
        for (i, r) in self.ref_pic_list0.iter().take(32).enumerate() {
            let off = h264_slice::REF_PIC_LIST0 + i * h264_picture::SIZE;
            b.0[off..off + h264_picture::SIZE].copy_from_slice(r);
        }
        b.into_vec()
    }
}

/// AV1 sequence parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Av1SeqParams {
    pub seq_level_idx: u8,
    pub intra_period: u32,
    pub ip_period: u32,
    pub bits_per_second: u32,
}

impl Av1SeqParams {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = Buf::zeroed(av1_seq::SIZE);
        b.u8(av1_seq::SEQ_PROFILE, 0) // Main profile (AV1 profile 0)
            .u8(av1_seq::SEQ_LEVEL_IDX, self.seq_level_idx)
            .u8(av1_seq::SEQ_TIER, 0)
            .u8(av1_seq::HIERARCHICAL_FLAG, 0)
            .u32(av1_seq::INTRA_PERIOD, self.intra_period)
            .u32(av1_seq::IP_PERIOD, self.ip_period)
            .u32(av1_seq::BITS_PER_SECOND, self.bits_per_second)
            .u32(
                av1_seq::SEQ_FIELDS,
                (1 << av1_seq::BIT_USE_128X128_SUPERBLOCK)
                    | (1 << av1_seq::BIT_ENABLE_ORDER_HINT)
                    | (1 << av1_seq::BIT_SUBSAMPLING_X)
                    | (1 << av1_seq::BIT_SUBSAMPLING_Y),
            )
            .u8(av1_seq::ORDER_HINT_BITS_MINUS_1, 6);
        b.into_vec()
    }
}

/// AV1 picture parameters for a single-tile, single-reference low-latency frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Av1PicParams {
    pub width: u32,
    pub height: u32,
    pub reconstructed_frame: u32,
    pub coded_buf: u32,
    /// Slots 0..8 of the reference frame array, indexed by `ref_frame_idx`.
    pub reference_frames: [u32; 8],
    pub ref_frame_idx: [u8; 7],
    pub frame_type: u32,
    pub refresh_frame_flags: u8,
    pub order_hint: u8,
    pub primary_ref_frame: u8,
    pub base_qindex: u8,
    pub width_in_sbs: u16,
    pub height_in_sbs: u16,
    /// Use one frame OBU instead of separate frame header and tile group OBUs.
    pub enable_frame_obu: bool,
}

/// AV1 `frame_type` values from the header comment (`picture_flags.frame_type`).
pub const AV1_KEY_FRAME: u32 = 0;
pub const AV1_INTER_FRAME: u32 = 1;
pub const AV1_INTRA_ONLY_FRAME: u32 = 2;

impl Av1PicParams {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = Buf::zeroed(av1_pic::SIZE);
        b.u16(av1_pic::FRAME_WIDTH_MINUS_1, (self.width - 1) as u16)
            .u16(av1_pic::FRAME_HEIGHT_MINUS_1, (self.height - 1) as u16)
            .u32(av1_pic::RECONSTRUCTED_FRAME, self.reconstructed_frame)
            .u32(av1_pic::CODED_BUF, self.coded_buf);
        for (i, s) in self.reference_frames.iter().enumerate() {
            b.u32(av1_pic::REFERENCE_FRAMES + i * 4, *s);
        }
        b.0[av1_pic::REF_FRAME_IDX..av1_pic::REF_FRAME_IDX + 7]
            .copy_from_slice(&self.ref_frame_idx);
        // Search index 1 selects ref_frame_idx[0] for list 0 (the one reference used).
        let ref_ctrl_l0: u32 = 1;
        let flags = (self.frame_type & 0x3) << av1_pic::BIT_FRAME_TYPE
            | (self.enable_frame_obu as u32) << av1_pic::BIT_ENABLE_FRAME_OBU
            | 1 << av1_pic::BIT_DISABLE_FRAME_END_UPDATE_CDF;
        b.u8(av1_pic::HIERARCHICAL_LEVEL_PLUS1, 0)
            .u8(av1_pic::PRIMARY_REF_FRAME, self.primary_ref_frame)
            .u8(av1_pic::ORDER_HINT, self.order_hint)
            .u8(av1_pic::REFRESH_FRAME_FLAGS, self.refresh_frame_flags)
            .u32(av1_pic::REF_FRAME_CTRL_L0, ref_ctrl_l0)
            .u32(av1_pic::REF_FRAME_CTRL_L1, 0)
            .u32(av1_pic::PICTURE_FLAGS, flags)
            .u8(av1_pic::NUM_TILE_GROUPS_MINUS1, 0)
            .u8(av1_pic::TILE_COLS, 0)
            .u8(av1_pic::TILE_ROWS, 0)
            .u8(av1_pic::BASE_QINDEX, self.base_qindex)
            .u16(
                av1_pic::WIDTH_IN_SBS_MINUS_1,
                self.width_in_sbs.saturating_sub(1),
            )
            .u16(
                av1_pic::HEIGHT_IN_SBS_MINUS_1,
                self.height_in_sbs.saturating_sub(1),
            )
            .u8(av1_pic::FILTER_LEVEL, 0)
            .u8(av1_pic::FILTER_LEVEL + 1, 0)
            .u8(av1_pic::FILTER_LEVEL_U, 0)
            .u8(av1_pic::FILTER_LEVEL_V, 0);
        b.into_vec()
    }
}

/// AV1 tile group covering tile 0 of a single-tile frame.
pub fn av1_tile_group_bytes() -> Vec<u8> {
    let mut b = Buf::zeroed(av1_tile_group::SIZE);
    b.u8(av1_tile_group::TG_START, 0)
        .u8(av1_tile_group::TG_END, 0);
    b.into_vec()
}

/// Rate-control misc buffer (type + payload), CBR at `bitrate_kbps`.
pub fn rate_control_misc(bitrate_kbps: u32, fps: u32) -> Vec<u8> {
    let bits = bitrate_kbps.saturating_mul(1000);
    let window_ms = (1000 / fps.max(1)).max(1);
    let mut b = misc_buffer(VA_ENC_MISC_RATE_CONTROL, misc::rate_control::SIZE);
    b.u32(misc::DATA + misc::rate_control::BITS_PER_SECOND, bits)
        .u32(misc::DATA + misc::rate_control::TARGET_PERCENTAGE, 100) // constant bitrate
        .u32(misc::DATA + misc::rate_control::WINDOW_SIZE, window_ms)
        .u32(misc::DATA + misc::rate_control::MAX_QP, 51);
    b.into_vec()
}

/// Frame-rate misc buffer (fps as a plain integer).
pub fn frame_rate_misc(fps: u32) -> Vec<u8> {
    let mut b = misc_buffer(VA_ENC_MISC_FRAME_RATE, misc::frame_rate::SIZE);
    b.u32(misc::DATA + misc::frame_rate::FRAMERATE, fps.max(1));
    b.into_vec()
}

/// HRD misc buffer: one frame of VBV buffering, matching the NVENC backend.
pub fn hrd_misc(bitrate_kbps: u32, fps: u32) -> Vec<u8> {
    let buffer = bitrate_kbps.saturating_mul(1000) / fps.max(1);
    let mut b = misc_buffer(VA_ENC_MISC_HRD, misc::hrd::SIZE);
    b.u32(misc::DATA + misc::hrd::INITIAL_BUFFER_FULLNESS, buffer)
        .u32(misc::DATA + misc::hrd::BUFFER_SIZE, buffer);
    b.into_vec()
}

/// Rolling intra refresh misc buffer. Inserts `insert_size` intra columns (or rows)
/// starting at `location`, both in macroblock units.
pub fn rir_misc(column: bool, location: u16, insert_size: u16) -> Vec<u8> {
    let mut b = misc_buffer(VA_ENC_MISC_RIR, misc::rir::SIZE);
    let flags = if column {
        1u32 << misc::rir::BIT_ENABLE_RIR_COLUMN
    } else {
        1u32 << misc::rir::BIT_ENABLE_RIR_ROW
    };
    b.u32(misc::DATA + misc::rir::RIR_FLAGS, flags)
        .u16(misc::DATA + misc::rir::INTRA_INSERTION_LOCATION, location)
        .u16(misc::DATA + misc::rir::INTRA_INSERT_SIZE, insert_size)
        .u8(misc::DATA + misc::rir::QP_DELTA_FOR_INSERTED_INTRA, 0);
    b.into_vec()
}

/// A misc parameter buffer header with `kind` and a zeroed payload of `payload_size`.
fn misc_buffer(kind: u32, payload_size: usize) -> Buf {
    let mut b = Buf::zeroed(misc::DATA + payload_size);
    b.u32(0, kind);
    b
}

/// Packed header parameter (`VAEncPackedHeaderParameterBuffer`) for `header_type`
/// (1 = sequence, 2 = picture, 3 = slice) whose data is `bit_length` bits long.
pub fn packed_header_param(
    header_type: u32,
    bit_length: u32,
    has_emulation_bytes: bool,
) -> Vec<u8> {
    let mut b = Buf::zeroed(packed_header::SIZE);
    b.u32(packed_header::TYPE, header_type)
        .u32(packed_header::BIT_LENGTH, bit_length)
        .u8(
            packed_header::HAS_EMULATION_BYTES,
            has_emulation_bytes as u8,
        );
    b.into_vec()
}

/// `VAProcPipelineParameterBuffer` for one RGB-to-NV12 conversion pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VppColour {
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    /// `VA_SOURCE_RANGE_FULL` (2) or `VA_SOURCE_RANGE_REDUCED` (1).
    pub source_range: u8,
}

pub fn vpp_pipeline_bytes(input_surface: u32, input: VppColour, output: VppColour) -> Vec<u8> {
    let mut b = Buf::zeroed(vpp::PIPELINE_SIZE);
    b.u32(vpp::SURFACE, input_surface)
        .u32(vpp::SURFACE_COLOR_STANDARD, VA_PROC_COLOR_STANDARD_EXPLICIT)
        .u32(vpp::OUTPUT_COLOR_STANDARD, VA_PROC_COLOR_STANDARD_EXPLICIT);
    write_vpp_colour(&mut b, vpp::INPUT_COLOR_PROPERTIES, input);
    write_vpp_colour(&mut b, vpp::OUTPUT_COLOR_PROPERTIES, output);
    b.into_vec()
}

fn write_vpp_colour(b: &mut Buf, off: usize, c: VppColour) {
    b.u8(off + vpp::COLOR_CHROMA_SITING, 0)
        .u8(off + vpp::COLOR_RANGE, c.source_range)
        .u8(off + vpp::COLOR_PRIMARIES, c.primaries)
        .u8(off + vpp::COLOR_TRANSFER, c.transfer)
        .u8(off + vpp::COLOR_MATRIX, c.matrix);
}

/// `VAProcColorStandardExplicit` (13 in libva 2.20): colour described by the
/// per-surface `VAProcColorProperties` instead of a named standard.
pub const VA_PROC_COLOR_STANDARD_EXPLICIT: u32 = 13;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const FIXTURE: &str = include_str!("../../tests/fixtures/libva-2.20.0-abi.txt");

    /// Parses `KIND KEY VALUE` lines into key -> value. Keys are unique across kinds.
    fn fixture() -> HashMap<String, i64> {
        let mut map = HashMap::new();
        for line in FIXTURE.lines().filter(|l| !l.starts_with('#')) {
            let mut parts = line.split_whitespace();
            let (Some(_kind), Some(key), Some(value)) = (parts.next(), parts.next(), parts.next())
            else {
                panic!("malformed fixture line: {line}");
            };
            let value: i64 = value.parse().expect("fixture value is an integer");
            assert!(
                map.insert(key.to_string(), value).is_none(),
                "duplicate key {key}"
            );
        }
        map
    }

    fn check(map: &HashMap<String, i64>, key: &str, rust_value: i64) {
        let header = *map
            .get(key)
            .unwrap_or_else(|| panic!("fixture has no entry for {key}"));
        assert_eq!(
            rust_value, header,
            "{key}: Rust value differs from libva 2.20.0 header"
        );
    }

    #[test]
    fn libva_constants_match_headers() {
        let f = fixture();
        check(&f, "VA_STATUS_SUCCESS", VA_STATUS_SUCCESS as i64);
        check(&f, "VA_PROGRESSIVE", VA_PROGRESSIVE as i64);
        check(&f, "VA_INVALID_ID", VA_INVALID_ID as i64);
        check(
            &f,
            "VA_ATTRIB_NOT_SUPPORTED",
            VA_ATTRIB_NOT_SUPPORTED as i64,
        );
        check(&f, "VAProfileHEVCMain", VA_PROFILE_HEVC_MAIN as i64);
        check(&f, "VAProfileAV1Profile0", VA_PROFILE_AV1_PROFILE0 as i64);
        check(&f, "VAProfileH264Main", VA_PROFILE_H264_MAIN as i64);
        check(&f, "VAProfileH264High", VA_PROFILE_H264_HIGH as i64);
        check(&f, "VAProfileNone", VA_PROFILE_NONE as i64);
        check(&f, "VAEntrypointEncSlice", VA_ENTRYPOINT_ENC_SLICE as i64);
        check(
            &f,
            "VAEntrypointEncSliceLP",
            VA_ENTRYPOINT_ENC_SLICE_LP as i64,
        );
        check(&f, "VAEntrypointVideoProc", VA_ENTRYPOINT_VIDEO_PROC as i64);
        check(
            &f,
            "VAConfigAttribRTFormat",
            VA_CONFIG_ATTRIB_RT_FORMAT as i64,
        );
        check(
            &f,
            "VAConfigAttribRateControl",
            VA_CONFIG_ATTRIB_RATE_CONTROL as i64,
        );
        check(
            &f,
            "VAConfigAttribEncPackedHeaders",
            VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS as i64,
        );
        check(
            &f,
            "VAConfigAttribEncMaxRefFrames",
            VA_CONFIG_ATTRIB_ENC_MAX_REF_FRAMES as i64,
        );
        check(
            &f,
            "VAConfigAttribEncIntraRefresh",
            VA_CONFIG_ATTRIB_ENC_INTRA_REFRESH as i64,
        );
        check(&f, "VA_RT_FORMAT_YUV420", VA_RT_FORMAT_YUV420 as i64);
        check(&f, "VA_RT_FORMAT_RGB32", VA_RT_FORMAT_RGB32 as i64);
        check(&f, "VA_RC_CBR", VA_RC_CBR as i64);
        check(
            &f,
            "VA_ENC_PACKED_HEADER_NONE",
            VA_ENC_PACKED_HEADER_NONE as i64,
        );
        check(
            &f,
            "VA_ENC_PACKED_HEADER_SEQUENCE",
            VA_ENC_PACKED_HEADER_SEQUENCE as i64,
        );
        check(
            &f,
            "VA_ENC_PACKED_HEADER_PICTURE",
            VA_ENC_PACKED_HEADER_PICTURE as i64,
        );
        check(
            &f,
            "VA_ENC_INTRA_REFRESH_ROLLING_COLUMN",
            VA_ENC_INTRA_REFRESH_ROLLING_COLUMN as i64,
        );
        check(
            &f,
            "VA_ENC_INTRA_REFRESH_ROLLING_ROW",
            VA_ENC_INTRA_REFRESH_ROLLING_ROW as i64,
        );
        check(
            &f,
            "VASurfaceAttribMemoryType",
            VA_SURFACE_ATTRIB_MEM_TYPE as i64,
        );
        check(
            &f,
            "VASurfaceAttribExternalBufferDescriptor",
            VA_SURFACE_ATTRIB_EXTERNAL_BUFFERS as i64,
        );
        check(
            &f,
            "VA_SURFACE_ATTRIB_SETTABLE",
            VA_SURFACE_ATTRIB_SETTABLE as i64,
        );
        check(
            &f,
            "VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2",
            VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2 as i64,
        );
        check(
            &f,
            "VAGenericValueTypeInteger",
            VA_GENERIC_VALUE_TYPE_INTEGER as i64,
        );
        check(
            &f,
            "VAGenericValueTypePointer",
            VA_GENERIC_VALUE_TYPE_POINTER as i64,
        );
        check(&f, "VAEncCodedBufferType", VA_BUF_ENC_CODED as i64);
        check(
            &f,
            "VAEncSequenceParameterBufferType",
            VA_BUF_ENC_SEQ as i64,
        );
        check(&f, "VAEncPictureParameterBufferType", VA_BUF_ENC_PIC as i64);
        check(&f, "VAEncSliceParameterBufferType", VA_BUF_ENC_SLICE as i64);
        check(&f, "VAEncMiscParameterBufferType", VA_BUF_ENC_MISC as i64);
        check(
            &f,
            "VAEncPackedHeaderParameterBufferType",
            VA_BUF_ENC_PACKED_HEADER_PARAMS as i64,
        );
        check(
            &f,
            "VAEncPackedHeaderDataBufferType",
            VA_BUF_ENC_PACKED_HEADER_DATA as i64,
        );
        check(
            &f,
            "VAProcPipelineParameterBufferType",
            VA_BUF_PROC_PIPELINE as i64,
        );
        check(
            &f,
            "VAEncMiscParameterTypeFrameRate",
            VA_ENC_MISC_FRAME_RATE as i64,
        );
        check(
            &f,
            "VAEncMiscParameterTypeRateControl",
            VA_ENC_MISC_RATE_CONTROL as i64,
        );
        check(&f, "VAEncMiscParameterTypeHRD", VA_ENC_MISC_HRD as i64);
        check(&f, "VAEncMiscParameterTypeRIR", VA_ENC_MISC_RIR as i64);
        check(
            &f,
            "VAEncPackedHeaderSequence",
            VA_ENC_PACKED_HEADER_TYPE_SEQUENCE as i64,
        );
        check(
            &f,
            "VAEncPackedHeaderPicture",
            VA_ENC_PACKED_HEADER_TYPE_PICTURE as i64,
        );
        check(
            &f,
            "VAEncPackedHeaderSlice",
            VA_ENC_PACKED_HEADER_TYPE_SLICE as i64,
        );
        check(
            &f,
            "VA_PICTURE_HEVC_INVALID",
            VA_PICTURE_HEVC_INVALID as i64,
        );
        check(
            &f,
            "VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE",
            VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE as i64,
        );
        check(&f, "VA_AV1_MAX_SEGMENTS", 8);
        check(
            &f,
            "VA_SOURCE_RANGE_REDUCED",
            VA_SOURCE_RANGE_REDUCED as i64,
        );
        check(&f, "VA_SOURCE_RANGE_FULL", VA_SOURCE_RANGE_FULL as i64);
        check(
            &f,
            "VAProcColorStandardExplicit",
            VA_PROC_COLOR_STANDARD_EXPLICIT as i64,
        );
        check(&f, "VA_FOURCC_BGRA", VA_FOURCC_BGRA as i64);
        check(&f, "VA_FOURCC_RGBA", VA_FOURCC_RGBA as i64);
        check(&f, "VA_FOURCC_NV12", VA_FOURCC_NV12 as i64);
        check(&f, "DRM_FORMAT_ARGB8888", DRM_FORMAT_ARGB8888 as i64);
        check(&f, "DRM_FORMAT_NV12", DRM_FORMAT_NV12 as i64);
    }

    #[test]
    fn struct_sizes_match_headers() {
        let f = fixture();
        check(&f, "VAPictureHEVC", hevc_picture::SIZE as i64);
        check(
            &f,
            "VAEncSequenceParameterBufferHEVC",
            hevc_seq::SIZE as i64,
        );
        check(&f, "VAEncPictureParameterBufferHEVC", hevc_pic::SIZE as i64);
        check(&f, "VAEncSliceParameterBufferHEVC", hevc_slice::SIZE as i64);
        check(&f, "VAPictureH264", h264_picture::SIZE as i64);
        check(
            &f,
            "VAEncSequenceParameterBufferH264",
            h264_seq::SIZE as i64,
        );
        check(&f, "VAEncPictureParameterBufferH264", h264_pic::SIZE as i64);
        check(&f, "VAEncSliceParameterBufferH264", h264_slice::SIZE as i64);
        check(&f, "VAEncSequenceParameterBufferAV1", av1_seq::SIZE as i64);
        check(&f, "VAEncPictureParameterBufferAV1", av1_pic::SIZE as i64);
        check(&f, "VAEncTileGroupBufferAV1", av1_tile_group::SIZE as i64);
        check(&f, "VAEncMiscParameterBuffer", misc::DATA as i64);
        check(
            &f,
            "VAEncMiscParameterRateControl",
            misc::rate_control::SIZE as i64,
        );
        check(
            &f,
            "VAEncMiscParameterFrameRate",
            misc::frame_rate::SIZE as i64,
        );
        check(&f, "VAEncMiscParameterHRD", misc::hrd::SIZE as i64);
        check(&f, "VAEncMiscParameterRIR", misc::rir::SIZE as i64);
        check(
            &f,
            "VAEncPackedHeaderParameterBuffer",
            packed_header::SIZE as i64,
        );
        check(&f, "VAGenericValue", 16);
        check(&f, "VASurfaceAttrib", surface_attrib::SIZE as i64);
        check(
            &f,
            "VADRMPRIMESurfaceDescriptor",
            std::mem::size_of::<DrmPrimeDescriptor>() as i64,
        );
        check(&f, "VAProcColorProperties", vpp::COLOR_SIZE as i64);
        check(
            &f,
            "VAProcPipelineParameterBuffer",
            vpp::PIPELINE_SIZE as i64,
        );
    }

    #[test]
    fn field_offsets_match_headers() {
        let f = fixture();
        let o = |key: &str, v: usize| check(&f, key, v as i64);
        // HEVC
        o(
            "VAEncSequenceParameterBufferHEVC.seq_fields",
            hevc_seq::SEQ_FIELDS,
        );
        o(
            "VAEncSequenceParameterBufferHEVC.intra_period",
            hevc_seq::INTRA_PERIOD,
        );
        o(
            "VAEncSequenceParameterBufferHEVC.vui_fields",
            hevc_seq::VUI_FIELDS,
        );
        o(
            "VAEncSequenceParameterBufferHEVC.vui_time_scale",
            hevc_seq::VUI_TIME_SCALE,
        );
        o(
            "VAEncPictureParameterBufferHEVC.reference_frames",
            hevc_pic::REFERENCE_FRAMES,
        );
        o(
            "VAEncPictureParameterBufferHEVC.coded_buf",
            hevc_pic::CODED_BUF,
        );
        o(
            "VAEncPictureParameterBufferHEVC.column_width_minus1",
            hevc_pic::COLUMN_WIDTH_MINUS1,
        );
        o(
            "VAEncPictureParameterBufferHEVC.ctu_max_bitsize_allowed",
            hevc_pic::CTU_MAX_BITSIZE_ALLOWED,
        );
        o(
            "VAEncPictureParameterBufferHEVC.nal_unit_type",
            hevc_pic::NAL_UNIT_TYPE,
        );
        o(
            "VAEncPictureParameterBufferHEVC.pic_fields",
            hevc_pic::PIC_FIELDS,
        );
        o(
            "VAEncPictureParameterBufferHEVC.hierarchical_level_plus1",
            hevc_pic::HIERARCHICAL_LEVEL_PLUS1,
        );
        o(
            "VAEncSliceParameterBufferHEVC.ref_pic_list0",
            hevc_slice::REF_PIC_LIST0,
        );
        o(
            "VAEncSliceParameterBufferHEVC.ref_pic_list1",
            hevc_slice::REF_PIC_LIST1,
        );
        o(
            "VAEncSliceParameterBufferHEVC.max_num_merge_cand",
            hevc_slice::MAX_NUM_MERGE_CAND,
        );
        o(
            "VAEncSliceParameterBufferHEVC.slice_fields",
            hevc_slice::SLICE_FIELDS,
        );
        // H.264
        o(
            "VAEncSequenceParameterBufferH264.seq_fields",
            h264_seq::SEQ_FIELDS,
        );
        o(
            "VAEncSequenceParameterBufferH264.vui_fields",
            h264_seq::VUI_FIELDS,
        );
        o(
            "VAEncSequenceParameterBufferH264.time_scale",
            h264_seq::TIME_SCALE,
        );
        o(
            "VAEncPictureParameterBufferH264.ReferenceFrames",
            h264_pic::REFERENCE_FRAMES,
        );
        o(
            "VAEncPictureParameterBufferH264.coded_buf",
            h264_pic::CODED_BUF,
        );
        o(
            "VAEncPictureParameterBufferH264.frame_num",
            h264_pic::FRAME_NUM,
        );
        o(
            "VAEncPictureParameterBufferH264.pic_fields",
            h264_pic::PIC_FIELDS,
        );
        o(
            "VAEncSliceParameterBufferH264.RefPicList0",
            h264_slice::REF_PIC_LIST0,
        );
        o(
            "VAEncSliceParameterBufferH264.RefPicList1",
            h264_slice::REF_PIC_LIST1,
        );
        o(
            "VAEncSliceParameterBufferH264.cabac_init_idc",
            h264_slice::CABAC_INIT_IDC,
        );
        // AV1
        o(
            "VAEncSequenceParameterBufferAV1.seq_fields",
            av1_seq::SEQ_FIELDS,
        );
        o(
            "VAEncSequenceParameterBufferAV1.order_hint_bits_minus_1",
            av1_seq::ORDER_HINT_BITS_MINUS_1,
        );
        o(
            "VAEncPictureParameterBufferAV1.reference_frames",
            av1_pic::REFERENCE_FRAMES,
        );
        o(
            "VAEncPictureParameterBufferAV1.ref_frame_idx",
            av1_pic::REF_FRAME_IDX,
        );
        o(
            "VAEncPictureParameterBufferAV1.picture_flags",
            av1_pic::PICTURE_FLAGS,
        );
        o(
            "VAEncPictureParameterBufferAV1.ref_frame_ctrl_l0",
            av1_pic::REF_FRAME_CTRL_L0,
        );
        o(
            "VAEncPictureParameterBufferAV1.width_in_sbs_minus_1",
            av1_pic::WIDTH_IN_SBS_MINUS_1,
        );
        o(
            "VAEncPictureParameterBufferAV1.height_in_sbs_minus_1",
            av1_pic::HEIGHT_IN_SBS_MINUS_1,
        );
        o(
            "VAEncPictureParameterBufferAV1.size_in_bits_frame_hdr_obu",
            av1_pic::SIZE_IN_BITS_FRAME_HDR_OBU,
        );
        o(
            "VAEncPictureParameterBufferAV1.bit_offset_qindex",
            av1_pic::BIT_OFFSET_QINDEX,
        );
        o(
            "VAEncPictureParameterBufferAV1.filter_level_v",
            av1_pic::FILTER_LEVEL_V,
        );
        o("VAEncTileGroupBufferAV1.tg_end", av1_tile_group::TG_END);
        // Misc payloads
        o(
            "VAEncMiscParameterRateControl.max_qp",
            misc::rate_control::MAX_QP,
        );
        o(
            "VAEncMiscParameterRateControl.rc_flags",
            misc::rate_control::RC_FLAGS,
        );
        o(
            "VAEncMiscParameterRateControl.window_size",
            misc::rate_control::WINDOW_SIZE,
        );
        o(
            "VAEncMiscParameterRateControl.target_frame_size",
            misc::rate_control::TARGET_FRAME_SIZE,
        );
        o(
            "VAEncMiscParameterFrameRate.framerate",
            misc::frame_rate::FRAMERATE,
        );
        o("VAEncMiscParameterHRD.buffer_size", misc::hrd::BUFFER_SIZE);
        o(
            "VAEncMiscParameterRIR.intra_insertion_location",
            misc::rir::INTRA_INSERTION_LOCATION,
        );
        o(
            "VAEncMiscParameterRIR.intra_insert_size",
            misc::rir::INTRA_INSERT_SIZE,
        );
        o(
            "VAEncMiscParameterRIR.qp_delta_for_inserted_intra",
            misc::rir::QP_DELTA_FOR_INSERTED_INTRA,
        );
        o(
            "VAEncPackedHeaderParameterBuffer.bit_length",
            packed_header::BIT_LENGTH,
        );
        o(
            "VAEncPackedHeaderParameterBuffer.has_emulation_bytes",
            packed_header::HAS_EMULATION_BYTES,
        );
        // Surfaces
        o("VASurfaceAttrib.value", surface_attrib::VALUE);
        o("VASurfaceAttrib.flags", surface_attrib::FLAGS);
        o("VAGenericValue.value", surface_attrib::GENERIC_VALUE);
        o(
            "VADRMPRIMESurfaceDescriptor.layers",
            std::mem::offset_of!(DrmPrimeDescriptor, layers),
        );
        o(
            "VADRMPRIMESurfaceDescriptor.objects",
            std::mem::offset_of!(DrmPrimeDescriptor, objects),
        );
        o(
            "VADRMPRIMESurfaceDescriptor.num_layers",
            std::mem::offset_of!(DrmPrimeDescriptor, num_layers),
        );
        // VPP
        o(
            "VAProcPipelineParameterBuffer.surface_region",
            vpp::SURFACE_REGION,
        );
        o(
            "VAProcPipelineParameterBuffer.surface_color_standard",
            vpp::SURFACE_COLOR_STANDARD,
        );
        o(
            "VAProcPipelineParameterBuffer.output_region",
            vpp::OUTPUT_REGION,
        );
        o(
            "VAProcPipelineParameterBuffer.output_color_standard",
            vpp::OUTPUT_COLOR_STANDARD,
        );
        o(
            "VAProcPipelineParameterBuffer.pipeline_flags",
            vpp::PIPELINE_FLAGS,
        );
        o(
            "VAProcPipelineParameterBuffer.num_filters",
            vpp::NUM_FILTERS,
        );
        o(
            "VAProcPipelineParameterBuffer.input_color_properties",
            vpp::INPUT_COLOR_PROPERTIES,
        );
        o(
            "VAProcPipelineParameterBuffer.output_color_properties",
            vpp::OUTPUT_COLOR_PROPERTIES,
        );
        o("VAProcColorProperties.color_range", vpp::COLOR_RANGE);
        o(
            "VAProcColorProperties.colour_primaries",
            vpp::COLOR_PRIMARIES,
        );
        o(
            "VAProcColorProperties.transfer_characteristics",
            vpp::COLOR_TRANSFER,
        );
        o(
            "VAProcColorProperties.matrix_coefficients",
            vpp::COLOR_MATRIX,
        );
    }

    #[test]
    fn bit_positions_match_headers() {
        let f = fixture();
        let b = |key: &str, v: u32| check(&f, key, v as i64);
        b(
            "VAEncSequenceParameterBufferHEVC.seq_fields.chroma_format_idc",
            hevc_seq::BIT_CHROMA_FORMAT_IDC,
        );
        b(
            "VAEncSequenceParameterBufferHEVC.seq_fields.sample_adaptive_offset_enabled_flag",
            hevc_seq::BIT_SAMPLE_ADAPTIVE_OFFSET,
        );
        b(
            "VAEncSequenceParameterBufferHEVC.seq_fields.low_delay_seq",
            hevc_seq::BIT_LOW_DELAY_SEQ,
        );
        b(
            "VAEncPictureParameterBufferHEVC.pic_fields.idr_pic_flag",
            hevc_pic::BIT_IDR_PIC_FLAG,
        );
        b(
            "VAEncPictureParameterBufferHEVC.pic_fields.coding_type",
            hevc_pic::BIT_CODING_TYPE,
        );
        b(
            "VAEncPictureParameterBufferHEVC.pic_fields.reference_pic_flag",
            hevc_pic::BIT_REFERENCE_PIC_FLAG,
        );
        b(
            "VAEncSliceParameterBufferHEVC.slice_fields.last_slice_of_pic_flag",
            hevc_slice::BIT_LAST_SLICE_OF_PIC,
        );
        b(
            "VAEncSequenceParameterBufferH264.seq_fields.chroma_format_idc",
            h264_seq::BIT_CHROMA_FORMAT_IDC,
        );
        b(
            "VAEncSequenceParameterBufferH264.seq_fields.frame_mbs_only_flag",
            h264_seq::BIT_FRAME_MBS_ONLY_FLAG,
        );
        b(
            "VAEncSequenceParameterBufferH264.seq_fields.direct_8x8_inference_flag",
            h264_seq::BIT_DIRECT_8X8_INFERENCE_FLAG,
        );
        b(
            "VAEncSequenceParameterBufferH264.vui_fields.timing_info_present_flag",
            h264_seq::VUI_BIT_TIMING_INFO_PRESENT,
        );
        b(
            "VAEncSequenceParameterBufferH264.vui_fields.bitstream_restriction_flag",
            h264_seq::VUI_BIT_BITSTREAM_RESTRICTION,
        );
        b(
            "VAEncPictureParameterBufferH264.pic_fields.idr_pic_flag",
            h264_pic::BIT_IDR_PIC_FLAG,
        );
        b(
            "VAEncPictureParameterBufferH264.pic_fields.reference_pic_flag",
            h264_pic::BIT_REFERENCE_PIC_FLAG,
        );
        b(
            "VAEncPictureParameterBufferH264.pic_fields.entropy_coding_mode_flag",
            h264_pic::BIT_ENTROPY_CODING_MODE_FLAG,
        );
        b(
            "VAEncSequenceParameterBufferAV1.seq_fields.still_picture",
            av1_seq::BIT_STILL_PICTURE,
        );
        b(
            "VAEncSequenceParameterBufferAV1.seq_fields.use_128x128_superblock",
            av1_seq::BIT_USE_128X128_SUPERBLOCK,
        );
        b(
            "VAEncSequenceParameterBufferAV1.seq_fields.enable_order_hint",
            av1_seq::BIT_ENABLE_ORDER_HINT,
        );
        b(
            "VAEncSequenceParameterBufferAV1.seq_fields.bit_depth_minus8",
            av1_seq::BIT_BIT_DEPTH_MINUS8,
        );
        b(
            "VAEncSequenceParameterBufferAV1.seq_fields.subsampling_x",
            av1_seq::BIT_SUBSAMPLING_X,
        );
        b(
            "VAEncSequenceParameterBufferAV1.seq_fields.subsampling_y",
            av1_seq::BIT_SUBSAMPLING_Y,
        );
        b(
            "VAEncSequenceParameterBufferAV1.seq_fields.mono_chrome",
            av1_seq::BIT_MONO_CHROME,
        );
        b(
            "VAEncPictureParameterBufferAV1.picture_flags.frame_type",
            av1_pic::BIT_FRAME_TYPE,
        );
        b(
            "VAEncPictureParameterBufferAV1.picture_flags.error_resilient_mode",
            av1_pic::BIT_ERROR_RESILIENT_MODE,
        );
        b(
            "VAEncPictureParameterBufferAV1.picture_flags.disable_frame_end_update_cdf",
            av1_pic::BIT_DISABLE_FRAME_END_UPDATE_CDF,
        );
        b(
            "VAEncPictureParameterBufferAV1.picture_flags.reduced_tx_set",
            av1_pic::BIT_REDUCED_TX_SET,
        );
        b(
            "VAEncPictureParameterBufferAV1.picture_flags.enable_frame_obu",
            av1_pic::BIT_ENABLE_FRAME_OBU,
        );
        b(
            "VAEncPictureParameterBufferAV1.picture_flags.long_term_reference",
            av1_pic::BIT_LONG_TERM_REFERENCE,
        );
        b(
            "VAEncPictureParameterBufferAV1.picture_flags.allow_intrabc",
            av1_pic::BIT_ALLOW_INTRABC,
        );
        // Documented in va.h's VAEncMiscParameterRIR bitfield; the probe does not
        // measure these two, so they are checked against the struct declaration order.
        assert_eq!(misc::rir::BIT_ENABLE_RIR_COLUMN, 0);
        assert_eq!(misc::rir::BIT_ENABLE_RIR_ROW, 1);
    }

    #[test]
    fn serialised_buffers_have_exact_struct_sizes() {
        assert_eq!(
            HevcSeqParams {
                intra_period: 1,
                intra_idr_period: 1,
                ip_period: 1,
                bits_per_second: 1,
                width: 1,
                height: 1
            }
            .to_bytes()
            .len(),
            hevc_seq::SIZE
        );
        let pic = HevcPicParams {
            decoded_curr_pic: VaPictureHevc::invalid(),
            reference_frames: [VaPictureHevc::invalid(); 15],
            coded_buf: 1,
            pic_init_qp: 26,
            num_ref_idx_l0_default_active_minus1: 0,
            nal_unit_type: 1,
            idr: false,
            coding_type: 2,
            reference_pic: true,
        };
        assert_eq!(pic.to_bytes().len(), hevc_pic::SIZE);
        let slice = HevcSliceParams {
            num_ctu_in_slice: 10,
            slice_type: 1,
            num_ref_idx_l0_active_minus1: 0,
            ref_pic_list0: [VaPictureHevc::invalid(); 15],
        };
        assert_eq!(slice.to_bytes().len(), hevc_slice::SIZE);
        assert_eq!(
            rate_control_misc(1000, 90).len(),
            misc::DATA + misc::rate_control::SIZE
        );
        assert_eq!(
            frame_rate_misc(90).len(),
            misc::DATA + misc::frame_rate::SIZE
        );
        assert_eq!(hrd_misc(1000, 90).len(), misc::DATA + misc::hrd::SIZE);
        assert_eq!(rir_misc(true, 0, 1).len(), misc::DATA + misc::rir::SIZE);
        assert_eq!(
            Av1SeqParams {
                seq_level_idx: 5,
                intra_period: 1,
                ip_period: 1,
                bits_per_second: 1
            }
            .to_bytes()
            .len(),
            av1_seq::SIZE
        );
        assert_eq!(
            Av1PicParams {
                width: 64,
                height: 64,
                reconstructed_frame: 1,
                coded_buf: 2,
                reference_frames: [0; 8],
                ref_frame_idx: [0; 7],
                frame_type: AV1_KEY_FRAME,
                refresh_frame_flags: 0xff,
                order_hint: 0,
                primary_ref_frame: 7,
                base_qindex: 100,
                width_in_sbs: 1,
                height_in_sbs: 1,
                enable_frame_obu: false,
            }
            .to_bytes()
            .len(),
            av1_pic::SIZE
        );
        assert_eq!(av1_tile_group_bytes().len(), av1_tile_group::SIZE);
        assert_eq!(
            H264SeqParams {
                level_idc: 42,
                intra_period: 1,
                intra_idr_period: 1,
                ip_period: 1,
                bits_per_second: 1,
                max_num_ref_frames: 1,
                width_mbs: 1,
                height_mbs: 1
            }
            .to_bytes()
            .len(),
            h264_seq::SIZE
        );
        assert_eq!(
            H264PicParams {
                curr_pic: [0; h264_picture::SIZE],
                reference_frames: [[0; h264_picture::SIZE]; 16],
                coded_buf: 1,
                frame_num: 0,
                idr: true,
                reference_pic: true,
                num_ref_idx_l0_active_minus1: 0,
            }
            .to_bytes()
            .len(),
            h264_pic::SIZE
        );
        assert_eq!(
            H264SliceParams {
                num_macroblocks: 1,
                slice_type: 2,
                idr_pic_id: 0,
                pic_order_cnt_lsb: 0,
                num_ref_idx_l0_active_minus1: 0,
                ref_pic_list0: Vec::new(),
            }
            .to_bytes()
            .len(),
            h264_slice::SIZE
        );
    }

    #[test]
    fn hevc_picture_fields_land_at_header_offsets() {
        let mut refs = [VaPictureHevc::invalid(); 15];
        refs[0] = VaPictureHevc {
            picture_id: 77,
            pic_order_cnt: -3,
            flags: VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE,
        };
        let pic = HevcPicParams {
            decoded_curr_pic: VaPictureHevc {
                picture_id: 5,
                pic_order_cnt: 9,
                flags: 0,
            },
            reference_frames: refs,
            coded_buf: 0xABCD,
            pic_init_qp: 26,
            num_ref_idx_l0_default_active_minus1: 0,
            nal_unit_type: HEVC_NAL_TRAIL_R,
            idr: false,
            coding_type: VA_CODING_TYPE_P,
            reference_pic: true,
        }
        .to_bytes();
        let u32_at = |off: usize| u32::from_le_bytes(pic[off..off + 4].try_into().unwrap());
        assert_eq!(
            u32_at(hevc_pic::DECODED_CURR_PIC + hevc_picture::PICTURE_ID),
            5
        );
        assert_eq!(
            u32_at(hevc_pic::REFERENCE_FRAMES + hevc_picture::PICTURE_ID),
            77
        );
        assert_eq!(
            u32_at(hevc_pic::REFERENCE_FRAMES + hevc_picture::FLAGS),
            VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE
        );
        assert_eq!(u32_at(hevc_pic::CODED_BUF), 0xABCD);
        assert_eq!(pic[hevc_pic::NAL_UNIT_TYPE], HEVC_NAL_TRAIL_R);
        let pic_fields = u32_at(hevc_pic::PIC_FIELDS);
        assert_eq!(pic_fields & 1, 0, "not an IDR");
        assert_eq!(
            (pic_fields >> hevc_pic::BIT_CODING_TYPE) & 0x7,
            VA_CODING_TYPE_P
        );
        assert_eq!((pic_fields >> hevc_pic::BIT_REFERENCE_PIC_FLAG) & 1, 1);
    }

    #[test]
    fn misc_payload_fields_are_relative_to_data() {
        let rc = rate_control_misc(20_000, 60);
        assert_eq!(
            u32::from_le_bytes(rc[0..4].try_into().unwrap()),
            VA_ENC_MISC_RATE_CONTROL
        );
        let at = |off: usize| {
            u32::from_le_bytes(
                rc[misc::DATA + off..misc::DATA + off + 4]
                    .try_into()
                    .unwrap(),
            )
        };
        assert_eq!(at(misc::rate_control::BITS_PER_SECOND), 20_000_000);
        assert_eq!(at(misc::rate_control::TARGET_PERCENTAGE), 100);
        assert_eq!(at(misc::rate_control::MAX_QP), 51);
        assert_eq!(at(misc::rate_control::WINDOW_SIZE), 16);
    }
}

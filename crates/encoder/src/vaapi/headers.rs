//! HEVC parameter sets (VPS, SPS, PPS) written by this crate, for drivers that take
//! packed headers. The SPS carries a VUI colour description (video_signal_type with
//! colour_description), which the VA sequence buffer cannot express.
//!
//! Each parameter set is returned as a packed header: an Annex-B NAL unit with a
//! start code and emulation-prevention bytes already inserted, with the bit length
//! that `VAEncPackedHeaderParameterBuffer` takes. Byte-exact output is checked by
//! `ffprobe` in `scripts/libva-abi/check_hevc_vui.sh`, which is independent of libva.

use super::consts::VA_ENC_PACKED_HEADER_TYPE_PICTURE;
use super::consts::VA_ENC_PACKED_HEADER_TYPE_SEQUENCE;
use super::consts::{
    HEVC_LEVEL_5_1, HEVC_MAIN_PROFILE_IDC, HEVC_NAL_PPS, HEVC_NAL_SPS, HEVC_NAL_VPS,
};
use crate::{ColourConfig, EncoderConfig};

/// MSB-first bit writer for RBSP syntax.
#[derive(Debug, Default)]
pub struct BitWriter {
    out: Vec<u8>,
    /// Bits already placed in the last byte of `out` (0..8).
    used: u8,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Writes the low `bits` bits of `v`, most significant first.
    pub fn u(&mut self, bits: u32, v: u64) -> &mut Self {
        for i in (0..bits).rev() {
            self.bit(((v >> i) & 1) as u8);
        }
        self
    }

    pub fn flag(&mut self, b: bool) -> &mut Self {
        self.bit(b as u8)
    }

    /// Unsigned Exp-Golomb code.
    pub fn ue(&mut self, v: u32) -> &mut Self {
        let x = v as u64 + 1;
        let len = 64 - x.leading_zeros();
        for _ in 0..len - 1 {
            self.bit(0);
        }
        self.u(len, x)
    }

    /// Signed Exp-Golomb code.
    pub fn se(&mut self, v: i32) -> &mut Self {
        let k = if v > 0 {
            (2 * v as i64 - 1) as u32
        } else {
            (-2 * v as i64) as u32
        };
        self.ue(k)
    }

    /// `rbsp_trailing_bits()`: a stop bit then zeros to the byte boundary.
    pub fn trailing(&mut self) -> Vec<u8> {
        self.bit(1);
        while self.used != 0 {
            self.bit(0);
        }
        std::mem::take(&mut self.out)
    }

    fn bit(&mut self, b: u8) -> &mut Self {
        if self.used == 0 {
            self.out.push(0);
        }
        let last = self.out.last_mut().expect("byte pushed above");
        *last |= (b & 1) << (7 - self.used);
        self.used = (self.used + 1) % 8;
        self
    }

    /// Bits written so far.
    pub fn bit_len(&self) -> usize {
        if self.used == 0 {
            self.out.len() * 8
        } else {
            (self.out.len() - 1) * 8 + self.used as usize
        }
    }
}

/// Inserts emulation-prevention bytes (`0x000003`) into an RBSP.
pub fn add_emulation_prevention(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 8);
    let mut zeros = 0usize;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// An Annex-B NAL unit: start code, two-byte HEVC NAL header, then the RBSP with
/// emulation-prevention bytes applied.
pub fn hevc_nal(nal_unit_type: u8, rbsp: &[u8]) -> Vec<u8> {
    let mut nal = vec![0, 0, 0, 1, nal_unit_type << 1, 1];
    nal.extend(add_emulation_prevention(rbsp));
    nal
}

/// `profile_tier_level(1, 0)` for Main profile, level 5.1.
fn profile_tier_level(w: &mut BitWriter) {
    w.u(2, 0) // general_profile_space
        .u(1, 0) // general_tier_flag
        .u(5, HEVC_MAIN_PROFILE_IDC as u64) // general_profile_idc
        // general_profile_compatibility_flag[1] and [2] (Main and Main 10).
        .u(32, 0x6000_0000)
        .flag(true) // progressive_source_flag
        .flag(false) // interlaced_source_flag
        .flag(false) // non_packed_constraint_flag
        .flag(true) // frame_only_constraint_flag
        .u(43, 0) // reserved / constraint flags
        .u(1, 0) // general_inbld_flag / reserved
        .u(8, HEVC_LEVEL_5_1 as u64); // general_level_idc
}

/// Video parameter set for one independent layer with no timing info.
pub fn vps_rbsp() -> Vec<u8> {
    let mut w = BitWriter::new();
    w.u(4, 0) // vps_video_parameter_set_id
        .flag(true) // vps_base_layer_internal_flag
        .flag(true) // vps_base_layer_available_flag
        .u(6, 0) // vps_max_layers_minus1
        .u(3, 0) // vps_max_sub_layers_minus1
        .flag(true) // vps_temporal_id_nesting_flag
        .u(16, 0xffff); // vps_reserved_0xffff_16bits
    profile_tier_level(&mut w);
    w.flag(true) // vps_sub_layer_ordering_info_present_flag
        .ue(1) // vps_max_dec_pic_buffering_minus1 (current + one reference)
        .ue(0) // vps_max_num_reorder_pics
        .ue(0) // vps_max_latency_increase_plus1
        .u(6, 0) // vps_max_layer_id
        .ue(0) // vps_num_layer_sets_minus1
        .flag(false) // vps_timing_info_present_flag
        .flag(false); // vps_extension_flag
    w.trailing()
}

/// Sequence parameter set for `config`, with a VUI colour description when
/// `config.colour.signal` is set.
pub fn sps_rbsp(config: &EncoderConfig) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.u(4, 0) // sps_video_parameter_set_id
        .u(3, 0) // sps_max_sub_layers_minus1
        .flag(true); // sps_temporal_id_nesting_flag
    profile_tier_level(&mut w);
    w.ue(0) // sps_seq_parameter_set_id
        .ue(1) // chroma_format_idc: 4:2:0
        .ue(config.width) // pic_width_in_luma_samples
        .ue(config.height) // pic_height_in_luma_samples
        .flag(false) // conformance_window_flag
        .ue(0) // bit_depth_luma_minus8
        .ue(0) // bit_depth_chroma_minus8
        .ue(4) // log2_max_pic_order_cnt_lsb_minus4 (8-bit POC lsb)
        .flag(true) // sps_sub_layer_ordering_info_present_flag
        .ue(1) // sps_max_dec_pic_buffering_minus1
        .ue(0) // sps_max_num_reorder_pics
        .ue(0) // sps_max_latency_increase_plus1
        .ue(0) // log2_min_luma_coding_block_size_minus3 (8x8)
        .ue(2) // log2_diff_max_min_luma_coding_block_size (32x32 CTB)
        .ue(0) // log2_min_luma_transform_block_size_minus2 (4x4)
        .ue(3) // log2_diff_max_min_luma_transform_block_size (32x32)
        .ue(2) // max_transform_hierarchy_depth_inter
        .ue(2) // max_transform_hierarchy_depth_intra
        .flag(false) // scaling_list_enabled_flag
        .flag(false) // amp_enabled_flag
        .flag(true) // sample_adaptive_offset_enabled_flag
        .flag(false) // pcm_enabled_flag
        .ue(0) // num_short_term_ref_pic_sets
        .flag(false) // long_term_ref_pics_present_flag
        .flag(false) // sps_temporal_mvp_enabled_flag
        .flag(false) // strong_intra_smoothing_enabled_flag
        .flag(config.colour.signal); // vui_parameters_present_flag
    if config.colour.signal {
        vui_colour(&mut w, &config.colour);
    }
    w.flag(false); // sps_extension_present_flag
    w.trailing()
}

/// `vui_parameters()` carrying only the colour description.
fn vui_colour(w: &mut BitWriter, c: &ColourConfig) {
    w.flag(false) // aspect_ratio_info_present_flag
        .flag(false) // overscan_info_present_flag
        .flag(true) // video_signal_type_present_flag
        .u(3, 5) // video_format: unspecified
        .flag(c.full_range) // video_full_range_flag
        .flag(true) // colour_description_present_flag
        .u(8, c.primaries as u64)
        .u(8, c.transfer as u64)
        .u(8, c.matrix as u64)
        .flag(false) // chroma_loc_info_present_flag
        .flag(false) // neutral_chroma_indication_flag
        .flag(false) // field_seq_flag
        .flag(false) // frame_field_info_present_flag
        .flag(false) // default_display_window_flag
        .flag(false) // vui_timing_info_present_flag
        .flag(false); // bitstream_restriction_flag
}

/// Picture parameter set matching the slice buffers this backend submits: one slice,
/// no tiles, no weighted prediction, no SAO or deblocking overrides.
pub fn pps_rbsp() -> Vec<u8> {
    let mut w = BitWriter::new();
    w.ue(0) // pps_pic_parameter_set_id
        .ue(0) // pps_seq_parameter_set_id
        .flag(false) // dependent_slice_segments_enabled_flag
        .flag(false) // output_flag_present_flag
        .u(3, 0) // num_extra_slice_header_bits
        .flag(false) // sign_data_hiding_enabled_flag
        .flag(false) // cabac_init_present_flag
        .ue(0) // num_ref_idx_l0_default_active_minus1
        .ue(0) // num_ref_idx_l1_default_active_minus1
        .se(0) // init_qp_minus26
        .flag(false) // constrained_intra_pred_flag
        .flag(false) // transform_skip_enabled_flag
        .flag(false) // cu_qp_delta_enabled_flag
        .se(0) // pps_cb_qp_offset
        .se(0) // pps_cr_qp_offset
        .flag(false) // pps_slice_chroma_qp_offsets_present_flag
        .flag(false) // weighted_pred_flag
        .flag(false) // weighted_bipred_flag
        .flag(false) // transquant_bypass_enabled_flag
        .flag(false) // tiles_enabled_flag
        .flag(false) // entropy_coding_sync_enabled_flag
        .flag(false) // pps_loop_filter_across_slices_enabled_flag
        .flag(false) // deblocking_filter_control_present_flag
        .flag(false) // pps_scaling_list_data_present_flag
        .flag(false) // lists_modification_present_flag
        .ue(0) // log2_parallel_merge_level_minus2
        .flag(false) // slice_segment_header_extension_present_flag
        .flag(false); // pps_extension_present_flag
    w.trailing()
}

/// The HEVC packed headers for a session, as (`VAEncPackedHeaderType`, data) pairs,
/// in the order they must be submitted: VPS, SPS, PPS.
pub fn hevc_parameter_sets(config: &EncoderConfig) -> Vec<(u32, Vec<u8>)> {
    vec![
        (
            VA_ENC_PACKED_HEADER_TYPE_SEQUENCE,
            hevc_nal(HEVC_NAL_VPS, &vps_rbsp()),
        ),
        (
            VA_ENC_PACKED_HEADER_TYPE_SEQUENCE,
            hevc_nal(HEVC_NAL_SPS, &sps_rbsp(config)),
        ),
        (
            VA_ENC_PACKED_HEADER_TYPE_PICTURE,
            hevc_nal(HEVC_NAL_PPS, &pps_rbsp()),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb_codes_match_the_spec_table() {
        // ue(0)=1, ue(1)=010, ue(2)=011, ue(3)=00100; se(1)=010, se(-1)=011.
        let mut w = BitWriter::new();
        w.ue(0).ue(1).ue(2).ue(3).se(1).se(-1);
        let bits: String = (0..w.bit_len())
            .map(|i| {
                let byte = w.out[i / 8];
                if (byte >> (7 - i % 8)) & 1 == 1 {
                    '1'
                } else {
                    '0'
                }
            })
            .collect();
        assert_eq!(bits, "101001100100010011");
    }

    #[test]
    fn emulation_prevention_is_inserted_before_small_bytes() {
        assert_eq!(
            add_emulation_prevention(&[0, 0, 1, 0, 0, 0, 0, 2]),
            vec![0, 0, 3, 1, 0, 0, 3, 0, 0, 3, 2]
        );
        assert_eq!(add_emulation_prevention(&[0, 0, 4]), vec![0, 0, 4]);
    }

    #[test]
    fn hevc_nal_starts_with_start_code_and_header() {
        let nal = hevc_nal(HEVC_NAL_SPS, &[0x80]);
        assert_eq!(&nal[..6], &[0, 0, 0, 1, HEVC_NAL_SPS << 1, 1]);
        assert_eq!(nal[6], 0x80);
    }

    #[test]
    fn sps_colour_description_is_present_only_when_signalled() {
        let mut cfg = EncoderConfig::default();
        let with = sps_rbsp(&cfg);
        cfg.colour.signal = false;
        let without = sps_rbsp(&cfg);
        assert!(with.len() > without.len(), "VUI adds bits");
    }

    /// Writes the packed HEVC parameter sets to the file named by
    /// `LINUX_QUEST_DUMP_HEVC_PS`. Used by `scripts/libva-abi/check_hevc_vui.sh`.
    #[test]
    fn dump_hevc_parameter_sets_for_ffprobe() {
        let Ok(path) = std::env::var("LINUX_QUEST_DUMP_HEVC_PS") else {
            return;
        };
        let bytes: Vec<u8> = hevc_parameter_sets(&EncoderConfig::default())
            .into_iter()
            .flat_map(|(_, b)| b)
            .collect();
        std::fs::write(path, bytes).expect("write parameter sets");
    }

    #[test]
    fn packed_sets_are_sequence_sequence_picture() {
        let sets = hevc_parameter_sets(&EncoderConfig::default());
        let types: Vec<u32> = sets.iter().map(|(t, _)| *t).collect();
        assert_eq!(
            types,
            vec![
                VA_ENC_PACKED_HEADER_TYPE_SEQUENCE,
                VA_ENC_PACKED_HEADER_TYPE_SEQUENCE,
                VA_ENC_PACKED_HEADER_TYPE_PICTURE
            ]
        );
        for (_, bytes) in &sets {
            assert_eq!(&bytes[..4], &[0, 0, 0, 1]);
        }
    }
}

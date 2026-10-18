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

/// Minimum luma coding block size of the SPS this module writes (8x8). HEVC requires
/// the coded picture size to be a multiple of it.
pub const HEVC_MIN_CB_SIZE: u32 = 8;

/// Coded picture size for a `width` x `height` display size: each side rounded up to
/// [`HEVC_MIN_CB_SIZE`]. The SPS carries a conformance window that crops back to the
/// display size.
pub fn hevc_coded_size(width: u32, height: u32) -> (u32, u32) {
    (
        width.next_multiple_of(HEVC_MIN_CB_SIZE),
        height.next_multiple_of(HEVC_MIN_CB_SIZE),
    )
}

/// Sequence parameter set for `config`, with a VUI colour description when
/// `config.colour.signal` is set. When the display size is not a multiple of the
/// minimum coding block size, the SPS codes the rounded-up size and a conformance
/// window crops it back (offsets are in chroma units: 2 luma samples for 4:2:0).
pub fn sps_rbsp(config: &EncoderConfig) -> Vec<u8> {
    let (coded_w, coded_h) = hevc_coded_size(config.width, config.height);
    let crop_right = (coded_w - config.width) / 2;
    let crop_bottom = (coded_h - config.height) / 2;
    let mut w = BitWriter::new();
    w.u(4, 0) // sps_video_parameter_set_id
        .u(3, 0) // sps_max_sub_layers_minus1
        .flag(true); // sps_temporal_id_nesting_flag
    profile_tier_level(&mut w);
    w.ue(0) // sps_seq_parameter_set_id
        .ue(1) // chroma_format_idc: 4:2:0
        .ue(coded_w) // pic_width_in_luma_samples
        .ue(coded_h); // pic_height_in_luma_samples
    if crop_right != 0 || crop_bottom != 0 {
        w.flag(true) // conformance_window_flag
            .ue(0) // conf_win_left_offset
            .ue(crop_right) // conf_win_right_offset
            .ue(0) // conf_win_top_offset
            .ue(crop_bottom); // conf_win_bottom_offset
    } else {
        w.flag(false); // conformance_window_flag
    }
    w.ue(0) // bit_depth_luma_minus8
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

    /// MSB-first bit reader over an RBSP, written independently of `BitWriter`.
    struct BitReader<'a> {
        data: &'a [u8],
        pos: usize,
    }

    impl<'a> BitReader<'a> {
        fn bit(&mut self) -> u32 {
            let b = (self.data[self.pos / 8] >> (7 - self.pos % 8)) & 1;
            self.pos += 1;
            b as u32
        }
        fn bits(&mut self, n: u32) -> u32 {
            (0..n).fold(0, |acc, _| (acc << 1) | self.bit())
        }
        fn ue(&mut self) -> u32 {
            let mut zeros = 0;
            while self.bit() == 0 {
                zeros += 1;
            }
            (1 << zeros) - 1 + self.bits(zeros)
        }
    }

    /// (coded width, coded height, conformance window as left/right/top/bottom) read
    /// back from an SPS RBSP.
    fn parse_sps_size(rbsp: &[u8]) -> (u32, u32, Option<[u32; 4]>) {
        let mut r = BitReader { data: rbsp, pos: 0 };
        r.bits(4 + 3 + 1); // vps id, max sub layers, temporal id nesting
        r.bits(2 + 1 + 5); // profile space, tier, profile idc
        r.bits(32); // compatibility flags
        r.bits(4); // progressive, interlaced, non-packed, frame-only
        r.bits(32);
        r.bits(12); // 43 constraint bits + 1 reserved
        r.bits(8); // level idc
        assert_eq!(r.ue(), 0, "sps id");
        assert_eq!(r.ue(), 1, "4:2:0");
        let (w, h) = (r.ue(), r.ue());
        let window = (r.bit() == 1).then(|| [r.ue(), r.ue(), r.ue(), r.ue()]);
        (w, h, window)
    }

    fn cfg_size(width: u32, height: u32) -> EncoderConfig {
        EncoderConfig {
            width,
            height,
            ..EncoderConfig::default()
        }
    }

    #[test]
    fn sps_round_trips_sizes_that_are_multiples_of_the_min_cb() {
        for (w, h) in [(1920, 1080), (1280, 720), (3840, 2160)] {
            let (cw, ch, window) = parse_sps_size(&sps_rbsp(&cfg_size(w, h)));
            assert_eq!((cw, ch, window), (w, h, None), "{w}x{h}");
        }
    }

    #[test]
    fn sps_conformance_window_crops_1366x768_back_to_the_display_size() {
        let (cw, ch, window) = parse_sps_size(&sps_rbsp(&cfg_size(1366, 768)));
        assert_eq!(
            (cw, ch),
            (1368, 768),
            "coded width rounds up to a multiple of 8"
        );
        // 2 columns cropped = 1 chroma unit on the right; no rows cropped.
        assert_eq!(window, Some([0, 1, 0, 0]));
        assert_eq!(cw - 2 * window.unwrap()[1], 1366);
    }

    /// Value of a syntax element in ffmpeg's `trace_headers` output, which prints
    /// `name  <bits> = <value>` per element.
    fn trace_value(log: &str, name: &str) -> Option<i64> {
        log.lines().find_map(|l| {
            let (left, value) = l.rsplit_once(" = ")?;
            (left.split_whitespace().nth(4) == Some(name))
                .then(|| value.trim().parse().ok())
                .flatten()
        })
    }

    /// Cross-check with ffmpeg's `trace_headers` bitstream filter, which parses the
    /// SPS with its own code. Skipped when ffmpeg is not installed.
    #[test]
    fn ffmpeg_trace_headers_agrees_on_the_coded_size_and_window() {
        use std::process::Command;
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("ffmpeg not installed; skipping the independent SPS parse");
            return;
        }
        let dir = std::env::temp_dir().join(format!("lq-sps-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // (display size, coded size, conformance window right/bottom offsets)
        let cases = [
            ((1366u32, 768u32), (1368i64, 768i64), (1i64, 0i64)),
            ((1920, 1080), (1920, 1080), (0, 0)),
            ((1280, 722), (1280, 728), (0, 3)),
        ];
        for ((w, h), (cw, ch), (right, bottom)) in cases {
            let stream: Vec<u8> = hevc_parameter_sets(&cfg_size(w, h))
                .into_iter()
                .flat_map(|(_, b)| b)
                .collect();
            let path = dir.join(format!("{w}x{h}.h265"));
            std::fs::write(&path, stream).unwrap();
            let out = Command::new("ffmpeg")
                .args(["-v", "trace", "-f", "hevc", "-i"])
                .arg(&path)
                .args(["-c", "copy", "-bsf:v", "trace_headers", "-f", "null", "-"])
                .output()
                .expect("run ffmpeg");
            let log = String::from_utf8_lossy(&out.stderr).to_string();
            let get = |name: &str| trace_value(&log, name);
            assert_eq!(get("pic_width_in_luma_samples"), Some(cw), "{w}x{h}\n{log}");
            assert_eq!(get("pic_height_in_luma_samples"), Some(ch), "{w}x{h}");
            let flag = i64::from(right != 0 || bottom != 0);
            assert_eq!(get("conformance_window_flag"), Some(flag), "{w}x{h}");
            if flag == 1 {
                assert_eq!(get("conf_win_left_offset"), Some(0));
                assert_eq!(get("conf_win_right_offset"), Some(right), "{w}x{h}");
                assert_eq!(get("conf_win_top_offset"), Some(0));
                assert_eq!(get("conf_win_bottom_offset"), Some(bottom), "{w}x{h}");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
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

/* libva ABI probe. Prints "KIND KEY VALUE" lines for the layout facts that
 * linux-quest's vaapi encoder relies on. Built against the real headers.
 *   C  <macro>            compile-time constant value
 *   S  <Type>             sizeof
 *   O  <Type>.<field>     offsetof
 *   B  <Type>.<u>.<bit>   bit position inside bitfield union member <u>
 */
#include <stdio.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include <va/va.h>
#include <va/va_enc_hevc.h>
#include <va/va_enc_h264.h>
#include <va/va_enc_av1.h>
#include <va/va_drmcommon.h>
#include <va/va_vpp.h>

#define C(name) printf("C %s %lld\n", #name, (long long)(name))
#define S(T) printf("S %s %zu\n", #T, sizeof(T))
#define O(T, f) printf("O %s.%s %zu\n", #T, #f, offsetof(T, f))
#define B(T, u, bit)                                                        \
    do {                                                                    \
        T probe_t;                                                          \
        memset(&probe_t, 0, sizeof probe_t);                                \
        probe_t.u.bits.bit = 1;                                             \
        printf("B %s.%s.%s %d\n", #T, #u, #bit,                             \
               __builtin_ctz(probe_t.u.value));                             \
    } while (0)

int main(void) {
    /* ---- va.h enums and macros ---- */
    C(VA_STATUS_SUCCESS); C(VA_PROGRESSIVE); C(VA_INVALID_ID);
    C(VA_ATTRIB_NOT_SUPPORTED);
    C(VAProfileNone); C(VAProfileHEVCMain); C(VAProfileAV1Profile0);
    C(VAProfileH264Main); C(VAProfileH264High);
    C(VA_PICTURE_H264_INVALID); C(VA_PICTURE_H264_SHORT_TERM_REFERENCE);
    C(VAEntrypointVideoProc);
    C(VAEntrypointEncSlice); C(VAEntrypointEncSliceLP);
    C(VAConfigAttribRTFormat); C(VAConfigAttribRateControl);
    C(VAConfigAttribEncPackedHeaders); C(VAConfigAttribEncMaxRefFrames);
    C(VAConfigAttribEncIntraRefresh);
    C(VA_RT_FORMAT_YUV420); C(VA_RT_FORMAT_RGB32);
    C(VA_RC_CBR); C(VA_ENC_PACKED_HEADER_NONE);
    C(VA_ENC_PACKED_HEADER_SEQUENCE); C(VA_ENC_PACKED_HEADER_PICTURE);
    C(VA_ENC_PACKED_HEADER_SLICE);
    C(VA_ENC_INTRA_REFRESH_ROLLING_COLUMN); C(VA_ENC_INTRA_REFRESH_ROLLING_ROW);
    C(VASurfaceAttribMemoryType); C(VASurfaceAttribExternalBufferDescriptor);
    C(VA_SURFACE_ATTRIB_SETTABLE); C(VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2);
    C(VAGenericValueTypeInteger); C(VAGenericValueTypePointer);
    C(VAEncCodedBufferType); C(VAEncSequenceParameterBufferType);
    C(VAEncPictureParameterBufferType); C(VAEncSliceParameterBufferType);
    C(VAEncMiscParameterBufferType); C(VAEncPackedHeaderParameterBufferType);
    C(VAEncPackedHeaderDataBufferType); C(VAProcPipelineParameterBufferType);
    C(VAEncMiscParameterTypeFrameRate); C(VAEncMiscParameterTypeRateControl);
    C(VAEncMiscParameterTypeHRD); C(VAEncMiscParameterTypeRIR);
    C(VAEncPackedHeaderSequence); C(VAEncPackedHeaderPicture);
    C(VAEncPackedHeaderSlice);
    C(VA_PADDING_LOW); C(VA_PADDING_MEDIUM); C(VA_PADDING_HIGH);
    C(VAProcColorStandardNone); C(VAProcColorStandardBT601);
    C(VAProcColorStandardBT709); C(VAProcColorStandardExplicit);
    C(VA_FOURCC_BGRA); C(VA_FOURCC_RGBA); C(VA_FOURCC_NV12);
    C(VA_SOURCE_RANGE_UNKNOWN); C(VA_SOURCE_RANGE_REDUCED); C(VA_SOURCE_RANGE_FULL);
    C(VA_ENC_INTRA_REFRESH_NONE);
    C(VA_PICTURE_HEVC_RPS_ST_CURR_AFTER);
    /* libdrm drm_fourcc.h: fourcc_code(a, b, c, d) as defined there. */
#define FOURCC(a, b, c, d) ((unsigned int)(a) | ((unsigned int)(b) << 8) | ((unsigned int)(c) << 16) | ((unsigned int)(d) << 24))
    printf("C DRM_FORMAT_ARGB8888 %u\n", FOURCC('A', 'R', '2', '4'));
    printf("C DRM_FORMAT_ABGR8888 %u\n", FOURCC('A', 'B', '2', '4'));
    printf("C DRM_FORMAT_NV12 %u\n", FOURCC('N', 'V', '1', '2'));

    /* ---- VAPictureHEVC, HEVC ---- */
    S(VAPictureHEVC); O(VAPictureHEVC, picture_id); O(VAPictureHEVC, pic_order_cnt); O(VAPictureHEVC, flags);
    C(VA_PICTURE_HEVC_INVALID); C(VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE);
    S(VAEncSequenceParameterBufferHEVC);
    O(VAEncSequenceParameterBufferHEVC, general_profile_idc);
    O(VAEncSequenceParameterBufferHEVC, general_level_idc);
    O(VAEncSequenceParameterBufferHEVC, general_tier_flag);
    O(VAEncSequenceParameterBufferHEVC, intra_period);
    O(VAEncSequenceParameterBufferHEVC, intra_idr_period);
    O(VAEncSequenceParameterBufferHEVC, ip_period);
    O(VAEncSequenceParameterBufferHEVC, bits_per_second);
    O(VAEncSequenceParameterBufferHEVC, pic_width_in_luma_samples);
    O(VAEncSequenceParameterBufferHEVC, pic_height_in_luma_samples);
    O(VAEncSequenceParameterBufferHEVC, seq_fields);
    O(VAEncSequenceParameterBufferHEVC, log2_min_luma_coding_block_size_minus3);
    O(VAEncSequenceParameterBufferHEVC, log2_diff_max_min_luma_coding_block_size);
    O(VAEncSequenceParameterBufferHEVC, log2_min_transform_block_size_minus2);
    O(VAEncSequenceParameterBufferHEVC, log2_diff_max_min_transform_block_size);
    O(VAEncSequenceParameterBufferHEVC, max_transform_hierarchy_depth_inter);
    O(VAEncSequenceParameterBufferHEVC, max_transform_hierarchy_depth_intra);
    O(VAEncSequenceParameterBufferHEVC, vui_parameters_present_flag);
    O(VAEncSequenceParameterBufferHEVC, vui_fields);
    O(VAEncSequenceParameterBufferHEVC, vui_num_units_in_tick);
    O(VAEncSequenceParameterBufferHEVC, vui_time_scale);
    B(VAEncSequenceParameterBufferHEVC, seq_fields, chroma_format_idc);
    B(VAEncSequenceParameterBufferHEVC, seq_fields, sample_adaptive_offset_enabled_flag);
    B(VAEncSequenceParameterBufferHEVC, seq_fields, low_delay_seq);

    S(VAEncPictureParameterBufferHEVC);
    O(VAEncPictureParameterBufferHEVC, decoded_curr_pic);
    O(VAEncPictureParameterBufferHEVC, reference_frames);
    O(VAEncPictureParameterBufferHEVC, coded_buf);
    O(VAEncPictureParameterBufferHEVC, collocated_ref_pic_index);
    O(VAEncPictureParameterBufferHEVC, last_picture);
    O(VAEncPictureParameterBufferHEVC, pic_init_qp);
    O(VAEncPictureParameterBufferHEVC, diff_cu_qp_delta_depth);
    O(VAEncPictureParameterBufferHEVC, pps_cb_qp_offset);
    O(VAEncPictureParameterBufferHEVC, pps_cr_qp_offset);
    O(VAEncPictureParameterBufferHEVC, num_tile_columns_minus1);
    O(VAEncPictureParameterBufferHEVC, num_tile_rows_minus1);
    O(VAEncPictureParameterBufferHEVC, column_width_minus1);
    O(VAEncPictureParameterBufferHEVC, row_height_minus1);
    O(VAEncPictureParameterBufferHEVC, log2_parallel_merge_level_minus2);
    O(VAEncPictureParameterBufferHEVC, ctu_max_bitsize_allowed);
    O(VAEncPictureParameterBufferHEVC, num_ref_idx_l0_default_active_minus1);
    O(VAEncPictureParameterBufferHEVC, num_ref_idx_l1_default_active_minus1);
    O(VAEncPictureParameterBufferHEVC, slice_pic_parameter_set_id);
    O(VAEncPictureParameterBufferHEVC, nal_unit_type);
    O(VAEncPictureParameterBufferHEVC, pic_fields);
    O(VAEncPictureParameterBufferHEVC, hierarchical_level_plus1);
    O(VAEncPictureParameterBufferHEVC, scc_fields);
    B(VAEncPictureParameterBufferHEVC, pic_fields, idr_pic_flag);
    B(VAEncPictureParameterBufferHEVC, pic_fields, coding_type);
    B(VAEncPictureParameterBufferHEVC, pic_fields, reference_pic_flag);

    S(VAEncSliceParameterBufferHEVC);
    O(VAEncSliceParameterBufferHEVC, slice_segment_address);
    O(VAEncSliceParameterBufferHEVC, num_ctu_in_slice);
    O(VAEncSliceParameterBufferHEVC, slice_type);
    O(VAEncSliceParameterBufferHEVC, slice_pic_parameter_set_id);
    O(VAEncSliceParameterBufferHEVC, num_ref_idx_l0_active_minus1);
    O(VAEncSliceParameterBufferHEVC, num_ref_idx_l1_active_minus1);
    O(VAEncSliceParameterBufferHEVC, ref_pic_list0);
    O(VAEncSliceParameterBufferHEVC, ref_pic_list1);
    O(VAEncSliceParameterBufferHEVC, luma_log2_weight_denom);
    O(VAEncSliceParameterBufferHEVC, max_num_merge_cand);
    O(VAEncSliceParameterBufferHEVC, slice_qp_delta);
    O(VAEncSliceParameterBufferHEVC, slice_tc_offset_div2);
    O(VAEncSliceParameterBufferHEVC, slice_fields);
    O(VAEncSliceParameterBufferHEVC, pred_weight_table_bit_offset);
    O(VAEncSliceParameterBufferHEVC, pred_weight_table_bit_length);
    B(VAEncSliceParameterBufferHEVC, slice_fields, last_slice_of_pic_flag);

    /* ---- H.264 ---- */
    S(VAPictureH264); O(VAPictureH264, picture_id); O(VAPictureH264, frame_idx); O(VAPictureH264, flags);
    O(VAPictureH264, TopFieldOrderCnt); O(VAPictureH264, BottomFieldOrderCnt);
    S(VAEncSequenceParameterBufferH264);
    O(VAEncSequenceParameterBufferH264, seq_parameter_set_id);
    O(VAEncSequenceParameterBufferH264, level_idc);
    O(VAEncSequenceParameterBufferH264, intra_period);
    O(VAEncSequenceParameterBufferH264, intra_idr_period);
    O(VAEncSequenceParameterBufferH264, ip_period);
    O(VAEncSequenceParameterBufferH264, bits_per_second);
    O(VAEncSequenceParameterBufferH264, max_num_ref_frames);
    O(VAEncSequenceParameterBufferH264, picture_width_in_mbs);
    O(VAEncSequenceParameterBufferH264, picture_height_in_mbs);
    O(VAEncSequenceParameterBufferH264, seq_fields);
    O(VAEncSequenceParameterBufferH264, vui_parameters_present_flag);
    O(VAEncSequenceParameterBufferH264, vui_fields);
    O(VAEncSequenceParameterBufferH264, aspect_ratio_idc);
    O(VAEncSequenceParameterBufferH264, num_units_in_tick);
    O(VAEncSequenceParameterBufferH264, time_scale);
    B(VAEncSequenceParameterBufferH264, seq_fields, chroma_format_idc);
    B(VAEncSequenceParameterBufferH264, seq_fields, frame_mbs_only_flag);
    B(VAEncSequenceParameterBufferH264, seq_fields, direct_8x8_inference_flag);
    B(VAEncSequenceParameterBufferH264, seq_fields, log2_max_frame_num_minus4);
    B(VAEncSequenceParameterBufferH264, seq_fields, pic_order_cnt_type);
    B(VAEncSequenceParameterBufferH264, seq_fields, log2_max_pic_order_cnt_lsb_minus4);
    O(VAEncSequenceParameterBufferH264, frame_cropping_flag);
    O(VAEncSequenceParameterBufferH264, frame_crop_left_offset);
    O(VAEncSequenceParameterBufferH264, frame_crop_right_offset);
    O(VAEncSequenceParameterBufferH264, frame_crop_top_offset);
    O(VAEncSequenceParameterBufferH264, frame_crop_bottom_offset);
    B(VAEncSequenceParameterBufferH264, vui_fields, timing_info_present_flag);
    B(VAEncSequenceParameterBufferH264, vui_fields, bitstream_restriction_flag);
    S(VAEncPictureParameterBufferH264);
    O(VAEncPictureParameterBufferH264, CurrPic);
    O(VAEncPictureParameterBufferH264, ReferenceFrames);
    O(VAEncPictureParameterBufferH264, coded_buf);
    O(VAEncPictureParameterBufferH264, pic_parameter_set_id);
    O(VAEncPictureParameterBufferH264, seq_parameter_set_id);
    O(VAEncPictureParameterBufferH264, last_picture);
    O(VAEncPictureParameterBufferH264, frame_num);
    O(VAEncPictureParameterBufferH264, pic_init_qp);
    O(VAEncPictureParameterBufferH264, num_ref_idx_l0_active_minus1);
    O(VAEncPictureParameterBufferH264, num_ref_idx_l1_active_minus1);
    O(VAEncPictureParameterBufferH264, chroma_qp_index_offset);
    O(VAEncPictureParameterBufferH264, second_chroma_qp_index_offset);
    O(VAEncPictureParameterBufferH264, pic_fields);
    B(VAEncPictureParameterBufferH264, pic_fields, idr_pic_flag);
    B(VAEncPictureParameterBufferH264, pic_fields, reference_pic_flag);
    B(VAEncPictureParameterBufferH264, pic_fields, entropy_coding_mode_flag);
    S(VAEncSliceParameterBufferH264);
    O(VAEncSliceParameterBufferH264, macroblock_address);
    O(VAEncSliceParameterBufferH264, num_macroblocks);
    O(VAEncSliceParameterBufferH264, slice_type);
    O(VAEncSliceParameterBufferH264, pic_parameter_set_id);
    O(VAEncSliceParameterBufferH264, idr_pic_id);
    O(VAEncSliceParameterBufferH264, pic_order_cnt_lsb);
    O(VAEncSliceParameterBufferH264, num_ref_idx_active_override_flag);
    O(VAEncSliceParameterBufferH264, num_ref_idx_l0_active_minus1);
    O(VAEncSliceParameterBufferH264, num_ref_idx_l1_active_minus1);
    O(VAEncSliceParameterBufferH264, RefPicList0);
    O(VAEncSliceParameterBufferH264, RefPicList1);
    O(VAEncSliceParameterBufferH264, slice_qp_delta);
    O(VAEncSliceParameterBufferH264, disable_deblocking_filter_idc);
    O(VAEncSliceParameterBufferH264, slice_alpha_c0_offset_div2);
    O(VAEncSliceParameterBufferH264, slice_beta_offset_div2);
    O(VAEncSliceParameterBufferH264, direct_spatial_mv_pred_flag);
    O(VAEncSliceParameterBufferH264, cabac_init_idc);

    C(VA_LSB_FIRST);
    /* ---- AV1 ---- */
    S(VAEncSequenceParameterBufferAV1);
    O(VAEncSequenceParameterBufferAV1, seq_profile);
    O(VAEncSequenceParameterBufferAV1, seq_level_idx);
    O(VAEncSequenceParameterBufferAV1, seq_tier);
    O(VAEncSequenceParameterBufferAV1, hierarchical_flag);
    O(VAEncSequenceParameterBufferAV1, intra_period);
    O(VAEncSequenceParameterBufferAV1, ip_period);
    O(VAEncSequenceParameterBufferAV1, bits_per_second);
    O(VAEncSequenceParameterBufferAV1, seq_fields);
    O(VAEncSequenceParameterBufferAV1, order_hint_bits_minus_1);
    B(VAEncSequenceParameterBufferAV1, seq_fields, still_picture);
    B(VAEncSequenceParameterBufferAV1, seq_fields, use_128x128_superblock);
    B(VAEncSequenceParameterBufferAV1, seq_fields, enable_order_hint);
    B(VAEncSequenceParameterBufferAV1, seq_fields, bit_depth_minus8);
    B(VAEncSequenceParameterBufferAV1, seq_fields, subsampling_x);
    B(VAEncSequenceParameterBufferAV1, seq_fields, subsampling_y);
    B(VAEncSequenceParameterBufferAV1, seq_fields, mono_chrome);

    S(VAEncPictureParameterBufferAV1);
    O(VAEncPictureParameterBufferAV1, frame_width_minus_1);
    O(VAEncPictureParameterBufferAV1, frame_height_minus_1);
    O(VAEncPictureParameterBufferAV1, reconstructed_frame);
    O(VAEncPictureParameterBufferAV1, coded_buf);
    O(VAEncPictureParameterBufferAV1, reference_frames);
    O(VAEncPictureParameterBufferAV1, ref_frame_idx);
    O(VAEncPictureParameterBufferAV1, hierarchical_level_plus1);
    O(VAEncPictureParameterBufferAV1, primary_ref_frame);
    O(VAEncPictureParameterBufferAV1, order_hint);
    O(VAEncPictureParameterBufferAV1, refresh_frame_flags);
    O(VAEncPictureParameterBufferAV1, ref_frame_ctrl_l0);
    O(VAEncPictureParameterBufferAV1, ref_frame_ctrl_l1);
    O(VAEncPictureParameterBufferAV1, picture_flags);
    O(VAEncPictureParameterBufferAV1, seg_id_block_size);
    O(VAEncPictureParameterBufferAV1, num_tile_groups_minus1);
    O(VAEncPictureParameterBufferAV1, temporal_id);
    O(VAEncPictureParameterBufferAV1, filter_level);
    O(VAEncPictureParameterBufferAV1, filter_level_u);
    O(VAEncPictureParameterBufferAV1, filter_level_v);
    O(VAEncPictureParameterBufferAV1, interpolation_filter);
    O(VAEncPictureParameterBufferAV1, base_qindex);
    O(VAEncPictureParameterBufferAV1, min_base_qindex);
    O(VAEncPictureParameterBufferAV1, max_base_qindex);
    O(VAEncPictureParameterBufferAV1, y_dc_delta_q);
    O(VAEncPictureParameterBufferAV1, tile_cols);
    O(VAEncPictureParameterBufferAV1, tile_rows);
    O(VAEncPictureParameterBufferAV1, width_in_sbs_minus_1);
    O(VAEncPictureParameterBufferAV1, height_in_sbs_minus_1);
    O(VAEncPictureParameterBufferAV1, context_update_tile_id);
    O(VAEncPictureParameterBufferAV1, bit_offset_qindex);
    O(VAEncPictureParameterBufferAV1, size_in_bits_frame_hdr_obu);
    O(VAEncPictureParameterBufferAV1, byte_offset_frame_hdr_obu_size);
    B(VAEncPictureParameterBufferAV1, picture_flags, frame_type);
    B(VAEncPictureParameterBufferAV1, picture_flags, error_resilient_mode);
    B(VAEncPictureParameterBufferAV1, picture_flags, enable_frame_obu);
    B(VAEncPictureParameterBufferAV1, picture_flags, disable_frame_end_update_cdf);
    B(VAEncPictureParameterBufferAV1, picture_flags, allow_intrabc);
    B(VAEncPictureParameterBufferAV1, picture_flags, reduced_tx_set);
    B(VAEncPictureParameterBufferAV1, picture_flags, long_term_reference);
    S(VAEncTileGroupBufferAV1);
    O(VAEncTileGroupBufferAV1, tg_start);
    O(VAEncTileGroupBufferAV1, tg_end);
    O(VAEncTileGroupBufferAV1, va_reserved);
    C(VA_AV1_MAX_SEGMENTS);

    /* ---- Misc parameter payloads and packed headers ---- */
    S(VAEncMiscParameterBuffer); O(VAEncMiscParameterBuffer, type); O(VAEncMiscParameterBuffer, data);
    S(VAEncMiscParameterRateControl);
    O(VAEncMiscParameterRateControl, bits_per_second);
    O(VAEncMiscParameterRateControl, target_percentage);
    O(VAEncMiscParameterRateControl, window_size);
    O(VAEncMiscParameterRateControl, initial_qp);
    O(VAEncMiscParameterRateControl, min_qp);
    O(VAEncMiscParameterRateControl, basic_unit_size);
    O(VAEncMiscParameterRateControl, rc_flags);
    O(VAEncMiscParameterRateControl, ICQ_quality_factor);
    O(VAEncMiscParameterRateControl, max_qp);
    O(VAEncMiscParameterRateControl, quality_factor);
    O(VAEncMiscParameterRateControl, target_frame_size);
    S(VAEncMiscParameterFrameRate); O(VAEncMiscParameterFrameRate, framerate);
    S(VAEncMiscParameterHRD);
    O(VAEncMiscParameterHRD, initial_buffer_fullness);
    O(VAEncMiscParameterHRD, buffer_size);
    S(VAEncMiscParameterRIR);
    O(VAEncMiscParameterRIR, rir_flags);
    O(VAEncMiscParameterRIR, intra_insertion_location);
    O(VAEncMiscParameterRIR, intra_insert_size);
    O(VAEncMiscParameterRIR, qp_delta_for_inserted_intra);
    S(VAEncPackedHeaderParameterBuffer);
    O(VAEncPackedHeaderParameterBuffer, type);
    O(VAEncPackedHeaderParameterBuffer, bit_length);
    O(VAEncPackedHeaderParameterBuffer, has_emulation_bytes);

    /* ---- Surfaces and DRM PRIME ---- */
    S(VAGenericValue); O(VAGenericValue, type); O(VAGenericValue, value);
    S(VASurfaceAttrib); O(VASurfaceAttrib, type); O(VASurfaceAttrib, flags); O(VASurfaceAttrib, value);
    S(VADRMPRIMESurfaceDescriptor);
    O(VADRMPRIMESurfaceDescriptor, fourcc);
    O(VADRMPRIMESurfaceDescriptor, num_objects);
    O(VADRMPRIMESurfaceDescriptor, objects);
    O(VADRMPRIMESurfaceDescriptor, num_layers);
    O(VADRMPRIMESurfaceDescriptor, layers);
    S(VASurfaceAttribExternalBuffers);
    S(VAImage); O(VAImage, image_id); O(VAImage, format); O(VAImage, buf);
    O(VAImage, width); O(VAImage, height); O(VAImage, data_size);
    O(VAImage, num_planes); O(VAImage, pitches); O(VAImage, offsets);
    S(VACodedBufferSegment); O(VACodedBufferSegment, size); O(VACodedBufferSegment, bit_offset);
    O(VACodedBufferSegment, status); O(VACodedBufferSegment, buf); O(VACodedBufferSegment, next);

    /* ---- VPP ---- */
    S(VAProcColorProperties);
    O(VAProcColorProperties, chroma_sample_location);
    O(VAProcColorProperties, color_range);
    O(VAProcColorProperties, colour_primaries);
    O(VAProcColorProperties, transfer_characteristics);
    O(VAProcColorProperties, matrix_coefficients);
    S(VAProcPipelineParameterBuffer);
    O(VAProcPipelineParameterBuffer, surface);
    O(VAProcPipelineParameterBuffer, surface_region);
    O(VAProcPipelineParameterBuffer, surface_color_standard);
    O(VAProcPipelineParameterBuffer, output_region);
    O(VAProcPipelineParameterBuffer, output_background_color);
    O(VAProcPipelineParameterBuffer, output_color_standard);
    O(VAProcPipelineParameterBuffer, pipeline_flags);
    O(VAProcPipelineParameterBuffer, filter_flags);
    O(VAProcPipelineParameterBuffer, filters);
    O(VAProcPipelineParameterBuffer, num_filters);
    O(VAProcPipelineParameterBuffer, forward_references);
    O(VAProcPipelineParameterBuffer, num_forward_references);
    O(VAProcPipelineParameterBuffer, backward_references);
    O(VAProcPipelineParameterBuffer, num_backward_references);
    O(VAProcPipelineParameterBuffer, input_color_properties);
    O(VAProcPipelineParameterBuffer, output_color_properties);
    return 0;
}

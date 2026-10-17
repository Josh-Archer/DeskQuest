//! Unit tests for the VA-API backend, run against [`MockVaLayer`]. Nothing here
//! touches libva or a GPU. The struct layouts are checked against the libva headers
//! in `abi::tests`.

use super::abi::{
    self, av1_pic, av1_seq, av1_tile_group, h264_pic, h264_seq, h264_slice, hevc_pic, hevc_picture,
    hevc_seq, hevc_slice,
};
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
        colour: ColourConfig::default(),
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

/// An RGB DMA-BUF frame. `cpu_copy` controls whether a CPU copy is also present.
fn rgb_dmabuf(w: u32, h: u32, pts: u64, cpu_copy: bool) -> RawFrame {
    RawFrame {
        display_id: 0,
        width: w,
        height: h,
        stride: w * 4,
        format: PixelFormat::Bgra8,
        pts_us: pts,
        dma_buf: Some(vec![DmaBufPlane::create_test_memfd(w * 4, 0, 0)]),
        data: if cpu_copy {
            Bytes::from(vec![0u8; (w * h * 4) as usize])
        } else {
            Bytes::new()
        },
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

/// Buffer kinds of the most recently rendered picture, in submission order.
fn render_kinds(st: &MockVaState) -> Vec<c_int> {
    st.rendered
        .last()
        .expect("a picture was rendered")
        .iter()
        .map(|id| st.params[id].0)
        .collect()
}

/// Bytes of the first rendered buffer of `kind` in the last picture.
fn last_param(st: &MockVaState, kind: c_int) -> Vec<u8> {
    let ids = st.rendered.last().expect("a picture was rendered");
    let id = ids
        .iter()
        .find(|id| st.params[id].0 == kind)
        .expect("buffer kind rendered");
    st.params[id].1.clone()
}

/// Every misc payload type in the last picture, read from the misc header word.
fn last_misc_types(st: &MockVaState) -> Vec<u32> {
    let ids = st.rendered.last().expect("a picture was rendered");
    ids.iter()
        .filter(|id| st.params[id].0 == VA_BUF_ENC_MISC)
        .map(|id| u32::from_le_bytes(st.params[id].1[0..4].try_into().unwrap()))
        .collect()
}

fn u32_at(bytes: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap())
}

fn count_calls(st: &MockVaState, pred: impl Fn(&VaCall) -> bool) -> usize {
    st.calls.iter().filter(|c| pred(c)).count()
}

// ---------------------------------------------------------------------------
// Profile, entrypoint and attribute selection.
// ---------------------------------------------------------------------------

#[test]
fn hevc_prefers_lp_and_requires_cbr() {
    let (_s, mut layer) = mock(MockVaState::hevc_lp());
    let report = probe_with(layer.as_mut(), VideoCodec::Hevc).expect("probe");
    assert_eq!(report.vendor, "mock-vendor");
    assert_eq!(report.selection.profile, VA_PROFILE_HEVC_MAIN);
    assert_eq!(report.selection.entrypoint, VA_ENTRYPOINT_ENC_SLICE_LP);
    assert_eq!(report.selection.intra_refresh, 0);
    assert!(!report.selection.vpp);
}

#[test]
fn hevc_uses_full_slice_when_lp_missing() {
    let st = MockVaState::with_codec(VA_PROFILE_HEVC_MAIN, &[VA_ENTRYPOINT_ENC_SLICE]);
    let (_s, mut layer) = mock(st);
    let sel = select_encode(layer.as_mut(), VideoCodec::Hevc).expect("select");
    assert_eq!(sel.entrypoint, VA_ENTRYPOINT_ENC_SLICE);
}

#[test]
fn hevc_rejects_driver_without_cbr() {
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
fn hevc_rejects_missing_profile() {
    let mut st = MockVaState::hevc_lp();
    st.profiles.clear();
    let (_s, mut layer) = mock(st);
    assert!(matches!(
        select_encode(layer.as_mut(), VideoCodec::Hevc),
        Err(VaError::Unsupported(_))
    ));
}

#[test]
fn av1_is_lp_only() {
    let st = MockVaState::with_codec(VA_PROFILE_AV1_PROFILE0, &[VA_ENTRYPOINT_ENC_SLICE_LP]);
    let (_s, mut layer) = mock(st);
    let sel = select_encode(layer.as_mut(), VideoCodec::Av1).expect("AV1 LP selects");
    assert_eq!(sel.profile, VA_PROFILE_AV1_PROFILE0);
    assert_eq!(sel.entrypoint, VA_ENTRYPOINT_ENC_SLICE_LP);

    let st = MockVaState::with_codec(VA_PROFILE_AV1_PROFILE0, &[VA_ENTRYPOINT_ENC_SLICE]);
    let (_s, mut layer) = mock(st);
    let err = select_encode(layer.as_mut(), VideoCodec::Av1).unwrap_err();
    assert!(
        err.to_string().contains("no usable encode entrypoint"),
        "{err}"
    );
}

#[test]
fn h264_prefers_high_profile_then_main() {
    let st = MockVaState::with_codec(VA_PROFILE_H264_HIGH, &[VA_ENTRYPOINT_ENC_SLICE_LP]);
    let (_s, mut layer) = mock(st);
    assert_eq!(
        select_encode(layer.as_mut(), VideoCodec::H264)
            .unwrap()
            .profile,
        VA_PROFILE_H264_HIGH
    );

    let st = MockVaState::with_codec(VA_PROFILE_H264_MAIN, &[VA_ENTRYPOINT_ENC_SLICE]);
    let (_s, mut layer) = mock(st);
    let sel = select_encode(layer.as_mut(), VideoCodec::H264).expect("main profile");
    assert_eq!(sel.profile, VA_PROFILE_H264_MAIN);
    assert_eq!(sel.entrypoint, VA_ENTRYPOINT_ENC_SLICE);
}

#[test]
fn selection_reports_intra_refresh_and_vpp() {
    let st = MockVaState::hevc_lp()
        .with_intra_refresh(VA_ENC_INTRA_REFRESH_ROLLING_COLUMN)
        .with_vpp();
    let (_s, mut layer) = mock(st);
    let sel = select_encode(layer.as_mut(), VideoCodec::Hevc).unwrap();
    assert_eq!(sel.intra_refresh, VA_ENC_INTRA_REFRESH_ROLLING_COLUMN);
    assert!(sel.vpp);
}

// ---------------------------------------------------------------------------
// Encode call sequence and buffer contents.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hevc_idr_sends_sequence_misc_picture_and_slice() {
    let (shared, layer) = mock(MockVaState::hevc_lp());
    let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 0), layer);
    enc.init(cfg(VideoCodec::Hevc, 0)).await.expect("init");
    let frame = enc.encode(&bgra(1280, 720, 1, [0; 4])).await;
    // The frame is BGRA with a CPU copy, so it goes through the CPU upload path.
    let frame = frame.expect("encode");
    assert!(frame.is_keyframe && !frame.is_intra_refresh);
    let st = shared.lock().unwrap();
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
    assert_eq!(
        last_misc_types(&st),
        vec![
            VA_ENC_MISC_RATE_CONTROL,
            VA_ENC_MISC_FRAME_RATE,
            VA_ENC_MISC_HRD
        ]
    );
    assert_eq!(last_param(&st, VA_BUF_ENC_SEQ).len(), hevc_seq::SIZE);
    assert_eq!(last_param(&st, VA_BUF_ENC_PIC).len(), hevc_pic::SIZE);
    assert_eq!(last_param(&st, VA_BUF_ENC_SLICE).len(), hevc_slice::SIZE);
    // The IDR flag and NAL type are in the picture buffer.
    let pic = last_param(&st, VA_BUF_ENC_PIC);
    assert_eq!(pic[hevc_pic::NAL_UNIT_TYPE], HEVC_NAL_IDR_W_RADL);
    assert_eq!(u32_at(&pic, hevc_pic::PIC_FIELDS) & 1, 1);
    // The slice is the whole picture in one slice: 40 x 23 CTBs of 32 luma samples.
    let slice = last_param(&st, VA_BUF_ENC_SLICE);
    assert_eq!(u32_at(&slice, hevc_slice::NUM_CTU_IN_SLICE), 40 * 23);
    assert_eq!(
        u32_at(&slice, hevc_slice::SLICE_FIELDS),
        1,
        "last slice of picture"
    );
}

#[tokio::test]
async fn hevc_p_frame_references_the_previous_reconstruction() {
    let (shared, layer) = mock(MockVaState::hevc_lp());
    let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 0), layer);
    enc.init(cfg(VideoCodec::Hevc, 0)).await.unwrap();
    enc.encode(&bgra(1280, 720, 1, [0; 4])).await.unwrap();
    let idr_recon = {
        let st = shared.lock().unwrap();
        u32_at(
            &last_param(&st, VA_BUF_ENC_PIC),
            hevc_pic::DECODED_CURR_PIC + hevc_picture::PICTURE_ID,
        )
    };
    let p = enc.encode(&bgra(1280, 720, 2, [0; 4])).await.unwrap();
    assert!(!p.is_keyframe);
    let st = shared.lock().unwrap();
    let pic = last_param(&st, VA_BUF_ENC_PIC);
    assert_eq!(pic[hevc_pic::NAL_UNIT_TYPE], HEVC_NAL_TRAIL_R);
    assert_eq!(
        u32_at(&pic, hevc_pic::REFERENCE_FRAMES + hevc_picture::PICTURE_ID),
        idr_recon
    );
    assert_eq!(
        u32_at(&pic, hevc_pic::REFERENCE_FRAMES + hevc_picture::FLAGS),
        VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE
    );
    // Entry 1 is unused and stays invalid.
    assert_eq!(
        u32_at(
            &pic,
            hevc_pic::REFERENCE_FRAMES + hevc_picture::SIZE + hevc_picture::FLAGS
        ),
        VA_PICTURE_HEVC_INVALID
    );
}

#[tokio::test]
async fn hevc_packed_parameter_sets_precede_the_idr_when_driver_accepts_them() {
    let st = MockVaState::hevc_lp()
        .with_packed_headers(VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_PICTURE);
    let (shared, layer) = mock(st);
    let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 0), layer);
    enc.init(cfg(VideoCodec::Hevc, 0)).await.unwrap();
    enc.encode(&bgra(1280, 720, 1, [0; 4])).await.unwrap();
    let st = shared.lock().unwrap();
    let kinds = render_kinds(&st);
    assert_eq!(
        &kinds[..6],
        &[
            VA_BUF_ENC_PACKED_HEADER_PARAMS,
            VA_BUF_ENC_PACKED_HEADER_DATA,
            VA_BUF_ENC_PACKED_HEADER_PARAMS,
            VA_BUF_ENC_PACKED_HEADER_DATA,
            VA_BUF_ENC_PACKED_HEADER_PARAMS,
            VA_BUF_ENC_PACKED_HEADER_DATA,
        ]
    );
    // The first packed header is the VPS: type sequence, start code, VPS NAL type.
    let ids = st.rendered.last().unwrap();
    let param = &st.params[&ids[0]].1;
    assert_eq!(
        u32_at(param, abi::packed_header::TYPE),
        VA_ENC_PACKED_HEADER_TYPE_SEQUENCE
    );
    assert_eq!(param[abi::packed_header::HAS_EMULATION_BYTES], 1);
    let data = &st.params[&ids[1]].1;
    assert_eq!(&data[..6], &[0, 0, 0, 1, HEVC_NAL_VPS << 1, 1]);
    assert_eq!(
        u32_at(param, abi::packed_header::BIT_LENGTH) as usize,
        data.len() * 8
    );
    // The picture packed header (PPS) is the last pair.
    let pps_param = &st.params[&ids[4]].1;
    assert_eq!(
        u32_at(pps_param, abi::packed_header::TYPE),
        VA_ENC_PACKED_HEADER_TYPE_PICTURE
    );
}

#[tokio::test]
async fn hevc_without_packed_headers_colour_is_not_signalled_but_still_encodes() {
    let (shared, layer) = mock(MockVaState::hevc_lp());
    let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 0), layer);
    enc.init(cfg(VideoCodec::Hevc, 0)).await.unwrap();
    enc.encode(&bgra(1280, 720, 1, [0; 4])).await.unwrap();
    let st = shared.lock().unwrap();
    assert!(!render_kinds(&st).contains(&VA_BUF_ENC_PACKED_HEADER_PARAMS));
}

#[tokio::test]
async fn av1_and_h264_buffers_have_the_verified_sizes() {
    let st = MockVaState::with_codec(VA_PROFILE_AV1_PROFILE0, &[VA_ENTRYPOINT_ENC_SLICE_LP]);
    let (shared, layer) = mock(st);
    let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Av1, 0), layer);
    enc.init(cfg(VideoCodec::Av1, 0)).await.unwrap();
    enc.encode(&bgra(1280, 720, 1, [0; 4])).await.unwrap();
    {
        let st = shared.lock().unwrap();
        assert_eq!(last_param(&st, VA_BUF_ENC_SEQ).len(), av1_seq::SIZE);
        assert_eq!(last_param(&st, VA_BUF_ENC_PIC).len(), av1_pic::SIZE);
        assert_eq!(
            last_param(&st, VA_BUF_ENC_SLICE).len(),
            av1_tile_group::SIZE
        );
        let pic = last_param(&st, VA_BUF_ENC_PIC);
        assert_eq!(
            u32_at(&pic, av1_pic::PICTURE_FLAGS) & 0b11,
            abi::AV1_KEY_FRAME
        );
        assert_eq!(pic[av1_pic::REFRESH_FRAME_FLAGS], 0xff);
    }

    let st = MockVaState::with_codec(VA_PROFILE_H264_HIGH, &[VA_ENTRYPOINT_ENC_SLICE_LP]);
    let (shared, layer) = mock(st);
    let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::H264, 0), layer);
    enc.init(cfg(VideoCodec::H264, 0)).await.unwrap();
    enc.encode(&bgra(1280, 720, 1, [0; 4])).await.unwrap();
    enc.encode(&bgra(1280, 720, 2, [0; 4])).await.unwrap();
    let st = shared.lock().unwrap();
    assert_eq!(last_param(&st, VA_BUF_ENC_PIC).len(), h264_pic::SIZE);
    assert_eq!(last_param(&st, VA_BUF_ENC_SLICE).len(), h264_slice::SIZE);
    // The second picture is a P picture referencing the first.
    let pic = last_param(&st, VA_BUF_ENC_PIC);
    assert_eq!(pic[h264_pic::NUM_REF_IDX_L0_ACTIVE_MINUS1], 0);
    assert_ne!(u32_at(&pic, h264_pic::REFERENCE_FRAMES), VA_INVALID_ID);
    assert_eq!(u32_at(&pic, h264_pic::PIC_FIELDS) & 1, 0, "not an IDR");
    let seq_len = st
        .params
        .values()
        .filter(|(k, _)| *k == VA_BUF_ENC_SEQ)
        .count();
    assert_eq!(seq_len, 1, "sequence buffer is sent once, on the IDR only");
    let seq = st
        .params
        .values()
        .find(|(k, _)| *k == VA_BUF_ENC_SEQ)
        .unwrap();
    assert_eq!(seq.1.len(), h264_seq::SIZE);
    assert_eq!(seq.1[h264_seq::LEVEL_IDC], 51);
}

#[tokio::test]
async fn h264_sequence_fields_encode_420_frame_mbs_only() {
    let st = MockVaState::with_codec(VA_PROFILE_H264_HIGH, &[VA_ENTRYPOINT_ENC_SLICE_LP]);
    let (shared, layer) = mock(st);
    let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::H264, 0), layer);
    enc.init(cfg(VideoCodec::H264, 0)).await.unwrap();
    enc.encode(&bgra(1280, 720, 1, [0; 4])).await.unwrap();
    let st = shared.lock().unwrap();
    let seq = st
        .params
        .values()
        .find(|(k, _)| *k == VA_BUF_ENC_SEQ)
        .unwrap()
        .1
        .clone();
    let fields = u32_at(&seq, h264_seq::SEQ_FIELDS);
    assert_eq!((fields >> h264_seq::BIT_CHROMA_FORMAT_IDC) & 0x3, 1);
    assert_eq!((fields >> h264_seq::BIT_FRAME_MBS_ONLY_FLAG) & 1, 1);
    assert_eq!(
        u16::from_le_bytes([
            seq[h264_seq::PICTURE_WIDTH_IN_MBS],
            seq[h264_seq::PICTURE_WIDTH_IN_MBS + 1]
        ]),
        80
    );
    assert_eq!(
        u16::from_le_bytes([
            seq[h264_seq::PICTURE_HEIGHT_IN_MBS],
            seq[h264_seq::PICTURE_HEIGHT_IN_MBS + 1]
        ]),
        45
    );
}

// ---------------------------------------------------------------------------
// Intra refresh.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rolling_intra_refresh_sends_moving_rir_band_on_p_frames() {
    let st = MockVaState::hevc_lp().with_intra_refresh(VA_ENC_INTRA_REFRESH_ROLLING_COLUMN);
    let (shared, layer) = mock(st);
    // Period 10 frames: the picture is 80 MB columns wide, so 8 columns per frame band.
    let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 10), layer);
    enc.init(cfg(VideoCodec::Hevc, 10)).await.unwrap();
    enc.encode(&bgra(1280, 720, 0, [0; 4])).await.unwrap();

    let mut locations = Vec::new();
    for i in 1..=3u64 {
        let f = enc.encode(&bgra(1280, 720, i, [0; 4])).await.unwrap();
        assert!(!f.is_keyframe);
        assert!(f.is_intra_refresh, "rolling refresh frames are flagged");
        assert_ne!(f.picture_type, 6, "no periodic I-frame");
        let st = shared.lock().unwrap();
        assert!(!render_kinds(&st).contains(&VA_BUF_ENC_SEQ));
        assert_eq!(last_misc_types(&st), vec![VA_ENC_MISC_RIR]);
        let rir = last_param(&st, VA_BUF_ENC_MISC);
        let base = abi::misc::DATA;
        assert_eq!(
            u32_at(&rir, base + abi::misc::rir::RIR_FLAGS),
            1,
            "column mode"
        );
        locations.push(u16::from_le_bytes([
            rir[base + abi::misc::rir::INTRA_INSERTION_LOCATION],
            rir[base + abi::misc::rir::INTRA_INSERTION_LOCATION + 1],
        ]));
        let size = u16::from_le_bytes([
            rir[base + abi::misc::rir::INTRA_INSERT_SIZE],
            rir[base + abi::misc::rir::INTRA_INSERT_SIZE + 1],
        ]);
        assert_eq!(size, 8);
    }
    assert_eq!(locations, vec![8, 16, 24]);
}

#[tokio::test]
async fn periodic_i_frames_are_the_fallback_without_driver_support() {
    let (shared, layer) = mock(MockVaState::hevc_lp());
    let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 3), layer);
    enc.init(cfg(VideoCodec::Hevc, 3)).await.unwrap();
    let mut kinds = Vec::new();
    for i in 0..4u64 {
        let f = enc.encode(&bgra(1280, 720, i, [0; 4])).await.unwrap();
        kinds.push((f.is_keyframe, f.is_intra_refresh));
    }
    // Frame 0 is the IDR (frame_index 1). frame_index 3 is the third encode and
    // is the first flagged full I-frame at period 3.
    assert_eq!(kinds[0], (true, false));
    assert_eq!(kinds[1], (false, false));
    assert_eq!(kinds[2], (false, true));
    let st = shared.lock().unwrap();
    assert!(
        !render_kinds(&st).contains(&VA_BUF_ENC_MISC),
        "no RIR on a periodic I-frame"
    );
}

// ---------------------------------------------------------------------------
// Input paths: NV12 import, VPP for RGB DMA-BUF, CPU fallback.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rgb_dmabuf_is_converted_by_vpp_without_a_cpu_upload() {
    let st = MockVaState::hevc_lp().with_vpp();
    let (shared, layer) = mock(st);
    let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 0), layer);
    enc.init(cfg(VideoCodec::Hevc, 0)).await.unwrap();
    enc.encode(&rgb_dmabuf(1280, 720, 1, false))
        .await
        .expect("VPP path encodes");
    let st = shared.lock().unwrap();
    assert_eq!(
        count_calls(&st, |c| matches!(c, VaCall::Upload { .. })),
        0,
        "no CPU upload"
    );
    // The VPP pipeline buffer was rendered into a context, with the encode input as target.
    let pipeline = st
        .params
        .values()
        .filter(|(k, _)| *k == VA_BUF_PROC_PIPELINE)
        .count();
    assert_eq!(pipeline, 1);
    let pipe = st
        .params
        .values()
        .find(|(k, _)| *k == VA_BUF_PROC_PIPELINE)
        .unwrap()
        .1
        .clone();
    assert_eq!(pipe.len(), abi::vpp::PIPELINE_SIZE);
    assert_eq!(
        u32_at(&pipe, abi::vpp::OUTPUT_COLOR_STANDARD),
        abi::VA_PROC_COLOR_STANDARD_EXPLICIT
    );
    // BT.709 limited output: primaries 1, transfer 1, matrix 1, reduced range.
    assert_eq!(
        pipe[abi::vpp::OUTPUT_COLOR_PROPERTIES + abi::vpp::COLOR_PRIMARIES],
        1
    );
    assert_eq!(
        pipe[abi::vpp::OUTPUT_COLOR_PROPERTIES + abi::vpp::COLOR_MATRIX],
        1
    );
    assert_eq!(
        pipe[abi::vpp::OUTPUT_COLOR_PROPERTIES + abi::vpp::COLOR_RANGE],
        VA_SOURCE_RANGE_REDUCED
    );
    // The imported RGB surface is destroyed after the conversion.
    assert!(count_calls(&st, |c| matches!(c, VaCall::DestroySurfaces { count: 1 })) >= 1);
}

#[tokio::test]
async fn rgb_dmabuf_without_vpp_and_without_cpu_copy_names_vpp() {
    let (_shared, layer) = mock(MockVaState::hevc_lp());
    let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 0), layer);
    enc.init(cfg(VideoCodec::Hevc, 0)).await.unwrap();
    let err = enc
        .encode(&rgb_dmabuf(1280, 720, 1, false))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("VPP"), "{msg}");
}

#[tokio::test]
async fn rgb_dmabuf_with_cpu_copy_falls_back_when_vpp_is_missing() {
    let (shared, layer) = mock(MockVaState::hevc_lp());
    let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 0), layer);
    enc.init(cfg(VideoCodec::Hevc, 0)).await.unwrap();
    enc.encode(&rgb_dmabuf(1280, 720, 1, true))
        .await
        .expect("CPU fallback encodes");
    let st = shared.lock().unwrap();
    assert_eq!(count_calls(&st, |c| matches!(c, VaCall::Upload { .. })), 1);
}

#[tokio::test]
async fn nv12_dmabuf_is_imported_zero_copy() {
    let (shared, layer) = mock(MockVaState::hevc_lp());
    let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 0), layer);
    enc.init(cfg(VideoCodec::Hevc, 0)).await.unwrap();
    enc.encode(&dmabuf_nv12(1280, 720, 1, vec![0x10; 1280 * 720 * 3 / 2]))
        .await
        .unwrap();
    let st = shared.lock().unwrap();
    assert_eq!(count_calls(&st, |c| matches!(c, VaCall::Import { .. })), 1);
    assert_eq!(count_calls(&st, |c| matches!(c, VaCall::Upload { .. })), 0);
}

#[tokio::test]
async fn cpu_path_uploads_one_converted_nv12_frame() {
    let (shared, layer) = mock(MockVaState::hevc_lp());
    let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 0), layer);
    enc.init(cfg(VideoCodec::Hevc, 0)).await.unwrap();
    enc.encode(&bgra(1280, 720, 1, [0, 255, 0, 255]))
        .await
        .unwrap();
    let st = shared.lock().unwrap();
    assert_eq!(count_calls(&st, |c| matches!(c, VaCall::Upload { .. })), 1);
}

// ---------------------------------------------------------------------------
// CPU RGB to NV12 colour conversion.
// ---------------------------------------------------------------------------

fn rgb_frame(w: u32, h: u32, px: [u8; 4]) -> RawFrame {
    bgra(w, h, 0, px)
}

#[test]
fn bt709_limited_white_and_black_hit_the_limited_range_ends() {
    let colour = ColourConfig::default();
    let white = nv12_from_frame(&rgb_frame(2, 2, [255, 255, 255, 255]), &colour).unwrap();
    assert_eq!(&white[..4], &[235, 235, 235, 235]);
    assert_eq!(&white[4..6], &[128, 128]);
    let black = nv12_from_frame(&rgb_frame(2, 2, [0, 0, 0, 255]), &colour).unwrap();
    assert_eq!(&black[..4], &[16, 16, 16, 16]);
}

#[test]
fn full_range_white_is_255() {
    let colour = ColourConfig {
        full_range: true,
        ..Default::default()
    };
    let white = nv12_from_frame(&rgb_frame(2, 2, [255, 255, 255, 255]), &colour).unwrap();
    assert_eq!(white[0], 255);
}

#[test]
fn bt601_and_bt709_green_differ_in_luma() {
    // Pure green has luma weight 0.587 under BT.601 and 0.7152 under BT.709, so BT.709
    // luma of green is higher (173 against 145 in limited range).
    let g601 = nv12_from_frame(
        &rgb_frame(2, 2, [0, 255, 0, 255]),
        &ColourConfig {
            matrix: 6,
            ..Default::default()
        },
    )
    .unwrap()[0];
    let g709 =
        nv12_from_frame(&rgb_frame(2, 2, [0, 255, 0, 255]), &ColourConfig::default()).unwrap()[0];
    assert!(
        g709 > g601,
        "BT.709 luma of green ({g709}) is higher than BT.601 ({g601})"
    );
}

#[test]
fn red_has_high_v_and_low_u_in_limited_range() {
    // Pixel bytes are B, G, R, A: this is pure red.
    let out =
        nv12_from_frame(&rgb_frame(2, 2, [0, 0, 255, 255]), &ColourConfig::default()).unwrap();
    let (cb, cr) = (out[4], out[5]);
    assert!(cr > 200, "V high for red, got {cr}");
    assert!(cb < 128, "U low for red, got {cb}");
}

#[test]
fn nv12_frame_copies_planes_respecting_stride() {
    let w = 4u32;
    let h = 2u32;
    let stride = 8u32;
    let mut data = vec![0u8; (stride * h * 3 / 2) as usize];
    for row in 0..h as usize {
        for x in 0..w as usize {
            data[row * stride as usize + x] = (row * 10 + x) as u8;
        }
    }
    let uv_base = (stride * h) as usize;
    for x in 0..w as usize {
        data[uv_base + x] = 200 + x as u8;
    }
    let frame = RawFrame {
        display_id: 0,
        width: w,
        height: h,
        stride,
        format: PixelFormat::Nv12,
        pts_us: 0,
        dma_buf: None,
        data: Bytes::from(data),
    };
    let out = nv12_from_frame(&frame, &ColourConfig::default()).unwrap();
    assert_eq!(&out[0..4], &[0, 1, 2, 3]);
    assert_eq!(&out[4..8], &[10, 11, 12, 13]);
    assert_eq!(&out[8..12], &[200, 201, 202, 203]);
}

#[test]
fn prime_descriptors_only_match_their_format() {
    let nv12 = dmabuf_nv12(1280, 720, 0, Vec::new());
    assert!(prime_nv12_descriptor(&nv12).is_some());
    assert!(prime_rgb_descriptor(&nv12).is_none());

    let rgb = rgb_dmabuf(1280, 720, 0, false);
    let desc = prime_rgb_descriptor(&rgb).expect("BGRA one-plane DMA-BUF");
    assert_eq!(desc.fourcc, VA_FOURCC_BGRA);
    assert_eq!(desc.drm_format, DRM_FORMAT_ARGB8888);
    assert!(prime_nv12_descriptor(&rgb).is_none());
}

// ---------------------------------------------------------------------------
// Errors, invalidation and lifecycle.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn init_reports_open_failure_reason() {
    let mut st = MockVaState::hevc_lp();
    st.open_error = Some("no render node".into());
    let (_s, layer) = mock(st);
    let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 0), layer);
    let err = enc.init(cfg(VideoCodec::Hevc, 0)).await.unwrap_err();
    assert!(err.to_string().contains("no render node"), "{err}");
}

#[tokio::test]
async fn encode_before_init_is_an_error_not_a_panic() {
    let (_s, layer) = mock(MockVaState::hevc_lp());
    let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 0), layer);
    assert!(enc.encode(&bgra(1280, 720, 1, [0; 4])).await.is_err());
}

#[tokio::test]
async fn invalidated_newest_reference_falls_back_to_older_valid_one() {
    let (shared, layer) = mock(MockVaState::hevc_lp());
    let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 0), layer);
    enc.init(cfg(VideoCodec::Hevc, 0)).await.unwrap();
    enc.encode(&bgra(1280, 720, 10, [0; 4])).await.unwrap(); // IDR, pts 10
    enc.encode(&bgra(1280, 720, 20, [0; 4])).await.unwrap(); // P, pts 20
    enc.invalidate_reference_picture(20);
    // The next frame references the newest valid picture, which is the IDR.
    let next = enc.encode(&bgra(1280, 720, 30, [0; 4])).await.unwrap();
    assert!(!next.is_keyframe);
    assert_eq!(enc.last_rpi_frame(), Some(20));
    let st = shared.lock().unwrap();
    let pic = last_param(&st, VA_BUF_ENC_PIC);
    let refs = u32_at(&pic, hevc_pic::REFERENCE_FRAMES + hevc_picture::PICTURE_ID);
    assert_ne!(refs, VA_INVALID_ID);
}

#[tokio::test]
async fn invalidating_every_reference_codes_a_recovery_i_frame() {
    let (_shared, layer) = mock(MockVaState::hevc_lp());
    let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 0), layer);
    enc.init(cfg(VideoCodec::Hevc, 0)).await.unwrap();
    enc.encode(&bgra(1280, 720, 10, [0; 4])).await.unwrap();
    enc.invalidate_reference_picture(10);
    let rec = enc.encode(&bgra(1280, 720, 20, [0; 4])).await.unwrap();
    assert!(rec.is_keyframe, "non-IDR I-frame is a recovery point");
    assert_eq!(rec.picture_type, 2);
}

#[test]
fn dpb_caps_entries_and_invalidates_by_pts() {
    let mut dpb = Dpb::default();
    for i in 0..6u32 {
        dpb.push(DpbEntry {
            surface: i,
            pts_us: i as u64,
            poc: i as i32,
            valid: true,
        });
    }
    assert_eq!(dpb.len(), DPB_CAPACITY);
    assert!(!dpb.holds(0), "oldest entries are evicted");
    assert!(dpb.invalidate(5));
    assert_eq!(
        dpb.valid_refs(1)[0].surface,
        4,
        "newest valid entry is picked"
    );
}

#[test]
fn libva_missing_library_is_a_reason_not_a_panic() {
    let mut layer = LibVa::with_library_names(
        &["/nonexistent/libva.so.2"],
        &["/nonexistent/libva-drm.so.2"],
    );
    let err = probe_with(&mut layer, VideoCodec::Hevc).unwrap_err();
    assert!(!err.to_string().is_empty());
}

#[test]
fn real_probe_never_panics() {
    // Either a usable driver or a reason; never a panic.
    let _ = probe(VideoCodec::Hevc);
    let _ = probe(VideoCodec::Av1);
    let _ = probe(VideoCodec::H264);
}

#[test]
fn rate_control_and_hrd_payloads_are_sized_to_the_structs() {
    assert_eq!(
        abi::rate_control_misc(20_000, 60).len(),
        abi::misc::DATA + abi::misc::rate_control::SIZE
    );
    assert_eq!(
        abi::hrd_misc(20_000, 60).len(),
        abi::misc::DATA + abi::misc::hrd::SIZE
    );
}

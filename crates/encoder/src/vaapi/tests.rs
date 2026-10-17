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

/// Default options: every opt-in feature off.
const OFF: VaapiOptions = VaapiOptions {
    av1: false,
    packed_headers: false,
};
const AV1_ON: VaapiOptions = VaapiOptions {
    av1: true,
    packed_headers: false,
};
const PACKED_ON: VaapiOptions = VaapiOptions {
    av1: false,
    packed_headers: true,
};

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
    let report = probe_with(layer.as_mut(), VideoCodec::Hevc, &OFF).expect("probe");
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
    let sel = select_encode(layer.as_mut(), VideoCodec::Hevc, &OFF).expect("select");
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
    let err = select_encode(layer.as_mut(), VideoCodec::Hevc, &OFF).unwrap_err();
    assert!(err.to_string().contains("constant-bitrate"), "{err}");
}

#[test]
fn hevc_rejects_missing_profile() {
    let mut st = MockVaState::hevc_lp();
    st.profiles.clear();
    let (_s, mut layer) = mock(st);
    assert!(matches!(
        select_encode(layer.as_mut(), VideoCodec::Hevc, &OFF),
        Err(VaError::Unsupported(_))
    ));
}

#[test]
fn av1_is_lp_only() {
    let st = MockVaState::with_codec(VA_PROFILE_AV1_PROFILE0, &[VA_ENTRYPOINT_ENC_SLICE_LP]);
    let (_s, mut layer) = mock(st);
    let sel = select_encode(layer.as_mut(), VideoCodec::Av1, &AV1_ON).expect("AV1 LP selects");
    assert_eq!(sel.profile, VA_PROFILE_AV1_PROFILE0);
    assert_eq!(sel.entrypoint, VA_ENTRYPOINT_ENC_SLICE_LP);

    let st = MockVaState::with_codec(VA_PROFILE_AV1_PROFILE0, &[VA_ENTRYPOINT_ENC_SLICE]);
    let (_s, mut layer) = mock(st);
    let err = select_encode(layer.as_mut(), VideoCodec::Av1, &AV1_ON).unwrap_err();
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
        select_encode(layer.as_mut(), VideoCodec::H264, &OFF)
            .unwrap()
            .profile,
        VA_PROFILE_H264_HIGH
    );

    let st = MockVaState::with_codec(VA_PROFILE_H264_MAIN, &[VA_ENTRYPOINT_ENC_SLICE]);
    let (_s, mut layer) = mock(st);
    let sel = select_encode(layer.as_mut(), VideoCodec::H264, &OFF).expect("main profile");
    assert_eq!(sel.profile, VA_PROFILE_H264_MAIN);
    assert_eq!(sel.entrypoint, VA_ENTRYPOINT_ENC_SLICE);
}

#[test]
fn selection_reports_intra_refresh_and_vpp() {
    let st = MockVaState::hevc_lp()
        .with_intra_refresh(VA_ENC_INTRA_REFRESH_ROLLING_COLUMN)
        .with_vpp();
    let (_s, mut layer) = mock(st);
    let sel = select_encode(layer.as_mut(), VideoCodec::Hevc, &OFF).unwrap();
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
    let mut enc = VaapiEncoder::with_layer_options(cfg(VideoCodec::Hevc, 0), layer, PACKED_ON);
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
    let mut enc = VaapiEncoder::with_layer_options(cfg(VideoCodec::Av1, 0), layer, AV1_ON);
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

/// Config with an explicit picture size.
fn cfg_sz(codec: VideoCodec, refresh: u32, width: u32, height: u32) -> EncoderConfig {
    EncoderConfig {
        width,
        height,
        ..cfg(codec, refresh)
    }
}

/// What the encoder submitted for one HEVC picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HevcPicture {
    /// The reconstruction surface of this picture.
    recon: u32,
    /// First reference list entry (`VA_INVALID_ID` for intra pictures).
    ref0: u32,
    /// Whether the encoder flagged the frame as a keyframe (IDR or recovery).
    keyframe: bool,
    picture_type: u32,
}

async fn encode_hevc(
    enc: &mut VaapiEncoder,
    shared: &Shared,
    width: u32,
    height: u32,
    pts: u64,
) -> HevcPicture {
    let f = enc
        .encode(&bgra(width, height, pts, [0; 4]))
        .await
        .expect("encode");
    let st = shared.lock().unwrap();
    let pic = last_param(&st, VA_BUF_ENC_PIC);
    HevcPicture {
        recon: u32_at(&pic, hevc_pic::DECODED_CURR_PIC + hevc_picture::PICTURE_ID),
        ref0: u32_at(&pic, hevc_pic::REFERENCE_FRAMES + hevc_picture::PICTURE_ID),
        keyframe: f.is_keyframe,
        picture_type: f.picture_type,
    }
}

/// An initialised small HEVC encoder that has coded an IDR at pts 10 and P pictures
/// at 20, 30 and 40 (each predicted from the one before).
async fn hevc_chain() -> (VaapiEncoder, Shared, Vec<HevcPicture>) {
    let (shared, layer) = mock(MockVaState::hevc_lp());
    let c = cfg_sz(VideoCodec::Hevc, 0, 256, 128);
    let mut enc = VaapiEncoder::with_layer(c.clone(), layer);
    enc.init(c).await.unwrap();
    let mut pics = Vec::new();
    for pts in [10, 20, 30, 40] {
        pics.push(encode_hevc(&mut enc, &shared, 256, 128, pts).await);
    }
    assert!(pics[0].keyframe);
    for i in 1..4 {
        assert!(!pics[i].keyframe);
        assert_eq!(
            pics[i].ref0,
            pics[i - 1].recon,
            "P{i} chains off the previous picture"
        );
    }
    (enc, shared, pics)
}

#[tokio::test]
async fn invalidating_the_newest_picture_falls_back_to_the_one_before_it() {
    let (mut enc, shared, pics) = hevc_chain().await;
    enc.invalidate_reference_picture(40);
    assert_eq!(enc.last_rpi_frame(), Some(40));
    let next = encode_hevc(&mut enc, &shared, 256, 128, 50).await;
    assert!(!next.keyframe, "a clean reference survives");
    assert_eq!(
        next.ref0, pics[2].recon,
        "predicted from the picture before the lost one"
    );
    // The chain continues from the new picture, not the lost one.
    let after = encode_hevc(&mut enc, &shared, 256, 128, 60).await;
    assert_eq!(after.ref0, next.recon);
}

#[tokio::test]
async fn invalidating_a_middle_picture_taints_everything_after_it() {
    let (mut enc, shared, pics) = hevc_chain().await;
    // 30 was lost: 40 was predicted from it, and 20 is gone from the decoder's
    // reference set, so no clean reference remains.
    enc.invalidate_reference_picture(30);
    let rec = encode_hevc(&mut enc, &shared, 256, 128, 50).await;
    assert!(rec.keyframe, "recovery I-frame");
    assert_eq!(rec.picture_type, 2);
    assert_eq!(rec.ref0, VA_INVALID_ID);
    assert!(![pics[2].recon, pics[3].recon].contains(&rec.recon));
    // After the recovery frame, prediction resumes from it.
    let p = encode_hevc(&mut enc, &shared, 256, 128, 60).await;
    assert!(!p.keyframe);
    assert_eq!(p.ref0, rec.recon);
}

#[tokio::test]
async fn invalidating_an_older_picture_taints_the_whole_chain() {
    let (mut enc, shared, _pics) = hevc_chain().await;
    enc.invalidate_reference_picture(20);
    let rec = encode_hevc(&mut enc, &shared, 256, 128, 50).await;
    assert!(rec.keyframe && rec.picture_type == 2);
}

#[tokio::test]
async fn invalidating_a_picture_older_than_the_dpb_taints_the_whole_chain() {
    let (mut enc, shared, _pics) = hevc_chain().await;
    // pts 5 is not held any more, but everything we hold descends from it.
    enc.invalidate_reference_picture(5);
    let rec = encode_hevc(&mut enc, &shared, 256, 128, 50).await;
    assert!(rec.keyframe && rec.picture_type == 2);
}

#[tokio::test]
async fn invalidating_every_picture_codes_a_recovery_i_frame() {
    let (mut enc, shared, _pics) = hevc_chain().await;
    for pts in [10, 20, 30, 40] {
        enc.invalidate_reference_picture(pts);
    }
    let rec = encode_hevc(&mut enc, &shared, 256, 128, 50).await;
    assert!(rec.keyframe && rec.picture_type == 2, "{rec:?}");
    assert_eq!(rec.ref0, VA_INVALID_ID);
}

#[tokio::test]
async fn invalidating_a_future_pts_changes_nothing() {
    let (mut enc, shared, pics) = hevc_chain().await;
    enc.invalidate_reference_picture(9_999);
    let next = encode_hevc(&mut enc, &shared, 256, 128, 50).await;
    assert!(!next.keyframe);
    assert_eq!(next.ref0, pics[3].recon);
}

#[tokio::test]
async fn a_second_loss_after_a_fallback_taints_the_fallback_picture_too() {
    let (mut enc, shared, pics) = hevc_chain().await;
    enc.invalidate_reference_picture(40);
    let fallback = encode_hevc(&mut enc, &shared, 256, 128, 50).await;
    assert_eq!(fallback.ref0, pics[2].recon);
    // The fallback picture (50) is lost as well. Its own reference (30) is still
    // clean and still held by the decoder.
    enc.invalidate_reference_picture(50);
    let again = encode_hevc(&mut enc, &shared, 256, 128, 60).await;
    assert!(!again.keyframe);
    assert_eq!(again.ref0, pics[2].recon);
    // Losing 30 itself leaves nothing.
    enc.invalidate_reference_picture(30);
    let rec = encode_hevc(&mut enc, &shared, 256, 128, 70).await;
    assert!(rec.keyframe && rec.picture_type == 2);
}

#[test]
fn dpb_caps_entries_and_taints_from_the_lost_picture_onward() {
    let entry = |i: u32| DpbEntry {
        surface: i,
        pts_us: i as u64 * 10,
        poc: i as i32,
        frame_num: i,
        valid: true,
    };
    let mut dpb = Dpb::default();
    for i in 0..6 {
        dpb.push(entry(i));
    }
    assert_eq!(dpb.len(), DPB_CAPACITY);
    assert!(!dpb.holds(0), "oldest entries are evicted");
    // Entries 2..=5 are held. Losing 40 (entry 4) taints 4 and 5 but not 2 and 3.
    assert!(dpb.invalidate(40));
    assert_eq!(
        dpb.valid_refs(1)[0].surface,
        3,
        "newest clean entry is picked"
    );
    assert_eq!(dpb.valid_refs(8).len(), 2);
    // Nothing at or after a future pts.
    assert!(!dpb.invalidate(10_000));
    // A pts older than every entry taints them all.
    assert!(dpb.invalidate(1));
    assert!(dpb.valid_refs(1).is_empty());
}

#[test]
fn dpb_push_retaining_keeps_only_the_used_reference() {
    let entry = |i: u32| DpbEntry {
        surface: i,
        pts_us: i as u64,
        poc: i as i32,
        frame_num: i,
        valid: true,
    };
    let mut dpb = Dpb::default();
    dpb.push(entry(1));
    dpb.push(entry(2));
    dpb.push_retaining(entry(3), &[2]);
    assert!(!dpb.holds(1) && dpb.holds(2) && dpb.holds(3));
    dpb.push_retaining(entry(4), &[]);
    assert_eq!(dpb.len(), 1);
    assert_eq!(dpb.newest().map(|e| e.surface), Some(4));
}

#[tokio::test]
async fn h264_keeps_only_the_newest_picture_so_a_loss_means_recovery() {
    let st = MockVaState::with_codec(VA_PROFILE_H264_HIGH, &[VA_ENTRYPOINT_ENC_SLICE_LP]);
    let (_shared, layer) = mock(st);
    let c = cfg_sz(VideoCodec::H264, 0, 256, 128);
    let mut enc = VaapiEncoder::with_layer(c.clone(), layer);
    enc.init(c).await.unwrap();
    for pts in [10, 20, 30] {
        enc.encode(&bgra(256, 128, pts, [0; 4])).await.unwrap();
    }
    // The sliding window holds one reference frame, so there is no older fallback.
    enc.invalidate_reference_picture(30);
    let rec = enc.encode(&bgra(256, 128, 40, [0; 4])).await.unwrap();
    assert!(rec.is_keyframe && rec.picture_type == 2);
}

// ---------------------------------------------------------------------------
// H.264 reference signalling.
// ---------------------------------------------------------------------------

/// Fields of the last H.264 picture and slice buffers.
struct H264Pic {
    frame_num: u16,
    idr: bool,
    cur_idx: u32,
    cur_flags: u32,
    cur_poc: (i32, i32),
    ref0: (u32, u32, u32, i32),
    ref1_flags: u32,
    ref1_id: u32,
    idr_pic_id: u16,
    poc_lsb: u16,
    list0: (u32, u32),
    list0_1_flags: u32,
}

fn read_h264(st: &MockVaState) -> H264Pic {
    let pic = last_param(st, VA_BUF_ENC_PIC);
    let slice = last_param(st, VA_BUF_ENC_SLICE);
    let i32_at = |b: &[u8], off: usize| i32::from_le_bytes(b[off..off + 4].try_into().unwrap());
    let u16_at = |b: &[u8], off: usize| u16::from_le_bytes(b[off..off + 2].try_into().unwrap());
    let rf = h264_pic::REFERENCE_FRAMES;
    let sz = abi::h264_picture::SIZE;
    H264Pic {
        frame_num: u16_at(&pic, h264_pic::FRAME_NUM),
        idr: u32_at(&pic, h264_pic::PIC_FIELDS) & 1 == 1,
        cur_idx: u32_at(&pic, h264_pic::CURR_PIC + abi::h264_picture::FRAME_IDX),
        cur_flags: u32_at(&pic, h264_pic::CURR_PIC + abi::h264_picture::FLAGS),
        cur_poc: (
            i32_at(
                &pic,
                h264_pic::CURR_PIC + abi::h264_picture::TOP_FIELD_ORDER_CNT,
            ),
            i32_at(
                &pic,
                h264_pic::CURR_PIC + abi::h264_picture::BOTTOM_FIELD_ORDER_CNT,
            ),
        ),
        ref0: (
            u32_at(&pic, rf + abi::h264_picture::PICTURE_ID),
            u32_at(&pic, rf + abi::h264_picture::FRAME_IDX),
            u32_at(&pic, rf + abi::h264_picture::FLAGS),
            i32_at(&pic, rf + abi::h264_picture::TOP_FIELD_ORDER_CNT),
        ),
        ref1_flags: u32_at(&pic, rf + sz + abi::h264_picture::FLAGS),
        ref1_id: u32_at(&pic, rf + sz + abi::h264_picture::PICTURE_ID),
        idr_pic_id: u16_at(&slice, h264_slice::IDR_PIC_ID),
        poc_lsb: u16_at(&slice, h264_slice::PIC_ORDER_CNT_LSB),
        list0: (
            u32_at(
                &slice,
                h264_slice::REF_PIC_LIST0 + abi::h264_picture::PICTURE_ID,
            ),
            u32_at(
                &slice,
                h264_slice::REF_PIC_LIST0 + abi::h264_picture::FRAME_IDX,
            ),
        ),
        list0_1_flags: u32_at(
            &slice,
            h264_slice::REF_PIC_LIST0 + sz + abi::h264_picture::FLAGS,
        ),
    }
}

async fn h264_encoder(w: u32, h: u32) -> (VaapiEncoder, Shared) {
    let st = MockVaState::with_codec(VA_PROFILE_H264_HIGH, &[VA_ENTRYPOINT_ENC_SLICE_LP]);
    let (shared, layer) = mock(st);
    let c = cfg_sz(VideoCodec::H264, 0, w, h);
    let mut enc = VaapiEncoder::with_layer(c.clone(), layer);
    enc.init(c).await.unwrap();
    (enc, shared)
}

#[tokio::test]
async fn h264_frame_num_is_zero_at_the_idr_and_references_carry_flags_and_poc() {
    let (mut enc, shared) = h264_encoder(256, 128).await;
    enc.encode(&bgra(256, 128, 1, [0; 4])).await.unwrap();
    let idr = {
        let st = shared.lock().unwrap();
        read_h264(&st)
    };
    assert!(idr.idr);
    assert_eq!(idr.frame_num, 0, "frame_num is 0 at an IDR");
    assert_eq!(idr.cur_idx, 0);
    assert_eq!(idr.cur_flags, VA_PICTURE_H264_SHORT_TERM_REFERENCE);
    assert_eq!(idr.cur_poc, (0, 0));
    assert_eq!(idr.ref0.0, VA_INVALID_ID, "no reference at an IDR");
    assert_eq!(idr.ref0.2, VA_PICTURE_H264_INVALID);
    assert!(idr.idr_pic_id > 0);

    enc.encode(&bgra(256, 128, 2, [0; 4])).await.unwrap();
    let p1 = {
        let st = shared.lock().unwrap();
        read_h264(&st)
    };
    assert!(!p1.idr);
    assert_eq!(p1.frame_num, 1);
    assert_eq!(p1.cur_poc, (2, 2));
    assert_eq!(p1.poc_lsb, 2);
    assert_eq!(p1.ref0.1, 0, "frame_idx of a reference is its frame_num");
    assert_eq!(p1.ref0.2, VA_PICTURE_H264_SHORT_TERM_REFERENCE);
    assert_eq!(p1.ref0.3, 0, "reference POC");
    assert_ne!(p1.ref0.0, VA_INVALID_ID);
    assert_eq!(p1.ref1_id, VA_INVALID_ID);
    assert_eq!(
        p1.ref1_flags, VA_PICTURE_H264_INVALID,
        "unused slot is marked invalid"
    );
    assert_eq!(p1.list0, (p1.ref0.0, 0));
    assert_eq!(p1.list0_1_flags, VA_PICTURE_H264_INVALID);

    enc.encode(&bgra(256, 128, 3, [0; 4])).await.unwrap();
    let p2 = {
        let st = shared.lock().unwrap();
        read_h264(&st)
    };
    assert_eq!(p2.frame_num, 2);
    assert_eq!(p2.ref0.1, 1, "references the previous picture's frame_num");
    assert_eq!(p2.ref0.3, 2);
}

#[tokio::test]
async fn h264_frame_num_does_not_advance_on_a_failed_encode() {
    let (mut enc, shared) = h264_encoder(256, 128).await;
    enc.encode(&bgra(256, 128, 1, [0; 4])).await.unwrap();
    enc.encode(&bgra(256, 128, 2, [0; 4])).await.unwrap();
    shared.lock().unwrap().fail_end_picture = 1;
    assert!(enc.encode(&bgra(256, 128, 3, [0; 4])).await.is_err());
    enc.encode(&bgra(256, 128, 4, [0; 4])).await.unwrap();
    let p = {
        let st = shared.lock().unwrap();
        read_h264(&st)
    };
    assert_eq!(p.frame_num, 2, "no gap after the failure");
    assert_eq!(p.ref0.1, 1, "still predicted from the last coded picture");
    assert_eq!(p.cur_poc.0, 4);
}

#[tokio::test]
async fn h264_a_new_idr_resets_frame_num_and_changes_idr_pic_id() {
    let (mut enc, shared) = h264_encoder(256, 128).await;
    enc.encode(&bgra(256, 128, 1, [0; 4])).await.unwrap();
    let first_id = {
        let st = shared.lock().unwrap();
        read_h264(&st).idr_pic_id
    };
    enc.encode(&bgra(256, 128, 2, [0; 4])).await.unwrap();
    enc.request_keyframe();
    enc.encode(&bgra(256, 128, 3, [0; 4])).await.unwrap();
    let again = {
        let st = shared.lock().unwrap();
        read_h264(&st)
    };
    assert!(again.idr);
    assert_eq!(again.frame_num, 0);
    assert_eq!(again.cur_poc, (0, 0));
    assert_ne!(
        again.idr_pic_id, first_id,
        "consecutive IDRs differ in idr_pic_id"
    );
}

#[tokio::test]
async fn h264_frame_num_wraps_at_the_declared_maximum() {
    let (mut enc, shared) = h264_encoder(64, 64).await;
    for i in 0..258u64 {
        enc.encode(&bgra(64, 64, i + 1, [0; 4])).await.unwrap();
        if i == 255 || i == 256 || i == 257 {
            let st = shared.lock().unwrap();
            let p = read_h264(&st);
            let expect = (i % 256) as u16;
            assert_eq!(p.frame_num, expect, "picture {i}");
            if i == 256 {
                assert_eq!(p.ref0.1, 255, "reference frame_num before the wrap");
            }
            // POC lsb wraps at 256 (poc steps by two).
            assert_eq!(p.poc_lsb as i32, (i as i32 * 2) % 256);
        }
    }
}

#[tokio::test]
async fn h264_sequence_crops_1080_to_1088_rows_and_1366_columns() {
    for (w, h, right, bottom) in [
        (1920u32, 1080u32, 0u32, 4u32),
        (1366, 768, 5, 0),
        (1280, 720, 0, 0),
    ] {
        let (mut enc, shared) = h264_encoder(w, h).await;
        enc.encode(&bgra(w, h, 1, [0; 4])).await.unwrap();
        let st = shared.lock().unwrap();
        let seq = last_param(&st, VA_BUF_ENC_SEQ);
        let flag = seq[h264_seq::FRAME_CROPPING_FLAG];
        assert_eq!(flag == 1, right != 0 || bottom != 0, "{w}x{h}");
        assert_eq!(
            u32_at(&seq, h264_seq::FRAME_CROP_RIGHT_OFFSET),
            right,
            "{w}x{h}"
        );
        assert_eq!(
            u32_at(&seq, h264_seq::FRAME_CROP_BOTTOM_OFFSET),
            bottom,
            "{w}x{h}"
        );
        assert_eq!(u32_at(&seq, h264_seq::FRAME_CROP_LEFT_OFFSET), 0);
        assert_eq!(u32_at(&seq, h264_seq::FRAME_CROP_TOP_OFFSET), 0);
    }
}

// ---------------------------------------------------------------------------
// Opt-in features and init validation.
// ---------------------------------------------------------------------------

#[test]
fn av1_is_not_offered_unless_opted_in() {
    let st = MockVaState::with_codec(VA_PROFILE_AV1_PROFILE0, &[VA_ENTRYPOINT_ENC_SLICE_LP]);
    let (shared, mut layer) = mock(st);
    let err = select_encode(layer.as_mut(), VideoCodec::Av1, &OFF).unwrap_err();
    assert!(err.to_string().contains(VAAPI_AV1_ENV), "{err}");
    let err = probe_with(layer.as_mut(), VideoCodec::Av1, &OFF).unwrap_err();
    assert!(matches!(err, VaError::Unsupported(_)), "{err}");
    assert!(
        shared.lock().unwrap().calls.is_empty(),
        "a disabled codec is refused before the driver is used"
    );
    assert!(probe_with(layer.as_mut(), VideoCodec::Av1, &AV1_ON).is_ok());
}

#[tokio::test]
async fn av1_init_fails_by_default_and_names_the_opt_in() {
    let st = MockVaState::with_codec(VA_PROFILE_AV1_PROFILE0, &[VA_ENTRYPOINT_ENC_SLICE_LP]);
    let (_s, layer) = mock(st);
    let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Av1, 0), layer);
    let err = enc.init(cfg(VideoCodec::Av1, 0)).await.unwrap_err();
    assert!(err.to_string().contains(VAAPI_AV1_ENV), "{err}");
}

#[test]
fn options_read_the_opt_in_environment_variables() {
    // Only the parser is exercised here; the process environment is not modified.
    assert!(!VaapiOptions::default().av1 && !VaapiOptions::default().packed_headers);
    assert!(!env_flag("LINUX_QUEST_VAAPI_TEST_FLAG_THAT_IS_NEVER_SET"));
}

#[tokio::test]
async fn av1_order_hint_fits_the_sequence_bits_and_one_tile_is_declared() {
    let st = MockVaState::with_codec(VA_PROFILE_AV1_PROFILE0, &[VA_ENTRYPOINT_ENC_SLICE_LP]);
    let (shared, layer) = mock(st);
    let c = cfg_sz(VideoCodec::Av1, 0, 128, 128);
    let mut enc = VaapiEncoder::with_layer_options(c.clone(), layer, AV1_ON);
    enc.init(c).await.unwrap();
    let mut max_hint = 0u8;
    for i in 0..300u64 {
        enc.encode(&bgra(128, 128, i + 1, [0; 4])).await.unwrap();
        let st = shared.lock().unwrap();
        let pic = last_param(&st, VA_BUF_ENC_PIC);
        max_hint = max_hint.max(pic[av1_pic::ORDER_HINT]);
        assert_eq!(pic[av1_pic::TILE_COLS], 1);
        assert_eq!(pic[av1_pic::TILE_ROWS], 1);
    }
    assert_eq!(
        max_hint, 127,
        "order_hint wraps at the 7 bits order_hint_bits_minus_1 declares"
    );
    let st = shared.lock().unwrap();
    let seq = st
        .params
        .values()
        .find(|(k, _)| *k == VA_BUF_ENC_SEQ)
        .unwrap()
        .1
        .clone();
    assert_eq!(seq[av1_seq::ORDER_HINT_BITS_MINUS_1], 6);
}

fn rate_control_max_qp(st: &MockVaState) -> u32 {
    let ids = st.rendered.last().unwrap();
    let rc = ids
        .iter()
        .map(|id| &st.params[id])
        .find(|(k, b)| *k == VA_BUF_ENC_MISC && u32_at(b, 0) == VA_ENC_MISC_RATE_CONTROL)
        .expect("rate control buffer");
    u32_at(&rc.1, abi::misc::DATA + abi::misc::rate_control::MAX_QP)
}

#[tokio::test]
async fn max_qp_matches_the_codec_range() {
    let (shared, layer) = mock(MockVaState::hevc_lp());
    let mut enc = VaapiEncoder::with_layer(cfg_sz(VideoCodec::Hevc, 0, 128, 128), layer);
    enc.init(cfg_sz(VideoCodec::Hevc, 0, 128, 128))
        .await
        .unwrap();
    enc.encode(&bgra(128, 128, 1, [0; 4])).await.unwrap();
    assert_eq!(rate_control_max_qp(&shared.lock().unwrap()), 51);

    let st = MockVaState::with_codec(VA_PROFILE_AV1_PROFILE0, &[VA_ENTRYPOINT_ENC_SLICE_LP]);
    let (shared, layer) = mock(st);
    let c = cfg_sz(VideoCodec::Av1, 0, 128, 128);
    let mut enc = VaapiEncoder::with_layer_options(c.clone(), layer, AV1_ON);
    enc.init(c).await.unwrap();
    enc.encode(&bgra(128, 128, 1, [0; 4])).await.unwrap();
    assert_eq!(rate_control_max_qp(&shared.lock().unwrap()), 255);
}

fn config_packed_request(st: &MockVaState) -> u32 {
    st.calls
        .iter()
        .find_map(|c| match c {
            VaCall::CreateConfig { attribs } => attribs
                .iter()
                .find(|(t, _)| *t == VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS)
                .map(|(_, v)| *v),
            _ => None,
        })
        .expect("a config with a packed-header attribute was created")
}

#[tokio::test]
async fn packed_headers_are_off_by_default_even_when_the_driver_accepts_them() {
    let st = MockVaState::hevc_lp()
        .with_packed_headers(VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_PICTURE);
    let (shared, layer) = mock(st);
    let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 0), layer);
    assert!(cfg(VideoCodec::Hevc, 0).colour.signal);
    enc.init(cfg(VideoCodec::Hevc, 0)).await.unwrap();
    enc.encode(&bgra(1280, 720, 1, [0; 4])).await.unwrap();
    let st = shared.lock().unwrap();
    assert!(!render_kinds(&st).contains(&VA_BUF_ENC_PACKED_HEADER_DATA));
    assert_eq!(config_packed_request(&st), VA_ENC_PACKED_HEADER_NONE);
}

#[tokio::test]
async fn packed_headers_opt_in_requests_them_from_the_driver() {
    let st = MockVaState::hevc_lp()
        .with_packed_headers(VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_PICTURE);
    let (shared, layer) = mock(st);
    let mut enc = VaapiEncoder::with_layer_options(cfg(VideoCodec::Hevc, 0), layer, PACKED_ON);
    enc.init(cfg(VideoCodec::Hevc, 0)).await.unwrap();
    let st = shared.lock().unwrap();
    assert_eq!(
        config_packed_request(&st),
        VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_PICTURE
    );
}

#[tokio::test]
async fn hevc_rejects_sizes_that_are_not_a_multiple_of_the_min_cb() {
    for (w, h) in [(1366u32, 768u32), (1920, 1082), (1284, 724)] {
        let (_s, layer) = mock(MockVaState::hevc_lp());
        let mut enc = VaapiEncoder::with_layer(cfg_sz(VideoCodec::Hevc, 0, w, h), layer);
        let err = enc
            .init(cfg_sz(VideoCodec::Hevc, 0, w, h))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("multiples of 8"), "{w}x{h}: {err}");
    }
    for (w, h) in [(1920u32, 1080u32), (1280, 720), (2560, 1440)] {
        let (_s, layer) = mock(MockVaState::hevc_lp());
        let mut enc = VaapiEncoder::with_layer(cfg_sz(VideoCodec::Hevc, 0, w, h), layer);
        enc.init(cfg_sz(VideoCodec::Hevc, 0, w, h))
            .await
            .unwrap_or_else(|e| panic!("{w}x{h}: {e}"));
    }
}

#[tokio::test]
async fn h264_accepts_sizes_that_are_not_a_multiple_of_8() {
    let st = MockVaState::with_codec(VA_PROFILE_H264_HIGH, &[VA_ENTRYPOINT_ENC_SLICE_LP]);
    let (_s, layer) = mock(st);
    let c = cfg_sz(VideoCodec::H264, 0, 1366, 768);
    let mut enc = VaapiEncoder::with_layer(c.clone(), layer);
    enc.init(c).await.unwrap();
}

#[tokio::test]
async fn rolling_refresh_keeps_a_slower_periodic_i_frame_as_a_safety_net() {
    let st = MockVaState::hevc_lp().with_intra_refresh(VA_ENC_INTRA_REFRESH_ROLLING_COLUMN);
    let (_shared, layer) = mock(st);
    let c = cfg_sz(VideoCodec::Hevc, 5, 128, 128);
    let mut enc = VaapiEncoder::with_layer(c.clone(), layer);
    enc.init(c).await.unwrap();
    let mut full_intra = Vec::new();
    for i in 1..=45u64 {
        let f = enc.encode(&bgra(128, 128, i, [0; 4])).await.unwrap();
        if f.picture_type == 6 {
            full_intra.push(i);
        }
    }
    let every = 5 * ROLLING_SAFETY_NET_FACTOR;
    assert_eq!(full_intra, vec![every, 2 * every]);
}

// ---------------------------------------------------------------------------
// Teardown order and upload bounds.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn teardown_destroys_contexts_then_configs_then_surfaces_vpp_context_first() {
    let st = MockVaState::hevc_lp().with_vpp();
    let (shared, layer) = mock(st);
    let mut enc = VaapiEncoder::with_layer(cfg(VideoCodec::Hevc, 0), layer);
    enc.init(cfg(VideoCodec::Hevc, 0)).await.unwrap();
    enc.encode(&rgb_dmabuf(1280, 720, 1, false)).await.unwrap();
    shared.lock().unwrap().calls.clear();
    drop(enc);
    let st = shared.lock().unwrap();
    let kinds: Vec<&str> = st
        .calls
        .iter()
        .map(|c| match c {
            VaCall::DestroyBuffer { .. } => "buffer",
            VaCall::DestroyContext { .. } => "context",
            VaCall::DestroyConfig { .. } => "config",
            VaCall::DestroySurfaces { .. } => "surfaces",
            other => panic!("unexpected call during teardown: {other:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        vec!["buffer", "context", "context", "config", "config", "surfaces"]
    );
    let contexts: Vec<u32> = st
        .calls
        .iter()
        .filter_map(|c| match c {
            VaCall::DestroyContext { context } => Some(*context),
            _ => None,
        })
        .collect();
    // The VPP context was created after the encode context, and goes first.
    assert!(contexts[0] > contexts[1], "{contexts:?}");
}

#[test]
fn nv12_image_bounds_accept_a_tight_image_and_reject_an_overrun() {
    // 64x32: Y plane 64 x 32 at offset 0 pitch 64, UV plane 64 x 16 at offset 2048.
    assert!(check_nv12_image_bounds(64, 32, 64, 64, 0, 2048, 3072).is_ok());
    // One byte short of the last chroma row.
    assert!(check_nv12_image_bounds(64, 32, 64, 64, 0, 2048, 3071).is_err());
    // Pitch narrower than the row.
    assert!(check_nv12_image_bounds(64, 32, 32, 64, 0, 2048, 3072).is_err());
    // Padded pitch pushes the last row out of the buffer.
    assert!(check_nv12_image_bounds(64, 32, 128, 64, 0, 4096, 4096 + 64 * 16).is_ok());
    assert!(check_nv12_image_bounds(64, 32, 128, 64, 0, 4096, 5000).is_err());
    // Offsets that overflow usize are an error, not a wrap.
    assert!(check_nv12_image_bounds(64, 32, 64, 64, usize::MAX, 0, 3072).is_err());
}

#[test]
fn va_image_layout_matches_the_header() {
    use std::mem::offset_of;
    // VAImage (libva 2.20): image_id 0, format 4 (VAImageFormat is 48 bytes), buf 52,
    // width 56, height 58, data_size 60, num_planes 64, pitches 68, offsets 80.
    assert_eq!(std::mem::size_of::<VaImageFormat>(), 48);
    assert_eq!(offset_of!(VaImage, format), 4);
    assert_eq!(offset_of!(VaImage, buf), 52);
    assert_eq!(offset_of!(VaImage, width), 56);
    assert_eq!(offset_of!(VaImage, data_size), 60);
    assert_eq!(offset_of!(VaImage, num_planes), 64);
    assert_eq!(offset_of!(VaImage, pitches), 68);
    assert_eq!(offset_of!(VaImage, offsets), 80);
}

#[test]
fn libva_missing_library_is_a_reason_not_a_panic() {
    let mut layer = LibVa::with_library_names(
        &["/nonexistent/libva.so.2"],
        &["/nonexistent/libva-drm.so.2"],
    );
    let err = probe_with(&mut layer, VideoCodec::Hevc, &OFF).unwrap_err();
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
        abi::rate_control_misc(20_000, 51).len(),
        abi::misc::DATA + abi::misc::rate_control::SIZE
    );
    assert_eq!(
        abi::hrd_misc(20_000).len(),
        abi::misc::DATA + abi::misc::hrd::SIZE
    );
}

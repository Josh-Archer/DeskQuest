pub mod audio;
pub mod nvenc;
pub mod opus;
pub mod vaapi;

use std::str::FromStr;

use async_trait::async_trait;
pub use audio::{
    AudioApplication, AudioEncoder, AudioEncoderConfig, AudioEncoderError, AutoAudioEncoder,
    EncodedAudioFrame, MockAudioEncoder,
};
use linux_quest_capture::RawFrame;
use linux_quest_protocol::{VideoChunk, VideoChunkMeta, VideoCodec};
pub use nvenc::NvencEncoder;
pub use opus::OpusAudioEncoder;
use thiserror::Error;
use tracing::{info, warn};
pub use vaapi::{VaLayer, VaapiEncoder};

#[derive(Error, Debug)]
pub enum EncoderError {
    #[error("Encoder initialization failed: {0}")]
    InitFailed(String),

    #[error("Frame encoding failed: {0}")]
    EncodeFailed(String),

    #[error("Unsupported codec: {0:?}")]
    UnsupportedCodec(VideoCodec),
}

/// Colour description of the encoded stream and of the RGB to NV12 conversion.
///
/// Values are ITU-T H.273 code points. The default is BT.709 with limited (video)
/// range, which is what HD content and most decoders expect. The HEVC VA-API path
/// signals this in the VUI only when crate-written packed parameter sets are opted
/// in to and the driver accepts them; otherwise nothing is signalled. See the
/// `vaapi` module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColourConfig {
    /// colour_primaries: 1 = BT.709, 6 = BT.601 (525), 9 = BT.2020.
    pub primaries: u8,
    /// transfer_characteristics: 1 = BT.709, 13 = sRGB/sYCC.
    pub transfer: u8,
    /// matrix_coeffs: 1 = BT.709, 6 = BT.601, 9 = BT.2020 non-constant luminance.
    pub matrix: u8,
    /// True for full range (0-255 luma), false for limited (16-235 luma).
    pub full_range: bool,
    /// Write the colour description into the bitstream (VUI) when the backend can.
    pub signal: bool,
}

impl Default for ColourConfig {
    fn default() -> Self {
        Self {
            primaries: 1,
            transfer: 1,
            matrix: 1,
            full_range: false,
            signal: true,
        }
    }
}

impl ColourConfig {
    /// Luma and chroma kr/kb coefficients for the matrix. Returns `(kr, kb)`.
    pub fn luma_coefficients(&self) -> (f32, f32) {
        match self.matrix {
            6 | 5 => (0.299, 0.114),
            9 | 10 => (0.2627, 0.0593),
            _ => (0.2126, 0.0722),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub intra_refresh_period: u32,
    pub max_chunk_size: usize,
    pub colour: ColourConfig,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            codec: VideoCodec::Av1,
            width: 1920,
            height: 1080,
            fps: 90,
            bitrate_kbps: 80_000,
            intra_refresh_period: 30,
            max_chunk_size: 1200,
            colour: ColourConfig::default(),
        }
    }
}

/// Codecs a backend can encode, most preferred first.
pub fn backend_codecs(kind: BackendKind) -> &'static [VideoCodec] {
    match kind {
        BackendKind::Nvenc => &[VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264],
        BackendKind::Vaapi => &[VideoCodec::Hevc, VideoCodec::Av1, VideoCodec::H264],
        BackendKind::Mock => &[VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264],
    }
}

#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub frame_id: u64,
    pub is_keyframe: bool,
    pub is_intra_refresh: bool,
    pub picture_type: u32,
    pub chunks: Vec<VideoChunk>,
    pub encode_duration_us: u32,
}

#[async_trait]
pub trait VideoEncoder: Send + Sync {
    async fn init(&mut self, config: EncoderConfig) -> Result<(), EncoderError>;
    async fn encode(&mut self, frame: &RawFrame) -> Result<EncodedFrame, EncoderError>;
    fn request_keyframe(&mut self);
    fn invalidate_reference_picture(&mut self, pts_us: u64);
}

/// Software mock encoder for testing chunking, rate control, and RPI loops.
pub struct MockVideoEncoder {
    config: EncoderConfig,
    frame_counter: u64,
    force_keyframe: bool,
    last_rpi_frame: Option<u64>,
}

impl MockVideoEncoder {
    pub fn new(config: EncoderConfig) -> Self {
        Self {
            config,
            frame_counter: 0,
            force_keyframe: true,
            last_rpi_frame: None,
        }
    }

    pub fn last_rpi_frame(&self) -> Option<u64> {
        self.last_rpi_frame
    }

    pub fn last_rpi_pts(&self) -> Option<u64> {
        self.last_rpi_frame
    }
}

#[async_trait]
impl VideoEncoder for MockVideoEncoder {
    async fn init(&mut self, config: EncoderConfig) -> Result<(), EncoderError> {
        self.config = config;
        self.frame_counter = 0;
        self.force_keyframe = true;
        Ok(())
    }

    async fn encode(&mut self, frame: &RawFrame) -> Result<EncodedFrame, EncoderError> {
        self.frame_counter += 1;
        let is_keyframe = self.force_keyframe;
        let is_intra_refresh = !is_keyframe
            && self.config.intra_refresh_period > 0
            && self
                .frame_counter
                .is_multiple_of(self.config.intra_refresh_period as u64);
        self.force_keyframe = false;

        // Produce simulated bitstream from raw frame data
        let sample_len = (frame.data.len() / 200).clamp(64, 4096);
        let simulated_payload = frame.data.slice(..sample_len.min(frame.data.len()));

        let chunk_size = self.config.max_chunk_size.max(512);
        let total_chunks = simulated_payload.len().div_ceil(chunk_size) as u16;

        let mut chunks = Vec::with_capacity(total_chunks as usize);
        for i in 0..total_chunks {
            let start = (i as usize) * chunk_size;
            let end = (start + chunk_size).min(simulated_payload.len());
            let chunk_data = simulated_payload.slice(start..end);

            let meta = VideoChunkMeta {
                frame_id: self.frame_counter,
                chunk_index: i,
                total_chunks,
                codec: self.config.codec,
                is_keyframe,
                is_intra_refresh,
                width: self.config.width,
                height: self.config.height,
                fps: self.config.fps,
                pts_us: frame.pts_us,
            };

            chunks.push(VideoChunk::new(meta, chunk_data));
        }

        let picture_type = if is_keyframe {
            3 // IDR
        } else if is_intra_refresh {
            6 // Intra Refresh
        } else {
            0 // P-frame
        };

        Ok(EncodedFrame {
            frame_id: self.frame_counter,
            is_keyframe,
            is_intra_refresh,
            picture_type,
            chunks,
            encode_duration_us: 1500, // Simulated 1.5ms
        })
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn invalidate_reference_picture(&mut self, pts_us: u64) {
        self.last_rpi_frame = Some(pts_us);
    }
}

/// A concrete video encoder implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Nvenc,
    Vaapi,
    Mock,
}

impl BackendKind {
    pub fn name(self) -> &'static str {
        match self {
            BackendKind::Nvenc => "nvenc",
            BackendKind::Vaapi => "vaapi",
            BackendKind::Mock => "mock",
        }
    }
}

/// Backend selection requested by the user (config file, CLI or environment).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendPreference {
    /// NVENC, then VA-API. Never silently picks the mock on its own.
    #[default]
    Auto,
    Nvenc,
    Vaapi,
    /// Software mock. Only for tests and development.
    Mock,
}

impl BackendPreference {
    /// Environment variable read by [`BackendPreference::from_env`].
    pub const ENV_VAR: &'static str = "LINUX_QUEST_VIDEO_ENCODER";

    /// Reads [`Self::ENV_VAR`]. Unset or empty means `Auto`. Invalid values warn and mean `Auto`.
    pub fn from_env() -> Self {
        match std::env::var(Self::ENV_VAR) {
            Ok(v) if !v.trim().is_empty() => v.parse().unwrap_or_else(|e| {
                warn!("Ignoring {}: {e}", Self::ENV_VAR);
                Self::Auto
            }),
            _ => Self::Auto,
        }
    }
}

impl FromStr for BackendPreference {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(Self::Auto),
            "nvenc" | "nvidia" => Ok(Self::Nvenc),
            "vaapi" | "va-api" | "va" => Ok(Self::Vaapi),
            "mock" | "software" => Ok(Self::Mock),
            other => Err(format!(
                "unknown video encoder '{other}' (expected auto, nvenc, vaapi or mock)"
            )),
        }
    }
}

/// Hardware backends to try, in order, for a preference. The mock is never listed
/// here; it is only chosen explicitly or as the last resort in [`AutoVideoEncoder`].
pub fn hardware_order(preference: BackendPreference) -> &'static [BackendKind] {
    match preference {
        BackendPreference::Auto => &[BackendKind::Nvenc, BackendKind::Vaapi],
        BackendPreference::Nvenc => &[BackendKind::Nvenc],
        BackendPreference::Vaapi => &[BackendKind::Vaapi],
        BackendPreference::Mock => &[],
    }
}

/// Result of probing one backend without keeping it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendProbe {
    pub kind: BackendKind,
    pub available: bool,
    /// Why the backend is or is not usable. Always set.
    pub reason: String,
}

/// Probes every backend for `codec` and reports each one's status and reason.
pub fn probe_backends(codec: VideoCodec) -> Vec<BackendProbe> {
    let nvenc = match NvencEncoder::probe() {
        Ok(()) => BackendProbe {
            kind: BackendKind::Nvenc,
            available: true,
            reason: "NVENC runtime present (driver API >= 12.1)".into(),
        },
        Err(reason) => BackendProbe {
            kind: BackendKind::Nvenc,
            available: false,
            reason,
        },
    };
    let vaapi = match probe_vaapi(codec) {
        Ok(report) => BackendProbe {
            kind: BackendKind::Vaapi,
            available: true,
            reason: format!(
                "VA-API usable on {} (profile {}, entrypoint {}); unverified on hardware",
                report.vendor, report.selection.profile, report.selection.entrypoint
            ),
        },
        Err(e) => BackendProbe {
            kind: BackendKind::Vaapi,
            available: false,
            reason: e.to_string(),
        },
    };
    vec![
        nvenc,
        vaapi,
        BackendProbe {
            kind: BackendKind::Mock,
            available: true,
            reason: "software mock (no encoding, test and development only)".into(),
        },
    ]
}

#[cfg(feature = "vaapi")]
fn probe_vaapi(codec: VideoCodec) -> Result<vaapi::ProbeReport, vaapi::VaError> {
    vaapi::probe(codec)
}

#[cfg(not(feature = "vaapi"))]
fn probe_vaapi(_codec: VideoCodec) -> Result<vaapi::ProbeReport, vaapi::VaError> {
    Err(vaapi::VaError::Unavailable(
        "VA-API support is disabled at compile time; rebuild with the `vaapi` feature".into(),
    ))
}

/// Builds and initialises the first hardware encoder in the preference order.
/// Returns an error listing each backend's failure reason when none initialises.
/// Never falls back to the mock; `BackendPreference::Mock` builds the mock directly.
pub async fn create_encoder(
    config: EncoderConfig,
    preference: BackendPreference,
) -> Result<(BackendKind, Box<dyn VideoEncoder>), EncoderError> {
    if preference == BackendPreference::Mock {
        let mut mock: Box<dyn VideoEncoder> = Box::new(MockVideoEncoder::new(config.clone()));
        mock.init(config).await?;
        return Ok((BackendKind::Mock, mock));
    }
    let mut failures = Vec::new();
    for &kind in hardware_order(preference) {
        let mut candidate = build_backend(kind, &config);
        match candidate.init(config.clone()).await {
            Ok(()) => {
                info!("Video encoder backend selected: {}", kind.name());
                return Ok((kind, candidate));
            }
            Err(e) => failures.push(format!("{}: {e}", kind.name())),
        }
    }
    Err(EncoderError::InitFailed(format!(
        "no hardware video encoder available ({})",
        failures.join("; ")
    )))
}

fn build_backend(kind: BackendKind, config: &EncoderConfig) -> Box<dyn VideoEncoder> {
    match kind {
        BackendKind::Nvenc => Box::new(NvencEncoder::new(config.clone())),
        BackendKind::Vaapi => build_vaapi(config),
        BackendKind::Mock => Box::new(MockVideoEncoder::new(config.clone())),
    }
}

#[cfg(feature = "vaapi")]
fn build_vaapi(config: &EncoderConfig) -> Box<dyn VideoEncoder> {
    Box::new(VaapiEncoder::new(config.clone()))
}

#[cfg(not(feature = "vaapi"))]
fn build_vaapi(config: &EncoderConfig) -> Box<dyn VideoEncoder> {
    Box::new(UnavailableVaapiEncoder {
        config: config.clone(),
    })
}

#[cfg(not(feature = "vaapi"))]
struct UnavailableVaapiEncoder {
    config: EncoderConfig,
}

#[cfg(not(feature = "vaapi"))]
#[async_trait]
impl VideoEncoder for UnavailableVaapiEncoder {
    async fn init(&mut self, config: EncoderConfig) -> Result<(), EncoderError> {
        self.config = config;
        Err(EncoderError::InitFailed(
            "VA-API support is disabled at compile time; rebuild with the `vaapi` feature".into(),
        ))
    }

    async fn encode(&mut self, _frame: &RawFrame) -> Result<EncodedFrame, EncoderError> {
        Err(EncoderError::EncodeFailed(
            "VA-API support is disabled at compile time".into(),
        ))
    }

    fn request_keyframe(&mut self) {}

    fn invalidate_reference_picture(&mut self, _pts_us: u64) {}
}

/// Picks a backend at init: the preference's hardware order, then the software mock.
/// The mock last resort is kept so streaming still works on hosts without a GPU.
pub struct AutoVideoEncoder {
    preference: BackendPreference,
    inner: Box<dyn VideoEncoder>,
    selected: BackendKind,
}

impl AutoVideoEncoder {
    /// Uses [`BackendPreference::from_env`].
    pub fn new(config: EncoderConfig) -> Self {
        Self::with_preference(config, BackendPreference::from_env())
    }

    pub fn with_preference(config: EncoderConfig, preference: BackendPreference) -> Self {
        Self {
            preference,
            inner: Box::new(MockVideoEncoder::new(config)),
            selected: BackendKind::Mock,
        }
    }

    /// The backend that `init` settled on. Mock until `init` succeeds.
    pub fn selected_backend(&self) -> BackendKind {
        self.selected
    }
}

#[async_trait]
impl VideoEncoder for AutoVideoEncoder {
    async fn init(&mut self, config: EncoderConfig) -> Result<(), EncoderError> {
        let mut chain: Vec<BackendKind> = hardware_order(self.preference).to_vec();
        chain.push(BackendKind::Mock);
        let mut failures = Vec::new();
        for kind in chain {
            let mut candidate = build_backend(kind, &config);
            match candidate.init(config.clone()).await {
                Ok(()) => {
                    self.inner = candidate;
                    self.selected = kind;
                    return Ok(());
                }
                Err(e) => {
                    warn!(
                        "{} video encoder failed to initialize: {e}. Trying the next backend",
                        kind.name()
                    );
                    failures.push(format!("{}: {e}", kind.name()));
                }
            }
        }
        Err(EncoderError::InitFailed(failures.join("; ")))
    }

    async fn encode(&mut self, frame: &RawFrame) -> Result<EncodedFrame, EncoderError> {
        self.inner.encode(frame).await
    }

    fn request_keyframe(&mut self) {
        self.inner.request_keyframe();
    }

    fn invalidate_reference_picture(&mut self, pts_us: u64) {
        self.inner.invalidate_reference_picture(pts_us);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use nvenc::{NV_ENC_CODEC_AV1_GUID, NV_ENC_CODEC_HEVC_GUID};

    #[tokio::test]
    async fn test_mock_encoder_chunking() {
        let config = EncoderConfig {
            codec: VideoCodec::Av1,
            width: 1920,
            height: 1080,
            fps: 90,
            bitrate_kbps: 80_000,
            intra_refresh_period: 30,
            max_chunk_size: 1024,
            colour: ColourConfig::default(),
        };

        let mut encoder = MockVideoEncoder::new(config);
        let raw = RawFrame {
            display_id: 0,
            width: 1920,
            height: 1080,
            stride: 1920 * 4,
            format: linux_quest_capture::PixelFormat::Rgba8,
            pts_us: 1000,
            dma_buf: None,
            data: Bytes::from(vec![0xAA; 100_000]),
        };

        let encoded = encoder.encode(&raw).await.expect("Encode failed");
        assert!(encoded.is_keyframe);
        assert!(!encoded.chunks.is_empty());
        assert_eq!(encoded.chunks[0].meta.codec, VideoCodec::Av1);
        for chunk in &encoded.chunks {
            assert!(chunk.payload.len() <= 1024);
        }
    }

    #[test]
    fn test_rpi_invalidation() {
        let mut encoder = MockVideoEncoder::new(EncoderConfig::default());
        encoder.invalidate_reference_picture(42);
        assert_eq!(encoder.last_rpi_frame(), Some(42));
    }

    #[test]
    fn test_nvenc_codec_guid_resolution() {
        let av1_encoder = NvencEncoder::new(EncoderConfig {
            codec: VideoCodec::Av1,
            ..Default::default()
        });
        assert_eq!(av1_encoder.codec_guid(), NV_ENC_CODEC_AV1_GUID);

        let hevc_encoder = NvencEncoder::new(EncoderConfig {
            codec: VideoCodec::Hevc,
            ..Default::default()
        });
        assert_eq!(hevc_encoder.codec_guid(), NV_ENC_CODEC_HEVC_GUID);
    }

    #[tokio::test]
    async fn test_auto_encoder_initialization() {
        let mut auto = AutoVideoEncoder::new(EncoderConfig::default());
        auto.init(EncoderConfig::default())
            .await
            .expect("Auto encoder init failed");

        let raw = RawFrame {
            display_id: 0,
            width: 1280,
            height: 720,
            stride: 1280 * 4,
            format: linux_quest_capture::PixelFormat::Rgba8,
            pts_us: 5000,
            dma_buf: None,
            data: Bytes::from(vec![0x12; 4000]),
        };

        let encoded = auto.encode(&raw).await.expect("Encode failed");
        assert!(!encoded.chunks.is_empty());
    }

    #[test]
    fn test_backend_preference_parsing_and_order() {
        assert_eq!(
            "auto".parse::<BackendPreference>(),
            Ok(BackendPreference::Auto)
        );
        assert_eq!(
            "NVENC".parse::<BackendPreference>(),
            Ok(BackendPreference::Nvenc)
        );
        assert_eq!(
            "va-api".parse::<BackendPreference>(),
            Ok(BackendPreference::Vaapi)
        );
        assert_eq!(
            "mock".parse::<BackendPreference>(),
            Ok(BackendPreference::Mock)
        );
        assert!("cuda".parse::<BackendPreference>().is_err());

        // Auto is NVENC then VA-API; the mock is never a hardware candidate.
        assert_eq!(
            hardware_order(BackendPreference::Auto),
            &[BackendKind::Nvenc, BackendKind::Vaapi]
        );
        assert_eq!(
            hardware_order(BackendPreference::Vaapi),
            &[BackendKind::Vaapi]
        );
        assert!(hardware_order(BackendPreference::Mock).is_empty());
    }

    #[test]
    fn test_probe_backends_reports_reasons() {
        let probes = probe_backends(VideoCodec::Hevc);
        assert_eq!(probes.len(), 3);
        for p in &probes {
            assert!(!p.reason.is_empty(), "{:?} has no reason", p.kind);
        }
        let mock = probes.iter().find(|p| p.kind == BackendKind::Mock).unwrap();
        assert!(mock.available);
    }

    #[tokio::test]
    async fn test_create_encoder_explicit_mock_and_error_shape() {
        let (kind, mut enc) = create_encoder(EncoderConfig::default(), BackendPreference::Mock)
            .await
            .expect("mock always initialises");
        assert_eq!(kind, BackendKind::Mock);
        enc.init(EncoderConfig::default()).await.unwrap();

        // Without a usable VA-API driver the factory errors and does not substitute the mock.
        // On a host that has one, the call succeeds; either outcome is valid.
        match create_encoder(EncoderConfig::default(), BackendPreference::Vaapi).await {
            Ok((kind, _)) => assert_eq!(kind, BackendKind::Vaapi),
            Err(EncoderError::InitFailed(msg)) => {
                assert!(
                    msg.starts_with("no hardware video encoder available"),
                    "{msg}"
                );
                assert!(msg.contains("vaapi:"), "{msg}");
            }
            Err(other) => panic!("unexpected error {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_auto_encoder_with_mock_preference_selects_mock() {
        let mut auto =
            AutoVideoEncoder::with_preference(EncoderConfig::default(), BackendPreference::Mock);
        assert_eq!(auto.selected_backend(), BackendKind::Mock);
        auto.init(EncoderConfig::default()).await.unwrap();
        assert_eq!(auto.selected_backend(), BackendKind::Mock);
    }

    #[tokio::test]
    async fn test_auto_audio_encoder_pipeline() {
        let config = AudioEncoderConfig::default(); // 48kHz, stereo, 2.5ms (120 samples)
        let mut auto = AutoAudioEncoder::new(config.clone());
        auto.init(config)
            .await
            .expect("Auto audio encoder init failed");

        let raw = linux_quest_capture::RawAudioFrame::new(
            48000,
            2,
            linux_quest_capture::AudioSampleFormat::F32Le,
            120,
            10_000,
            Bytes::from(vec![0u8; 120 * 2 * 4]),
        );

        let encoded = auto.encode(&raw).await.expect("Auto audio encode failed");
        assert_eq!(encoded.frame_id, 1);
        assert_eq!(encoded.pts_us, 10_000);
        assert!(!encoded.data.is_empty());
    }
}

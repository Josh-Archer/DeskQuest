//! Pure description of the video formats offered to a ScreenCast node and of the format
//! the node settled on. Nothing here needs libpipewire; `pod.rs` turns these into SPA pods.

use std::time::Duration;

use crate::PixelFormat;

/// `DRM_FORMAT_MOD_LINEAR`.
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;
/// `DRM_FORMAT_MOD_INVALID`: "the producer picks, implicit modifier".
pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// Frame layouts we accept from a ScreenCast node, in the order they are offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoFormatKind {
    /// `SPA_VIDEO_FORMAT_BGRx`: B,G,R,x bytes. Native on most compositors (DRM XRGB8888).
    Bgrx,
    /// `SPA_VIDEO_FORMAT_BGRA` (DRM ARGB8888).
    Bgra,
    /// `SPA_VIDEO_FORMAT_RGBx` (DRM XBGR8888).
    Rgbx,
    /// `SPA_VIDEO_FORMAT_RGBA` (DRM ABGR8888).
    Rgba,
    /// `SPA_VIDEO_FORMAT_NV12`: planar 4:2:0, the encoders' native input.
    Nv12,
}

impl VideoFormatKind {
    /// Every kind, in offer order. RGB first because compositors render RGB natively; NV12
    /// is offered last so it is only chosen when the producer can supply it without a
    /// conversion pass of its own.
    pub const OFFER_ORDER: [VideoFormatKind; 5] = [
        VideoFormatKind::Bgrx,
        VideoFormatKind::Bgra,
        VideoFormatKind::Rgbx,
        VideoFormatKind::Rgba,
        VideoFormatKind::Nv12,
    ];

    /// The pixel layout as seen by the encoder crate. The `x` formats map to the alpha
    /// variants: the padding byte is carried but must be ignored by consumers.
    pub fn pixel_format(self) -> PixelFormat {
        match self {
            Self::Bgrx | Self::Bgra => PixelFormat::Bgra8,
            Self::Rgbx | Self::Rgba => PixelFormat::Rgba8,
            Self::Nv12 => PixelFormat::Nv12,
        }
    }

    /// Bytes per pixel of the first (or only) plane.
    pub fn bytes_per_pixel(self) -> u32 {
        match self {
            Self::Nv12 => 1,
            _ => 4,
        }
    }

    /// Minimum size in bytes of one contiguous buffer holding a frame of `height` rows at
    /// `stride` bytes per row of the first plane.
    pub fn buffer_size(self, stride: u32, height: u32) -> u32 {
        match self {
            // Y plane plus an interleaved UV plane at half vertical resolution.
            Self::Nv12 => stride * height + stride * height.div_ceil(2),
            _ => stride * height,
        }
    }
}

/// Knobs for [`build_offers`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferConfig {
    /// Offer DMA-BUF capable formats (with the modifiers below) before the SHM ones.
    pub dmabuf: bool,
    /// DRM format modifiers we are willing to import. Defaults to linear only: it is the one
    /// modifier every GPU and the VA-API/NVENC import paths accept. Tiled vendor modifiers can
    /// be added once the encoder side advertises what it can import.
    pub modifiers: Vec<u64>,
    /// Largest frame accepted.
    pub max_width: u32,
    /// Largest frame accepted.
    pub max_height: u32,
    /// Frame rate requested from the producer (it may deliver less, or variable).
    pub preferred_fps: u32,
    /// Upper bound of the accepted frame rate range.
    pub max_fps: u32,
}

impl Default for OfferConfig {
    fn default() -> Self {
        Self {
            dmabuf: true,
            modifiers: vec![DRM_FORMAT_MOD_LINEAR],
            max_width: 8192,
            max_height: 8192,
            preferred_fps: 60,
            max_fps: 240,
        }
    }
}

/// One `EnumFormat` entry: a pixel format with, for DMA-BUF, the importable modifiers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatOffer {
    pub kind: VideoFormatKind,
    /// `Some(non-empty)` for a DMA-BUF offer; `None` for plain memory (memfd/memptr).
    pub modifiers: Option<Vec<u64>>,
}

impl FormatOffer {
    pub fn is_dmabuf(&self) -> bool {
        self.modifiers.is_some()
    }
}

/// The ordered list of formats to offer: all DMA-BUF offers (if enabled and any modifier is
/// configured) first, then every kind again as plain memory so SHM remains a fallback.
pub fn build_offers(cfg: &OfferConfig) -> Vec<FormatOffer> {
    let mut offers = Vec::new();
    if cfg.dmabuf && !cfg.modifiers.is_empty() {
        for kind in VideoFormatKind::OFFER_ORDER {
            offers.push(FormatOffer {
                kind,
                modifiers: Some(cfg.modifiers.clone()),
            });
        }
    }
    for kind in VideoFormatKind::OFFER_ORDER {
        offers.push(FormatOffer {
            kind,
            modifiers: None,
        });
    }
    offers
}

/// What the producer settled on (the `Format` param).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiatedFormat {
    pub kind: VideoFormatKind,
    pub width: u32,
    pub height: u32,
    /// Negotiated frame rate; `0/1` means "variable, as fast as damage happens".
    pub fps_num: u32,
    pub fps_den: u32,
    /// Upper bound of a variable frame rate, when given.
    pub max_fps_num: u32,
    pub max_fps_den: u32,
    /// The DRM modifier when the format is a fixed DMA-BUF format.
    pub modifier: Option<u64>,
    /// Non-empty while the producer still lets us choose among several modifiers
    /// (`DONT_FIXATE`); we must answer with a format fixed to one of them.
    pub modifier_choices: Vec<u64>,
}

impl NegotiatedFormat {
    /// True when the buffers will be DMA-BUFs (the format carries a modifier).
    pub fn is_dmabuf(&self) -> bool {
        self.modifier.is_some() || !self.modifier_choices.is_empty()
    }

    /// True while a modifier still has to be picked and sent back.
    pub fn needs_fixation(&self) -> bool {
        !self.modifier_choices.is_empty()
    }

    /// Effective frames per second, rounded: the fixed rate, else the variable-rate bound,
    /// else `fallback`.
    pub fn fps(&self, fallback: u32) -> u32 {
        let rate = |n: u32, d: u32| -> Option<u32> {
            (n > 0 && d > 0).then(|| ((n as u64 + d as u64 / 2) / d as u64) as u32)
        };
        rate(self.fps_num, self.fps_den)
            .or_else(|| rate(self.max_fps_num, self.max_fps_den))
            .unwrap_or(fallback)
    }

    /// Nominal interval between frames.
    pub fn frame_interval(&self, fallback_fps: u32) -> Duration {
        Duration::from_secs_f64(1.0 / self.fps(fallback_fps).max(1) as f64)
    }

    /// Row stride to assume when a buffer chunk reports none.
    pub fn default_stride(&self) -> u32 {
        self.width * self.kind.bytes_per_pixel()
    }
}

/// Picks the modifier to fixate on: the first of `preferred` the producer offers, else the
/// first one it offers.
pub fn fixate_modifier(choices: &[u64], preferred: &[u64]) -> Option<u64> {
    preferred
        .iter()
        .copied()
        .find(|m| choices.contains(m))
        .or_else(|| choices.first().copied())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offers_put_dmabuf_before_shm_and_cover_all_kinds() {
        let offers = build_offers(&OfferConfig::default());
        assert_eq!(offers.len(), 10);
        assert!(offers[..5].iter().all(|o| o.is_dmabuf()));
        assert!(offers[5..].iter().all(|o| !o.is_dmabuf()));
        assert_eq!(offers[0].kind, VideoFormatKind::Bgrx);
        assert_eq!(offers[4].kind, VideoFormatKind::Nv12);
        assert_eq!(offers[0].modifiers.as_deref(), Some(&[0u64][..]));
    }

    #[test]
    fn offers_without_dmabuf_or_modifiers_are_shm_only() {
        let off = OfferConfig {
            dmabuf: false,
            ..Default::default()
        };
        assert!(build_offers(&off).iter().all(|o| !o.is_dmabuf()));
        let none = OfferConfig {
            modifiers: vec![],
            ..Default::default()
        };
        let offers = build_offers(&none);
        assert_eq!(offers.len(), 5);
        assert!(offers.iter().all(|o| !o.is_dmabuf()));
    }

    #[test]
    fn kinds_map_to_encoder_pixel_formats() {
        assert_eq!(VideoFormatKind::Bgrx.pixel_format(), PixelFormat::Bgra8);
        assert_eq!(VideoFormatKind::Bgra.pixel_format(), PixelFormat::Bgra8);
        assert_eq!(VideoFormatKind::Rgbx.pixel_format(), PixelFormat::Rgba8);
        assert_eq!(VideoFormatKind::Rgba.pixel_format(), PixelFormat::Rgba8);
        assert_eq!(VideoFormatKind::Nv12.pixel_format(), PixelFormat::Nv12);
        assert_eq!(VideoFormatKind::Bgrx.buffer_size(7680, 1080), 7680 * 1080);
        assert_eq!(
            VideoFormatKind::Nv12.buffer_size(1920, 1080),
            1920 * 1080 * 3 / 2
        );
        // Odd heights round the chroma plane up.
        assert_eq!(VideoFormatKind::Nv12.buffer_size(4, 3), 12 + 8);
    }

    fn fmt(num: u32, den: u32, max_num: u32, max_den: u32) -> NegotiatedFormat {
        NegotiatedFormat {
            kind: VideoFormatKind::Bgrx,
            width: 2560,
            height: 1440,
            fps_num: num,
            fps_den: den,
            max_fps_num: max_num,
            max_fps_den: max_den,
            modifier: None,
            modifier_choices: vec![],
        }
    }

    #[test]
    fn fps_prefers_fixed_then_max_then_fallback() {
        assert_eq!(fmt(60, 1, 0, 1).fps(30), 60);
        assert_eq!(fmt(60000, 1001, 0, 1).fps(30), 60);
        assert_eq!(fmt(0, 1, 144, 1).fps(30), 144);
        assert_eq!(fmt(0, 1, 0, 1).fps(30), 30);
        assert_eq!(fmt(0, 0, 0, 0).fps(30), 30);
    }

    #[test]
    fn modifier_state_flags() {
        let mut f = fmt(60, 1, 0, 1);
        assert!(!f.is_dmabuf() && !f.needs_fixation());
        f.modifier = Some(DRM_FORMAT_MOD_LINEAR);
        assert!(f.is_dmabuf() && !f.needs_fixation());
        f.modifier = None;
        f.modifier_choices = vec![1, 2];
        assert!(f.is_dmabuf() && f.needs_fixation());
    }

    #[test]
    fn fixation_prefers_our_order_then_producer_order() {
        assert_eq!(fixate_modifier(&[5, 0, 7], &[0, 7]), Some(0));
        assert_eq!(fixate_modifier(&[5, 7], &[0, 7]), Some(7));
        assert_eq!(fixate_modifier(&[5, 7], &[0]), Some(5));
        assert_eq!(fixate_modifier(&[], &[0]), None);
    }
}

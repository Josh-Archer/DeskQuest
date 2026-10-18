//! Chooses the backend and video codec for a stream from what the encoder can do and
//! what the client can decode. Pure logic over probe results, so it is unit-testable
//! without a GPU.

use linux_quest_encoder::{hardware_order, BackendKind, BackendPreference};
use linux_quest_protocol::VideoCodec;

/// Codecs the Quest client can decode (`ClientVideoCodec` in `crates/client/src/config.rs`).
/// H.264 is not in that list, so the client never negotiates it.
pub const CLIENT_DECODE_CODECS: &[VideoCodec] = &[VideoCodec::Av1, VideoCodec::Hevc];

/// Every codec the host knows, in the order the capability probes run: AV1, HEVC, H.264.
/// This is not the order a stream picks codecs in: each backend has its own order
/// ([`linux_quest_encoder::backend_codecs`]). NVENC puts AV1 first for quality per
/// bit; VA-API puts HEVC first because its AV1 path is experimental and off by default.
pub const CODEC_PREFERENCE: &[VideoCodec] = &[VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264];

/// The backend and codec a stream will use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamPlan {
    pub backend: BackendKind,
    pub codec: VideoCodec,
}

/// Picks a stream plan.
///
/// Backends are tried in the preference's order (for `auto`, NVENC then VA-API). For
/// each backend the codec is the first one in that backend's own order
/// ([`linux_quest_encoder::backend_codecs`]) that the client can decode and that
/// `available` reports as usable. `Mock` is only planned when it is the explicit
/// preference, never as a silent substitute.
pub fn plan_stream(
    preference: BackendPreference,
    client: &[VideoCodec],
    available: impl Fn(BackendKind, VideoCodec) -> bool,
) -> Option<StreamPlan> {
    let backends: Vec<BackendKind> = if preference == BackendPreference::Mock {
        vec![BackendKind::Mock]
    } else {
        hardware_order(preference).to_vec()
    };
    for backend in backends {
        let backend_codecs = linux_quest_encoder::backend_codecs(backend);
        for &codec in backend_codecs {
            if client.contains(&codec) && available(backend, codec) {
                return Some(StreamPlan { backend, codec });
            }
        }
    }
    None
}

/// Codec to negotiate with a client that advertised `client`: the first codec in
/// `encodable` that the client also decodes. `encodable` is in the chosen backend's
/// own order (see [`linux_quest_encoder::backend_codecs`]), so the host's preference
/// wins over the client's order. Used when the handshake is wired through.
pub fn negotiate_codec(client: &[VideoCodec], encodable: &[VideoCodec]) -> Option<VideoCodec> {
    encodable.iter().copied().find(|c| client.contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_available(_: BackendKind, _: VideoCodec) -> bool {
        true
    }

    #[test]
    fn av1_is_preferred_when_everything_is_available() {
        let plan =
            plan_stream(BackendPreference::Auto, CLIENT_DECODE_CODECS, all_available).unwrap();
        assert_eq!(
            plan,
            StreamPlan {
                backend: BackendKind::Nvenc,
                codec: VideoCodec::Av1
            }
        );
    }

    #[test]
    fn hevc_is_chosen_when_the_backend_has_no_av1() {
        // VA-API on hardware without AV1 encode: HEVC is the next choice.
        let plan = plan_stream(BackendPreference::Vaapi, CLIENT_DECODE_CODECS, |b, c| {
            b == BackendKind::Vaapi && c == VideoCodec::Hevc
        })
        .unwrap();
        assert_eq!(
            plan,
            StreamPlan {
                backend: BackendKind::Vaapi,
                codec: VideoCodec::Hevc
            }
        );
    }

    #[test]
    fn auto_falls_through_to_vaapi_when_nvenc_is_absent() {
        let plan = plan_stream(BackendPreference::Auto, CLIENT_DECODE_CODECS, |b, _| {
            b == BackendKind::Vaapi
        })
        .unwrap();
        assert_eq!(plan.backend, BackendKind::Vaapi);
        assert_eq!(
            plan.codec,
            VideoCodec::Hevc,
            "VA-API's own order puts HEVC before AV1, even when AV1 is reported usable"
        );
    }

    #[test]
    fn vaapi_uses_av1_only_when_hevc_is_not_usable() {
        let plan = plan_stream(BackendPreference::Vaapi, CLIENT_DECODE_CODECS, |b, c| {
            b == BackendKind::Vaapi && c == VideoCodec::Av1
        })
        .unwrap();
        assert_eq!(plan.codec, VideoCodec::Av1);
    }

    #[test]
    fn vaapi_with_av1_disabled_never_plans_av1() {
        // The VA-API probe reports AV1 unavailable unless it is opted in.
        let plan = plan_stream(BackendPreference::Vaapi, CLIENT_DECODE_CODECS, |_, c| {
            c != VideoCodec::Av1
        })
        .unwrap();
        assert_eq!(plan.codec, VideoCodec::Hevc);
        assert_eq!(
            plan_stream(BackendPreference::Vaapi, &[VideoCodec::Av1], |_, c| c
                != VideoCodec::Av1),
            None
        );
    }

    #[test]
    fn h264_is_never_planned_for_a_client_that_cannot_decode_it() {
        let only_h264 = |_: BackendKind, c: VideoCodec| c == VideoCodec::H264;
        assert_eq!(
            plan_stream(BackendPreference::Vaapi, CLIENT_DECODE_CODECS, only_h264),
            None
        );
        let plan = plan_stream(BackendPreference::Vaapi, &[VideoCodec::H264], only_h264).unwrap();
        assert_eq!(
            plan.codec,
            VideoCodec::H264,
            "a client that asks for it gets it"
        );
    }

    #[test]
    fn no_usable_backend_plans_nothing() {
        let none = |_: BackendKind, _: VideoCodec| false;
        assert_eq!(
            plan_stream(BackendPreference::Auto, CLIENT_DECODE_CODECS, none),
            None
        );
    }

    #[test]
    fn mock_is_only_planned_when_asked_for() {
        let plan =
            plan_stream(BackendPreference::Mock, CLIENT_DECODE_CODECS, all_available).unwrap();
        assert_eq!(plan.backend, BackendKind::Mock);
        assert_eq!(plan.codec, VideoCodec::Av1);
        // Auto never reaches the mock, even when every hardware probe fails.
        assert_eq!(
            plan_stream(BackendPreference::Auto, CLIENT_DECODE_CODECS, |b, _| b
                == BackendKind::Mock),
            None
        );
    }

    #[test]
    fn negotiation_follows_host_preference_not_client_order() {
        assert_eq!(
            negotiate_codec(
                &[VideoCodec::Hevc, VideoCodec::Av1],
                &[VideoCodec::Av1, VideoCodec::Hevc]
            ),
            Some(VideoCodec::Av1)
        );
        // A backend whose own order is HEVC first negotiates HEVC.
        assert_eq!(
            negotiate_codec(
                &[VideoCodec::Av1, VideoCodec::Hevc],
                &[VideoCodec::Hevc, VideoCodec::Av1]
            ),
            Some(VideoCodec::Hevc)
        );
        assert_eq!(
            negotiate_codec(&[VideoCodec::Hevc], &[VideoCodec::Av1]),
            None
        );
    }
}

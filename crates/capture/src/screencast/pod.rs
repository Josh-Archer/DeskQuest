//! SPA pod construction and parsing for the screencast stream: the `EnumFormat` offers, the
//! fixated `Format` answer, the `Buffers` / `Meta` requirements, and decoding of the format
//! PipeWire negotiated.
//!
//! The pods are plain byte vectors so they can be built and checked without a PipeWire
//! daemon.

use std::io::Cursor;

use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pipewire::spa::param::video::VideoFormat;
use pipewire::spa::param::ParamType;
use pipewire::spa::pod::deserialize::PodDeserializer;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{ChoiceValue, Object, Pod, Property, PropertyFlags, Value};
use pipewire::spa::sys as spa_sys;
use pipewire::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle, SpaTypes};

use super::format::{FormatOffer, NegotiatedFormat, OfferConfig, VideoFormatKind};

/// Why a format pod could not be understood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PodError {
    Malformed(String),
    UnsupportedFormat(u32),
    Missing(&'static str),
}

impl std::fmt::Display for PodError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(m) => write!(f, "malformed format pod: {m}"),
            Self::UnsupportedFormat(id) => write!(f, "unsupported SPA video format id {id}"),
            Self::Missing(what) => write!(f, "format pod has no {what}"),
        }
    }
}

impl std::error::Error for PodError {}

fn spa_format(kind: VideoFormatKind) -> VideoFormat {
    match kind {
        VideoFormatKind::Bgrx => VideoFormat::BGRx,
        VideoFormatKind::Bgra => VideoFormat::BGRA,
        VideoFormatKind::Rgbx => VideoFormat::RGBx,
        VideoFormatKind::Rgba => VideoFormat::RGBA,
        VideoFormatKind::Nv12 => VideoFormat::NV12,
    }
}

fn kind_from_id(id: u32) -> Result<VideoFormatKind, PodError> {
    VideoFormatKind::OFFER_ORDER
        .into_iter()
        .find(|k| spa_format(*k).as_raw() == id)
        .ok_or(PodError::UnsupportedFormat(id))
}

fn serialize(obj: Object) -> Vec<u8> {
    PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(obj))
        .expect("serializing an in-memory pod cannot fail")
        .0
        .into_inner()
}

fn id_prop(key: FormatProperties, id: u32) -> Property {
    Property::new(key.as_raw(), Value::Id(Id(id)))
}

fn format_object(props: Vec<Property>) -> Object {
    let mut all = vec![
        id_prop(FormatProperties::MediaType, MediaType::Video.as_raw()),
        id_prop(FormatProperties::MediaSubtype, MediaSubtype::Raw.as_raw()),
    ];
    all.extend(props);
    Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: all,
    }
}

fn rect(w: u32, h: u32) -> Rectangle {
    Rectangle {
        width: w,
        height: h,
    }
}

fn frac(num: u32, denom: u32) -> Fraction {
    Fraction { num, denom }
}

/// Builds the `EnumFormat` pod for one offer.
///
/// For a DMA-BUF offer the modifier property is `MANDATORY` (a producer that cannot satisfy
/// it must fall through to the next offer, the SHM one) and, when several modifiers are
/// offered, `DONT_FIXATE` so the producer hands the choice back to us.
pub fn enum_format_pod(offer: &FormatOffer, cfg: &OfferConfig) -> Vec<u8> {
    let mut props = vec![id_prop(
        FormatProperties::VideoFormat,
        spa_format(offer.kind).as_raw(),
    )];
    if let Some(mods) = &offer.modifiers {
        let default = mods.first().copied().unwrap_or(0) as i64;
        let mut flags = PropertyFlags::MANDATORY;
        let value = if mods.len() > 1 {
            flags |= PropertyFlags::DONT_FIXATE;
            Value::Choice(ChoiceValue::Long(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Enum {
                    default,
                    alternatives: mods.iter().map(|m| *m as i64).collect(),
                },
            )))
        } else {
            Value::Long(default)
        };
        props.push(Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags,
            value,
        });
    }
    props.push(Property::new(
        FormatProperties::VideoSize.as_raw(),
        Value::Choice(ChoiceValue::Rectangle(Choice(
            ChoiceFlags::empty(),
            ChoiceEnum::Range {
                default: rect(1920, 1080),
                min: rect(1, 1),
                max: rect(cfg.max_width, cfg.max_height),
            },
        ))),
    ));
    props.push(Property::new(
        FormatProperties::VideoFramerate.as_raw(),
        Value::Choice(ChoiceValue::Fraction(Choice(
            ChoiceFlags::empty(),
            ChoiceEnum::Range {
                default: frac(cfg.preferred_fps, 1),
                min: frac(0, 1),
                max: frac(cfg.max_fps, 1),
            },
        ))),
    ));
    serialize(format_object(props))
}

/// All `EnumFormat` pods for `offers`, in preference order.
pub fn enum_format_pods(offers: &[FormatOffer], cfg: &OfferConfig) -> Vec<Vec<u8>> {
    offers.iter().map(|o| enum_format_pod(o, cfg)).collect()
}

/// The `Format` answer that fixates `modifier` for an already negotiated format.
pub fn fixated_format_pod(neg: &NegotiatedFormat, modifier: u64) -> Vec<u8> {
    let mut obj = format_object(vec![
        id_prop(FormatProperties::VideoFormat, spa_format(neg.kind).as_raw()),
        Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags: PropertyFlags::MANDATORY,
            value: Value::Long(modifier as i64),
        },
        Property::new(
            FormatProperties::VideoSize.as_raw(),
            Value::Rectangle(rect(neg.width, neg.height)),
        ),
        Property::new(
            FormatProperties::VideoFramerate.as_raw(),
            Value::Fraction(frac(neg.fps_num, neg.fps_den.max(1))),
        ),
    ]);
    obj.id = ParamType::Format.as_raw();
    serialize(obj)
}

fn int_range(default: i32, min: i32, max: i32) -> Value {
    Value::Choice(ChoiceValue::Int(Choice(
        ChoiceFlags::empty(),
        ChoiceEnum::Range { default, min, max },
    )))
}

/// The `Buffers` requirements for the negotiated format: DMA-BUF only when the format
/// carries a modifier, otherwise memfd / memptr (mappable) buffers of at least `stride x
/// height` bytes.
pub fn buffers_pod(neg: &NegotiatedFormat) -> Vec<u8> {
    let mask: i32 = if neg.is_dmabuf() {
        1 << spa_sys::SPA_DATA_DmaBuf
    } else {
        (1 << spa_sys::SPA_DATA_MemFd) | (1 << spa_sys::SPA_DATA_MemPtr)
    };
    let mut props = vec![
        Property::new(spa_sys::SPA_PARAM_BUFFERS_buffers, int_range(4, 2, 8)),
        Property::new(
            spa_sys::SPA_PARAM_BUFFERS_dataType,
            Value::Choice(ChoiceValue::Int(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Flags {
                    default: mask,
                    flags: vec![mask],
                },
            ))),
        ),
    ];
    if !neg.is_dmabuf() {
        let size = neg.kind.buffer_size(neg.default_stride(), neg.height) as i32;
        props.push(Property::new(
            spa_sys::SPA_PARAM_BUFFERS_blocks,
            Value::Int(1),
        ));
        props.push(Property::new(
            spa_sys::SPA_PARAM_BUFFERS_size,
            int_range(size, size, i32::MAX),
        ));
    }
    serialize(Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: ParamType::Buffers.as_raw(),
        properties: props,
    })
}

fn meta_pod(meta_type: u32, size: Value) -> Vec<u8> {
    serialize(Object {
        type_: SpaTypes::ObjectParamMeta.as_raw(),
        id: ParamType::Meta.as_raw(),
        properties: vec![
            Property::new(spa_sys::SPA_PARAM_META_type, Value::Id(Id(meta_type))),
            Property::new(spa_sys::SPA_PARAM_META_size, size),
        ],
    })
}

/// `Meta` params: the buffer header (timestamps, flags) and video damage regions.
pub fn meta_pods() -> Vec<Vec<u8>> {
    let header = std::mem::size_of::<spa_sys::spa_meta_header>() as i32;
    let region = std::mem::size_of::<spa_sys::spa_meta_region>() as i32;
    vec![
        meta_pod(spa_sys::SPA_META_Header, Value::Int(header)),
        meta_pod(
            spa_sys::SPA_META_VideoDamage,
            int_range(region * 16, region, region * 64),
        ),
    ]
}

/// Borrows serialized bytes as a pod for `Stream::connect` / `update_params`.
pub fn as_pod(bytes: &[u8]) -> Option<&Pod> {
    Pod::from_bytes(bytes)
}

fn modifiers_of(value: &Value) -> Result<(Option<u64>, Vec<u64>), PodError> {
    match value {
        Value::Long(m) => Ok((Some(*m as u64), vec![])),
        Value::Choice(ChoiceValue::Long(Choice(_, choice))) => {
            let mut all: Vec<u64> = match choice {
                ChoiceEnum::None(v) => vec![*v as u64],
                ChoiceEnum::Enum {
                    default,
                    alternatives,
                } => std::iter::once(*default)
                    .chain(alternatives.iter().copied())
                    .map(|m| m as u64)
                    .collect(),
                ChoiceEnum::Flags { default, flags } => std::iter::once(*default)
                    .chain(flags.iter().copied())
                    .map(|m| m as u64)
                    .collect(),
                ChoiceEnum::Range { default, .. } | ChoiceEnum::Step { default, .. } => {
                    vec![*default as u64]
                }
            };
            let mut seen = Vec::new();
            all.retain(|m| {
                let new = !seen.contains(m);
                seen.push(*m);
                new
            });
            if all.len() == 1 {
                Ok((Some(all[0]), vec![]))
            } else {
                Ok((None, all))
            }
        }
        other => Err(PodError::Malformed(format!("modifier is {other:?}"))),
    }
}

fn first_rect(value: &Value) -> Option<Rectangle> {
    match value {
        Value::Rectangle(r) => Some(*r),
        Value::Choice(ChoiceValue::Rectangle(Choice(_, c))) => Some(match c {
            ChoiceEnum::None(v) => *v,
            ChoiceEnum::Range { default, .. }
            | ChoiceEnum::Step { default, .. }
            | ChoiceEnum::Enum { default, .. }
            | ChoiceEnum::Flags { default, .. } => *default,
        }),
        _ => None,
    }
}

fn first_frac(value: &Value) -> Option<Fraction> {
    match value {
        Value::Fraction(f) => Some(*f),
        Value::Choice(ChoiceValue::Fraction(Choice(_, c))) => Some(match c {
            ChoiceEnum::None(v) => *v,
            ChoiceEnum::Range { default, .. }
            | ChoiceEnum::Step { default, .. }
            | ChoiceEnum::Enum { default, .. }
            | ChoiceEnum::Flags { default, .. } => *default,
        }),
        _ => None,
    }
}

/// Decodes a `Format` param into the pure [`NegotiatedFormat`].
pub fn parse_format(bytes: &[u8]) -> Result<NegotiatedFormat, PodError> {
    let (_, value) = PodDeserializer::deserialize_any_from(bytes)
        .map_err(|e| PodError::Malformed(format!("{e:?}")))?;
    let Value::Object(obj) = value else {
        return Err(PodError::Malformed("not an object".into()));
    };

    let mut kind = None;
    let mut size = None;
    let mut rate = None;
    let mut max_rate = None;
    let mut modifiers = (None, vec![]);
    for p in &obj.properties {
        let key = FormatProperties::from_raw(p.key);
        if key == FormatProperties::VideoFormat {
            match &p.value {
                Value::Id(id) => kind = Some(kind_from_id(id.0)?),
                Value::Choice(ChoiceValue::Id(Choice(_, c))) => {
                    let id = match c {
                        ChoiceEnum::None(v) => v.0,
                        ChoiceEnum::Enum { default, .. }
                        | ChoiceEnum::Flags { default, .. }
                        | ChoiceEnum::Range { default, .. }
                        | ChoiceEnum::Step { default, .. } => default.0,
                    };
                    kind = Some(kind_from_id(id)?);
                }
                other => return Err(PodError::Malformed(format!("format is {other:?}"))),
            }
        } else if key == FormatProperties::VideoSize {
            size = first_rect(&p.value);
        } else if key == FormatProperties::VideoFramerate {
            rate = first_frac(&p.value);
        } else if key == FormatProperties::VideoMaxFramerate {
            max_rate = first_frac(&p.value);
        } else if key == FormatProperties::VideoModifier {
            modifiers = modifiers_of(&p.value)?;
        }
    }

    let kind = kind.ok_or(PodError::Missing("video format"))?;
    let size = size.ok_or(PodError::Missing("video size"))?;
    let rate = rate.unwrap_or(frac(0, 1));
    let max_rate = max_rate.unwrap_or(frac(0, 1));
    Ok(NegotiatedFormat {
        kind,
        width: size.width,
        height: size.height,
        fps_num: rate.num,
        fps_den: rate.denom,
        max_fps_num: max_rate.num,
        max_fps_den: max_rate.denom,
        modifier: modifiers.0,
        modifier_choices: modifiers.1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screencast::format::{build_offers, DRM_FORMAT_MOD_LINEAR};

    fn parse_obj(bytes: &[u8]) -> Object {
        match PodDeserializer::deserialize_any_from(bytes).unwrap().1 {
            Value::Object(o) => o,
            other => panic!("not an object: {other:?}"),
        }
    }

    fn prop(o: &Object, key: FormatProperties) -> Option<&Property> {
        o.properties.iter().find(|p| p.key == key.as_raw())
    }

    #[test]
    fn every_offer_serializes_to_a_valid_pod() {
        let cfg = OfferConfig::default();
        for bytes in enum_format_pods(&build_offers(&cfg), &cfg) {
            let pod = as_pod(&bytes).expect("valid pod");
            assert!(pod.is_object());
        }
    }

    #[test]
    fn dmabuf_offer_carries_a_mandatory_modifier_and_shm_offer_does_not() {
        let cfg = OfferConfig::default();
        let offers = build_offers(&cfg);
        let dma = parse_obj(&enum_format_pod(&offers[0], &cfg));
        let m = prop(&dma, FormatProperties::VideoModifier).expect("modifier");
        assert!(m.flags.contains(PropertyFlags::MANDATORY));
        assert!(
            !m.flags.contains(PropertyFlags::DONT_FIXATE),
            "single modifier"
        );
        assert_eq!(m.value, Value::Long(DRM_FORMAT_MOD_LINEAR as i64));

        let shm = parse_obj(&enum_format_pod(&offers[5], &cfg));
        assert!(prop(&shm, FormatProperties::VideoModifier).is_none());
    }

    #[test]
    fn several_modifiers_become_a_dont_fixate_enum() {
        let cfg = OfferConfig {
            modifiers: vec![0, 0x0100_0000_0000_0001],
            ..Default::default()
        };
        let obj = parse_obj(&enum_format_pod(&build_offers(&cfg)[0], &cfg));
        let m = prop(&obj, FormatProperties::VideoModifier).unwrap();
        assert!(m
            .flags
            .contains(PropertyFlags::MANDATORY | PropertyFlags::DONT_FIXATE));
        match &m.value {
            Value::Choice(ChoiceValue::Long(Choice(
                _,
                ChoiceEnum::Enum {
                    default,
                    alternatives,
                },
            ))) => {
                assert_eq!(*default, 0);
                assert_eq!(alternatives, &[0, 0x0100_0000_0000_0001]);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn offer_ranges_use_the_config() {
        let cfg = OfferConfig {
            max_width: 3840,
            max_height: 2160,
            preferred_fps: 90,
            max_fps: 120,
            ..Default::default()
        };
        let obj = parse_obj(&enum_format_pod(&build_offers(&cfg)[0], &cfg));
        match &prop(&obj, FormatProperties::VideoSize).unwrap().value {
            Value::Choice(ChoiceValue::Rectangle(Choice(_, ChoiceEnum::Range { max, .. }))) => {
                assert_eq!((max.width, max.height), (3840, 2160));
            }
            other => panic!("{other:?}"),
        }
        match &prop(&obj, FormatProperties::VideoFramerate).unwrap().value {
            Value::Choice(ChoiceValue::Fraction(Choice(
                _,
                ChoiceEnum::Range { default, max, .. },
            ))) => {
                assert_eq!((default.num, max.num), (90, 120));
            }
            other => panic!("{other:?}"),
        }
    }

    fn neg(kind: VideoFormatKind, modifier: Option<u64>) -> NegotiatedFormat {
        NegotiatedFormat {
            kind,
            width: 2560,
            height: 1440,
            fps_num: 144,
            fps_den: 1,
            max_fps_num: 0,
            max_fps_den: 1,
            modifier,
            modifier_choices: vec![],
        }
    }

    #[test]
    fn fixated_format_round_trips_through_the_parser() {
        let base = neg(VideoFormatKind::Bgrx, None);
        let bytes = fixated_format_pod(&base, 0x0100_0000_0000_0001);
        let parsed = parse_format(&bytes).unwrap();
        assert_eq!(parsed.kind, VideoFormatKind::Bgrx);
        assert_eq!((parsed.width, parsed.height), (2560, 1440));
        assert_eq!(parsed.fps(0), 144);
        assert_eq!(parsed.modifier, Some(0x0100_0000_0000_0001));
        assert!(!parsed.needs_fixation());
    }

    #[test]
    fn every_kind_round_trips() {
        for kind in VideoFormatKind::OFFER_ORDER {
            let parsed = parse_format(&fixated_format_pod(&neg(kind, None), 0)).unwrap();
            assert_eq!(parsed.kind, kind);
        }
    }

    #[test]
    fn a_still_open_modifier_choice_is_reported_for_fixation() {
        // What a producer sends back when it leaves the modifier choice to us.
        let obj = format_object(vec![
            id_prop(FormatProperties::VideoFormat, VideoFormat::BGRx.as_raw()),
            Property {
                key: FormatProperties::VideoModifier.as_raw(),
                flags: PropertyFlags::MANDATORY | PropertyFlags::DONT_FIXATE,
                value: Value::Choice(ChoiceValue::Long(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Enum {
                        default: 5,
                        alternatives: vec![5, 0, 7],
                    },
                ))),
            },
            Property::new(
                FormatProperties::VideoSize.as_raw(),
                Value::Rectangle(rect(1920, 1080)),
            ),
        ]);
        let parsed = parse_format(&serialize(obj)).unwrap();
        assert!(parsed.needs_fixation());
        assert_eq!(parsed.modifier_choices, vec![5, 0, 7]);
        assert_eq!(parsed.modifier, None);
        assert!(parsed.is_dmabuf());
        // No framerate in the pod: falls back to the caller's hint.
        assert_eq!(parsed.fps(60), 60);
    }

    #[test]
    fn variable_rate_uses_max_framerate() {
        let obj = format_object(vec![
            id_prop(FormatProperties::VideoFormat, VideoFormat::NV12.as_raw()),
            Property::new(
                FormatProperties::VideoSize.as_raw(),
                Value::Rectangle(rect(640, 480)),
            ),
            Property::new(
                FormatProperties::VideoFramerate.as_raw(),
                Value::Fraction(frac(0, 1)),
            ),
            Property::new(
                FormatProperties::VideoMaxFramerate.as_raw(),
                Value::Fraction(frac(75, 1)),
            ),
        ]);
        let parsed = parse_format(&serialize(obj)).unwrap();
        assert_eq!(parsed.kind, VideoFormatKind::Nv12);
        assert_eq!(parsed.fps(30), 75);
        assert!(!parsed.is_dmabuf());
    }

    #[test]
    fn unsupported_or_incomplete_formats_are_errors_not_panics() {
        let unsupported = format_object(vec![
            id_prop(FormatProperties::VideoFormat, VideoFormat::GRAY8.as_raw()),
            Property::new(
                FormatProperties::VideoSize.as_raw(),
                Value::Rectangle(rect(8, 8)),
            ),
        ]);
        assert!(matches!(
            parse_format(&serialize(unsupported)),
            Err(PodError::UnsupportedFormat(_))
        ));
        let no_size = format_object(vec![id_prop(
            FormatProperties::VideoFormat,
            VideoFormat::BGRx.as_raw(),
        )]);
        assert_eq!(
            parse_format(&serialize(no_size)),
            Err(PodError::Missing("video size"))
        );
        assert!(parse_format(&[1, 2, 3]).is_err());
    }

    fn buffers_props(bytes: &[u8]) -> Vec<(u32, Value)> {
        let (_, v) = PodDeserializer::deserialize_any_from(bytes).unwrap();
        match v {
            Value::Object(o) => o.properties.into_iter().map(|p| (p.key, p.value)).collect(),
            _ => panic!(),
        }
    }

    #[test]
    fn buffers_pod_requests_dmabuf_only_for_dmabuf_formats() {
        let dma = buffers_props(&buffers_pod(&neg(VideoFormatKind::Bgrx, Some(0))));
        let data_type = dma
            .iter()
            .find(|(k, _)| *k == spa_sys::SPA_PARAM_BUFFERS_dataType)
            .unwrap();
        match &data_type.1 {
            Value::Choice(ChoiceValue::Int(Choice(_, ChoiceEnum::Flags { default, .. }))) => {
                assert_eq!(*default, 1 << spa_sys::SPA_DATA_DmaBuf);
            }
            other => panic!("{other:?}"),
        }
        assert!(!dma
            .iter()
            .any(|(k, _)| *k == spa_sys::SPA_PARAM_BUFFERS_size));

        let shm = buffers_props(&buffers_pod(&neg(VideoFormatKind::Bgrx, None)));
        let data_type = shm
            .iter()
            .find(|(k, _)| *k == spa_sys::SPA_PARAM_BUFFERS_dataType)
            .unwrap();
        match &data_type.1 {
            Value::Choice(ChoiceValue::Int(Choice(_, ChoiceEnum::Flags { default, .. }))) => {
                assert_eq!(
                    *default,
                    (1 << spa_sys::SPA_DATA_MemFd) | (1 << spa_sys::SPA_DATA_MemPtr)
                );
            }
            other => panic!("{other:?}"),
        }
        let size = shm
            .iter()
            .find(|(k, _)| *k == spa_sys::SPA_PARAM_BUFFERS_size)
            .unwrap();
        match &size.1 {
            Value::Choice(ChoiceValue::Int(Choice(_, ChoiceEnum::Range { min, .. }))) => {
                assert_eq!(*min, 2560 * 1440 * 4);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn meta_pods_ask_for_header_and_damage() {
        let pods = meta_pods();
        assert_eq!(pods.len(), 2);
        let types: Vec<u32> = pods
            .iter()
            .map(|b| {
                buffers_props(b)
                    .into_iter()
                    .find(|(k, _)| *k == spa_sys::SPA_PARAM_META_type)
                    .map(|(_, v)| match v {
                        Value::Id(id) => id.0,
                        other => panic!("{other:?}"),
                    })
                    .unwrap()
            })
            .collect();
        assert_eq!(
            types,
            [spa_sys::SPA_META_Header, spa_sys::SPA_META_VideoDamage]
        );
    }
}

//! The DeskQuest logo with a connection badge, embedded as small RGBA buffers.
//! No image decoder, installed icon theme, or filesystem access is needed.

use super::model::ConnectionState;

pub const ICON_SIZES: [u32; 2] = [22, 32];
const LOGO_22: &[u8; 22 * 22 * 4] =
    include_bytes!("../../../../packaging/icons/tray/deskquest-22.rgba");
const LOGO_32: &[u8; 32 * 32 * 4] =
    include_bytes!("../../../../packaging/icons/tray/deskquest-32.rgba");

const BLUE: [u8; 3] = [0x00, 0x64, 0xed];
const CYAN: [u8; 3] = [0x00, 0xd2, 0xf0];
const AMBER: [u8; 3] = [0xff, 0xb2, 0x16];
const GREY: [u8; 3] = [0x80, 0x8b, 0x9b];
const NAVY: [u8; 3] = [0x0f, 0x18, 0x28];
const WHITE: [u8; 3] = [0xff, 0xff, 0xff];

fn coverage(distance: f32, radius: f32) -> f32 {
    (radius - distance + 0.5).clamp(0.0, 1.0)
}

/// Composites a straight-alpha colour over an SNI pixel (A, R, G, B).
fn over(pixel: &mut [u8], colour: [u8; 3], alpha: f32) {
    let background = f32::from(pixel[0]) / 255.0;
    let combined = alpha + background * (1.0 - alpha);
    if combined == 0.0 {
        return;
    }
    for (channel, foreground) in pixel[1..].iter_mut().zip(colour) {
        *channel = ((f32::from(foreground) * alpha
            + f32::from(*channel) * background * (1.0 - alpha))
            / combined)
            .round() as u8;
    }
    pixel[0] = (combined * 255.0).round() as u8;
}

/// Renders the logo in non-premultiplied SNI ARGB32 order. The badge is cyan for
/// USB, blue for Wi-Fi, amber when disconnected, and a grey ring when offline.
/// Disconnected/offline logos are desaturated; menu text also names the state.
pub fn render_argb(state: ConnectionState, size: u32) -> Vec<u8> {
    let (source, source_size): (&[u8], u32) = if size == 22 {
        (LOGO_22, 22)
    } else {
        (LOGO_32, 32)
    };
    let muted = matches!(
        state,
        ConnectionState::DaemonOffline | ConnectionState::Disconnected
    );
    let badge = match state {
        ConnectionState::Usb => CYAN,
        ConnectionState::Wifi => BLUE,
        ConnectionState::Disconnected => AMBER,
        ConnectionState::DaemonOffline => GREY,
    };
    let centre = size as f32 * 0.81;
    let radius = size as f32 * 0.13;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let source_index =
                ((y * source_size / size) * source_size + x * source_size / size) as usize * 4;
            let rgba = &source[source_index..source_index + 4];
            let mut pixel = [rgba[3], rgba[0], rgba[1], rgba[2]];
            // Remove nearly transparent resampling specks outside the silhouette.
            if pixel[0] < 8 {
                pixel.fill(0);
            }
            if muted {
                let grey =
                    (u32::from(rgba[0]) * 54 + u32::from(rgba[1]) * 183 + u32::from(rgba[2]) * 19)
                        / 256;
                pixel[1..].fill(grey as u8);
            }
            let distance =
                ((x as f32 + 0.5 - centre).powi(2) + (y as f32 + 0.5 - centre).powi(2)).sqrt();
            over(&mut pixel, WHITE, coverage(distance, radius + 1.0));
            over(&mut pixel, badge, coverage(distance, radius));
            if state == ConnectionState::DaemonOffline {
                over(&mut pixel, NAVY, coverage(distance, radius * 0.5));
            }
            out.extend_from_slice(&pixel);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATES: [ConnectionState; 4] = [
        ConnectionState::DaemonOffline,
        ConnectionState::Disconnected,
        ConnectionState::Usb,
        ConnectionState::Wifi,
    ];

    #[test]
    fn pixmaps_have_sni_lengths_and_transparent_corners() {
        for size in ICON_SIZES {
            for state in STATES {
                let pixels = render_argb(state, size);
                assert_eq!(pixels.len(), (size * size * 4) as usize);
                assert_eq!(pixels[0], 0);
                assert_eq!(pixels[pixels.len() - 4], 0);
            }
        }
        assert!(render_argb(ConnectionState::Wifi, 0).is_empty());
    }

    #[test]
    fn logo_channels_are_converted_from_rgba_to_sni_argb() {
        for size in ICON_SIZES {
            let source: &[u8] = if size == 22 { LOGO_22 } else { LOGO_32 };
            let offset = ((size / 2 * size + size / 2) * 4) as usize;
            let rgba = &source[offset..offset + 4];
            let pixels = render_argb(ConnectionState::Wifi, size);
            assert_eq!(
                pixels[offset..offset + 4],
                [rgba[3], rgba[0], rgba[1], rgba[2]]
            );
            assert!(rgba[2] > rgba[0], "the blue visor must survive conversion");
        }
    }

    #[test]
    fn transport_badges_match_the_palette_and_offline_is_a_ring() {
        for size in ICON_SIZES {
            let centre = (size as f32 * 0.81) as u32;
            let offset = ((centre * size + centre) * 4) as usize;
            for (state, colour) in [
                (ConnectionState::Usb, CYAN),
                (ConnectionState::Wifi, BLUE),
                (ConnectionState::Disconnected, AMBER),
                (ConnectionState::DaemonOffline, NAVY),
            ] {
                let pixels = render_argb(state, size);
                assert_eq!(
                    pixels[offset..offset + 4],
                    [255, colour[0], colour[1], colour[2]]
                );
            }
        }
    }

    #[test]
    fn every_connection_state_has_a_distinct_icon() {
        for (index, first) in STATES.iter().enumerate() {
            for second in &STATES[index + 1..] {
                assert_ne!(render_argb(*first, 32), render_argb(*second, 32));
            }
        }
    }
}

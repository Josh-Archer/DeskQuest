//! Tray icon pixels, generated in code so there are no image assets to ship.
//!
//! Each state is a round badge: a filled disk with a white centre dot when a
//! headset is connected or the daemon is up, and a hollow ring when the daemon
//! cannot be reached. Colour carries the transport: green for USB, blue for Wi-Fi,
//! grey otherwise.

use super::model::ConnectionState;

/// Pixmap sizes offered to the tray host. Hosts pick whichever suits their panel.
pub const ICON_SIZES: [u32; 2] = [22, 32];

const GREY: [u8; 3] = [0x80, 0x80, 0x80];
const GREEN: [u8; 3] = [0x2e, 0x9e, 0x4f];
const BLUE: [u8; 3] = [0x2f, 0x7b, 0xd9];
const WHITE: [u8; 3] = [0xff, 0xff, 0xff];

fn disk_colour(state: ConnectionState) -> [u8; 3] {
    match state {
        ConnectionState::DaemonOffline | ConnectionState::Disconnected => GREY,
        ConnectionState::Usb => GREEN,
        ConnectionState::Wifi => BLUE,
    }
}

/// Anti-aliased coverage of a disk of `radius` at distance `d` from its centre.
fn coverage(d: f32, radius: f32) -> f32 {
    (radius - d + 0.5).clamp(0.0, 1.0)
}

/// Renders `state` as a `size` x `size` ARGB32 image in SNI order (A, R, G, B per
/// pixel, rows top to bottom), with non-premultiplied alpha.
pub fn render_argb(state: ConnectionState, size: u32) -> Vec<u8> {
    let centre = size as f32 / 2.0;
    let outer = centre - 1.0;
    let dot = outer * 0.35;
    let hole = outer * 0.6;
    let disk = disk_colour(state);

    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 + 0.5 - centre;
            let dy = y as f32 + 0.5 - centre;
            let d = (dx * dx + dy * dy).sqrt();
            let edge = coverage(d, outer);

            let (alpha, rgb) = match state {
                ConnectionState::DaemonOffline => {
                    // Ring: outer disk minus the inner hole.
                    let alpha = edge * (1.0 - coverage(d, hole));
                    (alpha, disk)
                }
                _ => {
                    let w = coverage(d, dot);
                    let rgb = [
                        mix(disk[0], WHITE[0], w),
                        mix(disk[1], WHITE[1], w),
                        mix(disk[2], WHITE[2], w),
                    ];
                    (edge, rgb)
                }
            };

            out.extend_from_slice(&[to_byte(alpha), rgb[0], rgb[1], rgb[2]]);
        }
    }
    out
}

fn mix(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t).round() as u8
}

fn to_byte(v: f32) -> u8 {
    (v * 255.0).round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns (A, R, G, B) for pixel (x, y).
    fn px(buf: &[u8], size: u32, x: u32, y: u32) -> (u8, u8, u8, u8) {
        let i = ((y * size + x) * 4) as usize;
        (buf[i], buf[i + 1], buf[i + 2], buf[i + 3])
    }

    #[test]
    fn buffer_length_matches_size() {
        for size in ICON_SIZES {
            let buf = render_argb(ConnectionState::Usb, size);
            assert_eq!(buf.len(), (size * size * 4) as usize);
        }
    }

    #[test]
    fn corners_are_transparent_for_every_state() {
        let size = 22;
        for state in [
            ConnectionState::DaemonOffline,
            ConnectionState::Disconnected,
            ConnectionState::Usb,
            ConnectionState::Wifi,
        ] {
            let buf = render_argb(state, size);
            assert_eq!(px(&buf, size, 0, 0).0, 0);
            assert_eq!(px(&buf, size, size - 1, size - 1).0, 0);
        }
    }

    #[test]
    fn connected_badges_have_white_centre_and_coloured_ring() {
        let size = 22;
        let c = size / 2;
        // Ring sample: 80% of the way out, clear of the centre dot.
        let ring_x = c + 8;
        let usb = render_argb(ConnectionState::Usb, size);
        assert_eq!(px(&usb, size, c, c), (255, 255, 255, 255));
        let (a, r, g, b) = px(&usb, size, ring_x, c);
        assert_eq!(a, 255);
        assert!(g > r && g > b, "USB ring should be green, got {r},{g},{b}");

        let wifi = render_argb(ConnectionState::Wifi, size);
        let (a, r, g, b) = px(&wifi, size, ring_x, c);
        assert_eq!(a, 255);
        assert!(b > r && b > g, "Wi-Fi ring should be blue, got {r},{g},{b}");
    }

    #[test]
    fn offline_is_hollow_ring() {
        let size = 22;
        let c = size / 2;
        let offline = render_argb(ConnectionState::DaemonOffline, size);
        assert_eq!(px(&offline, size, c, c).0, 0, "centre should be a hole");
        assert_eq!(px(&offline, size, c + 8, c).0, 255, "ring should be opaque");
    }

    #[test]
    fn sizes_are_deterministic() {
        let a = render_argb(ConnectionState::Wifi, 32);
        let b = render_argb(ConnectionState::Wifi, 32);
        assert_eq!(a, b);
    }
}

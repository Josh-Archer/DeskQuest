//! A small event-driven 3D renderer for the Blender export. No GPU context or render loop.
use super::Model;
use gtk::cairo;
use std::sync::OnceLock;

type Point = [f32; 3];

#[derive(Clone)]
struct Triangle {
    vertices: [Point; 3],
    normals: [Point; 3],
    color: Point,
}

struct Mesh {
    triangles: Vec<Triangle>,
    center: Point,
}

impl Mesh {
    fn parse(bytes: &[u8]) -> Result<Self, &'static str> {
        if bytes.len() < 8 || &bytes[..4] != b"LQM2" {
            return Err("invalid mesh header");
        }
        let count = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        if count == 0 || count > 30_000 || bytes.len() != 8 + count * 84 {
            return Err("invalid triangle count");
        }
        let mut min = [f32::INFINITY; 3];
        let mut max = [f32::NEG_INFINITY; 3];
        let mut triangles = Vec::with_capacity(count);
        for record in bytes[8..].as_chunks::<84>().0 {
            let mut values = [0f32; 21];
            for (value, bytes) in values.iter_mut().zip(record.as_chunks::<4>().0) {
                *value = f32::from_le_bytes(*bytes);
                if !value.is_finite() {
                    return Err("non-finite mesh data");
                }
            }
            let vertices: [Point; 3] = [
                values[0..3].try_into().unwrap(),
                values[3..6].try_into().unwrap(),
                values[6..9].try_into().unwrap(),
            ];
            for vertex in &vertices {
                for axis in 0..3 {
                    min[axis] = min[axis].min(vertex[axis]);
                    max[axis] = max[axis].max(vertex[axis]);
                }
            }
            let normals: [Point; 3] = [
                values[9..12].try_into().unwrap(),
                values[12..15].try_into().unwrap(),
                values[15..18].try_into().unwrap(),
            ];
            if normals
                .iter()
                .any(|normal| normal.iter().map(|v| v * v).sum::<f32>() < 0.5)
            {
                return Err("invalid normal");
            }
            let color: Point = values[18..21].try_into().unwrap();
            if color.iter().any(|v| !(0.0..=1.0).contains(v)) {
                return Err("invalid material color");
            }
            triangles.push(Triangle {
                vertices,
                normals,
                color,
            });
        }
        Ok(Self {
            triangles,
            center: std::array::from_fn(|i| (min[i] + max[i]) * 0.5),
        })
    }
}

fn mesh(model: Model) -> &'static Mesh {
    static QUEST: OnceLock<Mesh> = OnceLock::new();
    static GENERIC: OnceLock<Mesh> = OnceLock::new();
    let (slot, bytes): (&OnceLock<Mesh>, &[u8]) = match model {
        Model::Quest3 => (
            &QUEST,
            include_bytes!("../../../../assets/headsets/quest-3.lqmesh"),
        ),
        Model::Generic => (
            &GENERIC,
            include_bytes!("../../../../assets/headsets/generic.lqmesh"),
        ),
    };
    slot.get_or_init(|| Mesh::parse(bytes).expect("validated embedded headset mesh"))
}

#[derive(Debug, Clone, Copy)]
pub struct Orbit {
    pub yaw: f32,
    pub pitch: f32,
    pub zoom: f32,
}

impl Default for Orbit {
    fn default() -> Self {
        Self {
            yaw: -0.5,
            pitch: 0.32,
            zoom: 1.0,
        }
    }
}

impl Orbit {
    fn rotate(self, p: Point) -> Point {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        let x = p[0] * cy + p[2] * sy;
        let z = -p[0] * sy + p[2] * cy;
        [x, p[1] * cp - z * sp, p[1] * sp + z * cp]
    }
}

#[derive(Clone, Copy)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Copy)]
struct Projected {
    points: [(f64, f64); 3],
    depths: [f32; 3],
    colors: [[f64; 3]; 3],
}

fn project(mesh: &Mesh, orbit: Orbit, rect: Rect) -> Vec<Projected> {
    let vertices: Vec<_> = mesh
        .triangles
        .iter()
        .map(|triangle| {
            triangle
                .vertices
                .map(|v| orbit.rotate(std::array::from_fn(|i| v[i] - mesh.center[i])))
        })
        .collect();
    let mut min = [f32::INFINITY; 2];
    let mut max = [f32::NEG_INFINITY; 2];
    for vertex in vertices.iter().flatten() {
        for axis in 0..2 {
            min[axis] = min[axis].min(vertex[axis]);
            max[axis] = max[axis].max(vertex[axis]);
        }
    }
    let scale = (rect.width / f64::from(max[0] - min[0]).max(0.001))
        .min(rect.height / f64::from(max[1] - min[1]).max(0.001))
        * 0.88
        * f64::from(orbit.zoom);
    let center = [(min[0] + max[0]) * 0.5, (min[1] + max[1]) * 0.5];
    let light = [0.35, 0.65, 0.67];
    let projected: Vec<_> = mesh
        .triangles
        .iter()
        .zip(vertices)
        .filter_map(|(triangle, vertices)| {
            let u: Point = std::array::from_fn(|i| vertices[1][i] - vertices[0][i]);
            let v: Point = std::array::from_fn(|i| vertices[2][i] - vertices[0][i]);
            if u[0] * v[1] - u[1] * v[0] <= 0.0 {
                return None;
            }
            let colors = triangle.normals.map(|normal| {
                let normal = orbit.rotate(normal);
                let diffuse =
                    (normal[0] * light[0] + normal[1] * light[1] + normal[2] * light[2]).max(0.0);
                let brightness = 0.48 + 0.52 * f64::from(diffuse);
                triangle
                    .color
                    .map(|channel| (f64::from(channel) * brightness).clamp(0.0, 1.0))
            });
            Some(Projected {
                points: vertices.map(|p| {
                    (
                        rect.x + rect.width / 2.0 + f64::from(p[0] - center[0]) * scale,
                        rect.y + rect.height / 2.0 - f64::from(p[1] - center[1]) * scale,
                    )
                }),
                depths: vertices.map(|vertex| vertex[2]),
                colors,
            })
        })
        .collect();
    projected
}

/// Per-pixel depth prevents a large shell triangle from painting over a small camera
/// lens. The image is supersampled and only rebuilt on exposure or interaction.
fn rasterize(triangles: &[Projected], width: i32, height: i32) -> cairo::ImageSurface {
    let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, width, height)
        .expect("headset image surface");
    let stride = surface.stride() as usize;
    let mut depths = vec![f32::NEG_INFINITY; width as usize * height as usize];
    {
        let mut data = surface.data().expect("exclusive headset image");
        for triangle in triangles {
            let [a, b, c] = triangle.points;
            let denominator = (b.1 - c.1) * (a.0 - c.0) + (c.0 - b.0) * (a.1 - c.1);
            if denominator.abs() < 0.00001 {
                continue;
            }
            let min_x = a.0.min(b.0).min(c.0).floor().max(0.0) as i32;
            let max_x = a.0.max(b.0).max(c.0).ceil().min(f64::from(width - 1)) as i32;
            let min_y = a.1.min(b.1).min(c.1).floor().max(0.0) as i32;
            let max_y = a.1.max(b.1).max(c.1).ceil().min(f64::from(height - 1)) as i32;
            for y in min_y..=max_y {
                for x in min_x..=max_x {
                    let (px, py) = (f64::from(x) + 0.5, f64::from(y) + 0.5);
                    let wa = ((b.1 - c.1) * (px - c.0) + (c.0 - b.0) * (py - c.1)) / denominator;
                    let wb = ((c.1 - a.1) * (px - c.0) + (a.0 - c.0) * (py - c.1)) / denominator;
                    let wc = 1.0 - wa - wb;
                    if wa < -0.000001 || wb < -0.000001 || wc < -0.000001 {
                        continue;
                    }
                    let z = wa as f32 * triangle.depths[0]
                        + wb as f32 * triangle.depths[1]
                        + wc as f32 * triangle.depths[2];
                    let index = y as usize * width as usize + x as usize;
                    if z > depths[index] {
                        depths[index] = z;
                        let offset = y as usize * stride + x as usize * 4;
                        let rgb: [u8; 3] = std::array::from_fn(|channel| {
                            ((triangle.colors[0][channel] * wa
                                + triangle.colors[1][channel] * wb
                                + triangle.colors[2][channel] * wc)
                                .clamp(0.0, 1.0)
                                * 255.)
                                .round() as u8
                        });
                        let pixel = u32::from_be_bytes([255, rgb[0], rgb[1], rgb[2]]).to_ne_bytes();
                        data[offset..offset + 4].copy_from_slice(&pixel);
                    }
                }
            }
        }
    }
    surface
}

fn image(model: Model, orbit: Orbit, width: i32, height: i32) -> cairo::ImageSurface {
    let triangles = project(
        mesh(model),
        orbit,
        Rect {
            x: 0.,
            y: 0.,
            width: f64::from(width),
            height: f64::from(height),
        },
    );
    rasterize(&triangles, width, height)
}

pub fn draw(cr: &cairo::Context, model: Model, rect: Rect, orbit: Orbit) {
    if rect.width <= 0.0 || rect.height <= 0.0 {
        return;
    }
    let sampling = 2.0;
    let width = (rect.width * sampling).ceil().clamp(1.0, 2048.0) as i32;
    let height = (rect.height * sampling).ceil().clamp(1.0, 2048.0) as i32;
    let surface = image(model, orbit, width, height);
    surface.set_device_scale(
        f64::from(width) / rect.width,
        f64::from(height) / rect.height,
    );
    let _ = cr.save();
    cr.rectangle(rect.x, rect.y, rect.width, rect.height);
    cr.clip();
    if cr.set_source_surface(&surface, rect.x, rect.y).is_ok() {
        let _ = cr.paint();
    }
    let _ = cr.restore();
}

struct OriginImage {
    model: Model,
    scale: i32,
    surface: cairo::ImageSurface,
}

/// Cache the small top-down marker so the status timer does no 3D rendering work.
pub fn draw_origin(cr: &cairo::Context, model: Model, x: f64, y: f64, scale: i32) {
    thread_local! { static ORIGINS: std::cell::RefCell<Vec<OriginImage>> = const { std::cell::RefCell::new(Vec::new()) }; }
    ORIGINS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if !cache
            .iter()
            .any(|entry| entry.model == model && entry.scale == scale)
        {
            let sampling = scale.max(1) * 2;
            let surface = image(
                model,
                Orbit {
                    yaw: std::f32::consts::PI,
                    pitch: 1.2,
                    zoom: 1.0,
                },
                68 * sampling,
                54 * sampling,
            );
            surface.set_device_scale(f64::from(sampling), f64::from(sampling));
            cache.push(OriginImage {
                model,
                scale,
                surface,
            });
        }
        let entry = cache
            .iter()
            .find(|entry| entry.model == model && entry.scale == scale)
            .unwrap();
        let _ = cr.save();
        if cr
            .set_source_surface(&entry.surface, x - 34., y - 27.)
            .is_ok()
        {
            let _ = cr.paint();
        }
        let _ = cr.restore();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exported_meshes_are_finite_and_have_materials() {
        for model in [Model::Quest3, Model::Generic] {
            let mesh = mesh(model);
            assert!(mesh.triangles.len() > 1000);
            assert!(mesh.triangles.iter().any(|t| t.color[0] < 0.1));
            assert!(mesh.triangles.iter().any(|t| t.color[0] > 0.8));
        }
    }

    #[test]
    fn projection_stays_inside_its_viewport_at_different_angles() {
        let rect = Rect {
            x: 10.,
            y: 20.,
            width: 480.,
            height: 360.,
        };
        for yaw in [-3.0, -1.0, 0.0, 1.0, 3.0] {
            for pitch in [-1.3, 0.0, 1.3] {
                let projected = project(
                    mesh(Model::Quest3),
                    Orbit {
                        yaw,
                        pitch,
                        zoom: 1.0,
                    },
                    rect,
                );
                assert!(projected.len() > 100);
                for point in projected.iter().flat_map(|triangle| triangle.points) {
                    assert!((rect.x..=rect.x + rect.width).contains(&point.0));
                    assert!((rect.y..=rect.y + rect.height).contains(&point.1));
                }
            }
        }
    }

    #[test]
    fn truncated_and_non_finite_assets_are_rejected() {
        assert!(Mesh::parse(b"LQM2").is_err());
        let mut bytes = include_bytes!("../../../../assets/headsets/quest-3.lqmesh").to_vec();
        bytes[8..12].copy_from_slice(&f32::NAN.to_le_bytes());
        assert!(Mesh::parse(&bytes).is_err());
    }
    #[test]
    fn nearer_geometry_wins_regardless_of_triangle_order() {
        let points = [(0., 0.), (8., 0.), (0., 8.)];
        let near = Projected {
            points,
            depths: [2.; 3],
            colors: [[1., 0., 0.]; 3],
        };
        let far = Projected {
            points,
            depths: [1.; 3],
            colors: [[0., 0., 1.]; 3],
        };
        for triangles in [[near, far], [far, near]] {
            let mut surface = rasterize(&triangles, 8, 8);
            let stride = surface.stride() as usize;
            let data = surface.data().unwrap();
            let offset = 2 * stride + 2 * 4;
            assert_eq!(&data[offset..offset + 4], &0xffff0000u32.to_ne_bytes());
        }
    }
    #[test]
    fn a_full_size_preview_has_a_transparent_background_and_visible_geometry() {
        let started = std::time::Instant::now();
        for yaw in [-0.5, 0.5, 2.0] {
            let mut surface = image(
                Model::Quest3,
                Orbit {
                    yaw,
                    ..Default::default()
                },
                640,
                420,
            );
            let data = surface.data().unwrap();
            let visible = data
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|pixel| u32::from_ne_bytes(**pixel) >> 24 != 0)
                .count();
            assert!(visible > 10_000 && visible < 640 * 420);
        }
        eprintln!(
            "640x420 headset render: {:.2} ms/frame",
            started.elapsed().as_secs_f64() * 1000. / 3.
        );
    }
}

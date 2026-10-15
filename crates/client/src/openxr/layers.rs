//! OpenXR composition layer construction for Quad, Cylinder, and HUD layers.

use openxr::{
    CompositionLayerCylinderKHR, CompositionLayerFlags, CompositionLayerQuad, Extent2Df, Extent2Di,
    EyeVisibility, Graphics, Offset2Di, Posef, Quaternionf, Rect2Di, Space, Swapchain,
    SwapchainSubImage, Vector3f,
};

use crate::config::DisplayMode;

/// Parameters defining the 3D spatial presentation of the virtual monitor.
#[derive(Debug, Clone, PartialEq)]
pub struct DesktopLayerConfig {
    /// Display geometry mode (Flat or Curved).
    pub mode: DisplayMode,
    /// Physical dimensions in meters for FlatQuad mode (width, height).
    pub quad_size: (f32, f32),
    /// Distance from head origin in meters.
    pub distance: f32,
    /// Cylinder curve radius in meters.
    pub cylinder_radius: f32,
    /// Cylinder arc central angle in radians (e.g. 1.2 rad ≈ 68.75°).
    pub cylinder_central_angle: f32,
    /// Cylinder aspect ratio (width / height).
    pub cylinder_aspect_ratio: f32,
    /// Optional 6DoF pose override for custom spatial positioning.
    pub pose: Option<Posef>,
}

impl Default for DesktopLayerConfig {
    fn default() -> Self {
        Self {
            mode: DisplayMode::CurvedCylinder,
            quad_size: (1.6, 0.9),
            distance: 1.2,
            cylinder_radius: 1.5,
            cylinder_central_angle: 1.2,
            cylinder_aspect_ratio: 16.0 / 9.0,
            pose: None,
        }
    }
}

/// Builds an `XrCompositionLayerQuad` for flat desktop display presentation.
pub fn build_quad_layer<'a, G: Graphics>(
    space: &'a Space,
    swapchain: &'a Swapchain<G>,
    width_px: u32,
    height_px: u32,
    config: &DesktopLayerConfig,
) -> CompositionLayerQuad<'a, G> {
    let sub_image = SwapchainSubImage::new()
        .swapchain(swapchain)
        .image_rect(Rect2Di {
            offset: Offset2Di { x: 0, y: 0 },
            extent: Extent2Di {
                width: width_px as i32,
                height: height_px as i32,
            },
        });

    let pose = if let Some(custom_pose) = config.pose {
        custom_pose
    } else {
        Posef {
            orientation: Quaternionf::IDENTITY,
            position: Vector3f {
                x: 0.0,
                y: 1.4, // Eye level in stage space
                z: -config.distance,
            },
        }
    };

    let size = Extent2Df {
        width: config.quad_size.0,
        height: config.quad_size.1,
    };

    CompositionLayerQuad::new()
        .layer_flags(CompositionLayerFlags::CORRECT_CHROMATIC_ABERRATION)
        .space(space)
        .eye_visibility(EyeVisibility::BOTH)
        .sub_image(sub_image)
        .pose(pose)
        .size(size)
}

/// Builds an `XrCompositionLayerCylinderKHR` for immersive curved desktop presentation.
pub fn build_cylinder_layer<'a, G: Graphics>(
    space: &'a Space,
    swapchain: &'a Swapchain<G>,
    width_px: u32,
    height_px: u32,
    config: &DesktopLayerConfig,
) -> CompositionLayerCylinderKHR<'a, G> {
    let sub_image = SwapchainSubImage::new()
        .swapchain(swapchain)
        .image_rect(Rect2Di {
            offset: Offset2Di { x: 0, y: 0 },
            extent: Extent2Di {
                width: width_px as i32,
                height: height_px as i32,
            },
        });

    // For curved desktop centered on the user, the cylinder axis passes through
    // the reference space origin, and radius defines the viewing distance.
    let pose = if let Some(custom_pose) = config.pose {
        custom_pose
    } else {
        Posef {
            orientation: Quaternionf::IDENTITY,
            position: Vector3f {
                x: 0.0,
                y: 1.4, // Eye level in stage space
                z: 0.0,
            },
        }
    };

    CompositionLayerCylinderKHR::new()
        .layer_flags(CompositionLayerFlags::CORRECT_CHROMATIC_ABERRATION)
        .space(space)
        .eye_visibility(EyeVisibility::BOTH)
        .sub_image(sub_image)
        .pose(pose)
        .radius(config.cylinder_radius)
        .central_angle(config.cylinder_central_angle)
        .aspect_ratio(config.cylinder_aspect_ratio)
}

/// Builds an `XrCompositionLayerPassthroughFB` to blend virtual displays with the real-world environment.
pub fn build_passthrough_layer<'a, G: Graphics>(
    space: &'a Space,
    passthrough_layer: &'a openxr::PassthroughLayerFB,
) -> crate::openxr::passthrough::CompositionLayerPassthroughFB<'a, G> {
    crate::openxr::passthrough::CompositionLayerPassthroughFB::new()
        .flags(CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA)
        .space(space)
        .layer_handle(passthrough_layer)
}

/// Builds an `XrCompositionLayerQuad` directly from a `ScreenTransform`.
pub fn build_quad_layer_from_transform<'a, G: Graphics>(
    space: &'a Space,
    swapchain: &'a Swapchain<G>,
    width_px: u32,
    height_px: u32,
    transform: &crate::spatial::ScreenTransform,
) -> CompositionLayerQuad<'a, G> {
    let config = transform.to_desktop_layer_config();
    build_quad_layer(space, swapchain, width_px, height_px, &config)
}

/// Builds an `XrCompositionLayerCylinderKHR` directly from a `ScreenTransform`.
pub fn build_cylinder_layer_from_transform<'a, G: Graphics>(
    space: &'a Space,
    swapchain: &'a Swapchain<G>,
    width_px: u32,
    height_px: u32,
    transform: &crate::spatial::ScreenTransform,
) -> CompositionLayerCylinderKHR<'a, G> {
    let config = transform.to_desktop_layer_config();
    build_cylinder_layer(space, swapchain, width_px, height_px, &config)
}

/// Builds a head-locked performance telemetry HUD quad layer.
pub fn build_hud_layer<'a, G: Graphics>(
    view_space: &'a Space,
    hud_swapchain: &'a Swapchain<G>,
    hud_width_px: u32,
    hud_height_px: u32,
) -> CompositionLayerQuad<'a, G> {
    let sub_image = SwapchainSubImage::new()
        .swapchain(hud_swapchain)
        .image_rect(Rect2Di {
            offset: Offset2Di { x: 0, y: 0 },
            extent: Extent2Di {
                width: hud_width_px as i32,
                height: hud_height_px as i32,
            },
        });

    // Positioned below line-of-sight in view space
    let pose = Posef {
        orientation: Quaternionf::IDENTITY,
        position: Vector3f {
            x: 0.0,
            y: -0.28,
            z: -0.85,
        },
    };

    let size = Extent2Df {
        width: 0.45,
        height: 0.22,
    };

    CompositionLayerQuad::new()
        .layer_flags(
            CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA
                | CompositionLayerFlags::CORRECT_CHROMATIC_ABERRATION,
        )
        .space(view_space)
        .eye_visibility(EyeVisibility::BOTH)
        .sub_image(sub_image)
        .pose(pose)
        .size(size)
}

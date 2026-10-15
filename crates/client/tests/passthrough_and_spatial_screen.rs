use std::f32::consts::FRAC_PI_2;
use std::path::PathBuf;

use linux_quest_client::config::ClientConfig;
use linux_quest_client::openxr::{
    CompositionLayerPassthroughFB, MockPassthroughBackend, OpenXrContext, PassthroughBackend,
    PassthroughState,
};
use linux_quest_client::runtime::QuestClientRuntime;
use linux_quest_client::spatial::{
    Quat, ScreenTransform, SpatialLayoutStore, SpatialScreenLayout, Vec3, CURRENT_LAYOUT_VERSION,
    DEFAULT_DISTANCE_M, DEFAULT_HEIGHT_M, MAX_CURVE_ANGLE_RAD, MAX_CURVE_RADIUS_M, MAX_DISTANCE_M,
    MAX_TILT_RAD, MAX_YAW_RAD, MIN_CURVE_ANGLE_RAD, MIN_CURVE_RADIUS_M, MIN_DISTANCE_M,
    MIN_TILT_RAD, MIN_YAW_RAD,
};
use openxr::EnvironmentBlendMode;

struct TestTempDir {
    path: PathBuf,
}

impl TestTempDir {
    fn new(prefix: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "{prefix}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TestTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn test_passthrough_mock_backend_transitions_and_blend_modes() {
    let mut backend = MockPassthroughBackend::new(true, true);
    assert!(backend.is_supported());
    assert!(backend.is_enabled());
    assert_eq!(backend.state(), PassthroughState::Active);
    assert_eq!(backend.blend_mode(), EnvironmentBlendMode::ALPHA_BLEND);

    // Disable passthrough
    backend.set_enabled(false).unwrap();
    assert!(!backend.is_enabled());
    assert_eq!(backend.state(), PassthroughState::Disabled);
    assert_eq!(backend.blend_mode(), EnvironmentBlendMode::OPAQUE);
    assert_eq!(backend.paused_count(), 1);

    // Re-enable passthrough
    backend.set_enabled(true).unwrap();
    assert!(backend.is_enabled());
    assert_eq!(backend.state(), PassthroughState::Active);
    assert_eq!(backend.blend_mode(), EnvironmentBlendMode::ALPHA_BLEND);
    assert_eq!(backend.resumed_count(), 1);
    assert_eq!(backend.set_enabled_calls(), 2);
}

#[test]
fn test_passthrough_unsupported_fallback_and_safety() {
    let mut unsupported = MockPassthroughBackend::unsupported();
    assert!(!unsupported.is_supported());
    assert!(!unsupported.is_enabled());
    assert_eq!(unsupported.state(), PassthroughState::Unsupported);
    assert_eq!(unsupported.blend_mode(), EnvironmentBlendMode::OPAQUE);

    // Enabling passthrough on unsupported hardware gracefully keeps it disabled & opaque
    unsupported.set_enabled(true).unwrap();
    assert!(!unsupported.is_enabled());
    assert_eq!(unsupported.state(), PassthroughState::Unsupported);
    assert_eq!(unsupported.blend_mode(), EnvironmentBlendMode::OPAQUE);

    // When alpha blend is unsupported by the runtime, falls back to OPAQUE
    let no_alpha = MockPassthroughBackend::new(true, true).with_alpha_blend(false);
    assert!(no_alpha.is_supported());
    assert!(no_alpha.is_enabled());
    assert_eq!(no_alpha.blend_mode(), EnvironmentBlendMode::OPAQUE);
}

#[test]
fn test_composition_layer_passthrough_builder_and_deref() {
    let layer: CompositionLayerPassthroughFB<'_, openxr::Vulkan> =
        CompositionLayerPassthroughFB::new();
    assert_eq!(
        layer.as_raw().ty,
        openxr::sys::StructureType::COMPOSITION_LAYER_PASSTHROUGH_FB
    );

    let flagged = layer.flags(openxr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA);
    assert!(flagged
        .as_raw()
        .flags
        .contains(openxr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA));

    // CompositionLayerPassthroughFB dereferences to CompositionLayerBase
    let base: &openxr::CompositionLayerBase<'_, openxr::Vulkan> = &flagged;
    let _ = base;
}

#[test]
fn test_openxr_context_passthrough_integration() {
    let config = ClientConfig {
        enable_passthrough: true,
        ..Default::default()
    };
    let mut ctx = OpenXrContext::new_simulated(&config);

    assert!(ctx.is_passthrough_supported());
    assert!(ctx.is_passthrough_enabled());
    assert_eq!(
        ctx.passthrough().blend_mode(),
        EnvironmentBlendMode::ALPHA_BLEND
    );

    // Toggle off
    ctx.set_passthrough_enabled(false).unwrap();
    assert!(!ctx.is_passthrough_enabled());
    assert_eq!(ctx.passthrough().blend_mode(), EnvironmentBlendMode::OPAQUE);

    // Toggle on
    ctx.set_passthrough_enabled(true).unwrap();
    assert!(ctx.is_passthrough_enabled());
    assert_eq!(
        ctx.passthrough().blend_mode(),
        EnvironmentBlendMode::ALPHA_BLEND
    );
}

#[test]
fn test_spatial_math_vectors_and_quaternions() {
    let v1 = Vec3::new(1.0, 2.0, 3.0);
    let v2 = Vec3::new(2.0, -1.0, 0.5);
    let sum = v1 + v2;
    assert_eq!(sum, Vec3::new(3.0, 1.0, 3.5));

    let len = Vec3::new(3.0, 4.0, 0.0).length();
    assert!((len - 5.0).abs() < 1e-6);

    let unit_x = Vec3::UNIT_X;
    let unit_y = Vec3::UNIT_Y;
    let unit_z = unit_x.cross(unit_y);
    assert!((unit_z - Vec3::UNIT_Z).length() < 1e-6);

    // Quaternion identity and Euler angles
    let q_ident = Quat::IDENTITY;
    let (p, y, r) = q_ident.to_euler();
    assert_eq!(p, 0.0);
    assert_eq!(y, 0.0);
    assert_eq!(r, 0.0);

    let pitch_rad = 0.35;
    let yaw_rad = -0.75;
    let roll_rad = 0.0;
    let q_rot = Quat::from_euler(pitch_rad, yaw_rad, roll_rad);
    let (p_res, y_res, r_res) = q_rot.to_euler();
    assert!((p_res - pitch_rad).abs() < 1e-4);
    assert!((y_res - yaw_rad).abs() < 1e-4);
    assert!(r_res.abs() < 1e-4);

    // Vector rotation: 90 degrees around Y rotates -Z (forward) to -X (left)
    let q_90_y = Quat::from_euler(0.0, FRAC_PI_2, 0.0);
    let forward = Vec3::new(0.0, 0.0, -1.0);
    let rotated = q_90_y.rotate_vec(forward);
    assert!((rotated.x - (-1.0)).abs() < 1e-5);
    assert!(rotated.y.abs() < 1e-5);
    assert!(rotated.z.abs() < 1e-5);
}

#[test]
fn test_screen_transform_and_ergonomic_clamping() {
    let mut transform = ScreenTransform::default();
    assert_eq!(transform.distance, DEFAULT_DISTANCE_M);
    assert_eq!(transform.position.z, -DEFAULT_DISTANCE_M);
    assert_eq!(transform.position.y, DEFAULT_HEIGHT_M);

    // 1. Distance push/pull with clamping
    transform.push_pull(0.3);
    assert!((transform.distance - (DEFAULT_DISTANCE_M + 0.3)).abs() < 1e-4);

    // Overshoot MAX_DISTANCE_M
    transform.set_distance(100.0);
    assert_eq!(transform.distance, MAX_DISTANCE_M);

    // Undershoot MIN_DISTANCE_M
    transform.set_distance(0.05);
    assert_eq!(transform.distance, MIN_DISTANCE_M);

    // 2. Tilt clamping
    transform.set_tilt(5.0);
    assert_eq!(transform.tilt, MAX_TILT_RAD);
    transform.set_tilt(-5.0);
    assert_eq!(transform.tilt, MIN_TILT_RAD);

    // 3. Yaw clamping
    transform.set_yaw(10.0);
    assert_eq!(transform.yaw, MAX_YAW_RAD);
    transform.set_yaw(-10.0);
    assert_eq!(transform.yaw, MIN_YAW_RAD);

    // 4. Curvature clamping
    transform.set_curve(100.0, 10.0);
    assert_eq!(transform.curve_radius, MAX_CURVE_RADIUS_M);
    assert_eq!(transform.curve_angle, MAX_CURVE_ANGLE_RAD);

    transform.set_curve(0.01, 0.01);
    assert_eq!(transform.curve_radius, MIN_CURVE_RADIUS_M);
    assert_eq!(transform.curve_angle, MIN_CURVE_ANGLE_RAD);

    // 5. Size and aspect ratio clamping
    transform.set_quad_size(20.0, 0.01);
    assert_eq!(transform.quad_size.0, 5.0);
    assert_eq!(transform.quad_size.1, 0.1);

    transform.set_aspect_ratio(50.0);
    assert_eq!(transform.aspect_ratio, 4.0);
}

#[test]
fn test_screen_transform_translation_and_rotation_methods() {
    let mut transform = ScreenTransform::default();

    // Translation
    transform.translate(Vec3::new(0.2, -0.1, 0.0));
    assert!((transform.position.x - 0.2).abs() < 1e-4);
    assert!((transform.position.y - (DEFAULT_HEIGHT_M - 0.1)).abs() < 1e-4);

    // Rotation
    transform.rotate(0.1, -0.2, 0.0);
    assert!((transform.tilt - 0.1).abs() < 1e-4);
    assert!((transform.yaw - (-0.2)).abs() < 1e-4);
}

#[test]
fn test_mapping_to_quad_and_cylinder_layer_parameters() {
    let mut transform = ScreenTransform::default();
    transform.set_quad_size(2.4, 1.35);
    transform.set_curve(1.8, 1.4);
    transform.set_aspect_ratio(16.0 / 9.0);

    // Flat quad parameters
    let quad = transform.to_quad_layer_params();
    assert_eq!(quad.size.width, 2.4);
    assert_eq!(quad.size.height, 1.35);
    assert_eq!(quad.pose.position.z, transform.position.z);

    // Curved cylinder parameters
    let cyl = transform.to_cylinder_layer_params();
    assert_eq!(cyl.radius, 1.8);
    assert_eq!(cyl.central_angle, 1.4);
    assert!((cyl.aspect_ratio - (16.0 / 9.0)).abs() < 1e-4);
    assert_eq!(cyl.pose.position.x, 0.0);
    assert_eq!(cyl.pose.position.y, transform.position.y);
    assert_eq!(cyl.pose.position.z, 0.0);

    // Interop with DesktopLayerConfig
    let desktop_cfg = transform.to_desktop_layer_config();
    assert_eq!(desktop_cfg.cylinder_radius, 1.8);
    assert_eq!(desktop_cfg.cylinder_central_angle, 1.4);
    assert_eq!(desktop_cfg.quad_size, (2.4, 1.35));
    assert!(desktop_cfg.pose.is_some());

    let from_cfg = ScreenTransform::from_desktop_layer_config(&desktop_cfg);
    assert_eq!(from_cfg.curve_radius, 1.8);
    assert_eq!(from_cfg.curve_angle, 1.4);
    assert_eq!(from_cfg.quad_size, (2.4, 1.35));
}

#[test]
fn test_multi_monitor_spatial_cockpit_layout() {
    let center = ScreenTransform::for_display(0);
    let right = ScreenTransform::for_display(1);
    let left = ScreenTransform::for_display(2);

    // Center monitor is directly forward
    assert_eq!(center.yaw, 0.0);
    assert_eq!(center.position.x, 0.0);

    // Right monitor is offset in +X and angled with negative yaw
    assert!(right.position.x > 0.0);
    assert!(right.yaw < 0.0);

    // Left monitor is offset in -X and angled with positive yaw
    assert!(left.position.x < 0.0);
    assert!(left.yaw > 0.0);
}

#[test]
fn test_spatial_layout_persistence_and_versioning() {
    let dir = TestTempDir::new("layout_test");
    let file_path = dir.path().join("screens_layout.json");
    let store = SpatialLayoutStore::new(&file_path);

    let mut layout = SpatialScreenLayout::new("DeveloperCockpit");
    layout.insert(0, ScreenTransform::for_display(0));
    layout.insert(1, ScreenTransform::for_display(1));
    layout.insert(2, ScreenTransform::for_display(2));
    assert_eq!(layout.len(), 3);

    // Save layout to disk
    store.save(&layout).expect("Save failed");
    assert!(file_path.exists());

    // Restore from disk
    let loaded = store.load().expect("Load failed");
    assert_eq!(loaded.version, CURRENT_LAYOUT_VERSION);
    assert_eq!(loaded.profile_name, Some("DeveloperCockpit".to_string()));
    assert_eq!(loaded.len(), 3);
    assert_eq!(loaded.get(0).unwrap().yaw, 0.0);
    assert!(loaded.get(1).unwrap().yaw < 0.0);
    assert!(loaded.get(2).unwrap().yaw > 0.0);

    // Versioning validation
    let future_json = r#"{
        "version": 42,
        "screens": {},
        "profile_name": "future"
    }"#;
    std::fs::write(&file_path, future_json).unwrap();
    assert!(store.load().is_err());
}

#[test]
fn test_spatial_layout_corrupt_recovery_and_quarantine() {
    let dir = TestTempDir::new("corrupt_layout_test");
    let file_path = dir.path().join("screens.json");
    let store = SpatialLayoutStore::new(&file_path);

    // Corrupt file contents
    std::fs::write(&file_path, b"INVALID_GARBAGE_JSON_HERE").unwrap();

    // Normal load errors out
    assert!(store.load().is_err());

    // load_or_default catches error, quarantines file, and returns safe default
    let recovered = store.load_or_default();
    assert_eq!(recovered.version, CURRENT_LAYOUT_VERSION);
    assert_eq!(recovered.len(), 1);
    assert!(recovered.get(0).is_some());

    // Check that corrupt backup was created
    let mut found_backup = false;
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().to_string();
        if name.contains("corrupt") && name.ends_with(".bak") {
            found_backup = true;
            break;
        }
    }
    assert!(found_backup, "Expected a quarantined corrupt backup file");
}

#[test]
fn test_quest_client_runtime_spatial_and_passthrough_end_to_end() {
    let dir = TestTempDir::new("runtime_spatial_test");
    let layout_path = dir.path().join("runtime_layout.json");

    let config = ClientConfig {
        enable_passthrough: true,
        spatial_layout_path: Some(layout_path.clone()),
        ..Default::default()
    };

    let mut runtime = QuestClientRuntime::new(config).expect("Runtime initialization failed");
    assert!(runtime.is_running());

    // 1. Passthrough controls
    assert!(runtime.is_passthrough_supported());
    assert!(runtime.is_passthrough_enabled());
    assert_eq!(
        runtime.passthrough_blend_mode(),
        EnvironmentBlendMode::ALPHA_BLEND
    );

    let toggled_state = runtime.toggle_passthrough().unwrap();
    assert!(!toggled_state);
    assert!(!runtime.is_passthrough_enabled());
    assert_eq!(
        runtime.passthrough_blend_mode(),
        EnvironmentBlendMode::OPAQUE
    );

    runtime.set_passthrough_enabled(true).unwrap();
    assert!(runtime.is_passthrough_enabled());
    assert_eq!(
        runtime.passthrough_blend_mode(),
        EnvironmentBlendMode::ALPHA_BLEND
    );

    // 2. Spatial screen manipulations
    let initial_dist = runtime.screen_transform().distance;
    runtime.push_pull_screen(0.4);
    assert!((runtime.screen_transform().distance - (initial_dist + 0.4)).abs() < 1e-4);

    runtime.translate_screen(Vec3::new(0.1, 0.05, 0.0));
    assert!(runtime.screen_transform().position.x > 0.0);

    runtime.rotate_screen(0.05, -0.15, 0.0);
    assert!((runtime.screen_transform().tilt - 0.05).abs() < 1e-4);
    assert!((runtime.screen_transform().yaw - (-0.15)).abs() < 1e-4);

    runtime.set_screen_curve(2.2, 1.6);
    assert_eq!(runtime.screen_transform().curve_radius, 2.2);
    assert_eq!(runtime.screen_transform().curve_angle, 1.6);

    // 3. Step frame presentation
    let pacing = runtime.step_frame().expect("step_frame failed");
    assert!(pacing.predicted_display_time > 0);

    // 4. Save and load layout via runtime methods
    runtime
        .save_spatial_layout(&layout_path)
        .expect("save_spatial_layout failed");
    assert!(layout_path.exists());

    let runtime2 = QuestClientRuntime::new(ClientConfig {
        spatial_layout_path: Some(layout_path.clone()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(runtime2.screen_transform().curve_radius, 2.2);
    assert_eq!(runtime2.screen_transform().curve_angle, 1.6);
}

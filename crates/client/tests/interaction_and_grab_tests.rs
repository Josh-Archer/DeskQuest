//! Integration tests for controller & hand 6DoF interaction, ray intersections,
//! gesture hysteresis, and layout persistence on release.

use std::f32::consts::FRAC_PI_2;
use std::path::PathBuf;

use linux_quest_client::config::{ClientConfig, DisplayMode};
use linux_quest_client::openxr::{
    HandTrackingBackend, MockHandTrackingBackend, MockInteractionBackend, OpenXrContext,
};
use linux_quest_client::runtime::QuestClientRuntime;
use linux_quest_client::spatial::{
    ray_intersects_cylinder, ray_intersects_quad, ControllerInputState, GrabStateMachine,
    HandGestureDetector, HandJoint, HandTrackingData, Handedness, InteractionEvent,
    InteractionInput, InteractionSource, Quat, Ray, ScreenPose, ScreenTransform,
    SpatialLayoutStore, Vec3, DEFAULT_HEIGHT_M,
};

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

// ============================================================================
// 1. Pointing Ray and Ray Intersection Math Tests
// ============================================================================

#[test]
fn test_ray_construction_and_aim_pose_math() {
    let origin = Vec3::new(0.5, 1.4, -0.2);
    let dir = Vec3::new(0.0, 0.0, -2.0);
    let ray = Ray::new(origin, dir);

    assert_eq!(ray.origin, origin);
    assert_eq!(ray.direction, -Vec3::UNIT_Z);
    assert_eq!(ray.at(1.5), Vec3::new(0.5, 1.4, -1.7));

    // OpenXR aim pose pointing forward (-Z) with pitch up 45 deg
    let pitch_up_45 = FRAC_PI_2 * 0.5; // looking up
    let pose = ScreenPose::new(Vec3::ZERO, Quat::from_euler(pitch_up_45, 0.0, 0.0));
    let aim_ray = Ray::from_aim_pose(&pose);
    assert!((aim_ray.direction.length() - 1.0).abs() < 1e-6);
    assert!(aim_ray.direction.y > 0.6); // Positive Y (pointing up)
    assert!(aim_ray.direction.z < -0.6); // Negative Z (pointing forward)
}

#[test]
fn test_ray_intersects_quad_precision_and_boundaries() {
    let quad_pose = ScreenPose::new(Vec3::new(0.0, DEFAULT_HEIGHT_M, -1.5), Quat::IDENTITY);
    let size = (1.6, 0.9);

    // Exact center hit
    let center_ray = Ray::new(Vec3::new(0.0, DEFAULT_HEIGHT_M, 0.0), -Vec3::UNIT_Z);
    let hit = ray_intersects_quad(&center_ray, &quad_pose, size).expect("Must hit quad center");
    assert!((hit.distance - 1.5).abs() < 1e-5);
    assert_eq!(hit.point, Vec3::new(0.0, DEFAULT_HEIGHT_M, -1.5));
    assert!((hit.uv.0 - 0.5).abs() < 1e-5);
    assert!((hit.uv.1 - 0.5).abs() < 1e-5);
    assert_eq!(hit.normal, Vec3::UNIT_Z);

    // Four corners:
    // Top-Left: x = -0.8, y = +0.45 -> uv = (0.0, 0.0)
    let tl_ray = Ray::new(
        Vec3::new(0.0, DEFAULT_HEIGHT_M, 0.0),
        Vec3::new(-0.8, 0.45, -1.5),
    );
    let tl_hit = ray_intersects_quad(&tl_ray, &quad_pose, size).expect("Hit top-left");
    assert!((tl_hit.uv.0 - 0.0).abs() < 1e-4);
    assert!((tl_hit.uv.1 - 0.0).abs() < 1e-4);

    // Bottom-Right: x = +0.8, y = -0.45 -> uv = (1.0, 1.0)
    let br_ray = Ray::new(
        Vec3::new(0.0, DEFAULT_HEIGHT_M, 0.0),
        Vec3::new(0.8, -0.45, -1.5),
    );
    let br_hit = ray_intersects_quad(&br_ray, &quad_pose, size).expect("Hit bottom-right");
    assert!((br_hit.uv.0 - 1.0).abs() < 1e-4);
    assert!((br_hit.uv.1 - 1.0).abs() < 1e-4);

    // Ray parallel to quad plane (shooting along X or Y)
    let parallel_ray = Ray::new(Vec3::new(0.0, DEFAULT_HEIGHT_M, -1.5), Vec3::UNIT_X);
    assert!(ray_intersects_quad(&parallel_ray, &quad_pose, size).is_none());

    // Ray pointing backwards (away from screen)
    let backward_ray = Ray::new(Vec3::new(0.0, DEFAULT_HEIGHT_M, 0.0), Vec3::UNIT_Z);
    assert!(ray_intersects_quad(&backward_ray, &quad_pose, size).is_none());

    // Ray missing just outside width boundary (x = 0.81 > 0.80)
    let miss_x_ray = Ray::new(
        Vec3::new(0.0, DEFAULT_HEIGHT_M, 0.0),
        Vec3::new(0.81, 0.0, -1.5),
    );
    assert!(ray_intersects_quad(&miss_x_ray, &quad_pose, size).is_none());
}

#[test]
fn test_ray_intersects_cylinder_arc_and_height() {
    let axis_pose = ScreenPose::new(Vec3::new(0.0, DEFAULT_HEIGHT_M, 0.0), Quat::IDENTITY);
    let radius = 1.5;
    let central_angle = 1.2; // ~68.75 degrees
    let aspect_ratio = 16.0 / 9.0;
    let height = (radius * central_angle) / aspect_ratio;

    // Center hit from user origin
    let ray = Ray::new(Vec3::new(0.0, DEFAULT_HEIGHT_M, 0.0), -Vec3::UNIT_Z);
    let hit = ray_intersects_cylinder(&ray, &axis_pose, radius, central_angle, aspect_ratio)
        .expect("Must hit cylinder center");
    assert!((hit.distance - radius).abs() < 1e-4);
    assert!((hit.uv.0 - 0.5).abs() < 1e-4);
    assert!((hit.uv.1 - 0.5).abs() < 1e-4);

    // Hit near left edge of arc: angle ~ -0.55 rad (within -0.6 rad)
    let left_angle: f32 = -0.55;
    let left_dir = Vec3::new(left_angle.sin(), 0.0, -left_angle.cos());
    let left_ray = Ray::new(Vec3::new(0.0, DEFAULT_HEIGHT_M, 0.0), left_dir);
    let left_hit =
        ray_intersects_cylinder(&left_ray, &axis_pose, radius, central_angle, aspect_ratio)
            .expect("Must hit left sector of arc");
    assert!(left_hit.uv.0 < 0.15);

    // Miss outside arc angle: angle ~ -0.65 rad (beyond -0.6 rad)
    let outside_angle: f32 = -0.65;
    let outside_dir = Vec3::new(outside_angle.sin(), 0.0, -outside_angle.cos());
    let outside_ray = Ray::new(Vec3::new(0.0, DEFAULT_HEIGHT_M, 0.0), outside_dir);
    assert!(ray_intersects_cylinder(
        &outside_ray,
        &axis_pose,
        radius,
        central_angle,
        aspect_ratio
    )
    .is_none());

    // Miss above top height boundary
    let above_ray = Ray::new(
        Vec3::new(0.0, DEFAULT_HEIGHT_M, 0.0),
        Vec3::new(0.0, height * 0.6, -radius),
    );
    assert!(
        ray_intersects_cylinder(&above_ray, &axis_pose, radius, central_angle, aspect_ratio)
            .is_none()
    );
}

#[test]
fn test_closest_screen_selection_in_multi_display_cockpit() {
    let mut center = ScreenTransform::for_display(0);
    center.display_mode = DisplayMode::FlatQuad;
    center.set_distance(1.2);

    let mut right = ScreenTransform::for_display(1);
    right.display_mode = DisplayMode::FlatQuad;

    let mut left = ScreenTransform::for_display(2);
    left.display_mode = DisplayMode::FlatQuad;

    let screens = vec![(0, &center), (1, &right), (2, &left)];

    // Point straight forward -> hits display 0
    let ray_center = Ray::new(Vec3::new(0.0, 1.4, 0.0), -Vec3::UNIT_Z);
    let (id_c, hit_c) =
        ScreenTransform::find_closest_hit(&ray_center, screens.clone()).expect("Hits center");
    assert_eq!(id_c, 0);
    assert!((hit_c.distance - 1.2).abs() < 1e-4);

    // Point towards right screen (yaw ~ -25 deg, pointing into +X)
    let right_dir = Quat::from_euler(0.0, -0.45, 0.0).rotate_vec(-Vec3::UNIT_Z);
    let ray_right = Ray::new(Vec3::new(0.0, 1.4, 0.0), right_dir);
    let (id_r, _) =
        ScreenTransform::find_closest_hit(&ray_right, screens.clone()).expect("Hits right");
    assert_eq!(id_r, 1);

    // Point towards left screen (yaw ~ +25 deg, pointing into -X)
    let left_dir = Quat::from_euler(0.0, 0.45, 0.0).rotate_vec(-Vec3::UNIT_Z);
    let ray_left = Ray::new(Vec3::new(0.0, 1.4, 0.0), left_dir);
    let (id_l, _) = ScreenTransform::find_closest_hit(&ray_left, screens).expect("Hits left");
    assert_eq!(id_l, 2);
}

// ============================================================================
// 2. Grab State Machine & 6DoF Drag Tests
// ============================================================================

#[test]
fn test_grab_state_machine_rigid_drag_preserves_offset() {
    let mut sm = GrabStateMachine::new();
    let mut screen = ScreenTransform::for_display(0);
    screen.display_mode = DisplayMode::FlatQuad;
    screen.position = Vec3::new(0.0, 1.4, -1.5);
    screen.orientation = Quat::IDENTITY;
    let screens = vec![(0, &screen)];

    // Aim at screen and hover
    let hover_input = InteractionInput {
        source: InteractionSource::Controller(Handedness::Right),
        aim_ray: Some(Ray::new(Vec3::new(0.0, 1.4, 0.0), -Vec3::UNIT_Z)),
        grab_pose: Some(ScreenPose::new(Vec3::new(0.2, 1.2, -0.5), Quat::IDENTITY)),
        is_grabbing: false,
        is_selecting: false,
        thumbstick: (0.0, 0.0),
    };
    let ev_hover = sm
        .update(&hover_input, screens.clone(), 0.016)
        .expect("Hover");
    assert!(matches!(
        ev_hover,
        InteractionEvent::HoverStarted { display_id: 0, .. }
    ));

    // Squeeze grip to start grab
    let mut grab_input = hover_input;
    grab_input.is_grabbing = true;
    let ev_start = sm
        .update(&grab_input, screens.clone(), 0.016)
        .expect("GrabStart");
    assert!(matches!(
        ev_start,
        InteractionEvent::GrabStarted { display_id: 0, .. }
    ));

    // Drag: Controller moves by (+0.4m X, -0.2m Y, +0.3m Z)
    let mut drag_input = grab_input;
    drag_input.grab_pose = Some(ScreenPose::new(Vec3::new(0.6, 1.0, -0.2), Quat::IDENTITY));
    let ev_drag = sm
        .update(&drag_input, screens.clone(), 0.016)
        .expect("GrabUpdate");
    if let InteractionEvent::GrabUpdated { transform, .. } = ev_drag {
        // Original screen position: (0.0, 1.4, -1.5)
        // Controller delta: (0.4, -0.2, 0.3)
        // Expected screen position: (0.4, 1.2, -1.2)
        assert!((transform.position.x - 0.4).abs() < 1e-4);
        assert!((transform.position.y - 1.2).abs() < 1e-4);
        assert!((transform.position.z - (-1.2)).abs() < 1e-4);
    } else {
        panic!("Expected GrabUpdated");
    }

    // Release grab
    let mut release_input = drag_input;
    release_input.is_grabbing = false;
    let ev_end = sm.update(&release_input, screens, 0.016).expect("GrabEnd");
    assert!(matches!(
        ev_end,
        InteractionEvent::GrabEnded { display_id: 0, .. }
    ));
}

#[test]
fn test_grab_state_machine_thumbstick_push_pull_and_tilt() {
    let mut sm = GrabStateMachine::new();
    let mut screen = ScreenTransform::for_display(0);
    screen.display_mode = DisplayMode::FlatQuad;
    screen.set_distance(1.5);
    screen.set_tilt(0.0);
    let screens = vec![(0, &screen)];

    // Hover and grab
    let base_input = InteractionInput {
        source: InteractionSource::Controller(Handedness::Right),
        aim_ray: Some(Ray::new(Vec3::new(0.0, 1.4, 0.0), -Vec3::UNIT_Z)),
        grab_pose: Some(ScreenPose::new(Vec3::new(0.0, 1.4, 0.0), Quat::IDENTITY)),
        is_grabbing: false,
        is_selecting: false,
        thumbstick: (0.0, 0.0),
    };
    sm.update(&base_input, screens.clone(), 0.016);

    let mut grab_input = base_input;
    grab_input.is_grabbing = true;
    sm.update(&grab_input, screens.clone(), 0.016);

    // Push away with thumbstick Y = 1.0 for 0.5s (1.2 m/s * 0.5 = 0.6m)
    let mut push_input = grab_input;
    push_input.thumbstick = (0.0, 1.0);
    let ev1 = sm.update(&push_input, screens.clone(), 0.5).expect("Push");
    if let InteractionEvent::GrabUpdated { transform, .. } = ev1 {
        assert!((transform.distance - 2.1).abs() < 0.05);
    }

    // Tilt with thumbstick X = 1.0 for 0.3s
    let mut tilt_input = grab_input;
    tilt_input.thumbstick = (1.0, 0.0);
    let ev2 = sm.update(&tilt_input, screens, 0.3).expect("Tilt");
    if let InteractionEvent::GrabUpdated { transform, .. } = ev2 {
        assert!(transform.tilt > 0.1);
    }
}

// ============================================================================
// 3. XR_EXT_hand_tracking Joint Gesture Hysteresis Tests
// ============================================================================

#[test]
fn test_hand_tracking_pinch_gesture_with_hysteresis() {
    let mut detector = HandGestureDetector::new();
    let mut data = HandTrackingData {
        is_active: true,
        ..Default::default()
    };

    // Open hand: thumb and index tips 100mm apart
    data.joint_mut(HandJoint::ThumbTip).position = Vec3::new(0.0, 0.0, 0.0);
    data.joint_mut(HandJoint::IndexTip).position = Vec3::new(0.100, 0.0, 0.0);

    let state1 = detector.update(Handedness::Right, &data);
    assert!(!state1.is_pinching);

    // Closing towards pinch: 30mm apart (above 25mm start, below 40mm release)
    data.joint_mut(HandJoint::IndexTip).position = Vec3::new(0.030, 0.0, 0.0);
    let state2 = detector.update(Handedness::Right, &data);
    assert!(
        !state2.is_pinching,
        "Must NOT pinch before reaching 25mm start threshold"
    );

    // Close to 20mm (< 25mm start threshold)
    data.joint_mut(HandJoint::IndexTip).position = Vec3::new(0.020, 0.0, 0.0);
    let state3 = detector.update(Handedness::Right, &data);
    assert!(state3.is_pinching, "Must pinch once crossing 25mm");

    // Open back to 35mm (between 25mm and 40mm) -> HYSTERESIS KEEP PINCHED
    data.joint_mut(HandJoint::IndexTip).position = Vec3::new(0.035, 0.0, 0.0);
    let state4 = detector.update(Handedness::Right, &data);
    assert!(
        state4.is_pinching,
        "Must STAY pinched between 25mm and 40mm due to hysteresis"
    );

    // Open past 40mm release threshold (e.g. 45mm)
    data.joint_mut(HandJoint::IndexTip).position = Vec3::new(0.045, 0.0, 0.0);
    let state5 = detector.update(Handedness::Right, &data);
    assert!(
        !state5.is_pinching,
        "Must release pinch once crossing 40mm release threshold"
    );
}

#[test]
fn test_hand_tracking_fist_grab_with_hysteresis() {
    let mut detector = HandGestureDetector::new();
    let mut data = HandTrackingData {
        is_active: true,
        ..Default::default()
    };

    let palm_pos = Vec3::new(0.0, 1.2, -0.3);
    data.joint_mut(HandJoint::Palm).position = palm_pos;

    // Open fingers: avg distance 100mm
    for joint in [
        HandJoint::IndexTip,
        HandJoint::MiddleTip,
        HandJoint::RingTip,
        HandJoint::LittleTip,
    ] {
        data.joint_mut(joint).position = palm_pos + Vec3::new(0.0, 0.100, 0.0);
    }
    let s1 = detector.update(Handedness::Left, &data);
    assert!(!s1.is_grabbing);

    // Curl to 60mm (between start 55mm and release 75mm)
    for joint in [
        HandJoint::IndexTip,
        HandJoint::MiddleTip,
        HandJoint::RingTip,
        HandJoint::LittleTip,
    ] {
        data.joint_mut(joint).position = palm_pos + Vec3::new(0.0, 0.060, 0.0);
    }
    let s2 = detector.update(Handedness::Left, &data);
    assert!(
        !s2.is_grabbing,
        "Must NOT grab before reaching 55mm start threshold"
    );

    // Curl into tight fist (50mm < 55mm)
    for joint in [
        HandJoint::IndexTip,
        HandJoint::MiddleTip,
        HandJoint::RingTip,
        HandJoint::LittleTip,
    ] {
        data.joint_mut(joint).position = palm_pos + Vec3::new(0.0, 0.050, 0.0);
    }
    let s3 = detector.update(Handedness::Left, &data);
    assert!(s3.is_grabbing, "Must grab once crossing 55mm");

    // Relax fingers slightly to 70mm (between 55mm and 75mm) -> HYSTERESIS KEEP GRABBED
    for joint in [
        HandJoint::IndexTip,
        HandJoint::MiddleTip,
        HandJoint::RingTip,
        HandJoint::LittleTip,
    ] {
        data.joint_mut(joint).position = palm_pos + Vec3::new(0.0, 0.070, 0.0);
    }
    let s4 = detector.update(Handedness::Left, &data);
    assert!(
        s4.is_grabbing,
        "Must STAY grabbing between 55mm and 75mm due to hysteresis"
    );

    // Open fingers past 75mm (80mm)
    for joint in [
        HandJoint::IndexTip,
        HandJoint::MiddleTip,
        HandJoint::RingTip,
        HandJoint::LittleTip,
    ] {
        data.joint_mut(joint).position = palm_pos + Vec3::new(0.0, 0.080, 0.0);
    }
    let s5 = detector.update(Handedness::Left, &data);
    assert!(
        !s5.is_grabbing,
        "Must release grab once crossing 75mm release threshold"
    );
}

#[test]
fn test_hand_tracking_manipulation_drives_same_state_machine() {
    let mut sm = GrabStateMachine::new();
    let screen = ScreenTransform::for_display(0);
    let screens = vec![(0, &screen)];

    let mut hand_backend = MockHandTrackingBackend::new(true);
    let mut detector = HandGestureDetector::new();

    // 1. Hand pointing at screen without pinch
    hand_backend.simulate_open_hand(
        Handedness::Right,
        Vec3::new(0.0, 1.4, -0.4),
        Vec3::new(0.0, 0.0, -1.0),
    );
    let data_open = hand_backend
        .locate_joints(Handedness::Right, openxr::Time::from_nanos(0), None)
        .unwrap()
        .unwrap();
    let gesture_open = detector.update(Handedness::Right, &data_open);
    let input_open = InteractionInput::from_hand(Handedness::Right, &gesture_open);

    let ev_hover = sm
        .update(&input_open, screens.clone(), 0.016)
        .expect("Hover from hand");
    assert!(matches!(
        ev_hover,
        InteractionEvent::HoverStarted { display_id: 0, .. }
    ));

    // 2. Pinch gesture (distance 15mm < 25mm) initiates grab
    hand_backend.simulate_pinch(
        Handedness::Right,
        Vec3::new(0.0, 1.4, -0.4),
        Vec3::new(0.0, 0.0, -1.0),
        0.015,
    );
    let data_pinch = hand_backend
        .locate_joints(Handedness::Right, openxr::Time::from_nanos(0), None)
        .unwrap()
        .unwrap();
    let gesture_pinch = detector.update(Handedness::Right, &data_pinch);
    let input_pinch = InteractionInput::from_hand(Handedness::Right, &gesture_pinch);

    let ev_grab = sm
        .update(&input_pinch, screens.clone(), 0.016)
        .expect("Grab from hand pinch");
    assert!(matches!(
        ev_grab,
        InteractionEvent::GrabStarted { display_id: 0, .. }
    ));
    assert!(sm.is_grabbing());

    // 3. Move pinched hand to (+0.2m, +0.1m)
    hand_backend.simulate_pinch(
        Handedness::Right,
        Vec3::new(0.2, 1.5, -0.4),
        Vec3::new(0.0, 0.0, -1.0),
        0.015,
    );
    let data_moved = hand_backend
        .locate_joints(Handedness::Right, openxr::Time::from_nanos(0), None)
        .unwrap()
        .unwrap();
    let gesture_moved = detector.update(Handedness::Right, &data_moved);
    let input_moved = InteractionInput::from_hand(Handedness::Right, &gesture_moved);

    let ev_drag = sm
        .update(&input_moved, screens.clone(), 0.016)
        .expect("Drag from hand");
    if let InteractionEvent::GrabUpdated { transform, .. } = ev_drag {
        assert!((transform.position.x - 0.2).abs() < 1e-4);
        assert!((transform.position.y - 1.5).abs() < 1e-4);
    } else {
        panic!("Expected GrabUpdated");
    }

    // 4. Release pinch (fingers open to 50mm > 40mm)
    hand_backend.simulate_pinch(
        Handedness::Right,
        Vec3::new(0.2, 1.5, -0.4),
        Vec3::new(0.0, 0.0, -1.0),
        0.050,
    );
    let data_release = hand_backend
        .locate_joints(Handedness::Right, openxr::Time::from_nanos(0), None)
        .unwrap()
        .unwrap();
    let gesture_release = detector.update(Handedness::Right, &data_release);
    let input_release = InteractionInput::from_hand(Handedness::Right, &gesture_release);

    let ev_end = sm
        .update(&input_release, screens, 0.016)
        .expect("Release from hand pinch");
    assert!(matches!(
        ev_end,
        InteractionEvent::GrabEnded { display_id: 0, .. }
    ));
    assert!(!sm.is_grabbing());
}

// ============================================================================
// 4. Layout Persistence On Manipulation End Tests
// ============================================================================

#[test]
fn test_layout_persistence_on_manipulation_end() {
    let temp_dir = TestTempDir::new("layout_persist_test");
    let layout_path = temp_dir.path().join("spatial_layout.json");

    let config = ClientConfig {
        spatial_layout_path: Some(layout_path.clone()),
        ..Default::default()
    };
    let mut runtime = QuestClientRuntime::new(config).unwrap();

    // Step a frame to initialize
    runtime.step_frame().unwrap();

    // Hover and grab screen 0
    let hover_input = InteractionInput {
        source: InteractionSource::Controller(Handedness::Right),
        aim_ray: Some(Ray::new(Vec3::new(0.0, 1.4, 0.0), -Vec3::UNIT_Z)),
        grab_pose: Some(ScreenPose::new(Vec3::new(0.0, 1.4, 0.0), Quat::IDENTITY)),
        is_grabbing: false,
        is_selecting: false,
        thumbstick: (0.0, 0.0),
    };
    runtime.step_interaction(&hover_input, 0.016);

    let mut grab_input = hover_input;
    grab_input.is_grabbing = true;
    runtime.step_interaction(&grab_input, 0.016);
    assert!(runtime.interaction_state_machine().is_grabbing());

    // Drag screen to new position (+0.5m X, +0.2m Y)
    let mut drag_input = grab_input;
    drag_input.grab_pose = Some(ScreenPose::new(Vec3::new(0.5, 1.6, 0.0), Quat::IDENTITY));
    runtime.step_interaction(&drag_input, 0.016);
    assert!((runtime.screen_transform().position.x - 0.5).abs() < 1e-4);

    // Release grab: this triggers layout_store.save!
    let mut release_input = drag_input;
    release_input.is_grabbing = false;
    let end_event = runtime
        .step_interaction(&release_input, 0.016)
        .expect("GrabEnded event");
    assert!(matches!(end_event, InteractionEvent::GrabEnded { .. }));
    assert!(!runtime.interaction_state_machine().is_grabbing());

    // Verify the JSON layout file was written atomically to disk
    assert!(layout_path.exists(), "Layout JSON file must exist on disk");
    let store = SpatialLayoutStore::new(&layout_path);
    let loaded = store.load().expect("Must load persisted layout from disk");
    let saved_screen = loaded
        .get(0)
        .expect("Display 0 must be in persisted layout");

    assert!((saved_screen.position.x - 0.5).abs() < 1e-4);
    assert!((saved_screen.position.y - 1.6).abs() < 1e-4);
}

// ============================================================================
// 5. OpenXR Context Mock Backend Integration Tests
// ============================================================================

#[test]
fn test_openxr_context_interaction_and_hand_tracking_mock_backends() {
    let config = ClientConfig::default();
    let mut ctx = OpenXrContext::new_simulated(&config);

    assert!(ctx.interaction().is_active());
    assert!(ctx.is_hand_tracking_supported());

    // Inject simulated controller state
    let mut mock_ctrl = MockInteractionBackend::new();
    let state = ControllerInputState {
        aim_pose: Some(ScreenPose::new(Vec3::new(0.1, 1.3, -0.2), Quat::IDENTITY)),
        grip_pose: Some(ScreenPose::new(Vec3::new(0.1, 1.2, -0.3), Quat::IDENTITY)),
        grip_value: 0.95,
        grip_clicked: true,
        trigger_value: 0.8,
        trigger_clicked: true,
        thumbstick: (0.3, -0.7),
        thumbstick_clicked: false,
    };
    mock_ctrl.set_controller_state(Handedness::Right, Some(state));
    ctx.set_interaction_backend(Box::new(mock_ctrl));

    let queried_ctrl = ctx
        .interaction()
        .controller_state(Handedness::Right)
        .expect("Right controller state must be present");
    assert!(queried_ctrl.is_grabbing());
    assert!(queried_ctrl.is_selecting());
    assert_eq!(queried_ctrl.thumbstick, (0.3, -0.7));

    // Inject simulated hand tracking joints
    let mut mock_hand = MockHandTrackingBackend::new(true);
    mock_hand.simulate_open_hand(
        Handedness::Left,
        Vec3::new(-0.2, 1.2, -0.3),
        Vec3::new(0.0, 0.0, -1.0),
    );
    ctx.set_hand_tracking_backend(Box::new(mock_hand));

    let left_hand_data = ctx
        .locate_hand_joints(Handedness::Left, openxr::Time::from_nanos(0))
        .unwrap()
        .expect("Left hand data must be located");
    assert!(left_hand_data.is_active);
    assert!(left_hand_data.joint(HandJoint::ThumbTip).is_valid);
}

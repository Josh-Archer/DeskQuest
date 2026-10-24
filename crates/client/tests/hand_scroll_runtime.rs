use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use linux_quest_client::{
    ClientConfig, HandJoint, HandTrackingData, Handedness, MockHandTrackingBackend,
    QuestClientRuntime, Vec3,
};
use linux_quest_protocol::{ElementState, InputEvent};

#[test]
fn middle_pinch_scroll_reaches_host_input_without_clicking_or_moving_the_screen() {
    let mut runtime = QuestClientRuntime::new(ClientConfig::default()).unwrap();
    let screen = runtime.screen_transform().clone();
    let events = Arc::new(Mutex::new(Vec::new()));
    let output = events.clone();
    runtime.set_pointer_event_sink(Box::new(move |event| output.lock().unwrap().push(event)));
    let mut hand = HandTrackingData {
        is_active: true,
        ..Default::default()
    };
    let height = screen.position.y;
    for (joint, position) in [
        (HandJoint::Palm, Vec3::new(0.0, height, 0.0)),
        (HandJoint::Wrist, Vec3::new(0.0, height, 0.0)),
        (HandJoint::IndexProximal, Vec3::new(0.0, height, -0.07)),
        (HandJoint::ThumbTip, Vec3::new(0.0, height, -0.1)),
        (HandJoint::MiddleTip, Vec3::new(0.01, height, -0.1)),
        (HandJoint::IndexTip, Vec3::new(0.1, height, -0.15)),
        (HandJoint::RingTip, Vec3::new(0.1, height, -0.2)),
        (HandJoint::LittleTip, Vec3::new(0.1, height, -0.2)),
    ] {
        let data = hand.joint_mut(joint);
        data.position = position;
        data.is_valid = true;
    }
    let set_hand = |runtime: &mut QuestClientRuntime, data| {
        let mut backend = MockHandTrackingBackend::new(true);
        backend.set_hand_data(Handedness::Right, data);
        runtime
            .openxr_context_mut()
            .set_hand_tracking_backend(Box::new(backend));
    };
    let now = Instant::now();
    set_hand(&mut runtime, Some(hand.clone()));
    runtime.step_frame_at(now).unwrap();
    for joint in &mut hand.joints {
        joint.position.y += 0.1;
    }
    set_hand(&mut runtime, Some(hand));
    runtime
        .step_frame_at(now + Duration::from_millis(100))
        .unwrap();
    set_hand(&mut runtime, None);
    runtime
        .step_frame_at(now + Duration::from_millis(200))
        .unwrap();

    let events = events.lock().unwrap();
    let scrolls: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            InputEvent::MouseScrollHiRes { delta_y, .. } => Some(*delta_y),
            _ => None,
        })
        .collect();
    assert_eq!(scrolls.len(), 1);
    assert!((scrolls[0] + 4.0).abs() < 0.001);
    assert!(!events.iter().any(|e| matches!(
        e,
        InputEvent::MouseButton {
            state: ElementState::Pressed,
            ..
        }
    )));
    let positions: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            InputEvent::MouseMoveAbsolute { x, y, display_id } => Some((*x, *y, *display_id)),
            _ => None,
        })
        .collect();
    assert_eq!(positions.len(), 2);
    assert_eq!(positions[0], positions[1]);
    assert_eq!(runtime.screen_transform(), &screen);
}

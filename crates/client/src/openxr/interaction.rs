//! OpenXR controller action set integration and mock interaction backend.

use crate::error::{ClientError, ClientResult};
use crate::spatial::interaction::{ControllerInputState, Handedness};
use crate::spatial::pose::ScreenPose;

/// Backend interface for polling controller inputs and aim/grip poses.
pub trait InteractionBackend: Send + Sync {
    /// Polls action set states from the OpenXR runtime for the given predicted display time.
    fn poll_actions(
        &mut self,
        time: openxr::Time,
        stage_space: Option<&openxr::Space>,
    ) -> ClientResult<()>;

    /// Returns the latest controller input state for the specified hand.
    fn controller_state(&self, hand: Handedness) -> Option<ControllerInputState>;

    /// Returns true if this interaction backend is currently active and supplying input.
    fn is_active(&self) -> bool;
}

/// Simulated interaction backend for headless CI and unit tests.
#[derive(Debug, Clone, Default)]
pub struct MockInteractionBackend {
    left_state: Option<ControllerInputState>,
    right_state: Option<ControllerInputState>,
    active: bool,
    poll_count: usize,
}

impl MockInteractionBackend {
    /// Creates a new mock interaction backend.
    pub fn new() -> Self {
        Self {
            left_state: None,
            right_state: None,
            active: true,
            poll_count: 0,
        }
    }

    /// Sets the simulated controller state for the specified hand.
    pub fn set_controller_state(&mut self, hand: Handedness, state: Option<ControllerInputState>) {
        match hand {
            Handedness::Left => self.left_state = state,
            Handedness::Right => self.right_state = state,
        }
    }

    /// Sets whether the backend is considered active.
    pub fn set_active(&mut self, active: bool) {
        self.active = active;
    }

    /// Returns the number of times `poll_actions` has been called.
    pub fn poll_count(&self) -> usize {
        self.poll_count
    }
}

impl InteractionBackend for MockInteractionBackend {
    fn poll_actions(
        &mut self,
        _time: openxr::Time,
        _stage_space: Option<&openxr::Space>,
    ) -> ClientResult<()> {
        self.poll_count += 1;
        Ok(())
    }

    fn controller_state(&self, hand: Handedness) -> Option<ControllerInputState> {
        if !self.active {
            return None;
        }
        match hand {
            Handedness::Left => self.left_state,
            Handedness::Right => self.right_state,
        }
    }

    fn is_active(&self) -> bool {
        self.active
    }
}

/// Live OpenXR action set backend managing controller aim, grip, trigger, and thumbstick actions.
pub struct OpenXrInteractionBackend<G: openxr::Graphics> {
    session: openxr::Session<G>,
    action_set: openxr::ActionSet,
    _aim_action: openxr::Action<openxr::Posef>,
    _grip_action: openxr::Action<openxr::Posef>,
    grip_squeeze_action: openxr::Action<f32>,
    trigger_action: openxr::Action<f32>,
    thumbstick_action: openxr::Action<openxr::Vector2f>,
    subaction_left: openxr::Path,
    subaction_right: openxr::Path,
    left_aim_space: openxr::Space,
    right_aim_space: openxr::Space,
    left_grip_space: openxr::Space,
    right_grip_space: openxr::Space,
    left_state: ControllerInputState,
    right_state: ControllerInputState,
    is_active: bool,
}

impl<G: openxr::Graphics> OpenXrInteractionBackend<G> {
    /// Initializes OpenXR action sets, creates actions, binds suggested profiles, and attaches to the session.
    pub fn try_init(
        instance: &openxr::Instance,
        session: &openxr::Session<G>,
    ) -> ClientResult<Self> {
        let action_set = instance
            .create_action_set("screen_interaction", "Screen Interaction Action Set", 0)
            .map_err(|e| ClientError::OpenXr(format!("Failed to create action set: {e:?}")))?;

        let subaction_left = instance.string_to_path("/user/hand/left").map_err(|e| {
            ClientError::OpenXr(format!("Failed to get left subaction path: {e:?}"))
        })?;
        let subaction_right = instance.string_to_path("/user/hand/right").map_err(|e| {
            ClientError::OpenXr(format!("Failed to get right subaction path: {e:?}"))
        })?;
        let subaction_paths = [subaction_left, subaction_right];

        let aim_action = action_set
            .create_action::<openxr::Posef>("aim_pose", "Aim Pose", &subaction_paths)
            .map_err(|e| ClientError::OpenXr(format!("Failed to create aim action: {e:?}")))?;

        let grip_action = action_set
            .create_action::<openxr::Posef>("grip_pose", "Grip Pose", &subaction_paths)
            .map_err(|e| ClientError::OpenXr(format!("Failed to create grip action: {e:?}")))?;

        let grip_squeeze_action = action_set
            .create_action::<f32>("squeeze_value", "Grip Squeeze Value", &subaction_paths)
            .map_err(|e| ClientError::OpenXr(format!("Failed to create squeeze action: {e:?}")))?;

        let trigger_action = action_set
            .create_action::<f32>("trigger_value", "Trigger Value", &subaction_paths)
            .map_err(|e| ClientError::OpenXr(format!("Failed to create trigger action: {e:?}")))?;

        let thumbstick_action = action_set
            .create_action::<openxr::Vector2f>("thumbstick", "Thumbstick", &subaction_paths)
            .map_err(|e| {
                ClientError::OpenXr(format!("Failed to create thumbstick action: {e:?}"))
            })?;

        // Suggest bindings for Oculus Touch Controller
        if let Ok(touch_profile) =
            instance.string_to_path("/interaction_profiles/oculus/touch_controller")
        {
            let bindings = [
                // Left hand
                openxr::Binding::new(
                    &aim_action,
                    instance
                        .string_to_path("/user/hand/left/input/aim/pose")
                        .unwrap_or(openxr::Path::NULL),
                ),
                openxr::Binding::new(
                    &grip_action,
                    instance
                        .string_to_path("/user/hand/left/input/grip/pose")
                        .unwrap_or(openxr::Path::NULL),
                ),
                openxr::Binding::new(
                    &grip_squeeze_action,
                    instance
                        .string_to_path("/user/hand/left/input/squeeze/value")
                        .unwrap_or(openxr::Path::NULL),
                ),
                openxr::Binding::new(
                    &trigger_action,
                    instance
                        .string_to_path("/user/hand/left/input/trigger/value")
                        .unwrap_or(openxr::Path::NULL),
                ),
                openxr::Binding::new(
                    &thumbstick_action,
                    instance
                        .string_to_path("/user/hand/left/input/thumbstick")
                        .unwrap_or(openxr::Path::NULL),
                ),
                // Right hand
                openxr::Binding::new(
                    &aim_action,
                    instance
                        .string_to_path("/user/hand/right/input/aim/pose")
                        .unwrap_or(openxr::Path::NULL),
                ),
                openxr::Binding::new(
                    &grip_action,
                    instance
                        .string_to_path("/user/hand/right/input/grip/pose")
                        .unwrap_or(openxr::Path::NULL),
                ),
                openxr::Binding::new(
                    &grip_squeeze_action,
                    instance
                        .string_to_path("/user/hand/right/input/squeeze/value")
                        .unwrap_or(openxr::Path::NULL),
                ),
                openxr::Binding::new(
                    &trigger_action,
                    instance
                        .string_to_path("/user/hand/right/input/trigger/value")
                        .unwrap_or(openxr::Path::NULL),
                ),
                openxr::Binding::new(
                    &thumbstick_action,
                    instance
                        .string_to_path("/user/hand/right/input/thumbstick")
                        .unwrap_or(openxr::Path::NULL),
                ),
            ];
            let _ = instance.suggest_interaction_profile_bindings(touch_profile, &bindings);
        }

        // Suggest fallback bindings for Simple Controller
        if let Ok(simple_profile) =
            instance.string_to_path("/interaction_profiles/khr/simple_controller")
        {
            let bindings = [
                openxr::Binding::new(
                    &aim_action,
                    instance
                        .string_to_path("/user/hand/left/input/aim/pose")
                        .unwrap_or(openxr::Path::NULL),
                ),
                openxr::Binding::new(
                    &aim_action,
                    instance
                        .string_to_path("/user/hand/right/input/aim/pose")
                        .unwrap_or(openxr::Path::NULL),
                ),
                openxr::Binding::new(
                    &grip_action,
                    instance
                        .string_to_path("/user/hand/left/input/grip/pose")
                        .unwrap_or(openxr::Path::NULL),
                ),
                openxr::Binding::new(
                    &grip_action,
                    instance
                        .string_to_path("/user/hand/right/input/grip/pose")
                        .unwrap_or(openxr::Path::NULL),
                ),
                openxr::Binding::new(
                    &trigger_action,
                    instance
                        .string_to_path("/user/hand/left/input/select/click")
                        .unwrap_or(openxr::Path::NULL),
                ),
                openxr::Binding::new(
                    &trigger_action,
                    instance
                        .string_to_path("/user/hand/right/input/select/click")
                        .unwrap_or(openxr::Path::NULL),
                ),
            ];
            let _ = instance.suggest_interaction_profile_bindings(simple_profile, &bindings);
        }

        // Create action spaces for aim and grip poses
        let left_aim_space = aim_action
            .create_space(session, subaction_left, openxr::Posef::IDENTITY)
            .map_err(|e| ClientError::OpenXr(format!("Failed to create left aim space: {e:?}")))?;
        let right_aim_space = aim_action
            .create_space(session, subaction_right, openxr::Posef::IDENTITY)
            .map_err(|e| ClientError::OpenXr(format!("Failed to create right aim space: {e:?}")))?;
        let left_grip_space = grip_action
            .create_space(session, subaction_left, openxr::Posef::IDENTITY)
            .map_err(|e| ClientError::OpenXr(format!("Failed to create left grip space: {e:?}")))?;
        let right_grip_space = grip_action
            .create_space(session, subaction_right, openxr::Posef::IDENTITY)
            .map_err(|e| {
                ClientError::OpenXr(format!("Failed to create right grip space: {e:?}"))
            })?;

        // Attach action set to session
        session
            .attach_action_sets(&[&action_set])
            .map_err(|e| ClientError::OpenXr(format!("Failed to attach action sets: {e:?}")))?;

        Ok(Self {
            session: session.clone(),
            action_set,
            _aim_action: aim_action,
            _grip_action: grip_action,
            grip_squeeze_action,
            trigger_action,
            thumbstick_action,
            subaction_left,
            subaction_right,
            left_aim_space,
            right_aim_space,
            left_grip_space,
            right_grip_space,
            left_state: ControllerInputState::default(),
            right_state: ControllerInputState::default(),
            is_active: true,
        })
    }
}

impl<G: openxr::Graphics + Send + Sync> InteractionBackend for OpenXrInteractionBackend<G> {
    fn poll_actions(
        &mut self,
        time: openxr::Time,
        stage_space: Option<&openxr::Space>,
    ) -> ClientResult<()> {
        let active_set = openxr::ActiveActionSet::new(&self.action_set);
        if let Err(e) = self.session.sync_actions(&[active_set]) {
            tracing::warn!(error = ?e, "Failed to sync OpenXR actions");
            self.is_active = false;
            return Ok(());
        }
        self.is_active = true;

        let query_hand = |subaction: openxr::Path,
                          aim_space: &openxr::Space,
                          grip_space: &openxr::Space|
         -> ControllerInputState {
            let mut state = ControllerInputState::default();

            // Poses relative to stage space
            if let Some(stage) = stage_space {
                if let Ok(loc) = aim_space.locate(stage, time) {
                    if loc.location_flags.contains(
                        openxr::SpaceLocationFlags::POSITION_VALID
                            | openxr::SpaceLocationFlags::ORIENTATION_VALID,
                    ) {
                        state.aim_pose = Some(ScreenPose::from(loc.pose));
                    }
                }

                if let Ok(loc) = grip_space.locate(stage, time) {
                    if loc.location_flags.contains(
                        openxr::SpaceLocationFlags::POSITION_VALID
                            | openxr::SpaceLocationFlags::ORIENTATION_VALID,
                    ) {
                        state.grip_pose = Some(ScreenPose::from(loc.pose));
                    }
                }
            }

            // Analog trigger
            if let Ok(act) = self.trigger_action.state(&self.session, subaction) {
                if act.is_active {
                    state.trigger_value = act.current_state;
                    state.trigger_clicked = act.current_state >= 0.5;
                }
            }

            // Analog grip squeeze
            if let Ok(act) = self.grip_squeeze_action.state(&self.session, subaction) {
                if act.is_active {
                    state.grip_value = act.current_state;
                    state.grip_clicked = act.current_state >= 0.5;
                }
            }

            // Thumbstick
            if let Ok(act) = self.thumbstick_action.state(&self.session, subaction) {
                if act.is_active {
                    state.thumbstick = (act.current_state.x, act.current_state.y);
                }
            }

            state
        };

        self.left_state = query_hand(
            self.subaction_left,
            &self.left_aim_space,
            &self.left_grip_space,
        );
        self.right_state = query_hand(
            self.subaction_right,
            &self.right_aim_space,
            &self.right_grip_space,
        );

        Ok(())
    }

    fn controller_state(&self, hand: Handedness) -> Option<ControllerInputState> {
        if !self.is_active {
            return None;
        }
        match hand {
            Handedness::Left => Some(self.left_state),
            Handedness::Right => Some(self.right_state),
        }
    }

    fn is_active(&self) -> bool {
        self.is_active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_interaction_backend_lifecycle() {
        let mut backend = MockInteractionBackend::new();
        assert!(backend.is_active());
        assert_eq!(backend.poll_count(), 0);

        let initial_left = backend.controller_state(Handedness::Left);
        assert!(initial_left.is_none());

        let state = ControllerInputState {
            grip_value: 0.9,
            grip_clicked: true,
            ..Default::default()
        };
        backend.set_controller_state(Handedness::Left, Some(state));

        let polled_left = backend.controller_state(Handedness::Left).unwrap();
        assert_eq!(polled_left.grip_value, 0.9);
        assert!(polled_left.is_grabbing());

        backend
            .poll_actions(openxr::Time::from_nanos(1000), None)
            .unwrap();
        assert_eq!(backend.poll_count(), 1);

        backend.set_active(false);
        assert!(!backend.is_active());
        assert!(backend.controller_state(Handedness::Left).is_none());
    }
}

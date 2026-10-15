use std::marker::PhantomData;
use std::mem;
use std::ops::Deref;

use openxr::EnvironmentBlendMode;
use serde::{Deserialize, Serialize};

use crate::error::{ClientError, ClientResult};

/// Composition layer builder for `XrCompositionLayerPassthroughFB`.
#[derive(Copy, Clone)]
#[repr(transparent)]
pub struct CompositionLayerPassthroughFB<'a, G: openxr::Graphics> {
    inner: openxr::sys::CompositionLayerPassthroughFB,
    _marker: PhantomData<&'a G>,
}

impl<'a, G: openxr::Graphics> CompositionLayerPassthroughFB<'a, G> {
    /// Creates a new `CompositionLayerPassthroughFB` with default structure type.
    #[inline]
    pub fn new() -> Self {
        Self {
            inner: openxr::sys::CompositionLayerPassthroughFB {
                ty: openxr::sys::StructureType::COMPOSITION_LAYER_PASSTHROUGH_FB,
                ..unsafe { mem::zeroed() }
            },
            _marker: PhantomData,
        }
    }

    /// Sets composition layer flags.
    #[inline]
    pub fn flags(mut self, value: openxr::CompositionLayerFlags) -> Self {
        self.inner.flags = value;
        self
    }

    /// Sets the reference space for the layer.
    #[inline]
    pub fn space(mut self, value: &'a openxr::Space) -> Self {
        self.inner.space = value.as_raw();
        self
    }

    /// Sets the passthrough layer handle.
    #[inline]
    pub fn layer_handle(mut self, value: &'a openxr::PassthroughLayerFB) -> Self {
        self.inner.layer_handle = value.as_raw();
        self
    }

    /// Consumes the builder and returns the underlying raw sys struct.
    #[inline]
    pub fn into_raw(self) -> openxr::sys::CompositionLayerPassthroughFB {
        self.inner
    }

    /// Returns a reference to the underlying raw sys struct.
    #[inline]
    pub fn as_raw(&self) -> &openxr::sys::CompositionLayerPassthroughFB {
        &self.inner
    }
}

impl<'a, G: openxr::Graphics> Default for CompositionLayerPassthroughFB<'a, G> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a, G: openxr::Graphics> Deref for CompositionLayerPassthroughFB<'a, G> {
    type Target = openxr::CompositionLayerBase<'a, G>;
    #[inline]
    fn deref(&self) -> &Self::Target {
        unsafe { mem::transmute(&self.inner) }
    }
}

/// Real-time state of the Quest passthrough system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PassthroughState {
    /// Passthrough is not supported on this hardware runtime.
    Unsupported,
    /// Passthrough is supported but currently disabled (opaque VR world).
    Disabled,
    /// Passthrough is active and running (real-world environment visible).
    Active,
    /// Passthrough is paused.
    Paused,
}

/// Common trait for controlling Quest passthrough feed and layer composition.
pub trait PassthroughBackend: Send + Sync {
    /// Returns true if XR_FB_passthrough extension is supported by the OpenXR runtime.
    fn is_supported(&self) -> bool;

    /// Returns true if passthrough is currently enabled.
    fn is_enabled(&self) -> bool;

    /// Enables or disables passthrough.
    fn set_enabled(&mut self, enabled: bool) -> ClientResult<()>;

    /// Returns the environment blend mode to use for frame presentation.
    ///
    /// When passthrough is enabled and alpha blend is supported, returns `EnvironmentBlendMode::ALPHA_BLEND`.
    /// Otherwise gracefully falls back to `EnvironmentBlendMode::OPAQUE`.
    fn blend_mode(&self) -> EnvironmentBlendMode;

    /// Returns the current lifecycle state of passthrough.
    fn state(&self) -> PassthroughState;

    /// Returns the underlying live `PassthroughLayerFB` handle if running and supported.
    fn passthrough_layer(&self) -> Option<&openxr::PassthroughLayerFB>;
}

/// Simulated passthrough backend for headless testing and development environments.
#[derive(Debug, Clone)]
pub struct MockPassthroughBackend {
    supported: bool,
    enabled: bool,
    supports_alpha_blend: bool,
    resumed_count: usize,
    paused_count: usize,
    set_enabled_calls: usize,
}

impl MockPassthroughBackend {
    /// Creates a new mock passthrough backend with specified support and initial state.
    pub fn new(supported: bool, enabled: bool) -> Self {
        Self {
            supported,
            enabled: supported && enabled,
            supports_alpha_blend: true,
            resumed_count: 0,
            paused_count: 0,
            set_enabled_calls: 0,
        }
    }

    /// Configures whether the simulated runtime supports `EnvironmentBlendMode::ALPHA_BLEND`.
    pub fn with_alpha_blend(mut self, supported: bool) -> Self {
        self.supports_alpha_blend = supported;
        self
    }

    /// Creates an unsupported passthrough backend (e.g. running on non-Quest or opaque headset).
    pub fn unsupported() -> Self {
        Self::new(false, false)
    }

    /// Total count of resume transitions.
    pub fn resumed_count(&self) -> usize {
        self.resumed_count
    }

    /// Total count of pause transitions.
    pub fn paused_count(&self) -> usize {
        self.paused_count
    }

    /// Total count of `set_enabled` calls.
    pub fn set_enabled_calls(&self) -> usize {
        self.set_enabled_calls
    }
}

impl Default for MockPassthroughBackend {
    fn default() -> Self {
        Self::new(true, true)
    }
}

impl PassthroughBackend for MockPassthroughBackend {
    fn is_supported(&self) -> bool {
        self.supported
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn set_enabled(&mut self, enabled: bool) -> ClientResult<()> {
        self.set_enabled_calls += 1;
        if !self.supported {
            self.enabled = false;
            return Ok(());
        }

        if enabled && !self.enabled {
            self.resumed_count += 1;
        } else if !enabled && self.enabled {
            self.paused_count += 1;
        }

        self.enabled = enabled;
        Ok(())
    }

    fn blend_mode(&self) -> EnvironmentBlendMode {
        if self.supported && self.enabled {
            if self.supports_alpha_blend {
                EnvironmentBlendMode::ALPHA_BLEND
            } else {
                EnvironmentBlendMode::OPAQUE
            }
        } else {
            EnvironmentBlendMode::OPAQUE
        }
    }

    fn state(&self) -> PassthroughState {
        if !self.supported {
            PassthroughState::Unsupported
        } else if self.enabled {
            PassthroughState::Active
        } else {
            PassthroughState::Disabled
        }
    }

    fn passthrough_layer(&self) -> Option<&openxr::PassthroughLayerFB> {
        None
    }
}

/// Live OpenXR passthrough backend managing Meta Quest `XR_FB_passthrough`.
pub struct OpenXrPassthroughBackend {
    _passthrough: openxr::Passthrough,
    passthrough_layer: openxr::PassthroughLayerFB,
    supports_alpha_blend: bool,
    enabled: bool,
}

impl OpenXrPassthroughBackend {
    /// Attempts to initialize `XR_FB_passthrough` and creates a reconstruction passthrough layer.
    pub fn try_init<G: openxr::Graphics>(
        session: &openxr::Session<G>,
        supports_alpha_blend: bool,
        initial_enabled: bool,
    ) -> ClientResult<Self> {
        let passthrough = session
            .create_passthrough(openxr::PassthroughFlagsFB::IS_RUNNING_AT_CREATION)
            .map_err(|e| {
                ClientError::OpenXr(format!("Failed to create XR_FB_passthrough: {e:?}"))
            })?;

        let passthrough_layer = session
            .create_passthrough_layer(
                &passthrough,
                openxr::PassthroughFlagsFB::IS_RUNNING_AT_CREATION,
                openxr::PassthroughLayerPurposeFB::RECONSTRUCTION,
            )
            .map_err(|e| {
                ClientError::OpenXr(format!(
                    "Failed to create passthrough reconstruction layer: {e:?}"
                ))
            })?;

        if !initial_enabled {
            let _ = passthrough_layer.pause();
        }

        Ok(Self {
            _passthrough: passthrough,
            passthrough_layer,
            supports_alpha_blend,
            enabled: initial_enabled,
        })
    }
}

impl PassthroughBackend for OpenXrPassthroughBackend {
    fn is_supported(&self) -> bool {
        true
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn set_enabled(&mut self, enabled: bool) -> ClientResult<()> {
        if enabled && !self.enabled {
            self.passthrough_layer.resume().map_err(|e| {
                ClientError::OpenXr(format!("Failed to resume passthrough layer: {e:?}"))
            })?;
            self.enabled = true;
        } else if !enabled && self.enabled {
            self.passthrough_layer.pause().map_err(|e| {
                ClientError::OpenXr(format!("Failed to pause passthrough layer: {e:?}"))
            })?;
            self.enabled = false;
        }
        Ok(())
    }

    fn blend_mode(&self) -> EnvironmentBlendMode {
        if self.enabled {
            if self.supports_alpha_blend {
                EnvironmentBlendMode::ALPHA_BLEND
            } else {
                EnvironmentBlendMode::OPAQUE
            }
        } else {
            EnvironmentBlendMode::OPAQUE
        }
    }

    fn state(&self) -> PassthroughState {
        if self.enabled {
            PassthroughState::Active
        } else {
            PassthroughState::Disabled
        }
    }

    fn passthrough_layer(&self) -> Option<&openxr::PassthroughLayerFB> {
        if self.enabled {
            Some(&self.passthrough_layer)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_passthrough_lifecycle() {
        let mut mock = MockPassthroughBackend::new(true, true);
        assert!(mock.is_supported());
        assert!(mock.is_enabled());
        assert_eq!(mock.state(), PassthroughState::Active);
        assert_eq!(mock.blend_mode(), EnvironmentBlendMode::ALPHA_BLEND);

        // Disable passthrough
        mock.set_enabled(false).unwrap();
        assert!(!mock.is_enabled());
        assert_eq!(mock.state(), PassthroughState::Disabled);
        assert_eq!(mock.blend_mode(), EnvironmentBlendMode::OPAQUE);
        assert_eq!(mock.paused_count(), 1);

        // Re-enable passthrough
        mock.set_enabled(true).unwrap();
        assert!(mock.is_enabled());
        assert_eq!(mock.state(), PassthroughState::Active);
        assert_eq!(mock.blend_mode(), EnvironmentBlendMode::ALPHA_BLEND);
        assert_eq!(mock.resumed_count(), 1);
        assert_eq!(mock.set_enabled_calls(), 2);
    }

    #[test]
    fn test_unsupported_passthrough_graceful_fallback() {
        let mut unsupported = MockPassthroughBackend::unsupported();
        assert!(!unsupported.is_supported());
        assert!(!unsupported.is_enabled());
        assert_eq!(unsupported.state(), PassthroughState::Unsupported);
        assert_eq!(unsupported.blend_mode(), EnvironmentBlendMode::OPAQUE);

        // Attempting to enable on unsupported device remains disabled and opaque
        unsupported.set_enabled(true).unwrap();
        assert!(!unsupported.is_enabled());
        assert_eq!(unsupported.blend_mode(), EnvironmentBlendMode::OPAQUE);
    }

    #[test]
    fn test_passthrough_fallback_to_opaque_when_alpha_blend_unsupported() {
        let mock_no_alpha = MockPassthroughBackend::new(true, true).with_alpha_blend(false);
        assert!(mock_no_alpha.is_supported());
        assert!(mock_no_alpha.is_enabled());
        // Graceful fallback to OPAQUE when alpha blend is not supported by runtime
        assert_eq!(mock_no_alpha.blend_mode(), EnvironmentBlendMode::OPAQUE);
    }
}

//! OpenXR session lifecycle, reference space, and event polling context.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ash::vk;
use ash::vk::Handle;
#[cfg(target_os = "android")]
use openxr::sys::Handle as _;
use openxr::{
    ApplicationInfo, CompositionLayerBase, Entry, EnvironmentBlendMode, ExtensionSet, FormFactor,
    FrameState, FrameStream, FrameWaiter, Instance, Session, SessionState, Space, Swapchain,
    SwapchainCreateFlags, SwapchainCreateInfo, SwapchainUsageFlags, SystemId, Time, Version,
    ViewConfigurationType, Vulkan,
};

use crate::config::ClientConfig;
use crate::error::{ClientError, ClientResult};
use crate::graphics::VulkanContext;
use crate::openxr::hand_tracking::{
    HandTrackingBackend, MockHandTrackingBackend, OpenXrHandTrackingBackend,
};
use crate::openxr::interaction::{
    InteractionBackend, MockInteractionBackend, OpenXrInteractionBackend,
};
use crate::openxr::passthrough::{
    MockPassthroughBackend, OpenXrPassthroughBackend, PassthroughBackend,
};
use crate::openxr::refresh_rate::RefreshRateManager;
use crate::spatial::interaction::{HandTrackingData, Handedness};
use crate::spatial::ScreenTransform;

/// A small RGBA swapchain for one overlay texture (ray, cursor or border).
struct OverlaySwapchain {
    swapchain: Swapchain<Vulkan>,
    images: Vec<vk::Image>,
    size: (u32, u32),
    /// An image has been released with the current content. The compositor keeps showing the
    /// last released image, so a texture is only uploaded again when it changes (each upload
    /// waits on the GPU; doing it every frame stalled the app).
    ready: bool,
}

impl OverlaySwapchain {
    fn create(session: &Session<Vulkan>, format: u32, size: (u32, u32)) -> ClientResult<Self> {
        let info = SwapchainCreateInfo {
            create_flags: SwapchainCreateFlags::EMPTY,
            usage_flags: SwapchainUsageFlags::COLOR_ATTACHMENT
                | SwapchainUsageFlags::SAMPLED
                | SwapchainUsageFlags::TRANSFER_DST,
            format,
            sample_count: 1,
            width: size.0,
            height: size.1,
            face_count: 1,
            array_size: 1,
            mip_count: 1,
        };
        let swapchain = session.create_swapchain(&info).map_err(|e| {
            ClientError::OpenXr(format!("Failed to create overlay swapchain: {e:?}"))
        })?;
        let images = swapchain
            .enumerate_images()
            .map(|imgs| {
                imgs.into_iter()
                    .map(|raw| vk::Image::from_raw(raw as _))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            swapchain,
            images,
            size,
            ready: false,
        })
    }

    /// Uploads `rgba` into the next image. False if the image could not be filled this frame.
    fn upload(&mut self, vulkan: &VulkanContext, rgba: &[u8]) -> bool {
        let Ok(idx) = self.swapchain.acquire_image() else {
            return false;
        };
        let waited = self
            .swapchain
            .wait_image(openxr::Duration::INFINITE)
            .is_ok();
        let uploaded = waited
            && self.images.get(idx as usize).is_some_and(|&img| {
                vulkan
                    .upload_rgba_to_image(img, self.size.0, self.size.1, rgba)
                    .is_ok()
            });
        let released = self.swapchain.release_image().is_ok();
        self.ready = uploaded && released;
        self.ready
    }

    /// Uploads only if no current content has been released yet.
    fn ensure(&mut self, vulkan: &VulkanContext, rgba: &[u8]) -> bool {
        self.ready || self.upload(vulkan, rgba)
    }
}

/// An overlay swapchain and the image it shows.
struct OverlayTexture {
    chain: OverlaySwapchain,
    rgba: Vec<u8>,
}

impl OverlayTexture {
    fn create(
        session: &Session<Vulkan>,
        format: u32,
        size: (u32, u32),
        rgba: Vec<u8>,
    ) -> ClientResult<Self> {
        Ok(Self {
            chain: OverlaySwapchain::create(session, format, size)?,
            rgba,
        })
    }

    /// Uploads the image unless it already shows; false if it is not ready this frame.
    fn ensure(&mut self, vulkan: &VulkanContext) -> bool {
        self.chain.ensure(vulkan, &self.rgba)
    }

    fn swapchain(&self) -> &Swapchain<Vulkan> {
        &self.chain.swapchain
    }
}

/// Swapchains and cached textures for pointers, the window handles (each in an idle and an
/// active look) and the rounded-corner mask.
struct OverlaySwapchains {
    ray: OverlayTexture,
    cursor: OverlayTexture,
    mask: OverlayTexture,
    bar: [OverlayTexture; 2],
    edge: [OverlayTexture; 2],
    corner: [OverlayTexture; 2],
}

/// The swapchain and placement of the screen for a video stream other than the primary one.
struct ExtraScreen {
    swapchain: Option<Swapchain<Vulkan>>,
    #[cfg(target_os = "android")]
    surface_window: Option<ndk::native_window::NativeWindow>,
    size: (u32, u32),
    transform: ScreenTransform,
}

/// A desktop screen's composition layer, curved or flat.
enum DesktopLayer<'a> {
    Cylinder(openxr::CompositionLayerCylinderKHR<'a, Vulkan>),
    Quad(openxr::CompositionLayerQuad<'a, Vulkan>),
}

impl<'a> DesktopLayer<'a> {
    /// A cylinder layer when the screen is curved and the runtime supports it, else a quad,
    /// with `next` chained (layer settings, alpha blending) and `flags` added.
    fn build(
        space: &'a Space,
        swapchain: &'a Swapchain<Vulkan>,
        (width, height): (u32, u32),
        cfg: &crate::openxr::layers::DesktopLayerConfig,
        cylinder_supported: bool,
        next: *const std::ffi::c_void,
        flags: openxr::CompositionLayerFlags,
    ) -> Self {
        if cylinder_supported && cfg.mode == crate::config::DisplayMode::CurvedCylinder {
            let mut raw =
                crate::openxr::layers::build_cylinder_layer(space, swapchain, width, height, cfg)
                    .into_raw();
            raw.next = next;
            raw.layer_flags |= flags;
            Self::Cylinder(unsafe { openxr::CompositionLayerCylinderKHR::from_raw(raw) })
        } else {
            let mut raw =
                crate::openxr::layers::build_quad_layer(space, swapchain, width, height, cfg)
                    .into_raw();
            raw.next = next;
            raw.layer_flags |= flags;
            Self::Quad(unsafe { openxr::CompositionLayerQuad::from_raw(raw) })
        }
    }

    fn base(&self) -> &CompositionLayerBase<'a, Vulkan> {
        match self {
            Self::Cylinder(l) => l,
            Self::Quad(l) => l,
        }
    }
}

/// OpenXR application runtime context managing instance, session, and swapchains.
pub struct OpenXrContext {
    instance: Option<Instance>,
    system: SystemId,
    session: Option<Session<Vulkan>>,
    frame_waiter: Option<parking_lot::Mutex<FrameWaiter>>,
    frame_stream: Option<parking_lot::Mutex<FrameStream<Vulkan>>>,
    stage_space: Option<Space>,
    view_space: Option<Space>,
    desktop_swapchain: Option<Swapchain<Vulkan>>,
    hud_swapchain: Option<Swapchain<Vulkan>>,
    hud_images: Vec<vk::Image>,
    overlay_swapchains: Option<OverlaySwapchains>,
    #[cfg(target_os = "android")]
    surface_window: Option<ndk::native_window::NativeWindow>,
    #[cfg(target_os = "android")]
    android_surface_pfn: Option<openxr::raw::AndroidSurfaceSwapchainKHR>,
    #[cfg(target_os = "android")]
    supports_fb_surface_create: bool,
    swapchain_format: u32,
    swapchains_created: bool,
    refresh_rate_manager: Option<RefreshRateManager>,
    session_state: SessionState,
    session_running: Arc<AtomicBool>,
    supports_cylinder: bool,
    desktop_swapchain_is_surface: bool,
    /// Pixel size of the primary screen's swapchain, which its layer shows in full.
    desktop_size: (u32, u32),
    supports_layer_settings: bool,
    /// XR_FB_composition_layer_alpha_blend: screens get rounded corners.
    supports_alpha_blend: bool,
    /// Most composition layers the runtime takes in one frame.
    max_layers: usize,
    should_exit: Arc<AtomicBool>,
    timespec_converter: Option<openxr::raw::ConvertTimespecTimeKHR>,
    passthrough: Box<dyn PassthroughBackend>,
    interaction: Box<dyn InteractionBackend>,
    hand_tracking: Box<dyn HandTrackingBackend>,
    screen_transform: Option<ScreenTransform>,
    /// Screens for the video streams other than the primary one, by stream id.
    extra_screens: std::collections::BTreeMap<u16, ExtraScreen>,
    is_simulated: bool,
}

impl OpenXrContext {
    /// Creates a simulated OpenXR context for testing and headless CI.
    pub fn new_simulated(config: &ClientConfig) -> Self {
        let refresh_mgr =
            RefreshRateManager::new(vec![72.0, 90.0, 120.0], config.target_refresh_rate);

        Self {
            instance: None,
            system: SystemId::NULL,
            session: None,
            frame_waiter: None,
            frame_stream: None,
            stage_space: None,
            view_space: None,
            desktop_swapchain: None,
            hud_swapchain: None,
            hud_images: Vec::new(),
            overlay_swapchains: None,
            #[cfg(target_os = "android")]
            surface_window: None,
            #[cfg(target_os = "android")]
            android_surface_pfn: None,
            #[cfg(target_os = "android")]
            supports_fb_surface_create: false,
            swapchain_format: vk::Format::R8G8B8A8_UNORM.as_raw() as u32,
            swapchains_created: true,
            refresh_rate_manager: Some(refresh_mgr),
            session_state: SessionState::FOCUSED,
            session_running: Arc::new(AtomicBool::new(true)),
            supports_cylinder: true,
            desktop_swapchain_is_surface: false,
            desktop_size: (config.stream_width, config.stream_height),
            supports_layer_settings: false,
            supports_alpha_blend: false,
            max_layers: 16,
            should_exit: Arc::new(AtomicBool::new(false)),
            timespec_converter: None,
            passthrough: Box::new(MockPassthroughBackend::new(true, config.enable_passthrough)),
            interaction: Box::new(MockInteractionBackend::new()),
            hand_tracking: Box::new(MockHandTrackingBackend::new(true)),
            screen_transform: None,
            extra_screens: Default::default(),
            is_simulated: true,
        }
    }

    /// Attempts to initialize a live OpenXR session on Meta Horizon OS or desktop OpenXR.
    #[allow(clippy::missing_transmute_annotations)]
    pub fn try_new(
        config: &ClientConfig,
        vulkan_context: &mut VulkanContext,
    ) -> ClientResult<Self> {
        #[cfg(target_os = "android")]
        let platform_info = {
            let ctx = ndk_context::android_context();
            // OpenXR needs the Activity here, not the Application that ndk_context holds.
            let activity = crate::android::activity_jobject();
            let activity = if activity.is_null() {
                ctx.context()
            } else {
                activity
            };
            unsafe { openxr::AndroidPlatformInfo::new(ctx.vm(), activity) }
        };
        #[cfg(not(target_os = "android"))]
        let platform_info = ();

        // Load OpenXR entry. Android links the Khronos loader statically (see Cargo.toml);
        // other platforms load the system loader at runtime.
        #[cfg(target_os = "android")]
        let entry = Entry::linked(&platform_info).map_err(|e| {
            ClientError::OpenXr(format!("Failed to initialise OpenXR loader: {e:?}"))
        })?;
        #[cfg(not(target_os = "android"))]
        let entry = unsafe {
            Entry::load(&platform_info)
                .map_err(|e| ClientError::OpenXr(format!("Failed to load OpenXR loader: {e:?}")))?
        };

        // Query available instance extensions
        let available_exts = entry
            .enumerate_extensions()
            .map_err(|e| ClientError::OpenXr(format!("Failed to enumerate extensions: {e:?}")))?;

        let mut required_exts = ExtensionSet::default();
        if !available_exts.khr_vulkan_enable2 {
            return Err(ClientError::OpenXr(
                "XR_KHR_vulkan_enable2 is required but not supported by OpenXR runtime".into(),
            ));
        }
        required_exts.khr_vulkan_enable2 = true;

        let supports_cylinder = available_exts.khr_composition_layer_cylinder;
        if supports_cylinder {
            required_exts.khr_composition_layer_cylinder = true;
        }

        let supports_refresh_rate = available_exts.fb_display_refresh_rate;
        if supports_refresh_rate {
            required_exts.fb_display_refresh_rate = true;
        }

        if available_exts.fb_composition_layer_settings {
            required_exts.fb_composition_layer_settings = true;
        }

        // Rounded screen corners blend each screen by a mask's destination alpha.
        let supports_alpha_blend = available_exts.fb_composition_layer_alpha_blend;
        if supports_alpha_blend {
            required_exts.fb_composition_layer_alpha_blend = true;
        }

        if available_exts.khr_convert_timespec_time {
            required_exts.khr_convert_timespec_time = true;
        }

        let supports_passthrough = available_exts.fb_passthrough;
        if supports_passthrough {
            required_exts.fb_passthrough = true;
        }

        let supports_hand_tracking = available_exts.ext_hand_tracking;
        if supports_hand_tracking {
            required_exts.ext_hand_tracking = true;
        }
        #[cfg(target_os = "android")]
        {
            if !config.force_mock_decoder {
                if !available_exts.khr_convert_timespec_time {
                    return Err(ClientError::OpenXr(
                        "XR_KHR_convert_timespec_time is required on Meta Horizon OS".into(),
                    ));
                }
                if !available_exts.khr_android_surface_swapchain {
                    return Err(ClientError::OpenXr(
                        "XR_KHR_android_surface_swapchain is required on Meta Horizon OS".into(),
                    ));
                }
                if !available_exts.fb_android_surface_swapchain_create {
                    return Err(ClientError::OpenXr(
                        "XR_FB_android_surface_swapchain_create is required on Meta Horizon OS"
                            .into(),
                    ));
                }
            }

            if available_exts.khr_android_create_instance {
                required_exts.khr_android_create_instance = true;
            }
            if available_exts.khr_android_surface_swapchain {
                required_exts.khr_android_surface_swapchain = true;
            }
            if available_exts.fb_android_surface_swapchain_create {
                required_exts.fb_android_surface_swapchain_create = true;
            }
        }

        let app_info = ApplicationInfo {
            application_name: "linux-quest",
            application_version: 1,
            engine_name: "linux-quest-engine",
            engine_version: 1,
            api_version: Version::new(1, 0, 0),
        };

        let instance = entry
            .create_instance(&app_info, &required_exts, &[], &platform_info)
            .map_err(|e| ClientError::OpenXr(format!("Failed to create OpenXR instance: {e:?}")))?;

        let timespec_converter = if available_exts.khr_convert_timespec_time {
            let loaded = unsafe {
                openxr::raw::ConvertTimespecTimeKHR::load(&entry, instance.as_raw()).ok()
            };
            #[cfg(target_os = "android")]
            if !config.force_mock_decoder && loaded.is_none() {
                return Err(ClientError::OpenXr(
                    "Failed to load ConvertTimespecTimeKHR function pointers on Meta Horizon OS"
                        .into(),
                ));
            }
            loaded
        } else {
            None
        };

        let system = instance
            .system(FormFactor::HEAD_MOUNTED_DISPLAY)
            .map_err(|e| ClientError::OpenXr(format!("Failed to query HMD system: {e:?}")))?;
        let max_layers = instance
            .system_properties(system)
            .map(|p| p.graphics_properties.max_layer_count as usize)
            .unwrap_or(16)
            .max(1);
        tracing::info!(
            max_layers,
            rounded_corners = supports_alpha_blend,
            "OpenXR composition limits"
        );

        let vk_reqs = instance
            .graphics_requirements::<Vulkan>(system)
            .map_err(|e| {
                ClientError::OpenXr(format!("Failed to query graphics requirements: {e:?}"))
            })?;

        tracing::info!(
            min_vulkan_version = ?vk_reqs.min_api_version_supported,
            max_vulkan_version = ?vk_reqs.max_api_version_supported,
            "OpenXR Vulkan requirements verified"
        );

        let vk_entry = unsafe {
            ash::Entry::load().map_err(|e| {
                ClientError::Vulkan(format!("Failed to load Vulkan entry point: {e:?}"))
            })?
        };

        let vk_app_info = vk::ApplicationInfo::default()
            .application_version(vk::make_api_version(0, 0, 1, 0))
            .engine_version(vk::make_api_version(0, 0, 1, 0))
            .api_version(vk::make_api_version(0, 1, 1, 0));

        let vk_instance_create_info =
            vk::InstanceCreateInfo::default().application_info(&vk_app_info);

        let vk_instance_raw = unsafe {
            let res = instance
                .create_vulkan_instance(
                    system,
                    std::mem::transmute(vk_entry.static_fn().get_instance_proc_addr),
                    &vk_instance_create_info as *const _ as *const _,
                )
                .map_err(|e| {
                    ClientError::OpenXr(format!("XR error creating Vulkan instance: {e:?}"))
                })?;
            res.map_err(|e| ClientError::Vulkan(format!("Vulkan error creating instance: {e}")))?
        };

        let vk_physical_device_raw = unsafe {
            instance
                .vulkan_graphics_device(system, vk_instance_raw)
                .map_err(|e| {
                    ClientError::OpenXr(format!("Failed to get Vulkan graphics device: {e:?}"))
                })?
        };
        let vk_physical_device = vk::PhysicalDevice::from_raw(vk_physical_device_raw as _);

        let ash_instance = unsafe {
            ash::Instance::load(
                vk_entry.static_fn(),
                vk::Instance::from_raw(vk_instance_raw as _),
            )
        };

        let queue_family_index = unsafe {
            ash_instance
                .get_physical_device_queue_family_properties(vk_physical_device)
                .into_iter()
                .enumerate()
                .find_map(|(idx, info)| {
                    if info.queue_flags.contains(vk::QueueFlags::GRAPHICS) {
                        Some(idx as u32)
                    } else {
                        None
                    }
                })
                .ok_or_else(|| ClientError::Vulkan("No graphics queue family found".into()))?
        };

        let queue_priorities = [1.0f32];
        let queue_create_info = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family_index)
            .queue_priorities(&queue_priorities);
        let queue_create_infos = [queue_create_info];

        let vk_device_create_info =
            vk::DeviceCreateInfo::default().queue_create_infos(&queue_create_infos);

        let vk_device_raw = unsafe {
            let res = instance
                .create_vulkan_device(
                    system,
                    std::mem::transmute(vk_entry.static_fn().get_instance_proc_addr),
                    vk_physical_device_raw,
                    &vk_device_create_info as *const _ as *const _,
                )
                .map_err(|e| {
                    ClientError::OpenXr(format!("XR error creating Vulkan device: {e:?}"))
                })?;
            res.map_err(|e| ClientError::Vulkan(format!("Vulkan error creating device: {e}")))?
        };

        // Initialize VulkanContext from OpenXR-created handles
        *vulkan_context = VulkanContext::new_from_openxr(
            vk::Instance::from_raw(vk_instance_raw as _),
            vk_physical_device,
            vk::Device::from_raw(vk_device_raw as _),
            queue_family_index,
            0,
        )?;

        // Create OpenXR Session
        let (session, frame_waiter, frame_stream) = unsafe {
            instance
                .create_session::<Vulkan>(
                    system,
                    &openxr::vulkan::SessionCreateInfo {
                        instance: vk_instance_raw,
                        physical_device: vk_physical_device_raw,
                        device: vk_device_raw,
                        queue_family_index,
                        queue_index: 0,
                    },
                )
                .map_err(|e| {
                    ClientError::OpenXr(format!("Failed to create OpenXR session: {e:?}"))
                })?
        };

        // Reference spaces: Stage (fall back to Local) and View
        let stage_space = session
            .create_reference_space(openxr::ReferenceSpaceType::STAGE, openxr::Posef::IDENTITY)
            .or_else(|_| {
                session.create_reference_space(
                    openxr::ReferenceSpaceType::LOCAL,
                    openxr::Posef::IDENTITY,
                )
            })
            .map_err(|e| ClientError::OpenXr(format!("Failed to create reference space: {e:?}")))?;

        let view_space = session
            .create_reference_space(openxr::ReferenceSpaceType::VIEW, openxr::Posef::IDENTITY)
            .map_err(|e| ClientError::OpenXr(format!("Failed to create view space: {e:?}")))?;

        // Refresh rate manager
        let refresh_rate_manager = if supports_refresh_rate {
            match session.enumerate_display_refresh_rates() {
                Ok(rates) if !rates.is_empty() => {
                    tracing::info!(rates = ?rates, "Enumerated OpenXR display refresh rates");
                    let mgr = RefreshRateManager::new(rates, config.target_refresh_rate);
                    let target = mgr.current_rate();
                    if let Err(e) = session.request_display_refresh_rate(target) {
                        tracing::warn!(error = ?e, rate = target, "Failed to request initial refresh rate");
                    }
                    Some(mgr)
                }
                _ => Some(RefreshRateManager::new(
                    vec![72.0, 90.0, 120.0],
                    config.target_refresh_rate,
                )),
            }
        } else {
            Some(RefreshRateManager::new(
                vec![72.0, 90.0, 120.0],
                config.target_refresh_rate,
            ))
        };

        // Initialize Quest Passthrough (XR_FB_passthrough)
        let passthrough: Box<dyn PassthroughBackend> = if supports_passthrough {
            let blend_modes = instance
                .enumerate_environment_blend_modes(system, ViewConfigurationType::PRIMARY_STEREO)
                .unwrap_or_default();
            let supports_alpha_blend = blend_modes.contains(&EnvironmentBlendMode::ALPHA_BLEND);

            match OpenXrPassthroughBackend::try_init(
                &session,
                supports_alpha_blend,
                config.enable_passthrough,
            ) {
                Ok(backend) => {
                    tracing::info!(
                        supports_alpha_blend,
                        "XR_FB_passthrough backend initialized"
                    );
                    Box::new(backend)
                }
                Err(e) => {
                    tracing::warn!(error = ?e, "Failed to initialize OpenXR passthrough backend, falling back to opaque");
                    Box::new(MockPassthroughBackend::unsupported())
                }
            }
        } else {
            Box::new(MockPassthroughBackend::unsupported())
        };

        // Initialize OpenXR controller interaction actions
        let interaction: Box<dyn InteractionBackend> = match OpenXrInteractionBackend::try_init(
            &instance, &session,
        ) {
            Ok(backend) => {
                tracing::info!("OpenXR controller interaction action set initialized");
                Box::new(backend)
            }
            Err(e) => {
                tracing::warn!(error = ?e, "Failed to initialize OpenXR interaction actions; falling back to mock");
                Box::new(MockInteractionBackend::new())
            }
        };

        // Initialize OpenXR XR_EXT_hand_tracking
        let hand_tracking: Box<dyn HandTrackingBackend> = if supports_hand_tracking {
            match OpenXrHandTrackingBackend::try_init(&session) {
                Ok(backend) => {
                    tracing::info!("XR_EXT_hand_tracking backend initialized");
                    Box::new(backend)
                }
                Err(e) => {
                    tracing::warn!(error = ?e, "Failed to initialize OpenXR hand tracking; falling back to mock");
                    Box::new(MockHandTrackingBackend::unsupported())
                }
            }
        } else {
            Box::new(MockHandTrackingBackend::unsupported())
        };
        // Enumerate swapchain formats supported by OpenXR runtime
        let supported_formats = session.enumerate_swapchain_formats().unwrap_or_default();
        let swapchain_format =
            if supported_formats.contains(&(vk::Format::R8G8B8A8_UNORM.as_raw() as u32)) {
                vk::Format::R8G8B8A8_UNORM.as_raw() as u32
            } else if supported_formats.contains(&(vk::Format::R8G8B8A8_SRGB.as_raw() as u32)) {
                vk::Format::R8G8B8A8_SRGB.as_raw() as u32
            } else if let Some(&first) = supported_formats.first() {
                first
            } else {
                vk::Format::R8G8B8A8_UNORM.as_raw() as u32
            };

        // Swapchains are NOT created here. On Horizon OS the runtime rejects
        // xrCreateSwapchainAndroidSurfaceKHR until the activity is ready and the session is
        // running (SessionState::READY handled by `poll_events`). `create_swapchains` does it.
        #[cfg(target_os = "android")]
        let android_surface_pfn = if available_exts.khr_android_surface_swapchain {
            Some(unsafe {
                openxr::raw::AndroidSurfaceSwapchainKHR::load(&entry, instance.as_raw()).map_err(
                    |e| {
                        ClientError::OpenXr(format!(
                            "Failed to load AndroidSurfaceSwapchainKHR: {e:?}"
                        ))
                    },
                )?
            })
        } else {
            None
        };

        Ok(Self {
            instance: Some(instance),
            system,
            session: Some(session),
            frame_waiter: Some(parking_lot::Mutex::new(frame_waiter)),
            frame_stream: Some(parking_lot::Mutex::new(frame_stream)),
            stage_space: Some(stage_space),
            view_space: Some(view_space),
            desktop_swapchain: None,
            hud_swapchain: None,
            hud_images: Vec::new(),
            overlay_swapchains: None,
            #[cfg(target_os = "android")]
            surface_window: None,
            #[cfg(target_os = "android")]
            android_surface_pfn,
            #[cfg(target_os = "android")]
            supports_fb_surface_create: available_exts.fb_android_surface_swapchain_create,
            swapchain_format,
            swapchains_created: false,
            refresh_rate_manager,
            session_state: SessionState::IDLE,
            session_running: Arc::new(AtomicBool::new(false)),
            supports_cylinder,
            desktop_swapchain_is_surface: false,
            desktop_size: (config.stream_width, config.stream_height),
            supports_layer_settings: available_exts.fb_composition_layer_settings,
            supports_alpha_blend,
            max_layers,
            should_exit: Arc::new(AtomicBool::new(false)),
            timespec_converter,
            passthrough,
            interaction,
            hand_tracking,
            screen_transform: None,
            extra_screens: Default::default(),
            is_simulated: false,
        })
    }

    /// Whether the desktop and HUD swapchains exist (always true for a simulated context).
    pub fn swapchains_created(&self) -> bool {
        self.swapchains_created
    }

    /// Creates the desktop (Android surface) and HUD swapchains.
    ///
    /// Must only be called once the session is running (`SessionState::READY` handled and
    /// `xrBeginSession` succeeded): Horizon OS answers `xrCreateSwapchainAndroidSurfaceKHR`
    /// with a runtime failure before that. Returns `Ok(())` without doing anything if the
    /// swapchains already exist, and an error (to retry later) if the session is not running.
    pub fn create_swapchains(&mut self, config: &ClientConfig) -> ClientResult<()> {
        if self.swapchains_created {
            return Ok(());
        }
        if !self.is_session_running() {
            return Err(ClientError::OpenXr(
                "create_swapchains called before the OpenXR session is running".into(),
            ));
        }
        let session = self
            .session
            .clone()
            .ok_or_else(|| ClientError::OpenXr("Session not initialized".into()))?;
        let swapchain_format = self.swapchain_format;

        #[cfg(target_os = "android")]
        let (desktop_swapchain, surface_window, desktop_swapchain_is_surface) = if let Some(pfn) =
            self.android_surface_pfn
        {
            let (sc, native_window) = self.create_surface_swapchain(
                &session,
                pfn,
                config.stream_width,
                config.stream_height,
            )?;
            (Some(sc), Some(native_window), true)
        } else {
            if !config.force_mock_decoder {
                return Err(ClientError::OpenXr(
                    "XR_KHR_android_surface_swapchain is required on Meta Horizon OS".into(),
                ));
            }
            let desktop_info = SwapchainCreateInfo {
                create_flags: SwapchainCreateFlags::EMPTY,
                usage_flags: SwapchainUsageFlags::COLOR_ATTACHMENT | SwapchainUsageFlags::SAMPLED,
                format: swapchain_format,
                sample_count: 1,
                width: config.stream_width,
                height: config.stream_height,
                face_count: 1,
                array_size: 1,
                mip_count: 1,
            };
            let sc = session.create_swapchain(&desktop_info).map_err(|e| {
                ClientError::OpenXr(format!("Failed to create desktop swapchain: {e:?}"))
            })?;
            (Some(sc), None, false)
        };

        #[cfg(not(target_os = "android"))]
        let (desktop_swapchain, desktop_swapchain_is_surface) = {
            let desktop_info = SwapchainCreateInfo {
                create_flags: SwapchainCreateFlags::EMPTY,
                usage_flags: SwapchainUsageFlags::COLOR_ATTACHMENT | SwapchainUsageFlags::SAMPLED,
                format: swapchain_format,
                sample_count: 1,
                width: config.stream_width,
                height: config.stream_height,
                face_count: 1,
                array_size: 1,
                mip_count: 1,
            };
            let sc = session.create_swapchain(&desktop_info).map_err(|e| {
                ClientError::OpenXr(format!("Failed to create desktop swapchain: {e:?}"))
            })?;
            (Some(sc), false)
        };

        let hud_info = SwapchainCreateInfo {
            create_flags: SwapchainCreateFlags::EMPTY,
            usage_flags: SwapchainUsageFlags::COLOR_ATTACHMENT
                | SwapchainUsageFlags::SAMPLED
                | SwapchainUsageFlags::TRANSFER_DST,
            format: swapchain_format,
            sample_count: 1,
            width: 512,
            height: 256,
            face_count: 1,
            array_size: 1,
            mip_count: 1,
        };
        let hud_swapchain = session
            .create_swapchain(&hud_info)
            .map_err(|e| ClientError::OpenXr(format!("Failed to create HUD swapchain: {e:?}")))?;
        let hud_images = hud_swapchain
            .enumerate_images()
            .map(|imgs| {
                imgs.into_iter()
                    .map(|raw| vk::Image::from_raw(raw as _))
                    .collect()
            })
            .unwrap_or_default();

        self.desktop_swapchain = desktop_swapchain;
        self.desktop_swapchain_is_surface = desktop_swapchain_is_surface;
        self.desktop_size = (config.stream_width, config.stream_height);
        #[cfg(target_os = "android")]
        {
            self.surface_window = surface_window;
        }
        self.hud_swapchain = Some(hud_swapchain);
        self.hud_images = hud_images;
        // Pointers and border are optional: without them the desktop still works.
        let overlay = (|| -> ClientResult<OverlaySwapchains> {
            use crate::openxr::overlay::*;
            let tex = |size, rgba| OverlayTexture::create(&session, swapchain_format, size, rgba);
            Ok(OverlaySwapchains {
                ray: tex(RAY_TEXTURE, ray_rgba())?,
                cursor: tex(CURSOR_TEXTURE, cursor_rgba())?,
                mask: tex(MASK_TEXTURE, mask_rgba())?,
                bar: [
                    tex(BAR_TEXTURE, bar_rgba(0.55))?,
                    tex(BAR_TEXTURE, bar_rgba(0.95))?,
                ],
                edge: [
                    tex(EDGE_TEXTURE, edge_rgba(0.6))?,
                    tex(EDGE_TEXTURE, edge_rgba(1.0))?,
                ],
                corner: [
                    tex(CORNER_TEXTURE, corner_rgba(0.75))?,
                    tex(CORNER_TEXTURE, corner_rgba(1.0))?,
                ],
            })
        })();
        match overlay {
            Ok(o) => self.overlay_swapchains = Some(o),
            Err(e) => tracing::warn!(error = ?e, "pointer and window-handle overlays unavailable"),
        }
        self.swapchains_created = true;
        Ok(())
    }

    /// Creates an Android surface swapchain (the decoder renders straight into its surface) of
    /// `width` x `height` and returns it with its native window.
    #[cfg(target_os = "android")]
    fn create_surface_swapchain(
        &self,
        session: &Session<Vulkan>,
        pfn: openxr::raw::AndroidSurfaceSwapchainKHR,
        width: u32,
        height: u32,
    ) -> ClientResult<(Swapchain<Vulkan>, ndk::native_window::NativeWindow)> {
        let fb_surface_create_info = openxr::sys::AndroidSurfaceSwapchainCreateInfoFB {
            ty: openxr::sys::StructureType::ANDROID_SURFACE_SWAPCHAIN_CREATE_INFO_FB,
            next: std::ptr::null(),
            create_flags: openxr::sys::AndroidSurfaceSwapchainFlagsFB::USE_TIMESTAMPS,
        };
        let info = openxr::sys::SwapchainCreateInfo {
            ty: openxr::sys::StructureType::SWAPCHAIN_CREATE_INFO,
            next: if self.supports_fb_surface_create {
                &fb_surface_create_info as *const _ as *const std::ffi::c_void
            } else {
                std::ptr::null()
            },
            create_flags: openxr::sys::SwapchainCreateFlags::EMPTY,
            usage_flags: openxr::sys::SwapchainUsageFlags::COLOR_ATTACHMENT
                | openxr::sys::SwapchainUsageFlags::SAMPLED,
            // The Android Surface (MediaCodec output) owns the image format and layout;
            // Horizon OS rejects Vulkan values here ("info format validation failure").
            format: 0,
            sample_count: 0,
            width: width,
            height: height,
            face_count: 0,
            array_size: 0,
            mip_count: 0,
        };
        let mut raw_swapchain = openxr::sys::Swapchain::NULL;
        let mut surface: openxr::sys::platform::jobject = std::ptr::null_mut();
        let res = unsafe {
            (pfn.create_swapchain_android_surface)(
                session.as_raw(),
                &info,
                &mut raw_swapchain,
                &mut surface,
            )
        };
        if res.into_raw() < 0 {
            return Err(ClientError::OpenXr(format!(
                "xrCreateSwapchainAndroidSurfaceKHR failed with error code {}",
                res.into_raw()
            )));
        }
        let sc = unsafe { openxr::Swapchain::<Vulkan>::from_raw(session.clone(), raw_swapchain) };
        if surface.is_null() {
            return Err(ClientError::OpenXr(
                "xrCreateSwapchainAndroidSurfaceKHR returned a null surface jobject".into(),
            ));
        }
        let ctx = ndk_context::android_context();
        let vm = unsafe { jni::JavaVM::from_raw(ctx.vm() as *mut jni::sys::JavaVM) }
            .map_err(|e| ClientError::OpenXr(format!("JNI JavaVM error: {e:?}")))?;
        let (_guard, env_raw) = match vm.get_env() {
            Ok(env) => (None, env.get_raw()),
            Err(_) => {
                let guard = vm
                    .attach_current_thread()
                    .map_err(|e| ClientError::OpenXr(format!("JNI attach thread failed: {e:?}")))?;
                let raw = guard.get_raw();
                (Some(guard), raw)
            }
        };
        let win = unsafe { ndk_sys::ANativeWindow_fromSurface(env_raw as _, surface as _) };
        let native_window = std::ptr::NonNull::new(win)
            .map(|ptr| unsafe { ndk::native_window::NativeWindow::from_ptr(ptr) })
            .ok_or_else(|| ClientError::OpenXr("ANativeWindow_fromSurface returned NULL".into()))?;
        Ok((sc, native_window))
    }

    /// Whether this OpenXR context is running in simulated/mock mode.
    pub fn is_simulated(&self) -> bool {
        self.is_simulated
    }

    /// Returns the target Android native window surface if an Android surface swapchain is active.
    #[cfg(target_os = "android")]
    pub fn surface_window(&self) -> Option<&ndk::native_window::NativeWindow> {
        self.surface_window.as_ref()
    }

    /// Creates the screen for video stream `stream_id` (not 0, the primary screen): an Android
    /// surface swapchain of `width` x `height` for that stream's decoder, shown at
    /// `transform`. Needs a running session. A simulated context records the screen without
    /// a swapchain. Does nothing if the screen exists.
    pub fn create_extra_screen(
        &mut self,
        stream_id: u16,
        (width, height): (u32, u32),
        transform: ScreenTransform,
    ) -> ClientResult<()> {
        if self.extra_screens.contains_key(&stream_id) {
            return Ok(());
        }
        #[allow(unused_mut)]
        let mut screen = ExtraScreen {
            swapchain: None,
            #[cfg(target_os = "android")]
            surface_window: None,
            size: (width, height),
            transform,
        };
        if !self.is_simulated {
            #[cfg(target_os = "android")]
            {
                let session = self
                    .session
                    .clone()
                    .ok_or_else(|| ClientError::OpenXr("Session not initialized".into()))?;
                let pfn = self.android_surface_pfn.ok_or_else(|| {
                    ClientError::OpenXr(
                        "extra screens need XR_KHR_android_surface_swapchain".into(),
                    )
                })?;
                let (sc, window) = self.create_surface_swapchain(&session, pfn, width, height)?;
                screen.swapchain = Some(sc);
                screen.surface_window = Some(window);
            }
            #[cfg(not(target_os = "android"))]
            return Err(ClientError::OpenXr(
                "extra screens need XR_KHR_android_surface_swapchain".into(),
            ));
        }
        self.extra_screens.insert(stream_id, screen);
        Ok(())
    }

    /// The decoder surface of stream `stream_id`'s screen, once created.
    #[cfg(target_os = "android")]
    pub fn extra_surface_window(
        &self,
        stream_id: u16,
    ) -> Option<&ndk::native_window::NativeWindow> {
        self.extra_screens
            .get(&stream_id)
            .and_then(|s| s.surface_window.as_ref())
    }

    /// Pixel size of the primary screen's swapchain.
    pub fn desktop_size(&self) -> (u32, u32) {
        self.desktop_size
    }

    /// Re-creates the primary screen's surface swapchain at `size`, the size its stream
    /// decodes to. The compositor reads a surface swapchain's image rect in the decoded
    /// buffer's pixels, so a 1080p swapchain fed 4K frames shows only their top-left quarter.
    /// Release the decoder bound to the old surface first. Only a surface swapchain (or a
    /// simulated context) is resized; a regular one keeps its size.
    pub fn resize_desktop_surface(&mut self, size: (u32, u32)) -> ClientResult<()> {
        if size == self.desktop_size {
            return Ok(());
        }
        if self.is_simulated {
            self.desktop_size = size;
            return Ok(());
        }
        #[cfg(target_os = "android")]
        if self.desktop_swapchain_is_surface {
            let session = self
                .session
                .clone()
                .ok_or_else(|| ClientError::OpenXr("Session not initialized".into()))?;
            let pfn = self.android_surface_pfn.ok_or_else(|| {
                ClientError::OpenXr("XR_KHR_android_surface_swapchain is not available".into())
            })?;
            let (swapchain, window) =
                self.create_surface_swapchain(&session, pfn, size.0, size.1)?;
            self.surface_window = Some(window);
            self.desktop_swapchain = Some(swapchain);
            self.desktop_size = size;
        }
        Ok(())
    }

    /// Whether stream `stream_id` has its own screen.
    pub fn has_extra_screen(&self, stream_id: u16) -> bool {
        self.extra_screens.contains_key(&stream_id)
    }

    /// Moves stream `stream_id`'s screen.
    pub fn set_extra_screen_transform(&mut self, stream_id: u16, transform: ScreenTransform) {
        if let Some(screen) = self.extra_screens.get_mut(&stream_id) {
            screen.transform = transform;
        }
    }

    /// Requests a dynamic display refresh rate change from the OpenXR runtime.
    pub fn request_refresh_rate(&mut self, target_rate: f32) -> ClientResult<()> {
        let best = if let Some(mgr) = self.refresh_rate_manager.as_ref() {
            mgr.find_closest_supported_rate(target_rate)
        } else {
            return Ok(());
        };

        if let Some(session) = &self.session {
            session.request_display_refresh_rate(best).map_err(|e| {
                ClientError::OpenXr(format!("Failed to request refresh rate {best}: {e:?}"))
            })?;
        }

        if let Some(mgr) = self.refresh_rate_manager.as_mut() {
            mgr.on_refresh_rate_changed(best);
        }
        Ok(())
    }

    /// Returns current OpenXR session state.
    pub fn session_state(&self) -> SessionState {
        self.session_state
    }

    /// Whether the session is actively running.
    pub fn is_session_running(&self) -> bool {
        self.session_running.load(Ordering::SeqCst)
    }

    /// Whether the OpenXR session has received an EXITING or LOSS_PENDING event.
    pub fn should_exit(&self) -> bool {
        self.should_exit.load(Ordering::SeqCst)
    }

    /// Converts an OpenXR predicted presentation time (`XrTime`) to a `CLOCK_MONOTONIC` timestamp in nanoseconds.
    pub fn convert_time_to_monotonic_ns(&self, time: openxr::Time) -> i64 {
        if let (Some(ext), Some(instance)) = (&self.timespec_converter, &self.instance) {
            let mut ts = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            let res =
                unsafe { (ext.convert_time_to_timespec_time)(instance.as_raw(), time, &mut ts) };
            if res.into_raw() >= 0 {
                return ts.tv_sec * 1_000_000_000 + ts.tv_nsec;
            }
        }

        // Fallback for simulation mode or when XR_KHR_convert_timespec_time is unavailable
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe {
            libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now);
        }
        now.tv_sec * 1_000_000_000 + now.tv_nsec
    }

    /// Returns the current CLOCK_MONOTONIC timestamp in nanoseconds, the clock used by Android
    /// surface frame timestamps.
    pub fn monotonic_now_ns(&self) -> i64 {
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe {
            libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now);
        }
        now.tv_sec * 1_000_000_000 + now.tv_nsec
    }

    /// Returns the active refresh rate manager.
    pub fn refresh_rate_manager(&self) -> Option<&RefreshRateManager> {
        self.refresh_rate_manager.as_ref()
    }

    /// Returns the OpenXR SystemId.
    pub fn system(&self) -> SystemId {
        self.system
    }

    /// Returns a mutable reference to the active refresh rate manager.
    pub fn refresh_rate_manager_mut(&mut self) -> Option<&mut RefreshRateManager> {
        self.refresh_rate_manager.as_mut()
    }

    /// Polls OpenXR runtime events and updates session state machine.
    pub fn poll_events(&mut self) -> ClientResult<()> {
        if self.is_simulated {
            return Ok(());
        }

        let instance = self
            .instance
            .as_ref()
            .ok_or_else(|| ClientError::OpenXr("Instance not loaded".to_string()))?;

        let mut event_storage = openxr::EventDataBuffer::new();
        while let Some(event) = instance
            .poll_event(&mut event_storage)
            .map_err(|e| ClientError::OpenXr(format!("Poll event failed: {e:?}")))?
        {
            match event {
                openxr::Event::SessionStateChanged(state_event) => {
                    let new_state = state_event.state();
                    tracing::info!(old = ?self.session_state, new = ?new_state, "lq-lifecycle: OpenXR SessionState changed");
                    self.session_state = new_state;

                    match new_state {
                        SessionState::READY => {
                            if let Some(session) = &self.session {
                                if !self.session_running.load(Ordering::SeqCst) {
                                    session
                                        .begin(ViewConfigurationType::PRIMARY_STEREO)
                                        .map_err(|e| {
                                            ClientError::OpenXr(format!(
                                                "Session begin failed: {e:?}"
                                            ))
                                        })?;
                                    self.session_running.store(true, Ordering::SeqCst);
                                    tracing::info!("lq-lifecycle: xrBeginSession succeeded");
                                }
                            }
                        }
                        SessionState::STOPPING => {
                            if let Some(session) = &self.session {
                                if let Err(e) = session.end() {
                                    tracing::warn!(error = ?e, "lq-lifecycle: xrEndSession failed");
                                } else {
                                    tracing::info!("lq-lifecycle: xrEndSession succeeded");
                                }
                                self.session_running.store(false, Ordering::SeqCst);
                            }
                        }
                        SessionState::EXITING | SessionState::LOSS_PENDING => {
                            self.session_running.store(false, Ordering::SeqCst);
                            self.should_exit.store(true, Ordering::SeqCst);
                            tracing::info!(state = ?new_state, "lq-lifecycle: session ending, exiting");
                        }
                        _ => {}
                    }
                }
                openxr::Event::DisplayRefreshRateChangedFB(rate_event) => {
                    let new_rate = rate_event.to_display_refresh_rate();
                    if let Some(mgr) = &mut self.refresh_rate_manager {
                        mgr.on_refresh_rate_changed(new_rate);
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// Creates an OpenXR swapchain for desktop streaming.
    pub fn create_desktop_swapchain(
        &self,
        width: u32,
        height: u32,
        format: vk::Format,
    ) -> ClientResult<Option<Swapchain<Vulkan>>> {
        if self.is_simulated {
            return Ok(None);
        }

        let session = self
            .session
            .as_ref()
            .ok_or_else(|| ClientError::OpenXr("Session not initialized".to_string()))?;

        let info = SwapchainCreateInfo {
            create_flags: SwapchainCreateFlags::EMPTY,
            usage_flags: SwapchainUsageFlags::COLOR_ATTACHMENT | SwapchainUsageFlags::SAMPLED,
            format: format.as_raw() as u32,
            sample_count: 1,
            width,
            height,
            face_count: 1,
            array_size: 1,
            mip_count: 1,
        };

        let swapchain = session
            .create_swapchain(&info)
            .map_err(|e| ClientError::OpenXr(format!("Failed to create swapchain: {e:?}")))?;

        Ok(Some(swapchain))
    }

    /// Returns the stage reference space for 6DoF tracking.
    pub fn stage_space(&self) -> Option<&Space> {
        self.stage_space.as_ref()
    }

    /// Returns the head-locked view reference space.
    pub fn view_space(&self) -> Option<&Space> {
        self.view_space.as_ref()
    }

    /// Returns the active desktop swapchain, if created.
    pub fn desktop_swapchain(&self) -> Option<&Swapchain<Vulkan>> {
        self.desktop_swapchain.as_ref()
    }

    /// Returns the active HUD swapchain, if created.
    pub fn hud_swapchain(&self) -> Option<&Swapchain<Vulkan>> {
        self.hud_swapchain.as_ref()
    }

    /// Whether the OpenXR runtime supports curved cylinder layers.
    pub fn supports_cylinder(&self) -> bool {
        self.supports_cylinder
    }

    /// Waits for the next frame presentation pacing slot from OpenXR.
    pub fn wait_frame(&self) -> ClientResult<FrameState> {
        if self.is_simulated {
            Ok(FrameState {
                predicted_display_time: Time::from_nanos(1),
                predicted_display_period: openxr::Duration::from_nanos(11_111_111),
                should_render: true,
            })
        } else if let Some(waiter) = &self.frame_waiter {
            waiter
                .lock()
                .wait()
                .map_err(|e| ClientError::OpenXr(format!("wait_frame failed: {e:?}")))
        } else {
            Err(ClientError::OpenXr("Frame waiter not initialized".into()))
        }
    }

    /// Signals the start of GPU work for the current frame.
    pub fn begin_frame(&self) -> ClientResult<()> {
        if self.is_simulated {
            Ok(())
        } else if let Some(stream) = &self.frame_stream {
            stream
                .lock()
                .begin()
                .map_err(|e| ClientError::OpenXr(format!("begin_frame failed: {e:?}")))
        } else {
            Err(ClientError::OpenXr("Frame stream not initialized".into()))
        }
    }

    /// Submits composed layers to the OpenXR compositor for timewarp display.
    pub fn end_frame(
        &self,
        display_time: Time,
        layers: &[&CompositionLayerBase<'_, Vulkan>],
    ) -> ClientResult<()> {
        if self.is_simulated {
            Ok(())
        } else if let Some(stream) = &self.frame_stream {
            let blend_mode = self.passthrough.blend_mode();
            stream
                .lock()
                .end(display_time, blend_mode, layers)
                .map_err(|e| ClientError::OpenXr(format!("end_frame failed: {e:?}")))
        } else {
            Err(ClientError::OpenXr("Frame stream not initialized".into()))
        }
    }

    /// Submits an empty layer list when should_render is false.
    pub fn end_frame_empty(&self, display_time: Time) -> ClientResult<()> {
        self.end_frame(display_time, &[])
    }

    /// Composes and presents the desktop, passthrough, and optional HUD layers to OpenXR.
    pub fn render_and_present_layers(
        &mut self,
        display_time: Time,
        config: &ClientConfig,
        vulkan_context: &VulkanContext,
        hud_rgba: Option<&[u8]>,
        overlay: &crate::openxr::overlay::OverlayScene,
    ) -> ClientResult<()> {
        if self.is_simulated {
            return Ok(());
        }

        let (stream_width, stream_height) = self.desktop_size;

        if let (Some(space), Some(desktop_sc)) = (&self.stage_space, &mut self.desktop_swapchain) {
            // Under XR_KHR_android_surface_swapchain, AMediaCodec is the producer.
            // Calling xrAcquireSwapchainImage/xrWaitSwapchainImage/xrReleaseSwapchainImage on a surface
            // swapchain is forbidden by the OpenXR specification and causes compositor faults.
            if !self.desktop_swapchain_is_surface {
                let _ = desktop_sc.acquire_image();
                let _ = desktop_sc.wait_image(openxr::Duration::INFINITE);
                let _ = desktop_sc.release_image();
            }
            let desktop_sc: &Swapchain<Vulkan> = desktop_sc;

            let desktop_cfg = if let Some(ref tf) = self.screen_transform {
                tf.to_desktop_layer_config()
            } else {
                crate::openxr::layers::DesktopLayerConfig {
                    mode: config.display_mode,
                    quad_size: config.quad_size,
                    distance: config.quad_distance,
                    cylinder_radius: config.cylinder_radius,
                    cylinder_central_angle: config.cylinder_central_angle,
                    cylinder_aspect_ratio: config.cylinder_aspect_ratio,
                    pose: None,
                }
            };

            let stream_mutex = self
                .frame_stream
                .as_ref()
                .ok_or_else(|| ClientError::OpenXr("Frame stream not initialized".into()))?;
            let mut stream = stream_mutex.lock();

            let layer_settings = openxr::sys::CompositionLayerSettingsFB {
                ty: openxr::sys::StructureType::COMPOSITION_LAYER_SETTINGS_FB,
                next: std::ptr::null(),
                layer_flags: openxr::sys::CompositionLayerSettingsFlagsFB::QUALITY_SUPER_SAMPLING
                    | openxr::sys::CompositionLayerSettingsFlagsFB::QUALITY_SHARPENING,
            };

            let mut hud_layer_ready = false;
            if config.enable_hud && self.view_space.is_some() {
                if let Some(hud_sc) = &mut self.hud_swapchain {
                    if let Ok(idx) = hud_sc.acquire_image() {
                        let wait_res = hud_sc.wait_image(openxr::Duration::INFINITE);
                        let mut upload_ok = false;
                        if wait_res.is_ok() {
                            if let Some(rgba) = hud_rgba {
                                if let Some(&img) = self.hud_images.get(idx as usize) {
                                    if let Err(e) =
                                        vulkan_context.upload_rgba_to_image(img, 512, 256, rgba)
                                    {
                                        tracing::warn!(
                                            error = ?e,
                                            "Failed to upload HUD RGBA texture to OpenXR swapchain image"
                                        );
                                    } else {
                                        upload_ok = true;
                                    }
                                }
                            }
                        }
                        // Always release acquired image, even if wait or upload failed
                        let rel_res = hud_sc.release_image();
                        if wait_res.is_ok() && upload_ok && rel_res.is_ok() {
                            hud_layer_ready = true;
                        }
                    }
                }
            }

            let hud_layer = if hud_layer_ready {
                Some(crate::openxr::layers::build_hud_layer(
                    self.view_space.as_ref().unwrap(),
                    self.hud_swapchain.as_ref().unwrap(),
                    512,
                    256,
                ))
            } else {
                None
            };

            let blend_mode = self.passthrough.blend_mode();
            let pt_layer = if self.passthrough.is_enabled() {
                self.passthrough.passthrough_layer().map(|layer_handle| {
                    crate::openxr::layers::build_passthrough_layer(layer_handle)
                })
            } else {
                None
            };

            // Pointers, window handles and the corner mask: upload what is drawn (once; the
            // compositor keeps showing a swapchain's last released image), then build layers.
            use crate::openxr::overlay::{
                cursor_pose, ray_quad_pose, BAR_TEXTURE, CORNER_TEXTURE, CURSOR_SIZE_M,
                CURSOR_TEXTURE, EDGE_TEXTURE, MASK_TEXTURE, RAY_TEXTURE, RAY_WIDTH_M,
            };
            let chrome = [
                (overlay.chrome.bar, BAR_TEXTURE),
                (overlay.chrome.edge, EDGE_TEXTURE),
                (overlay.chrome.corner, CORNER_TEXTURE),
            ];
            let (mut rays_ok, mut cursor_ok, mut mask_ok) = (false, false, false);
            let mut chrome_ok = [false; 3];
            if let Some(o) = &mut self.overlay_swapchains {
                if !overlay.pointers.is_empty() {
                    rays_ok = o.ray.ensure(vulkan_context);
                    if overlay.pointers.iter().any(|p| p.hit) {
                        cursor_ok = o.cursor.ensure(vulkan_context);
                    }
                }
                if self.supports_alpha_blend {
                    mask_ok = o.mask.ensure(vulkan_context);
                }
                let looks = [&mut o.bar, &mut o.edge, &mut o.corner];
                for ((ok, (handle, _)), look) in chrome_ok.iter_mut().zip(&chrome).zip(looks) {
                    if let Some(h) = handle {
                        *ok = look[usize::from(h.active)].ensure(vulkan_context);
                    }
                }
            }
            let eye = self
                .view_space
                .as_ref()
                .and_then(|v| v.locate(space, display_time).ok())
                .filter(|l| {
                    l.location_flags
                        .contains(openxr::SpaceLocationFlags::POSITION_VALID)
                })
                .map(|l| crate::spatial::Vec3::from(l.pose.position))
                .unwrap_or(crate::spatial::Vec3::new(0.0, 1.6, 0.0));
            let ov = self.overlay_swapchains.as_ref();
            let mut pointer_layers = Vec::new();
            if let Some(o) = ov {
                let looks = [&o.bar, &o.edge, &o.corner];
                for ((ok, (handle, size_px)), look) in chrome_ok.iter().zip(&chrome).zip(looks) {
                    if let (true, Some(h)) = (ok, handle) {
                        pointer_layers.push(crate::openxr::layers::build_overlay_quad(
                            space,
                            look[usize::from(h.active)].swapchain(),
                            *size_px,
                            openxr::Posef::from(h.pose),
                            h.size,
                        ));
                    }
                }
                if rays_ok {
                    for p in &overlay.pointers {
                        pointer_layers.push(crate::openxr::layers::build_overlay_quad(
                            space,
                            o.ray.swapchain(),
                            RAY_TEXTURE,
                            openxr::Posef::from(ray_quad_pose(p, eye)),
                            (RAY_WIDTH_M, p.length.max(0.01)),
                        ));
                    }
                }
                if cursor_ok {
                    for p in overlay.pointers.iter().filter(|p| p.hit) {
                        pointer_layers.push(crate::openxr::layers::build_overlay_quad(
                            space,
                            o.cursor.swapchain(),
                            CURSOR_TEXTURE,
                            openxr::Posef::from(cursor_pose(p)),
                            (CURSOR_SIZE_M, CURSOR_SIZE_M),
                        ));
                    }
                }
            }

            // Each screen, primary first: with rounded corners, a mask layer of the same shape
            // first sets the destination alpha (leaving the colour), and the screen is blended
            // by it, so its corners show what is behind. Pointers and handles go over the
            // screens, then the HUD on top.
            use openxr::sys::BlendFactorFB as F;
            let settings_next = if self.supports_layer_settings {
                &layer_settings as *const _ as *const std::ffi::c_void
            } else {
                std::ptr::null()
            };
            let mask_blend = openxr::sys::CompositionLayerAlphaBlendFB {
                ty: openxr::sys::CompositionLayerAlphaBlendFB::TYPE,
                next: std::ptr::null_mut(),
                src_factor_color: F::ZERO,
                dst_factor_color: F::ONE,
                src_factor_alpha: F::ONE,
                dst_factor_alpha: F::ZERO,
            };
            let screen_blend = openxr::sys::CompositionLayerAlphaBlendFB {
                ty: openxr::sys::CompositionLayerAlphaBlendFB::TYPE,
                next: settings_next as *mut std::ffi::c_void,
                src_factor_color: F::DST_ALPHA,
                dst_factor_color: F::ONE_MINUS_DST_ALPHA,
                src_factor_alpha: F::ZERO,
                dst_factor_alpha: F::ONE,
            };
            let rounded = mask_ok && ov.is_some();
            let source_alpha = openxr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA;
            let screens = std::iter::once((desktop_sc, (stream_width, stream_height), desktop_cfg))
                .chain(self.extra_screens.values().filter_map(|screen| {
                    let sc = screen.swapchain.as_ref()?;
                    Some((sc, screen.size, screen.transform.to_desktop_layer_config()))
                }));
            let mut desktop_layers = Vec::new();
            for (sc, size, cfg) in screens {
                match ov.filter(|_| rounded) {
                    Some(o) => {
                        desktop_layers.push(DesktopLayer::build(
                            space,
                            o.mask.swapchain(),
                            MASK_TEXTURE,
                            &cfg,
                            self.supports_cylinder,
                            &mask_blend as *const _ as *const std::ffi::c_void,
                            source_alpha,
                        ));
                        desktop_layers.push(DesktopLayer::build(
                            space,
                            sc,
                            size,
                            &cfg,
                            self.supports_cylinder,
                            &screen_blend as *const _ as *const std::ffi::c_void,
                            source_alpha,
                        ));
                    }
                    None => desktop_layers.push(DesktopLayer::build(
                        space,
                        sc,
                        size,
                        &cfg,
                        self.supports_cylinder,
                        settings_next,
                        openxr::CompositionLayerFlags::EMPTY,
                    )),
                }
            }

            let mut layers: Vec<&CompositionLayerBase<'_, Vulkan>> =
                Vec::with_capacity(2 + desktop_layers.len() + pointer_layers.len());
            if let Some(ref pt) = pt_layer {
                layers.push(pt);
            }
            for l in &desktop_layers {
                layers.push(l.base());
            }
            for l in &pointer_layers {
                layers.push(l);
            }
            if let Some(ref hud) = hud_layer {
                layers.push(hud);
            }
            // Past the runtime's limit, drop overlays from the top rather than fail the frame.
            layers.truncate(self.max_layers);
            stream
                .end(display_time, blend_mode, &layers)
                .map_err(|e| ClientError::OpenXr(format!("end_frame failed: {e:?}")))?;
        } else if let Some(stream_mutex) = &self.frame_stream {
            let mut stream = stream_mutex.lock();
            stream
                .end(display_time, EnvironmentBlendMode::OPAQUE, &[])
                .map_err(|e| ClientError::OpenXr(format!("end_frame failed: {e:?}")))?;
        }

        Ok(())
    }

    /// Returns a reference to the active passthrough backend.
    pub fn passthrough(&self) -> &dyn PassthroughBackend {
        self.passthrough.as_ref()
    }

    /// Returns a mutable reference to the active passthrough backend.
    pub fn passthrough_mut(&mut self) -> &mut dyn PassthroughBackend {
        self.passthrough.as_mut()
    }

    /// Returns whether XR_FB_passthrough is supported.
    pub fn is_passthrough_supported(&self) -> bool {
        self.passthrough.is_supported()
    }

    /// Returns whether passthrough is currently enabled.
    pub fn is_passthrough_enabled(&self) -> bool {
        self.passthrough.is_enabled()
    }

    /// Enables or disables passthrough.
    pub fn set_passthrough_enabled(&mut self, enabled: bool) -> ClientResult<()> {
        self.passthrough.set_enabled(enabled)
    }

    /// Returns the active spatial screen transform if set.
    pub fn screen_transform(&self) -> Option<&ScreenTransform> {
        self.screen_transform.as_ref()
    }

    /// Sets or updates the active spatial screen transform.
    pub fn set_screen_transform(&mut self, transform: ScreenTransform) {
        self.screen_transform = Some(transform);
    }
    /// Returns a reference to the active controller interaction backend.
    pub fn interaction(&self) -> &dyn InteractionBackend {
        self.interaction.as_ref()
    }

    /// Returns a mutable reference to the active controller interaction backend.
    pub fn interaction_mut(&mut self) -> &mut Box<dyn InteractionBackend> {
        &mut self.interaction
    }

    /// Sets the controller interaction backend.
    pub fn set_interaction_backend(&mut self, backend: Box<dyn InteractionBackend>) {
        self.interaction = backend;
    }

    /// Returns a reference to the active skeletal hand tracking backend.
    pub fn hand_tracking(&self) -> &dyn HandTrackingBackend {
        self.hand_tracking.as_ref()
    }

    /// Returns a mutable reference to the active skeletal hand tracking backend.
    pub fn hand_tracking_mut(&mut self) -> &mut Box<dyn HandTrackingBackend> {
        &mut self.hand_tracking
    }

    /// Sets the skeletal hand tracking backend.
    pub fn set_hand_tracking_backend(&mut self, backend: Box<dyn HandTrackingBackend>) {
        self.hand_tracking = backend;
    }

    /// Polls controller interaction actions for the predicted display time.
    pub fn poll_interaction(&mut self, time: openxr::Time) -> ClientResult<()> {
        self.interaction
            .poll_actions(time, self.stage_space.as_ref())
    }

    /// Locates all 26 skeletal hand joints for the specified hand at the given display time.
    /// Where the head is in the stage space at `display_time`, when tracked.
    pub fn head_position(&self, display_time: Time) -> Option<crate::spatial::Vec3> {
        let (view, stage) = (self.view_space.as_ref()?, self.stage_space.as_ref()?);
        view.locate(stage, display_time)
            .ok()
            .filter(|l| {
                l.location_flags
                    .contains(openxr::SpaceLocationFlags::POSITION_VALID)
            })
            .map(|l| crate::spatial::Vec3::from(l.pose.position))
    }

    pub fn locate_hand_joints(
        &mut self,
        hand: Handedness,
        time: openxr::Time,
    ) -> ClientResult<Option<HandTrackingData>> {
        self.hand_tracking
            .locate_joints(hand, time, self.stage_space.as_ref())
    }

    /// Returns true if skeletal hand tracking (`XR_EXT_hand_tracking`) is supported.
    pub fn is_hand_tracking_supported(&self) -> bool {
        self.hand_tracking.is_supported()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simulated_openxr_context_lifecycle() {
        let config = ClientConfig::default();
        let mut ctx = OpenXrContext::new_simulated(&config);
        assert!(ctx.is_simulated());
        assert_eq!(ctx.session_state(), SessionState::FOCUSED);
        assert!(ctx.is_session_running());

        assert!(ctx.poll_events().is_ok());

        assert!(ctx.request_refresh_rate(120.0).is_ok());
        let refresh_mgr = ctx.refresh_rate_manager().unwrap();
        assert_eq!(refresh_mgr.current_rate(), 120.0);

        let vk_ctx = VulkanContext::new_simulated();
        assert!(ctx
            .render_and_present_layers(
                Time::from_nanos(1),
                &config,
                &vk_ctx,
                None,
                &Default::default()
            )
            .is_ok());

        // Test passthrough in simulated context
        assert!(ctx.is_passthrough_supported());
        assert!(ctx.is_passthrough_enabled());
        assert_eq!(
            ctx.passthrough().blend_mode(),
            EnvironmentBlendMode::ALPHA_BLEND
        );

        ctx.set_passthrough_enabled(false).unwrap();
        assert!(!ctx.is_passthrough_enabled());
        assert_eq!(ctx.passthrough().blend_mode(), EnvironmentBlendMode::OPAQUE);

        // Test spatial screen transform setting
        assert!(ctx.screen_transform().is_none());
        let transform = ScreenTransform::for_display(0);
        ctx.set_screen_transform(transform.clone());
        assert_eq!(ctx.screen_transform(), Some(&transform));
    }
}

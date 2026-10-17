//! Tests of the EVDI backend against the in-memory kernel model (`MockKernel`).
//!
//! The model encodes the rules read from the upstream EVDI sources (see the module docs of
//! `linux_quest_display::backend::kernel_drm::mock`). Passing here proves the backend follows
//! those rules; it does not prove the rules match a real kernel. Nothing here touches real
//! EVDI.

use std::time::Duration;

use linux_quest_display::backend::kernel_drm::abi::{
    self, as_bytes_mut, DrmEvdiConnect, DRM_IOCTL_EVDI_CONNECT, IOC_WRITE,
};
use linux_quest_display::backend::kernel_drm::mock::CommitOutcome;
use linux_quest_display::backend::kernel_drm::{
    scan_evdi_cards, validate_edid, KernelDrmDisplayBackend, KernelOps, KernelProbe, MockKernel,
};
use linux_quest_display::{
    DisplayError, DisplayLayoutMode, EdidGenerator, VirtualDisplayBackend, VirtualDisplayManager,
    VirtualMonitorConfig,
};

const FRAME_WAIT: Duration = Duration::from_secs(2);

fn backend(kernel: &MockKernel) -> KernelDrmDisplayBackend {
    KernelDrmDisplayBackend::with_ops(Box::new(kernel.clone()))
}

fn cfg(id: u32) -> VirtualMonitorConfig {
    VirtualMonitorConfig::preset_1440p(id, 60)
}

/// Connects a bare EDID through the ops API, bypassing the backend (no servicing thread).
fn raw_connect(
    kernel: &MockKernel,
    minor: u32,
) -> std::sync::Arc<dyn linux_quest_display::backend::kernel_drm::CardHandle> {
    let card = kernel
        .open_card(std::path::Path::new(&format!("/dev/dri/card{minor}")))
        .unwrap();
    let edid = EdidGenerator::generate(1920, 1080, 60, "raw", 7).unwrap();
    let mut args = DrmEvdiConnect {
        connected: 1,
        dev_index: minor as i32,
        edid: edid.as_ptr(),
        edid_length: edid.len() as u32,
        pixel_area_limit: 0,
        pixel_per_second_limit: 0,
        _pad: 0,
    };
    // SAFETY: DrmEvdiConnect has explicit padding only.
    let bytes = unsafe { as_bytes_mut(&mut args) };
    card.ioctl(DRM_IOCTL_EVDI_CONNECT, bytes).unwrap();
    card
}

// ---- creation, resolution, lifecycle ------------------------------------------------------

#[tokio::test]
async fn create_adds_a_card_and_connects_the_requested_resolution() {
    let kernel = MockKernel::new();
    let mut b = backend(&kernel);
    assert!(b.is_available());

    let monitor = b.create_monitor(&cfg(1)).await.unwrap();

    assert_eq!(kernel.add_calls(), 1);
    let minor = b.card_index(1).unwrap();
    assert_eq!(monitor.connector, format!("card{minor}-DVI-I-1"));
    assert!(kernel.is_connected(minor));
    // The resolution is carried by the EDID the compositor will parse.
    assert_eq!(kernel.edid_mode(minor), Some((2560, 1440)));
    validate_edid(&kernel.connected_edid(minor).unwrap()).unwrap();
    // 0 = no limit, as libevdi does when there is none.
    assert_eq!(kernel.pixel_limits(minor), Some((0, 0)));
    // Only valid requests reached an evdi node, nothing reached the GPU, nobody wiped cards.
    assert!(kernel.rejected_requests().is_empty());
    assert_eq!(kernel.foreign_private_ioctls(), 0);
    assert_eq!(kernel.remove_all_calls(), 0);
    assert!(!kernel.opened_nodes().contains(&0), "card0 is the GPU");
}

#[tokio::test]
async fn monitor_is_present_while_held_and_gone_after_destroy_but_card_stays() {
    let kernel = MockKernel::new();
    let mut b = backend(&kernel);
    b.create_monitor(&cfg(1)).await.unwrap();
    let minor = b.card_index(1).unwrap();

    // Still present while the handle is held (the old code dropped the fd right after the
    // ioctl, so this was false there).
    assert!(kernel.is_connected(minor));
    assert_eq!(
        kernel.open_fds(minor),
        1,
        "one fd, shared with the painter thread"
    );

    b.destroy_monitor(1).await.unwrap();
    assert!(!kernel.is_connected(minor), "closing the fd disconnects");
    assert_eq!(
        kernel.open_fds(minor),
        0,
        "painter thread joined, fd closed"
    );
    assert!(
        kernel.card_minors().contains(&minor),
        "card is never removed"
    );
    assert_eq!(kernel.remove_all_calls(), 0);
    assert!(b.list_monitors().is_empty());
    assert!(b.destroy_monitor(1).await.is_err());
}

#[tokio::test]
async fn dropping_the_backend_disconnects_every_monitor() {
    let kernel = MockKernel::new();
    let mut b = backend(&kernel);
    b.create_monitor(&cfg(1)).await.unwrap();
    b.create_monitor(&cfg(2)).await.unwrap();
    let minors = [b.card_index(1).unwrap(), b.card_index(2).unwrap()];
    assert!(minors.iter().all(|m| kernel.is_connected(*m)));

    drop(b);

    assert!(minors.iter().all(|m| !kernel.is_connected(*m)));
    assert!(minors.iter().all(|m| kernel.open_fds(*m) == 0));
}

#[tokio::test]
async fn second_create_reuses_the_idle_card() {
    let kernel = MockKernel::new();
    let mut b = backend(&kernel);
    b.create_monitor(&cfg(1)).await.unwrap();
    let first = b.card_index(1).unwrap();
    b.destroy_monitor(1).await.unwrap();

    b.create_monitor(&cfg(1)).await.unwrap();

    assert_eq!(b.card_index(1), Some(first));
    assert_eq!(kernel.add_calls(), 1, "no second `add`");
    assert_eq!(kernel.card_minors().len(), 1);
}

#[tokio::test]
async fn creates_use_distinct_cards_while_held() {
    let kernel = MockKernel::new();
    let mut b = backend(&kernel);
    b.create_monitor(&cfg(1)).await.unwrap();
    b.create_monitor(&cfg(2)).await.unwrap();
    assert_ne!(b.card_index(1), b.card_index(2));
    assert_eq!(kernel.add_calls(), 2);
    assert_eq!(kernel.double_connects(), 0);
}

#[tokio::test]
async fn preexisting_idle_card_is_used_without_add() {
    let kernel = MockKernel::new().with_idle_card();
    let mut b = backend(&kernel);
    b.create_monitor(&cfg(1)).await.unwrap();
    assert_eq!(kernel.add_calls(), 0);
}

#[tokio::test]
async fn never_takes_over_a_card_connected_by_another_app() {
    let kernel = MockKernel::new().with_card_in_use_by_other_app();
    let other = kernel.card_minors()[0];
    let mut b = backend(&kernel);
    b.create_monitor(&cfg(1)).await.unwrap();

    assert_ne!(b.card_index(1), Some(other));
    assert_eq!(
        kernel.double_connects(),
        0,
        "would have hijacked the other app"
    );
    assert!(kernel.is_connected(other));
    assert!(!kernel.opened_nodes().contains(&other));
}

#[tokio::test]
async fn never_touches_usb_attached_cards() {
    let kernel = MockKernel::new().with_usb_card();
    let usb = kernel.card_minors()[0];
    let mut b = backend(&kernel);
    b.create_monitor(&cfg(1)).await.unwrap();
    assert_ne!(b.card_index(1), Some(usb));
    assert!(!kernel.opened_nodes().contains(&usb));
}

#[tokio::test]
async fn unreadable_status_is_not_treated_as_idle() {
    // Root, so `add` works. A card with unknown status must not be hijacked.
    let kernel = MockKernel::new().with_idle_card().with_unreadable_status();
    let existing = kernel.card_minors()[0];
    let mut b = backend(&kernel);
    b.create_monitor(&cfg(1)).await.unwrap();
    assert_ne!(b.card_index(1), Some(existing));
    assert_eq!(kernel.add_calls(), 1);
}

// ---- asynchronous add, permissions ---------------------------------------------------------

#[tokio::test]
async fn waits_for_node_and_udev_permissions_after_add() {
    // sysfs entry immediately, node after 100ms, permissions 300ms later.
    let kernel = MockKernel::new().with_add_delays(0, 100, 300);
    let mut b = backend(&kernel);
    b.create_monitor(&cfg(1)).await.unwrap();
    assert!(kernel.is_connected(b.card_index(1).unwrap()));
}

#[tokio::test]
async fn sysfs_entry_appearing_late_is_found_by_polling() {
    let kernel = MockKernel::new().with_add_delays(500, 0, 0);
    let mut b = backend(&kernel);
    b.create_monitor(&cfg(1)).await.unwrap();
    assert!(kernel.is_connected(b.card_index(1).unwrap()));
}

#[tokio::test]
async fn gives_up_after_the_bounded_wait_and_keeps_the_card_for_reuse() {
    let kernel = MockKernel::new().with_add_delays(0, 5_000, 0);
    let mut b = backend(&kernel);
    let err = b.create_monitor(&cfg(1)).await.unwrap_err().to_string();
    assert!(err.contains("2000ms"), "{err}");
    assert!(err.contains("never removed individually"), "{err}");
    assert_eq!(kernel.add_calls(), 1);
    assert_eq!(kernel.card_minors().len(), 1, "added card is left in place");
    assert_eq!(kernel.remove_all_calls(), 0);
}

#[tokio::test]
async fn permission_denied_on_new_card_is_diagnosed_with_the_fix() {
    let kernel = MockKernel::new()
        .with_add_delays(0, 10, 10)
        .with_new_cards_denied();
    let mut b = backend(&kernel);
    let err = b.create_monitor(&cfg(1)).await.unwrap_err().to_string();
    assert!(err.contains("permission denied"), "{err}");
    assert!(err.contains("usermod -aG video"), "{err}");
    assert_eq!(kernel.remove_all_calls(), 0);
}

#[tokio::test]
async fn module_device_limit_is_reported_and_remove_all_is_never_used() {
    let kernel = MockKernel::new().with_all_slots_busy();
    let mut b = backend(&kernel);
    let err = b.create_monitor(&cfg(1)).await.unwrap_err().to_string();
    assert!(err.contains("at most 16"), "{err}");
    assert!(err.contains("remove_all"), "{err}");
    assert_eq!(kernel.remove_all_calls(), 0);
    assert_eq!(
        kernel.card_minors().len(),
        16,
        "other apps' cards untouched"
    );
}

// ---- availability and diagnostics ----------------------------------------------------------

#[test]
fn module_not_loaded_is_unavailable_with_install_and_modprobe_hints() {
    let kernel = MockKernel::new().without_module();
    let b = backend(&kernel);
    assert!(!b.is_available());
    let text = b.diagnostics().join("\n");
    assert!(text.contains("apt install evdi-dkms"), "{text}");
    assert!(text.contains("sudo modprobe evdi"), "{text}");
}

#[test]
fn unprivileged_without_cards_is_unavailable_and_explains_initial_device_count() {
    let kernel = MockKernel::new().with_euid(1000);
    let b = backend(&kernel);
    assert!(!b.is_available());
    let text = b.diagnostics().join("\n");
    assert!(text.contains("initial_device_count"), "{text}");
    assert!(text.contains("not writable by uid 1000"), "{text}");
}

#[tokio::test]
async fn unprivileged_with_an_accessible_idle_card_is_available_and_needs_no_add() {
    let kernel = MockKernel::new().with_euid(1000).with_idle_card();
    let mut b = backend(&kernel);
    assert!(b.is_available(), "{:?}", b.diagnostics());
    b.create_monitor(&cfg(1)).await.unwrap();
    assert_eq!(kernel.add_calls(), 0);

    // The only card is now held: a second monitor cannot be created and says why.
    assert!(matches!(
        b.create_monitor(&cfg(2)).await,
        Err(DisplayError::BackendUnavailable(_))
    ));
}

#[test]
fn idle_card_without_group_access_gets_the_group_hint() {
    let kernel = MockKernel::new().with_euid(1000).with_idle_card_denied();
    let b = backend(&kernel);
    assert!(!b.is_available());
    let text = b.diagnostics().join("\n");
    assert!(text.contains("usermod -aG video"), "{text}");
    assert!(text.contains("udev"), "{text}");
}

#[test]
fn probe_distinguishes_busy_usb_and_idle_cards() {
    let kernel = MockKernel::new()
        .with_idle_card()
        .with_usb_card()
        .with_card_in_use_by_other_app();
    let cards = scan_evdi_cards(&kernel);
    assert_eq!(cards.len(), 3);
    assert!(cards[0].is_reusable() && cards[0].accessible);
    assert!(cards[1].usb_attached && !cards[1].is_reusable());
    assert_eq!(cards[2].connected, Some(true));
    assert!(!cards[2].is_reusable());
    assert!(KernelProbe::scan(&kernel).is_ready());
}

// ---- EDID validation -----------------------------------------------------------------------

#[tokio::test]
async fn bad_edids_are_rejected_before_touching_the_kernel() {
    let kernel = MockKernel::new();
    let mut b = backend(&kernel);

    let mut bad_checksum = EdidGenerator::generate(1920, 1080, 60, "x", 1).unwrap();
    bad_checksum[20] ^= 0xFF;
    let truncated = EdidGenerator::generate(1920, 1080, 60, "x", 1).unwrap()[..100].to_vec();
    let mut bad_header = EdidGenerator::generate(1920, 1080, 60, "x", 1).unwrap();
    bad_header[0] = 0x55;
    let oversize = vec![0u8; 129 * 256];

    for bad in [bad_checksum, truncated, bad_header, oversize] {
        let config = cfg(1).with_edid(bad);
        assert!(matches!(
            b.create_monitor(&config).await,
            Err(DisplayError::Edid(_))
        ));
    }
    assert_eq!(kernel.open_calls(), 0);
    assert_eq!(kernel.add_calls(), 0);
}

#[test]
fn every_edid_block_checksum_is_validated() {
    let mut two_blocks = EdidGenerator::generate(1920, 1080, 60, "x", 1).unwrap();
    two_blocks.extend_from_slice(&[0u8; 127]);
    two_blocks.push(0x01); // extension block with a bad checksum
    assert!(validate_edid(&two_blocks).is_err());
    let last = two_blocks.len() - 1;
    two_blocks[last] = 0;
    assert!(validate_edid(&two_blocks).is_ok());
}

// ---- ABI enforcement by the model ---------------------------------------------------------

#[test]
fn model_rejects_wrong_ioctl_requests_with_enotty() {
    let kernel = MockKernel::new().with_idle_card();
    let minor = kernel.card_minors()[0];
    let card = kernel
        .open_card(std::path::Path::new(&format!("/dev/dri/card{minor}")))
        .unwrap();
    let mut buf = [0u8; 32];

    // Old bug: command index 0x02 (GRABPIX) with the connect struct size.
    let wrong_index = abi::iowr(abi::DRM_IOCTL_BASE, abi::DRM_COMMAND_BASE + 2, 32);
    // Right index, wrong direction (write only).
    let wrong_dir = abi::ioc(IOC_WRITE, abi::DRM_IOCTL_BASE, abi::DRM_COMMAND_BASE, 32);
    for request in [wrong_index, wrong_dir] {
        let err = card.ioctl(request, &mut buf).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ENOTTY), "{request:#x}");
    }
    // Right request, wrong argument length: the old 24-byte struct without limit fields.
    let err = card
        .ioctl(DRM_IOCTL_EVDI_CONNECT, &mut buf[..24])
        .unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EINVAL));
    assert_eq!(kernel.rejected_requests().len(), 2);
}

#[test]
fn model_refuses_evdi_ioctls_on_other_drivers() {
    let kernel = MockKernel::new();
    let gpu = kernel
        .open_card(std::path::Path::new("/dev/dri/card0"))
        .unwrap();
    let mut buf = [0u8; 32];
    assert!(gpu.ioctl(DRM_IOCTL_EVDI_CONNECT, &mut buf).is_err());
    assert_eq!(kernel.foreign_private_ioctls(), 1);
}

#[test]
fn model_edid_length_rules() {
    let kernel = MockKernel::new().with_idle_card();
    let minor = kernel.card_minors()[0];
    let card = kernel
        .open_card(std::path::Path::new(&format!("/dev/dri/card{minor}")))
        .unwrap();
    let edid = EdidGenerator::generate(1920, 1080, 60, "x", 1).unwrap();
    let send = |ptr: *const u8, len: u32| {
        let mut args = DrmEvdiConnect {
            connected: 1,
            dev_index: 0,
            edid: ptr,
            edid_length: len,
            pixel_area_limit: 0,
            pixel_per_second_limit: 0,
            _pad: 0,
        };
        // SAFETY: explicit padding only.
        let bytes = unsafe { as_bytes_mut(&mut args) };
        card.ioctl(DRM_IOCTL_EVDI_CONNECT, bytes)
    };
    assert_eq!(
        send(edid.as_ptr(), 64).unwrap_err().raw_os_error(),
        Some(libc::EINVAL)
    );
    assert_eq!(
        send(edid.as_ptr(), 256 * 128 + 1)
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EINVAL)
    );
    assert_eq!(
        send(std::ptr::null(), 128).unwrap_err().raw_os_error(),
        Some(libc::EFAULT)
    );
    assert!(send(edid.as_ptr(), 128).is_ok());
    assert!(kernel.is_connected(minor));
}

#[test]
fn model_invalid_edid_connects_but_yields_no_modes() {
    // Like the real module: connect only checks length; the connector then finds no modes.
    let kernel = MockKernel::new().with_idle_card();
    let minor = kernel.card_minors()[0];
    let card = kernel
        .open_card(std::path::Path::new(&format!("/dev/dri/card{minor}")))
        .unwrap();
    let mut edid = EdidGenerator::generate(1920, 1080, 60, "x", 1).unwrap();
    edid[10] ^= 0x55;
    let mut args = DrmEvdiConnect {
        connected: 1,
        dev_index: 0,
        edid: edid.as_ptr(),
        edid_length: 128,
        pixel_area_limit: 0,
        pixel_per_second_limit: 0,
        _pad: 0,
    };
    // SAFETY: explicit padding only.
    card.ioctl(DRM_IOCTL_EVDI_CONNECT, unsafe { as_bytes_mut(&mut args) })
        .unwrap();
    assert!(kernel.is_connected(minor));
    assert_eq!(
        kernel.compositor_commit(minor, 1920, 1080),
        CommitOutcome::NoModes
    );
}

#[test]
fn model_closing_the_connecting_fd_disconnects_but_other_fds_do_not() {
    let kernel = MockKernel::new().with_idle_card();
    let minor = kernel.card_minors()[0];
    let owner = raw_connect(&kernel, minor);
    let bystander = kernel
        .open_card(std::path::Path::new(&format!("/dev/dri/card{minor}")))
        .unwrap();
    drop(bystander);
    assert!(kernel.is_connected(minor), "closing another fd is harmless");

    // `connected = 0` from a different fd is EFAULT in the module.
    let other = kernel
        .open_card(std::path::Path::new(&format!("/dev/dri/card{minor}")))
        .unwrap();
    let mut args = DrmEvdiConnect {
        connected: 0,
        dev_index: 0,
        edid: std::ptr::null(),
        edid_length: 0,
        pixel_area_limit: 0,
        pixel_per_second_limit: 0,
        _pad: 0,
    };
    // SAFETY: explicit padding only.
    let err = other
        .ioctl(DRM_IOCTL_EVDI_CONNECT, unsafe { as_bytes_mut(&mut args) })
        .unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EFAULT));
    drop(other);
    assert!(kernel.is_connected(minor));

    drop(owner);
    assert!(!kernel.is_connected(minor));
}

#[test]
fn model_double_connect_silently_steals_the_card() {
    let kernel = MockKernel::new().with_idle_card();
    let minor = kernel.card_minors()[0];
    let first = raw_connect(&kernel, minor);
    let second = raw_connect(&kernel, minor);
    assert_eq!(kernel.double_connects(), 1);
    drop(first);
    assert!(kernel.is_connected(minor), "first holder no longer owns it");
    drop(second);
    assert!(!kernel.is_connected(minor));
}

#[tokio::test]
async fn card_whose_node_is_another_driver_never_receives_evdi_ioctls() {
    // sysfs says evdi, but the node turns out to be i915 (a race or a bad sysfs read).
    let kernel = MockKernel::new().with_idle_card();
    let minor = kernel.card_minors()[0];
    let kernel = kernel.with_card_driver(minor, "i915");
    let mut b = backend(&kernel);

    // The single idle card is skipped with an explanation; `add` then creates a good one.
    b.create_monitor(&cfg(1)).await.unwrap();
    assert_eq!(kernel.foreign_private_ioctls(), 0);
    assert_ne!(b.card_index(1), Some(minor));
    assert!(!kernel.is_connected(minor));
}

// ---- event servicing -----------------------------------------------------------------------

#[tokio::test]
async fn serviced_output_keeps_the_compositor_flipping() {
    let kernel = MockKernel::new();
    let mut b = backend(&kernel);
    b.create_monitor(&cfg(1)).await.unwrap();
    let minor = b.card_index(1).unwrap();

    for frame in 0..30 {
        assert!(
            kernel.commit_and_wait(minor, 2560, 1440, FRAME_WAIT),
            "page flip {frame} never completed: the output would stall"
        );
    }

    let stats = b.painter_stats(1).unwrap();
    assert_eq!(stats.mode, Some((2560, 1440)));
    assert!(stats.mode_changes >= 1);
    assert!(stats.grabs >= 29, "{stats:?}");
    // Every GRABPIX used a buffer matching the mode, 4 bytes per pixel, and MAX_DIRTS rects.
    let grabs = kernel.grab_requests();
    assert!(!grabs.is_empty());
    assert!(
        grabs.iter().all(|g| *g == (2560, 1440, 2560 * 4, 16)),
        "{grabs:?}"
    );
}

#[tokio::test]
async fn unserviced_output_stalls_the_compositor_which_is_why_servicing_exists() {
    // Model check: connect without any painter thread, as the old code effectively did.
    let kernel = MockKernel::new().with_idle_card();
    let minor = kernel.card_minors()[0];
    let _held = raw_connect(&kernel, minor);
    assert!(
        !kernel.commit_and_wait(minor, 1920, 1080, Duration::from_millis(150)),
        "first flip must stay pending without grabpix"
    );
}

#[tokio::test]
async fn painter_survives_the_compositor_connecting_before_and_after_modesets() {
    let kernel = MockKernel::new();
    let mut b = backend(&kernel);
    b.create_monitor(&cfg(1)).await.unwrap();
    let minor = b.card_index(1).unwrap();
    // Wrong mode for the EDID is refused by the compositor model, not by the painter.
    assert_eq!(
        kernel.compositor_commit(minor, 1280, 720),
        CommitOutcome::ModeNotInEdid
    );
    assert!(kernel.commit_and_wait(minor, 2560, 1440, FRAME_WAIT));
    b.destroy_monitor(1).await.unwrap();
    assert_eq!(
        kernel.compositor_commit(minor, 2560, 1440),
        CommitOutcome::NotConnected
    );
    // Reconnect: a fresh MODE_CHANGED is delivered and servicing resumes.
    b.create_monitor(&cfg(1)).await.unwrap();
    assert!(kernel.commit_and_wait(minor, 2560, 1440, FRAME_WAIT));
    assert_eq!(b.painter_stats(1).unwrap().mode, Some((2560, 1440)));
}

// ---- through the manager -------------------------------------------------------------------

#[tokio::test]
async fn manager_provisions_scales_and_reuses_cards() {
    let kernel = MockKernel::new();
    let mut manager = VirtualDisplayManager::new(Box::new(backend(&kernel)));
    let base = VirtualMonitorConfig::preset_1080p(1, 60);

    let monitors = manager
        .set_monitor_count(2, &base, DisplayLayoutMode::Horizontal)
        .await
        .unwrap();
    assert_eq!(monitors.len(), 2);
    assert_eq!(monitors[1].x, 1920, "horizontal layout");
    let minors = kernel.card_minors();
    assert_eq!(minors.len(), 2);
    for m in &minors {
        assert_eq!(kernel.edid_mode(*m), Some((1920, 1080)));
    }

    // Scale down: highest id disconnects, its card stays and is idle.
    manager
        .set_monitor_count(1, &base, DisplayLayoutMode::Horizontal)
        .await
        .unwrap();
    assert_eq!(
        minors.iter().filter(|m| kernel.is_connected(**m)).count(),
        1
    );
    assert_eq!(kernel.card_minors().len(), 2);

    // Resolution change destroys and recreates: the idle card is reused, nothing added.
    let adds = kernel.add_calls();
    manager
        .set_monitor_count(
            2,
            &VirtualMonitorConfig::preset_1440p(1, 60),
            DisplayLayoutMode::Horizontal,
        )
        .await
        .unwrap();
    assert_eq!(kernel.add_calls(), adds, "cards are reused, not re-added");
    for m in &minors {
        assert_eq!(kernel.edid_mode(*m), Some((2560, 1440)));
    }
    assert_eq!(kernel.remove_all_calls(), 0);

    drop(manager);
    assert!(minors.iter().all(|m| !kernel.is_connected(*m)));
}

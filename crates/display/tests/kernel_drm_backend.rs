use std::path::PathBuf;

use linux_quest_display::backend::kernel_drm::{KernelDrmDisplayBackend, MockKernel};
use linux_quest_display::{
    DisplayLayoutMode, VirtualDisplayBackend, VirtualDisplayManager, VirtualMonitorConfig,
};

fn vkms_host() -> MockKernel {
    MockKernel::new()
        .with_path("/dev/dri")
        .with_path("/sys/module/vkms")
        .with_path("/sys/kernel/config/vkms")
        .with_euid(0)
}

#[tokio::test]
async fn manager_provisions_outputs_with_edids_through_kernel_backend() {
    let kernel = vkms_host();
    let backend = KernelDrmDisplayBackend::with_ops(Box::new(kernel.clone()));
    assert!(backend.is_available());
    let mut manager = VirtualDisplayManager::new(Box::new(backend));

    let base = VirtualMonitorConfig::preset_1080p(1, 60);
    let monitors = manager
        .set_monitor_count(2, &base, DisplayLayoutMode::Horizontal)
        .await
        .expect("two VKMS outputs should be provisioned");

    assert_eq!(monitors.len(), 2);
    assert_eq!(monitors[0].connector, "VIRTUAL-1");
    assert_eq!(monitors[1].connector, "VIRTUAL-2");
    assert_eq!(
        monitors[1].x, 1920,
        "horizontal layout places monitor 2 to the right"
    );

    let edid_writes: Vec<_> = kernel
        .writes()
        .into_iter()
        .filter(|(path, _)| path.ends_with("edid"))
        .collect();
    assert_eq!(edid_writes.len(), 2);
    for (path, bytes) in &edid_writes {
        assert_eq!(bytes.len(), 128, "{}", path.display());
        assert_eq!(
            &bytes[..8],
            &[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]
        );
    }

    let monitors = manager
        .set_monitor_count(1, &base, DisplayLayoutMode::Horizontal)
        .await
        .expect("scaling down should destroy the highest id");
    assert_eq!(monitors.len(), 1);
    assert!(!kernel.has_path("/sys/kernel/config/vkms/linux-quest-2"));
    assert!(kernel.has_path("/sys/kernel/config/vkms/linux-quest-1"));
    assert!(kernel.writes().contains(&(
        PathBuf::from("/sys/kernel/config/vkms/linux-quest-2/enabled"),
        b"0".to_vec()
    )));
}

#[tokio::test]
async fn kernel_backend_unavailable_surfaces_diagnostics() {
    let backend = KernelDrmDisplayBackend::with_ops(Box::new(
        MockKernel::new().with_path("/dev/dri").with_euid(1000),
    ));
    assert!(!backend.is_available());
    assert!(backend
        .diagnostics()
        .iter()
        .any(|d| d.contains("sudo modprobe vkms")));
}

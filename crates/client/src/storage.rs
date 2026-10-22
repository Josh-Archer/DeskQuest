//! Where the headset keeps its settings and screen layout between launches.
//!
//! On Android this is the app's internal files directory (`Context.getFilesDir()`, which
//! NativeActivity exposes as `internalDataPath`), for example
//! `/data/user/0/com.linuxquest.client/files`. It needs no storage permission and is removed
//! with the app.

use std::path::{Path, PathBuf};

use crate::config::ClientConfig;
use crate::error::ClientResult;
use crate::runtime::QuestClientRuntime;
use crate::settings::SettingsStore;

/// File name of the persisted [`crate::settings::ClientSettings`].
pub const SETTINGS_FILE: &str = "settings.json";
/// File name of the persisted [`crate::spatial::SpatialScreenLayout`].
pub const LAYOUT_FILE: &str = "spatial_layout.json";

/// The Android package, used to derive the files directory when the activity gives none.
pub const ANDROID_PACKAGE: &str = "com.linuxquest.client";

/// The directory holding the client's persisted state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientStorage {
    dir: PathBuf,
}

impl ClientStorage {
    /// Uses `dir`, creating it if needed so the first save cannot fail on a missing parent.
    pub fn open(dir: impl Into<PathBuf>) -> ClientResult<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// The storage for an Android app: the activity's internal files directory when it has one,
    /// else the standard `/data/user/0/<package>/files`.
    pub fn for_android(internal_data_path: Option<PathBuf>) -> ClientResult<Self> {
        Self::open(android_files_dir(internal_data_path))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn settings_path(&self) -> PathBuf {
        self.dir.join(SETTINGS_FILE)
    }

    pub fn layout_path(&self) -> PathBuf {
        self.dir.join(LAYOUT_FILE)
    }

    /// Points `config` at the layout file, so a runtime built from it loads the saved layout
    /// and saves changes to it.
    pub fn configure(&self, config: &mut ClientConfig) {
        config.spatial_layout_path = Some(self.layout_path());
    }

    /// Loads the saved settings into `runtime` and saves its later changes.
    pub fn attach(&self, runtime: &mut QuestClientRuntime) {
        runtime.set_settings_store(SettingsStore::new(self.settings_path()));
    }
}

/// The files directory: `internal_data_path` when it is set and not empty, else
/// `/data/user/0/<package>/files`.
pub fn android_files_dir(internal_data_path: Option<PathBuf>) -> PathBuf {
    internal_data_path
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from(format!("/data/user/0/{ANDROID_PACKAGE}/files")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::SpatialLayoutStore;
    use linux_quest_protocol::HeadsetSettings;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lq-storage-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn android_storage_uses_the_internal_files_dir() {
        assert_eq!(
            android_files_dir(Some("/data/user/0/com.linuxquest.client/files".into())),
            PathBuf::from("/data/user/0/com.linuxquest.client/files")
        );
        assert_eq!(
            android_files_dir(None),
            PathBuf::from("/data/user/0/com.linuxquest.client/files")
        );
        assert_eq!(
            android_files_dir(Some(PathBuf::new())),
            PathBuf::from("/data/user/0/com.linuxquest.client/files")
        );
    }

    #[test]
    fn storage_paths_live_in_the_files_dir_and_the_dir_is_created() {
        let dir = temp_dir("paths").join("files");
        let storage = ClientStorage::for_android(Some(dir.clone())).unwrap();
        assert!(dir.is_dir());
        assert_eq!(storage.settings_path(), dir.join("settings.json"));
        assert_eq!(storage.layout_path(), dir.join("spatial_layout.json"));
        let mut config = ClientConfig::default();
        storage.configure(&mut config);
        assert_eq!(
            config.spatial_layout_path,
            Some(dir.join("spatial_layout.json"))
        );
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    /// A layout and settings changed by the host survive a restart: both files are written,
    /// and a new runtime built the way `android_main` builds one starts with them.
    #[test]
    fn host_changes_survive_a_restart() {
        use crate::host_control::HostCommand;
        let dir = temp_dir("restart");
        let storage = ClientStorage::open(&dir).unwrap();

        let start = |storage: &ClientStorage| {
            let mut config = ClientConfig::default();
            storage.configure(&mut config);
            let mut rt = QuestClientRuntime::new(config).unwrap();
            storage.attach(&mut rt);
            rt
        };

        let mut rt = start(&storage);
        let status = rt.headset_status();
        let current = *status.screen(0).unwrap();
        let yaw = -15f32.to_radians();
        let turned = linux_quest_protocol::ScreenPlacement::facing_user(
            0,
            yaw,
            0.0,
            current.horizontal_distance_m(),
            current.position[1],
            current.width_m,
            current.height_m,
            current.curve,
        );
        rt.apply_host_command(HostCommand::SetLayout(vec![turned]));
        let wanted = HeadsetSettings {
            hud: false,
            ..status.settings
        };
        rt.apply_host_command(HostCommand::SetSettings(wanted));
        let before = rt.headset_status();
        drop(rt);

        assert!(storage.layout_path().is_file(), "layout was not written");
        assert!(
            storage.settings_path().is_file(),
            "settings were not written"
        );
        assert!(
            !SettingsStore::new(storage.settings_path())
                .load()
                .unwrap()
                .hud_enabled
        );
        assert!(SpatialLayoutStore::new(storage.layout_path())
            .load()
            .unwrap()
            .get(0)
            .is_some());

        let rt = start(&storage);
        let after = rt.headset_status();
        assert!(!after.settings.hud, "HUD setting lost on restart");
        assert_eq!(after.settings, before.settings);
        let (a, b) = (after.screen(0).unwrap(), before.screen(0).unwrap());
        assert!(
            (a.yaw_rad() - b.yaw_rad()).abs() < 1e-3,
            "rotation lost on restart: {} vs {}",
            a.yaw_rad(),
            b.yaw_rad()
        );
        assert!((a.yaw_rad() - yaw).abs() < 1e-2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

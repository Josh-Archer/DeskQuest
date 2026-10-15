//! Persistent multi-monitor spatial layout storage with JSON serialization and versioning.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{ClientError, ClientResult};
use crate::spatial::transform::ScreenTransform;

/// Current spatial screen layout schema version.
pub const CURRENT_LAYOUT_VERSION: u32 = 1;

/// Versioned spatial layout configuration keyed by display ID.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpatialScreenLayout {
    /// Schema version for forward/backward compatibility.
    pub version: u32,

    /// Screen transforms keyed by display ID.
    pub screens: HashMap<u32, ScreenTransform>,

    /// Optional layout profile name (e.g. "Default", "Desk", "Triple-Monitor").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_name: Option<String>,
}

impl Default for SpatialScreenLayout {
    fn default() -> Self {
        let mut screens = HashMap::new();
        screens.insert(0, ScreenTransform::for_display(0));

        Self {
            version: CURRENT_LAYOUT_VERSION,
            screens,
            profile_name: Some("default".to_string()),
        }
    }
}

impl SpatialScreenLayout {
    /// Creates a new layout with the specified profile name.
    pub fn new(profile_name: impl Into<String>) -> Self {
        Self {
            version: CURRENT_LAYOUT_VERSION,
            screens: HashMap::new(),
            profile_name: Some(profile_name.into()),
        }
    }

    /// Retrieves the screen transform for a display ID, or creates a default one if not found.
    pub fn get_or_create(&mut self, display_id: u32) -> &mut ScreenTransform {
        self.screens
            .entry(display_id)
            .or_insert_with(|| ScreenTransform::for_display(display_id))
    }

    /// Gets the screen transform for a display ID if present.
    pub fn get(&self, display_id: u32) -> Option<&ScreenTransform> {
        self.screens.get(&display_id)
    }

    /// Inserts or replaces a screen transform for a display ID.
    pub fn insert(&mut self, display_id: u32, transform: ScreenTransform) {
        self.screens.insert(display_id, transform);
    }

    /// Removes a display ID from the layout.
    pub fn remove(&mut self, display_id: u32) -> Option<ScreenTransform> {
        self.screens.remove(&display_id)
    }

    /// Returns the number of screens configured in this layout.
    pub fn len(&self) -> usize {
        self.screens.len()
    }

    /// Returns true if the layout contains no screens.
    pub fn is_empty(&self) -> bool {
        self.screens.is_empty()
    }
}

/// Persistent storage manager for saving and restoring spatial layouts to disk.
#[derive(Debug, Clone)]
pub struct SpatialLayoutStore {
    path: PathBuf,
}

impl SpatialLayoutStore {
    /// Creates a new layout store pointing to the specified file path.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Returns the path to the layout JSON file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Saves the spatial layout atomically to the configured path.
    ///
    /// Writes to a temporary file in the same directory first, then renames it
    /// to guarantee atomicity and prevent corruption if interrupted.
    pub fn save(&self, layout: &SpatialScreenLayout) -> ClientResult<()> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let json = serde_json::to_string_pretty(layout)?;
        let tmp_path = self
            .path
            .with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&tmp_path, json.as_bytes())?;
        std::fs::rename(&tmp_path, &self.path)?;
        Ok(())
    }

    /// Loads the spatial layout from the configured path.
    ///
    /// # Errors
    /// Returns `ClientError::Io` if the file cannot be read,
    /// or `ClientError::Spatial` / `ClientError::Config` if the JSON is malformed
    /// or the schema version is incompatible.
    pub fn load(&self) -> ClientResult<SpatialScreenLayout> {
        let content = std::fs::read_to_string(&self.path)?;
        let layout: SpatialScreenLayout = serde_json::from_str(&content)?;

        if layout.version == 0 || layout.version > CURRENT_LAYOUT_VERSION {
            return Err(ClientError::Config(format!(
                "Unsupported spatial layout version {}: expected version 1..={}",
                layout.version, CURRENT_LAYOUT_VERSION
            )));
        }

        Ok(layout)
    }

    /// Loads the spatial layout with robust fallback.
    ///
    /// If the file is missing or contains corrupt/invalid JSON, this logs a warning,
    /// preserves a quarantined backup of the corrupted file, and returns a safe default layout.
    pub fn load_or_default(&self) -> SpatialScreenLayout {
        if !self.path.exists() {
            tracing::info!(path = ?self.path, "Spatial layout file does not exist, using default layout");
            return SpatialScreenLayout::default();
        }

        match self.load() {
            Ok(layout) => layout,
            Err(e) => {
                tracing::warn!(
                    path = ?self.path,
                    error = ?e,
                    "Spatial layout file corrupt or invalid; quarantining backup and using default layout"
                );

                // Preserve corrupt file with unique timestamp suffix
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                let backup_path = self.path.with_extension(format!("corrupt.{ts}.bak"));
                let _ = std::fs::copy(&self.path, &backup_path);

                SpatialScreenLayout::default()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestTempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn test_spatial_layout_save_and_load_roundtrip() {
        let dir = TestTempDir::new("linux_quest_layout_test");
        let path = dir.path().join("layout.json");
        let store = SpatialLayoutStore::new(&path);

        let mut layout = SpatialScreenLayout::new("Work");
        layout.insert(0, ScreenTransform::for_display(0));
        layout.insert(1, ScreenTransform::for_display(1));
        assert_eq!(layout.len(), 2);

        store.save(&layout).expect("Save failed");
        assert!(path.exists());

        let loaded = store.load().expect("Load failed");
        assert_eq!(loaded.version, CURRENT_LAYOUT_VERSION);
        assert_eq!(loaded.profile_name, Some("Work".to_string()));
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.get(0).unwrap().yaw, 0.0);
        assert!(loaded.get(1).unwrap().yaw < 0.0);
    }

    #[test]
    fn test_load_nonexistent_returns_io_error() {
        let store = SpatialLayoutStore::new("/path/that/does/not/exist/layout.json");
        assert!(store.load().is_err());
    }

    #[test]
    fn test_load_or_default_missing_file() {
        let dir = TestTempDir::new("linux_quest_missing_test");
        let path = dir.path().join("nonexistent_layout.json");
        let store = SpatialLayoutStore::new(&path);

        let layout = store.load_or_default();
        assert_eq!(layout.version, CURRENT_LAYOUT_VERSION);
        assert_eq!(layout.len(), 1);
        assert!(layout.get(0).is_some());
    }

    #[test]
    fn test_load_or_default_corrupt_file_recovery() {
        let dir = TestTempDir::new("linux_quest_corrupt_test");
        let path = dir.path().join("corrupt_layout.json");

        // Write corrupt JSON
        std::fs::write(&path, b"{ \"version\": 1, \"screens\": INVALID_JSON").unwrap();

        let store = SpatialLayoutStore::new(&path);
        assert!(store.load().is_err());

        // load_or_default should not panic, but return default and create a backup
        let layout = store.load_or_default();
        assert_eq!(layout.version, CURRENT_LAYOUT_VERSION);
        assert_eq!(layout.len(), 1);

        // Verify that a corrupt backup file was created in the directory
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert!(entries
            .iter()
            .any(|name| name.contains("corrupt") && name.ends_with(".bak")));
    }

    #[test]
    fn test_load_unsupported_version_rejected() {
        let dir = TestTempDir::new("linux_quest_ver_test");
        let path = dir.path().join("future_layout.json");

        let json = r#"{
            "version": 999,
            "screens": {},
            "profile_name": "future"
        }"#;
        std::fs::write(&path, json).unwrap();

        let store = SpatialLayoutStore::new(&path);
        let res = store.load();
        assert!(res.is_err());
    }
}

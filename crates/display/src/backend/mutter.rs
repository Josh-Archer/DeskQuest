use async_trait::async_trait;
use std::collections::HashMap;
use tracing::info;

use super::{VirtualDisplayBackend, VirtualMonitor};
use crate::config::VirtualMonitorConfig;
use crate::error::DisplayError;
use crate::layout::{DisplayLayout, DisplayLayoutMode};

/// GNOME Mutter virtual monitor backend via D-Bus (`org.gnome.Mutter.ScreenCast` & `org.gnome.Mutter.DisplayConfig`).
pub struct MutterDisplayBackend {
    connection: Option<zbus::Connection>,
    monitors: HashMap<u32, VirtualMonitor>,
    session_paths: HashMap<u32, (String, String)>, // id -> (session_path, stream_path)
    is_available: bool,
}

impl MutterDisplayBackend {
    pub fn new() -> Self {
        let is_avail = Self::check_quick_availability();
        Self {
            connection: None,
            monitors: HashMap::new(),
            session_paths: HashMap::new(),
            is_available: is_avail,
        }
    }

    fn check_quick_availability() -> bool {
        if let Ok(desktop) = std::env::var("XDG_CURRENT_DESKTOP") {
            if desktop.to_lowercase().contains("gnome") {
                return true;
            }
        }
        if let Ok(session) = std::env::var("DESKTOP_SESSION") {
            if session.to_lowercase().contains("gnome") {
                return true;
            }
        }
        false
    }

    /// Verifies that GNOME Mutter ScreenCast is actively registered and owned on the session bus.
    pub async fn probe_screencast_service(conn: &zbus::Connection) -> bool {
        let dbus_proxy = match zbus::Proxy::new(
            conn,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
        )
        .await
        {
            Ok(p) => p,
            Err(_) => return false,
        };

        let has_owner: Result<String, _> = dbus_proxy
            .call("GetNameOwner", &("org.gnome.Mutter.ScreenCast",))
            .await;

        has_owner.is_ok()
    }
}

// GDBus reports an expired object as UnknownMethod, while other implementations
// use UnknownObject. Do not hide permission errors or genuinely missing methods.
fn session_object_gone(name: &str, detail: Option<&str>) -> bool {
    name == "org.freedesktop.DBus.Error.UnknownObject"
        || (name == "org.freedesktop.DBus.Error.UnknownMethod"
            && detail.is_some_and(|d| d.contains("Object does not exist at path")))
}

impl Default for MutterDisplayBackend {
    fn default() -> Self {
        Self::new()
    }
}

// Mutter D-Bus type signatures:
// GetCurrentState: (ua((ssss)a(siiddada{sv})a{sv})a(iiduba(ssss)a{sv})a{sv})
// ApplyMonitorsConfig: (uua(iiduba(ssa{sv}))a{sv})
type MutterMonitorSpec = (String, String, String, String);
type MutterMode = (
    String,
    i32,
    i32,
    f64,
    f64,
    Vec<f64>,
    HashMap<String, zbus::zvariant::OwnedValue>,
);
type MutterMonitor = (
    MutterMonitorSpec,
    Vec<MutterMode>,
    HashMap<String, zbus::zvariant::OwnedValue>,
);
type MutterLogicalMonitorState = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<(String, String, String, String)>,
    HashMap<String, zbus::zvariant::OwnedValue>,
);
type MutterCurrentState = (
    u32,
    Vec<MutterMonitor>,
    Vec<MutterLogicalMonitorState>,
    HashMap<String, zbus::zvariant::OwnedValue>,
);

type MutterLogicalMonitorApply = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<(
        String,
        String,
        HashMap<String, zbus::zvariant::Value<'static>>,
    )>,
);

/// A logical monitor's rectangle in Mutter's layout coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LogicalRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

/// Room a monitor takes in the layout. In Mutter's logical layout mode (its default) that is
/// the mode size divided by the scale, so a 3840x2160 monitor at 125% is 3072x1728; in
/// physical mode it is the mode size. Turned 90 or 270 degrees, width and height swap.
fn logical_size(mode: (i32, i32), scale: f64, transform: u32, logical_mode: bool) -> (i32, i32) {
    let scale = if logical_mode && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let (w, h) = (
        (f64::from(mode.0) / scale).round() as i32,
        (f64::from(mode.1) / scale).round() as i32,
    );
    // Transforms 1 and 3 (and their flipped 5 and 7) turn the monitor sideways.
    if transform % 2 == 1 {
        (h, w)
    } else {
        (w, h)
    }
}

/// Positions for monitors of `sizes`, edge to edge after `existing`: a row to the right of
/// everything, top-aligned with the topmost monitor, or for a vertical layout a column below
/// everything, left-aligned. Mutter rejects a layout whose logical monitors do not touch
/// ("Logical monitors not adjacent") or overlap.
fn adjacent_positions(
    existing: &[LogicalRect],
    sizes: &[(i32, i32)],
    vertical: bool,
) -> Vec<(i32, i32)> {
    let left = existing.iter().map(|r| r.x).min().unwrap_or(0);
    let top = existing.iter().map(|r| r.y).min().unwrap_or(0);
    let right = existing.iter().map(|r| r.x + r.width).max().unwrap_or(0);
    let bottom = existing.iter().map(|r| r.y + r.height).max().unwrap_or(0);
    let mut cursor = if vertical { bottom } else { right };
    sizes
        .iter()
        .map(|&(w, h)| {
            let at = if vertical {
                (left, cursor)
            } else {
                (cursor, top)
            };
            cursor += if vertical { h } else { w };
            at
        })
        .collect()
}

/// The current mode of `connector` in Mutter's state: its id and size.
fn current_mode(monitors: &[MutterMonitor], connector: &str) -> Option<(String, i32, i32)> {
    let (_, modes, _) = monitors.iter().find(|(spec, _, _)| spec.0 == connector)?;
    let flag = |props: &HashMap<String, zbus::zvariant::OwnedValue>, key: &str| {
        props
            .get(key)
            .is_some_and(|v| matches!(&**v, zbus::zvariant::Value::Bool(true)))
    };
    modes
        .iter()
        .find(|m| flag(&m.6, "is-current"))
        .or_else(|| modes.iter().find(|m| flag(&m.6, "is-preferred")))
        .or_else(|| modes.first())
        .map(|m| (m.0.clone(), m.1, m.2))
}

#[async_trait]
impl VirtualDisplayBackend for MutterDisplayBackend {
    fn name(&self) -> &'static str {
        "GNOME Mutter (D-Bus)"
    }

    fn is_available(&self) -> bool {
        self.is_available
    }

    async fn init(&mut self) -> Result<(), DisplayError> {
        let conn = zbus::Connection::session()
            .await
            .map_err(|e| DisplayError::Dbus(format!("Failed to connect to session bus: {e}")))?;

        if !Self::probe_screencast_service(&conn).await {
            return Err(DisplayError::BackendUnavailable(
                "GNOME Mutter ScreenCast service is not owned on D-Bus session bus".into(),
            ));
        }

        self.connection = Some(conn);
        self.is_available = true;
        Ok(())
    }

    async fn create_monitor(
        &mut self,
        config: &VirtualMonitorConfig,
    ) -> Result<VirtualMonitor, DisplayError> {
        config.validate()?;

        let conn = self.connection.as_ref().ok_or_else(|| {
            DisplayError::BackendUnavailable(
                "Mutter backend D-Bus connection not initialized".into(),
            )
        })?;

        // Query existing connectors prior to creation to detect the newly added connector
        let display_config_proxy = zbus::Proxy::new(
            conn,
            "org.gnome.Mutter.DisplayConfig",
            "/org/gnome/Mutter/DisplayConfig",
            "org.gnome.Mutter.DisplayConfig",
        )
        .await
        .ok();

        let connectors_before: std::collections::HashSet<String> =
            if let Some(ref dcp) = display_config_proxy {
                let res: Result<MutterCurrentState, _> = dcp.call("GetCurrentState", &()).await;
                if let Ok(state) = res {
                    state.1.into_iter().map(|(spec, _, _)| spec.0).collect()
                } else {
                    std::collections::HashSet::new()
                }
            } else {
                std::collections::HashSet::new()
            };

        // 1. Connect to org.gnome.Mutter.ScreenCast
        let screencast_proxy = zbus::Proxy::new(
            conn,
            "org.gnome.Mutter.ScreenCast",
            "/org/gnome/Mutter/ScreenCast",
            "org.gnome.Mutter.ScreenCast",
        )
        .await
        .map_err(|e| DisplayError::Dbus(format!("Failed to bind ScreenCast interface: {e}")))?;

        // 2. Call CreateSession
        let empty_props: HashMap<String, zbus::zvariant::Value> = HashMap::new();
        let session_path: zbus::zvariant::OwnedObjectPath = screencast_proxy
            .call("CreateSession", &(empty_props,))
            .await
            .map_err(|e| {
                if e.to_string().contains("Session creation inhibited") {
                    DisplayError::Dbus(
                        "GNOME is blocking virtual monitor creation. Unlock your Linux desktop and try again."
                            .into(),
                    )
                } else {
                    DisplayError::Dbus(format!("ScreenCast.CreateSession failed: {e}"))
                }
            })?;

        let session_path_str = session_path.as_str().to_string();

        // 3. Connect to the new session
        let session_proxy = zbus::Proxy::new(
            conn,
            "org.gnome.Mutter.ScreenCast",
            session_path_str.as_str(),
            "org.gnome.Mutter.ScreenCast.Session",
        )
        .await
        .map_err(|e| DisplayError::Dbus(format!("Failed to bind Session proxy: {e}")))?;

        // 4. Call RecordVirtual with is-platform: true, modes dict (size, refresh-rate, is-preferred)
        let mut record_props = HashMap::new();
        record_props.insert("cursor-mode".to_string(), zbus::zvariant::Value::from(1u32));
        record_props.insert("is-platform".to_string(), zbus::zvariant::Value::from(true));
        record_props.insert(
            "width".to_string(),
            zbus::zvariant::Value::from(config.width as i32),
        );
        record_props.insert(
            "height".to_string(),
            zbus::zvariant::Value::from(config.height as i32),
        );
        record_props.insert(
            "refresh-rate".to_string(),
            zbus::zvariant::Value::from(config.refresh_rate as f64),
        );

        let mut mode_props: HashMap<String, zbus::zvariant::Value> = HashMap::new();
        mode_props.insert(
            "size".to_string(),
            zbus::zvariant::Value::from((config.width, config.height)),
        );
        mode_props.insert(
            "refresh-rate".to_string(),
            zbus::zvariant::Value::from(config.refresh_rate as f64),
        );
        mode_props.insert(
            "is-preferred".to_string(),
            zbus::zvariant::Value::from(true),
        );
        record_props.insert(
            "modes".to_string(),
            zbus::zvariant::Value::from(vec![mode_props]),
        );

        let stream_path: zbus::zvariant::OwnedObjectPath = session_proxy
            .call("RecordVirtual", &(record_props,))
            .await
            .map_err(|e| DisplayError::Dbus(format!("ScreenCast.RecordVirtual failed: {e}")))?;

        let stream_path_str = stream_path.as_str().to_string();

        // 5. Start the screencast session so the virtual monitor output remains active
        let _: () = session_proxy
            .call("Start", &())
            .await
            .map_err(|e| DisplayError::Dbus(format!("ScreenCast.Session.Start failed: {e}")))?;

        let stream_id = stream_path_str.rsplit('/').next().unwrap_or("0");
        let mut connector = format!("Mutter-Virtual-{stream_id}");

        // Detect newly registered connector name from GetCurrentState
        if let Some(ref dcp) = display_config_proxy {
            let res: Result<MutterCurrentState, _> = dcp.call("GetCurrentState", &()).await;
            if let Ok(state_after) = res {
                if let Some((spec, _, _)) = state_after
                    .1
                    .into_iter()
                    .find(|(spec, _, _)| !connectors_before.contains(&spec.0))
                {
                    connector = spec.0;
                }
            }
        }

        // Install EDID binary payload to runtime directory
        let edid_path = if let Some(edid_bytes) = &config.edid {
            let res: Result<String, DisplayError> = (|| {
                let edid_dir = crate::ensure_display_runtime_dir()?;
                let edid_file = edid_dir.join(format!("edid-{connector}.bin"));
                std::fs::write(&edid_file, edid_bytes).map_err(|e| {
                    DisplayError::CommandFailed(format!(
                        "Failed to write runtime EDID file {edid_file:?}: {e}"
                    ))
                })?;
                Ok(edid_file.to_string_lossy().to_string())
            })();

            match res {
                Ok(p) => Some(p),
                Err(e) => {
                    // Roll back session on EDID write failure
                    let _: Result<(), _> = session_proxy.call("Stop", &()).await;
                    return Err(e);
                }
            }
        } else {
            None
        };

        info!(
            id = config.id,
            connector = %connector,
            session = %session_path_str,
            stream = %stream_path_str,
            edid = ?edid_path,
            "Spawned persistent Mutter platform virtual display"
        );

        let monitor = VirtualMonitor {
            id: config.id,
            name: config.name.clone(),
            connector,
            width: config.width,
            height: config.height,
            refresh_rate: config.refresh_rate,
            x: 0,
            y: 0,
            dpi: config.dpi,
            is_virtual: true,
            edid_path,
        };

        self.session_paths
            .insert(config.id, (session_path_str, stream_path_str));
        self.monitors.insert(config.id, monitor.clone());

        Ok(monitor)
    }

    async fn destroy_monitor(&mut self, id: u32) -> Result<(), DisplayError> {
        let (session_path_str, _) = self
            .session_paths
            .get(&id)
            .ok_or_else(|| DisplayError::OutputNotFound(format!("Monitor {id} not found")))?
            .clone();

        if let Some(conn) = self.connection.as_ref() {
            let session_proxy = zbus::Proxy::new(
                conn,
                "org.gnome.Mutter.ScreenCast",
                session_path_str.as_str(),
                "org.gnome.Mutter.ScreenCast.Session",
            )
            .await
            .map_err(|e| DisplayError::Dbus(format!("Failed to bind Session proxy: {e}")))?;

            if let Err(e) = session_proxy.call::<_, _, ()>("Stop", &()).await {
                let gone = matches!(&e, zbus::Error::MethodError(name, detail, _)
                    if session_object_gone(name.as_str(), detail.as_deref()));
                if !gone {
                    return Err(DisplayError::Dbus(format!(
                        "ScreenCast.Session.Stop failed: {e}"
                    )));
                }
            }
        }

        // Remove state after Stop succeeds or the compositor has already destroyed the session
        self.session_paths.remove(&id);
        if let Some(m) = self.monitors.remove(&id) {
            if let Some(path) = m.edid_path {
                let _ = std::fs::remove_file(path);
            }
        }
        Ok(())
    }

    async fn apply_layout(&mut self, layout: &DisplayLayout) -> Result<(), DisplayError> {
        let placements = layout.compute_placements();
        for p in &placements {
            if let Some(m) = self.monitors.get_mut(&p.id) {
                m.x = p.x;
                m.y = p.y;
                m.width = p.width;
                m.height = p.height;
                m.refresh_rate = p.refresh_rate;
                m.dpi = p.dpi;
            }
        }

        // Apply monitor geometry via Mutter DisplayConfig
        if let Some(conn) = self.connection.as_ref() {
            let display_config_proxy = zbus::Proxy::new(
                conn,
                "org.gnome.Mutter.DisplayConfig",
                "/org/gnome/Mutter/DisplayConfig",
                "org.gnome.Mutter.DisplayConfig",
            )
            .await
            .map_err(|e| DisplayError::Dbus(format!("Failed to bind DisplayConfig proxy: {e}")))?;

            let state: MutterCurrentState = display_config_proxy
                .call("GetCurrentState", &())
                .await
                .map_err(|e| DisplayError::Dbus(format!("GetCurrentState failed: {e}")))?;

            let serial = state.0;
            let mutter_monitors = state.1;
            let current_logical_monitors = state.2;

            let virtual_connectors: std::collections::HashSet<String> = self
                .monitors
                .values()
                .map(|m| m.connector.clone())
                .collect();

            let mut logical_monitors: Vec<MutterLogicalMonitorApply> = Vec::new();
            let mut physical_has_primary = false;
            // Where the kept monitors are, so the virtual ones can go edge to edge with them.
            let mut kept_rects: Vec<LogicalRect> = Vec::new();
            let logical_mode = state
                .3
                .get("layout-mode")
                .and_then(|v| u32::try_from(&**v).ok())
                .is_none_or(|mode| mode != 2);

            // Preserve existing physical displays so laptop / external screens are not disabled
            for (lm_x, lm_y, lm_scale, lm_transform, lm_primary, lm_monitors, _) in
                current_logical_monitors
            {
                let mut converted_monitors = Vec::new();
                let mut size = None;
                for (conn_name, _, _, _) in lm_monitors
                    .into_iter()
                    .filter(|(conn_name, _, _, _)| !virtual_connectors.contains(conn_name))
                {
                    if let Some((mode_id, w, h)) = current_mode(&mutter_monitors, &conn_name) {
                        size.get_or_insert((w, h));
                        converted_monitors.push((
                            conn_name,
                            mode_id,
                            HashMap::<String, zbus::zvariant::Value>::new(),
                        ));
                    }
                }
                if let Some(size) = size {
                    if lm_primary {
                        physical_has_primary = true;
                    }
                    let (width, height) = logical_size(size, lm_scale, lm_transform, logical_mode);
                    kept_rects.push(LogicalRect {
                        x: lm_x,
                        y: lm_y,
                        width,
                        height,
                    });
                    logical_monitors.push((
                        lm_x,
                        lm_y,
                        lm_scale,
                        lm_transform,
                        lm_primary,
                        converted_monitors,
                    ));
                }
            }

            // Method 1 = TEMPORARY (verify and apply without persisting to config)
            let method = 1u32;

            // Ensure at least one monitor is marked primary
            let primary_id = placements.iter().map(|p| p.id).min().unwrap_or(1);

            // Our virtual monitors, in the layout's order, each on its own connector: the one
            // recorded at creation, else an unclaimed Mutter virtual monitor ("MetaVendor").
            let mut ordered: Vec<_> = placements.iter().collect();
            ordered.sort_by_key(|p| (p.x, p.y, p.id));
            let mut claimed: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut virtuals = Vec::new();
            for p in ordered {
                let Some(m) = self.monitors.get(&p.id) else {
                    continue;
                };
                let connector = mutter_monitors
                    .iter()
                    .map(|(spec, _, _)| spec)
                    .find(|spec| spec.0 == m.connector && !claimed.contains(&spec.0))
                    .or_else(|| {
                        mutter_monitors
                            .iter()
                            .map(|(spec, _, _)| spec)
                            .find(|spec| {
                                spec.1 == "MetaVendor"
                                    && !claimed.contains(&spec.0)
                                    && !logical_monitors
                                        .iter()
                                        .any(|lm| lm.5.iter().any(|(c, _, _)| *c == spec.0))
                            })
                    })
                    .map(|spec| spec.0.clone())
                    .unwrap_or_else(|| m.connector.clone());
                claimed.insert(connector.clone());
                let modes = mutter_monitors
                    .iter()
                    .find(|(spec, _, _)| spec.0 == connector)
                    .map(|(_, modes, _)| modes.as_slice())
                    .unwrap_or(&[]);
                let mode = modes
                    .iter()
                    .find(|mode| {
                        mode.1 == p.width as i32
                            && mode.2 == p.height as i32
                            && (mode.3 - p.refresh_rate as f64).abs() < 1.0
                    })
                    .or_else(|| {
                        modes
                            .iter()
                            .find(|mode| mode.1 == p.width as i32 && mode.2 == p.height as i32)
                    })
                    .or_else(|| modes.first());
                let (mode_id, size) = match mode {
                    Some(mode) => (mode.0.clone(), (mode.1, mode.2)),
                    None => (
                        format!("{}x{}@{}", p.width, p.height, p.refresh_rate),
                        (p.width as i32, p.height as i32),
                    ),
                };
                // HiDPI monitors at 200%, if their mode offers it; otherwise 100%.
                let scale = match mode {
                    Some(mode) if p.dpi > 144 && mode.5.iter().any(|s| (s - 2.0).abs() < 1e-6) => {
                        2.0
                    }
                    _ => 1.0,
                };
                virtuals.push((
                    p.id,
                    connector,
                    mode_id,
                    scale,
                    logical_size(size, scale, 0, logical_mode),
                ));
            }

            let sizes: Vec<(i32, i32)> = virtuals.iter().map(|v| v.4).collect();
            let positions = adjacent_positions(
                &kept_rects,
                &sizes,
                layout.mode == DisplayLayoutMode::Vertical,
            );
            for ((id, connector, mode_id, scale, _), (x, y)) in virtuals.into_iter().zip(positions)
            {
                if let Some(m) = self.monitors.get_mut(&id) {
                    m.x = x;
                    m.y = y;
                }
                let mon_props: HashMap<String, zbus::zvariant::Value> = HashMap::new();
                let monitor_spec = vec![(connector, mode_id, mon_props)];
                let is_primary = !physical_has_primary && (id == primary_id);
                logical_monitors.push((x, y, scale, 0u32, is_primary, monitor_spec));
            }

            // If neither physical nor virtual was marked primary, ensure at least first monitor is primary
            if !logical_monitors.is_empty() && !logical_monitors.iter().any(|lm| lm.4) {
                logical_monitors[0].4 = true;
            }

            let properties: HashMap<String, zbus::zvariant::Value> = HashMap::new();
            let _: () = display_config_proxy
                .call(
                    "ApplyMonitorsConfig",
                    &(serial, method, logical_monitors, properties),
                )
                .await
                .map_err(|e| DisplayError::Dbus(format!("ApplyMonitorsConfig failed: {e}")))?;
        }

        Ok(())
    }

    async fn refresh_monitors(&mut self) -> Result<(), DisplayError> {
        if self.monitors.is_empty() {
            return Ok(());
        }
        let conn = self.connection.as_ref().ok_or_else(|| {
            DisplayError::BackendUnavailable("Mutter connection is not initialized".into())
        })?;
        let proxy = zbus::Proxy::new(
            conn,
            "org.gnome.Mutter.DisplayConfig",
            "/org/gnome/Mutter/DisplayConfig",
            "org.gnome.Mutter.DisplayConfig",
        )
        .await
        .map_err(|e| DisplayError::Dbus(e.to_string()))?;
        let state: MutterCurrentState = proxy
            .call("GetCurrentState", &())
            .await
            .map_err(|e| DisplayError::Dbus(format!("GetCurrentState failed: {e}")))?;
        let missing: Vec<_> = self
            .monitors
            .values()
            .filter(|m| !state.1.iter().any(|(spec, _, _)| spec.0 == m.connector))
            .map(|m| m.id)
            .collect();
        drop(proxy);
        for id in missing {
            self.destroy_monitor(id).await?;
        }
        Ok(())
    }

    fn list_monitors(&self) -> Vec<VirtualMonitor> {
        let mut list: Vec<_> = self.monitors.values().cloned().collect();
        list.sort_by_key(|m| m.id);
        list
    }
}

#[cfg(test)]
mod placement_tests {
    use super::*;

    #[test]
    fn expired_sessions_can_be_removed_without_hiding_other_stop_errors() {
        assert!(session_object_gone(
            "org.freedesktop.DBus.Error.UnknownObject",
            None
        ));
        assert!(session_object_gone(
            "org.freedesktop.DBus.Error.UnknownMethod",
            Some("Object does not exist at path /org/gnome/Mutter/ScreenCast/Session/u23")
        ));
        assert!(!session_object_gone(
            "org.freedesktop.DBus.Error.UnknownMethod",
            Some("No such method Stop")
        ));
        assert!(!session_object_gone(
            "org.freedesktop.DBus.Error.AccessDenied",
            None
        ));
    }

    fn rect(x: i32, y: i32, width: i32, height: i32) -> LogicalRect {
        LogicalRect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn a_scaled_monitor_takes_its_mode_size_over_its_scale() {
        // The Samsung Odyssey G80SD on the test machine: 4K at 125%.
        assert_eq!(logical_size((3840, 2160), 1.25, 0, true), (3072, 1728));
        assert_eq!(logical_size((3840, 2160), 1.25, 0, false), (3840, 2160));
        assert_eq!(logical_size((2560, 1440), 1.0, 0, true), (2560, 1440));
        assert_eq!(logical_size((1920, 1080), 1.0, 1, true), (1080, 1920));
        assert_eq!(logical_size((3840, 2160), 2.0, 3, true), (1080, 1920));
    }

    #[test]
    fn virtual_monitors_go_edge_to_edge_after_the_real_ones() {
        // The layout GNOME reported when adding a 2560x1440 monitor: "not adjacent" before,
        // because the virtual monitor was placed at (0, 0) on top of the real one.
        let real = [rect(0, 0, 3072, 1728)];
        assert_eq!(
            adjacent_positions(&real, &[(2560, 1440)], false),
            vec![(3072, 0)]
        );
        assert_eq!(
            adjacent_positions(&real, &[(2560, 1440), (1920, 1080)], false),
            vec![(3072, 0), (5632, 0)]
        );
        assert_eq!(
            adjacent_positions(&real, &[(2560, 1440), (1920, 1080)], true),
            vec![(0, 1728), (0, 3168)]
        );
    }

    #[test]
    fn several_real_monitors_are_kept_and_the_row_starts_past_them_all() {
        let real = [rect(-1920, 120, 1920, 1080), rect(0, 0, 3072, 1728)];
        assert_eq!(
            adjacent_positions(&real, &[(2560, 1440)], false),
            vec![(3072, 0)]
        );
        assert_eq!(
            adjacent_positions(&real, &[(2560, 1440)], true),
            vec![(-1920, 1728)]
        );
    }

    #[test]
    fn with_no_real_monitor_the_row_starts_at_the_origin() {
        assert_eq!(
            adjacent_positions(&[], &[(1920, 1080), (1920, 1080)], false),
            vec![(0, 0), (1920, 0)]
        );
    }
}

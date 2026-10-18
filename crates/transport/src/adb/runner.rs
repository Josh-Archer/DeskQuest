use async_trait::async_trait;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use tokio::process::Command;

use super::device::{
    parse_devices_output, parse_reverse_list_output, AdbDevice, AdbDeviceState, AdbReverseRule,
};

#[derive(Error, Debug)]
pub enum AdbError {
    #[error("ADB command '{cmd}' failed (exit code: {status:?}): {stderr}")]
    CommandFailed {
        cmd: String,
        status: Option<i32>,
        stderr: String,
    },

    #[error("I/O error executing ADB: {0}")]
    Io(#[from] std::io::Error),

    #[error("Device '{0}' not found in ADB device list")]
    DeviceNotFound(String),

    #[error("ADB parsing error: {0}")]
    ParseError(String),
}

#[async_trait]
pub trait AdbCommandRunner: Send + Sync {
    async fn list_devices(&self) -> Result<Vec<AdbDevice>, AdbError>;
    async fn reverse(
        &self,
        serial: Option<&str>,
        remote_port: u16,
        local_port: u16,
    ) -> Result<(), AdbError>;
    async fn reverse_remove(&self, serial: Option<&str>, remote_port: u16) -> Result<(), AdbError>;
    async fn reverse_list(&self, serial: Option<&str>) -> Result<Vec<AdbReverseRule>, AdbError>;
    async fn check_connection(&self, serial: Option<&str>) -> Result<bool, AdbError>;
}

/// Lets one runner be shared (for example by the bridge and by the daemon's shutdown cleanup).
#[async_trait]
impl<T: AdbCommandRunner + ?Sized> AdbCommandRunner for Arc<T> {
    async fn list_devices(&self) -> Result<Vec<AdbDevice>, AdbError> {
        (**self).list_devices().await
    }
    async fn reverse(
        &self,
        serial: Option<&str>,
        remote_port: u16,
        local_port: u16,
    ) -> Result<(), AdbError> {
        (**self).reverse(serial, remote_port, local_port).await
    }
    async fn reverse_remove(&self, serial: Option<&str>, remote_port: u16) -> Result<(), AdbError> {
        (**self).reverse_remove(serial, remote_port).await
    }
    async fn reverse_list(&self, serial: Option<&str>) -> Result<Vec<AdbReverseRule>, AdbError> {
        (**self).reverse_list(serial).await
    }
    async fn check_connection(&self, serial: Option<&str>) -> Result<bool, AdbError> {
        (**self).check_connection(serial).await
    }
}

/// Production ADB command runner executing `/usr/bin/adb` or `PATH` `adb` binary.
pub struct SystemAdbRunner {
    adb_path: String,
}

impl Default for SystemAdbRunner {
    fn default() -> Self {
        Self {
            adb_path: "adb".to_string(),
        }
    }
}

impl SystemAdbRunner {
    pub fn new(adb_path: impl Into<String>) -> Self {
        Self {
            adb_path: adb_path.into(),
        }
    }

    fn build_cmd(&self, serial: Option<&str>, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.adb_path);
        cmd.kill_on_drop(true);
        if let Some(s) = serial {
            cmd.arg("-s").arg(s);
        }
        for arg in args {
            cmd.arg(arg);
        }
        cmd
    }

    async fn run_cmd(&self, mut cmd: Command) -> Result<std::process::Output, AdbError> {
        match tokio::time::timeout(std::time::Duration::from_secs(5), cmd.output()).await {
            Ok(Ok(output)) => Ok(output),
            Ok(Err(e)) => Err(AdbError::Io(e)),
            Err(_) => Err(AdbError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "ADB command timed out after 5 seconds",
            ))),
        }
    }
}

#[async_trait]
impl AdbCommandRunner for SystemAdbRunner {
    async fn list_devices(&self) -> Result<Vec<AdbDevice>, AdbError> {
        let output = self
            .run_cmd(self.build_cmd(None, &["devices", "-l"]))
            .await?;
        if !output.status.success() {
            return Err(AdbError::CommandFailed {
                cmd: format!("{} devices -l", self.adb_path),
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(parse_devices_output(&stdout))
    }

    async fn reverse(
        &self,
        serial: Option<&str>,
        remote_port: u16,
        local_port: u16,
    ) -> Result<(), AdbError> {
        let remote_arg = format!("tcp:{}", remote_port);
        let local_arg = format!("tcp:{}", local_port);
        let output = self
            .run_cmd(self.build_cmd(serial, &["reverse", &remote_arg, &local_arg]))
            .await?;

        if !output.status.success() {
            return Err(AdbError::CommandFailed {
                cmd: format!("{} reverse {} {}", self.adb_path, remote_arg, local_arg),
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }
        Ok(())
    }

    async fn reverse_remove(&self, serial: Option<&str>, remote_port: u16) -> Result<(), AdbError> {
        let remote_arg = format!("tcp:{}", remote_port);
        let output = self
            .run_cmd(self.build_cmd(serial, &["reverse", "--remove", &remote_arg]))
            .await?;

        if !output.status.success() {
            return Err(AdbError::CommandFailed {
                cmd: format!("{} reverse --remove {}", self.adb_path, remote_arg),
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }
        Ok(())
    }

    async fn reverse_list(&self, serial: Option<&str>) -> Result<Vec<AdbReverseRule>, AdbError> {
        let output = self
            .run_cmd(self.build_cmd(serial, &["reverse", "--list"]))
            .await?;

        if !output.status.success() {
            return Err(AdbError::CommandFailed {
                cmd: format!("{} reverse --list", self.adb_path),
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(parse_reverse_list_output(&stdout))
    }

    async fn check_connection(&self, serial: Option<&str>) -> Result<bool, AdbError> {
        let devices = self.list_devices().await?;
        if let Some(target_serial) = serial {
            Ok(devices
                .iter()
                .any(|d| d.serial == target_serial && d.state == AdbDeviceState::Device))
        } else {
            Ok(devices.iter().any(|d| d.state == AdbDeviceState::Device))
        }
    }
}

/// In-memory mock ADB runner for deterministic CI testing without physical hardware.
#[derive(Debug, Default)]
struct MockState {
    devices: Vec<AdbDevice>,
    reverse_rules: HashMap<String, Vec<AdbReverseRule>>,
    fail_next: Option<String>,
    binary_missing: bool,
}

fn missing_binary(state: &Mutex<MockState>) -> Option<AdbError> {
    state.lock().binary_missing.then(|| {
        AdbError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No such file or directory (os error 2)",
        ))
    })
}

#[derive(Clone, Default)]
pub struct MockAdbRunner {
    state: Arc<Mutex<MockState>>,
}

impl MockAdbRunner {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(MockState::default())),
        }
    }

    pub fn add_device(&self, device: AdbDevice) {
        self.state.lock().devices.push(device);
    }

    pub fn remove_device(&self, serial: &str) {
        let mut state = self.state.lock();
        state.devices.retain(|d| d.serial != serial);
        state.reverse_rules.remove(serial);
    }

    /// Simulates `adb` not being installed: every command fails with `ErrorKind::NotFound`.
    pub fn set_binary_missing(&self, missing: bool) {
        self.state.lock().binary_missing = missing;
    }

    /// Changes the state of an attached device (e.g. to `Unauthorized`). No-op if absent.
    pub fn set_device_state(&self, serial: &str, new_state: AdbDeviceState) {
        let mut state = self.state.lock();
        for d in state.devices.iter_mut().filter(|d| d.serial == serial) {
            d.state = new_state.clone();
        }
    }

    pub fn set_fail_next(&self, error_msg: Option<String>) {
        self.state.lock().fail_next = error_msg;
    }

    pub fn clear_rules(&self, serial: &str) {
        self.state.lock().reverse_rules.remove(serial);
    }

    pub fn get_rules(&self, serial: &str) -> Vec<AdbReverseRule> {
        self.state
            .lock()
            .reverse_rules
            .get(serial)
            .cloned()
            .unwrap_or_default()
    }
}

#[async_trait]
impl AdbCommandRunner for MockAdbRunner {
    async fn list_devices(&self) -> Result<Vec<AdbDevice>, AdbError> {
        if let Some(err) = missing_binary(&self.state) {
            return Err(err);
        }
        let mut state = self.state.lock();
        if let Some(err) = state.fail_next.take() {
            return Err(AdbError::CommandFailed {
                cmd: "adb devices -l".to_string(),
                status: Some(1),
                stderr: err,
            });
        }
        Ok(state.devices.clone())
    }

    async fn reverse(
        &self,
        serial: Option<&str>,
        remote_port: u16,
        local_port: u16,
    ) -> Result<(), AdbError> {
        if let Some(err) = missing_binary(&self.state) {
            return Err(err);
        }
        let mut state = self.state.lock();
        if let Some(err) = state.fail_next.take() {
            return Err(AdbError::CommandFailed {
                cmd: "adb reverse".to_string(),
                status: Some(1),
                stderr: err,
            });
        }

        let key = serial.unwrap_or("default").to_string();
        let rules = state.reverse_rules.entry(key).or_default();
        rules.retain(|r| r.remote_port != remote_port);
        rules.push(AdbReverseRule {
            remote_port,
            local_port,
        });
        Ok(())
    }

    async fn reverse_remove(&self, serial: Option<&str>, remote_port: u16) -> Result<(), AdbError> {
        if let Some(err) = missing_binary(&self.state) {
            return Err(err);
        }
        let mut state = self.state.lock();
        let key = serial.unwrap_or("default").to_string();
        if let Some(rules) = state.reverse_rules.get_mut(&key) {
            rules.retain(|r| r.remote_port != remote_port);
        }
        Ok(())
    }

    async fn reverse_list(&self, serial: Option<&str>) -> Result<Vec<AdbReverseRule>, AdbError> {
        if let Some(err) = missing_binary(&self.state) {
            return Err(err);
        }
        let state = self.state.lock();
        let key = serial.unwrap_or("default");
        Ok(state.reverse_rules.get(key).cloned().unwrap_or_default())
    }

    async fn check_connection(&self, serial: Option<&str>) -> Result<bool, AdbError> {
        if let Some(err) = missing_binary(&self.state) {
            return Err(err);
        }
        let state = self.state.lock();
        if let Some(target_serial) = serial {
            Ok(state
                .devices
                .iter()
                .any(|d| d.serial == target_serial && d.state == AdbDeviceState::Device))
        } else {
            Ok(state
                .devices
                .iter()
                .any(|d| d.state == AdbDeviceState::Device))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adb::device::AdbDeviceState;

    #[tokio::test]
    async fn test_mock_adb_runner_reverse_flow() {
        let runner = MockAdbRunner::new();
        runner.add_device(AdbDevice {
            serial: "QUEST3_SERIAL".to_string(),
            state: AdbDeviceState::Device,
            product: Some("eureka".to_string()),
            model: Some("Quest_3".to_string()),
            device_name: Some("eureka".to_string()),
            transport_id: Some("1".to_string()),
        });

        let devices = runner.list_devices().await.unwrap();
        assert_eq!(devices.len(), 1);
        assert!(devices[0].is_quest());

        runner
            .reverse(Some("QUEST3_SERIAL"), 8088, 8088)
            .await
            .unwrap();
        let rules = runner.reverse_list(Some("QUEST3_SERIAL")).await.unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].remote_port, 8088);

        runner
            .reverse_remove(Some("QUEST3_SERIAL"), 8088)
            .await
            .unwrap();
        let rules_after = runner.reverse_list(Some("QUEST3_SERIAL")).await.unwrap();
        assert!(rules_after.is_empty());
    }
}

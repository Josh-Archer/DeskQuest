use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

use super::device::{AdbDevice, AdbDeviceState};
use super::runner::{AdbCommandRunner, AdbError};
use crate::control::DEFAULT_CONTROL_PORT;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdbBridgeState {
    Idle,
    Connected {
        serial: String,
        model: Option<String>,
    },
    Reconnecting {
        serial: String,
        model: Option<String>,
        attempts: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdbBridgeEvent {
    DeviceConnected(AdbDevice),
    DeviceDisconnected(String),
    ReversePortEstablished { serial: String, port: u16 },
    CableBumpDetected { serial: String },
    ReconnectionSuccessful { serial: String },
    Error(String),
}

#[derive(Debug, Clone)]
pub struct AdbBridgeConfig {
    pub remote_port: u16,
    pub local_port: u16,
    pub poll_interval: Duration,
    pub max_reconnect_attempts: usize,
    pub require_quest_filter: bool,
    pub target_serial: Option<String>,
}

/// Default reverses the host control TCP port ([`DEFAULT_CONTROL_PORT`], 48444), the only TCP
/// port the host daemon listens on.
impl Default for AdbBridgeConfig {
    fn default() -> Self {
        Self {
            remote_port: DEFAULT_CONTROL_PORT,
            local_port: DEFAULT_CONTROL_PORT,
            poll_interval: Duration::from_millis(500),
            max_reconnect_attempts: 10,
            require_quest_filter: true,
            target_serial: None,
        }
    }
}

/// Automatic USB ADB reverse-tethering manager.
/// Detects Meta Quest USB connection, configures reverse port forwarding,
/// and automatically recovers from cable bumps or disconnections.
pub struct AdbBridge<R: AdbCommandRunner> {
    runner: R,
    config: AdbBridgeConfig,
    state: AdbBridgeState,
    event_sender: broadcast::Sender<AdbBridgeEvent>,
}

impl<R: AdbCommandRunner> AdbBridge<R> {
    pub fn new(runner: R, config: AdbBridgeConfig) -> Self {
        let (event_sender, _) = broadcast::channel(64);
        Self {
            runner,
            config,
            state: AdbBridgeState::Idle,
            event_sender,
        }
    }

    pub fn state(&self) -> &AdbBridgeState {
        &self.state
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AdbBridgeEvent> {
        self.event_sender.subscribe()
    }

    /// Single step of the device monitoring and reconnection state machine.
    pub async fn poll_step(&mut self) -> Result<Option<AdbBridgeEvent>, AdbError> {
        match &self.state.clone() {
            AdbBridgeState::Idle => {
                let devices = self.runner.list_devices().await?;
                for dev in devices {
                    let matches_target = match &self.config.target_serial {
                        Some(ts) => dev.serial == *ts,
                        None => true,
                    };

                    let matches_filter = if self.config.require_quest_filter {
                        dev.is_quest()
                    } else {
                        true
                    };

                    if matches_target && matches_filter && dev.state == AdbDeviceState::Device {
                        // Establish reverse port forwarding
                        match self
                            .runner
                            .reverse(
                                Some(&dev.serial),
                                self.config.remote_port,
                                self.config.local_port,
                            )
                            .await
                        {
                            Ok(()) => {
                                self.state = AdbBridgeState::Connected {
                                    serial: dev.serial.clone(),
                                    model: dev.model.clone(),
                                };
                                let _ = self
                                    .event_sender
                                    .send(AdbBridgeEvent::DeviceConnected(dev.clone()));
                                let evt = AdbBridgeEvent::ReversePortEstablished {
                                    serial: dev.serial,
                                    port: self.config.remote_port,
                                };
                                let _ = self.event_sender.send(evt.clone());
                                return Ok(Some(evt));
                            }
                            Err(e) => {
                                let evt = AdbBridgeEvent::Error(e.to_string());
                                let _ = self.event_sender.send(evt.clone());
                                return Ok(Some(evt));
                            }
                        }
                    }
                }
                Ok(None)
            }

            AdbBridgeState::Connected { serial, model } => {
                let connected = match self.runner.check_connection(Some(serial)).await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!("ADB check_connection error for {serial}: {e}");
                        false
                    }
                };

                if !connected {
                    // Device disconnected or connection check failed; begin reconnection
                    self.state = AdbBridgeState::Reconnecting {
                        serial: serial.clone(),
                        model: model.clone(),
                        attempts: 1,
                    };
                    let evt = AdbBridgeEvent::CableBumpDetected {
                        serial: serial.clone(),
                    };
                    let _ = self.event_sender.send(evt.clone());
                    return Ok(Some(evt));
                }

                // Verify reverse rule is still active
                let rules = self
                    .runner
                    .reverse_list(Some(serial))
                    .await
                    .unwrap_or_default();
                let rule_active = rules
                    .iter()
                    .any(|r| r.remote_port == self.config.remote_port);

                if !rule_active {
                    // Re-apply reverse rule if dropped
                    if let Err(e) = self
                        .runner
                        .reverse(
                            Some(serial),
                            self.config.remote_port,
                            self.config.local_port,
                        )
                        .await
                    {
                        tracing::warn!("Failed to re-apply reverse port forwarding rule: {e}");
                        self.state = AdbBridgeState::Reconnecting {
                            serial: serial.clone(),
                            model: model.clone(),
                            attempts: 1,
                        };
                        let evt = AdbBridgeEvent::Error(e.to_string());
                        let _ = self.event_sender.send(evt.clone());
                        return Ok(Some(evt));
                    }
                }

                Ok(None)
            }

            AdbBridgeState::Reconnecting {
                serial,
                model,
                attempts,
            } => {
                let connected = self
                    .runner
                    .check_connection(Some(serial))
                    .await
                    .unwrap_or(false);

                if connected {
                    // Re-apply reverse port forwarding
                    if self
                        .runner
                        .reverse(
                            Some(serial),
                            self.config.remote_port,
                            self.config.local_port,
                        )
                        .await
                        .is_ok()
                    {
                        self.state = AdbBridgeState::Connected {
                            serial: serial.clone(),
                            model: model.clone(),
                        };
                        let evt = AdbBridgeEvent::ReconnectionSuccessful {
                            serial: serial.clone(),
                        };
                        let _ = self.event_sender.send(evt.clone());
                        return Ok(Some(evt));
                    }
                }

                if *attempts >= self.config.max_reconnect_attempts {
                    // Max attempts exceeded, remove reverse port and return to Idle
                    let old_serial = serial.clone();
                    let _ = self
                        .runner
                        .reverse_remove(Some(&old_serial), self.config.remote_port)
                        .await;
                    self.state = AdbBridgeState::Idle;
                    let evt = AdbBridgeEvent::DeviceDisconnected(old_serial);
                    let _ = self.event_sender.send(evt.clone());
                    return Ok(Some(evt));
                } else {
                    self.state = AdbBridgeState::Reconnecting {
                        serial: serial.clone(),
                        model: model.clone(),
                        attempts: attempts + 1,
                    };
                }

                Ok(None)
            }
        }
    }
}

/// Handle allowing background monitoring loop control.
pub struct AdbBridgeHandle {
    running: Arc<AtomicBool>,
}

impl AdbBridgeHandle {
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }
}

impl Drop for AdbBridgeHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

impl<R: AdbCommandRunner + 'static> AdbBridge<R> {
    /// Spawns a background monitor task with the configured poll interval and exponential backoff on reconnection.
    pub fn spawn_monitor(mut self) -> (AdbBridgeHandle, broadcast::Receiver<AdbBridgeEvent>) {
        let running = Arc::new(AtomicBool::new(true));
        let handle = AdbBridgeHandle {
            running: running.clone(),
        };
        let rx = self.subscribe();

        let base_interval = self.config.poll_interval;
        tokio::spawn(async move {
            while running.load(Ordering::Relaxed) {
                if let Err(e) = self.poll_step().await {
                    tracing::warn!("ADB monitor poll_step error: {e}");
                }
                let sleep_duration = match &self.state {
                    AdbBridgeState::Reconnecting { attempts, .. } => {
                        let shift = (*attempts).min(5) as u32;
                        base_interval.saturating_mul(1 << shift)
                    }
                    _ => base_interval,
                };
                tokio::time::sleep(sleep_duration).await;
            }
        });

        (handle, rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adb::runner::MockAdbRunner;

    #[test]
    fn default_config_reverses_the_control_port() {
        let config = AdbBridgeConfig::default();
        assert_eq!(config.remote_port, DEFAULT_CONTROL_PORT);
        assert_eq!(config.local_port, DEFAULT_CONTROL_PORT);
    }

    #[tokio::test]
    async fn test_adb_bridge_detection_and_reconnection() {
        let runner = MockAdbRunner::new();
        let config = AdbBridgeConfig {
            remote_port: 8088,
            local_port: 8088,
            poll_interval: Duration::from_millis(10),
            max_reconnect_attempts: 3,
            require_quest_filter: true,
            target_serial: None,
        };

        let mut bridge = AdbBridge::new(runner.clone(), config);
        assert_eq!(*bridge.state(), AdbBridgeState::Idle);

        // Step 1: No devices
        let evt = bridge.poll_step().await.unwrap();
        assert!(evt.is_none());

        // Step 2: Quest 3 plugged in
        runner.add_device(AdbDevice {
            serial: "QUEST3_ABC".to_string(),
            state: AdbDeviceState::Device,
            product: Some("eureka".to_string()),
            model: Some("Quest_3".to_string()),
            device_name: Some("eureka".to_string()),
            transport_id: Some("1".to_string()),
        });

        let evt = bridge
            .poll_step()
            .await
            .unwrap()
            .expect("Should emit event");
        assert_eq!(
            evt,
            AdbBridgeEvent::ReversePortEstablished {
                serial: "QUEST3_ABC".to_string(),
                port: 8088,
            }
        );
        assert!(matches!(bridge.state(), AdbBridgeState::Connected { .. }));
        assert_eq!(runner.get_rules("QUEST3_ABC").len(), 1);

        // Step 3: Cable bump / disconnect
        runner.remove_device("QUEST3_ABC");
        let evt = bridge
            .poll_step()
            .await
            .unwrap()
            .expect("Should detect bump");
        assert_eq!(
            evt,
            AdbBridgeEvent::CableBumpDetected {
                serial: "QUEST3_ABC".to_string(),
            }
        );
        assert!(matches!(
            bridge.state(),
            AdbBridgeState::Reconnecting { .. }
        ));

        // Step 4: Cable reconnected
        runner.add_device(AdbDevice {
            serial: "QUEST3_ABC".to_string(),
            state: AdbDeviceState::Device,
            product: Some("eureka".to_string()),
            model: Some("Quest_3".to_string()),
            device_name: Some("eureka".to_string()),
            transport_id: Some("2".to_string()),
        });

        let evt = bridge.poll_step().await.unwrap().expect("Should reconnect");
        assert_eq!(
            evt,
            AdbBridgeEvent::ReconnectionSuccessful {
                serial: "QUEST3_ABC".to_string(),
            }
        );
        assert_eq!(
            bridge.state(),
            &AdbBridgeState::Connected {
                serial: "QUEST3_ABC".to_string(),
                model: Some("Quest_3".to_string()),
            }
        );
    }

    #[tokio::test]
    async fn test_adb_bridge_reverse_reapply_failure() {
        let runner = MockAdbRunner::new();
        let config = AdbBridgeConfig {
            remote_port: 8088,
            local_port: 8088,
            poll_interval: Duration::from_millis(10),
            max_reconnect_attempts: 3,
            require_quest_filter: false,
            target_serial: None,
        };

        let mut bridge = AdbBridge::new(runner.clone(), config);
        runner.add_device(AdbDevice {
            serial: "QUEST_TEST".to_string(),
            state: AdbDeviceState::Device,
            product: None,
            model: None,
            device_name: None,
            transport_id: None,
        });

        // Step 1: Connect and establish rule
        let evt = bridge.poll_step().await.unwrap();
        assert!(matches!(
            evt,
            Some(AdbBridgeEvent::ReversePortEstablished { .. })
        ));
        assert!(matches!(bridge.state(), AdbBridgeState::Connected { .. }));

        // Step 2: Drop rule and inject reverse failure
        runner.clear_rules("QUEST_TEST");
        runner.set_fail_next(Some("adb server error".to_string()));

        let evt = bridge.poll_step().await.unwrap();
        assert!(
            matches!(evt, Some(AdbBridgeEvent::Error(ref msg)) if msg.contains("adb server error"))
        );
        assert!(matches!(
            bridge.state(),
            AdbBridgeState::Reconnecting { .. }
        ));
    }
}

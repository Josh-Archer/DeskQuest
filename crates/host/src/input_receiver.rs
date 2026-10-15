//! Host-side input receiver service connecting low-latency transport channel to InputInjector.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

use linux_quest_input::InputInjector;
use linux_quest_transport::{InputChannelReceiver, TransportError};

use crate::HostError;

/// Host-side service that listens on the low-latency input channel
/// and hands all received `InputEvent`s directly to an `InputInjector`.
pub struct HostInputReceiver<I: InputInjector> {
    receiver: InputChannelReceiver,
    injector: Arc<Mutex<I>>,
    running: Arc<AtomicBool>,
}

impl<I: InputInjector> HostInputReceiver<I> {
    pub fn new(receiver: InputChannelReceiver, injector: Arc<Mutex<I>>) -> Self {
        Self {
            receiver,
            injector,
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn running_flag(&self) -> Arc<AtomicBool> {
        self.running.clone()
    }

    pub fn local_addr(&self) -> Result<SocketAddr, TransportError> {
        self.receiver.local_addr()
    }

    /// Process a single incoming input event batch from the transport channel,
    /// injecting each event into the `InputInjector`.
    /// Returns the number of events injected.
    pub async fn step(&mut self) -> Result<usize, HostError> {
        let events = self.receiver.recv_events().await?;
        let count = events.len();
        if count > 0 {
            let mut injector = self.injector.lock().map_err(|e| {
                HostError::Input(linux_quest_input::InputError::EmitFailed(e.to_string()))
            })?;
            for event in events {
                injector.inject_event(event)?;
            }
        }
        Ok(count)
    }

    /// Run the receiver loop until `running` flag is cleared.
    pub async fn run(&mut self) -> Result<(), HostError> {
        self.running.store(true, Ordering::SeqCst);
        while self.running.load(Ordering::Relaxed) {
            tokio::select! {
                res = self.step() => {
                    if let Err(e) = res {
                        tracing::warn!(error = ?e, "Error processing remote input event batch");
                    }
                }
                _ = tokio::time::sleep(tokio::time::Duration::from_millis(50)) => {
                    // Periodic check of running flag
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_quest_input::MockInputInjector;
    use linux_quest_protocol::{ElementState, InputEvent, KeyModifiers, MouseButton};
    use linux_quest_transport::InputChannelSender;
    use tokio::net::UdpSocket;

    #[tokio::test]
    async fn test_host_input_receiver_lifecycle() {
        let s_sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let s_receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let addr_sender = s_sender.local_addr().unwrap();
        let addr_receiver = s_receiver.local_addr().unwrap();

        let mut sender = InputChannelSender::new(s_sender, Some(addr_receiver), 4);
        let receiver = InputChannelReceiver::new(s_receiver, Some(addr_sender), true);

        let injector = Arc::new(Mutex::new(MockInputInjector::new()));
        let mut host_receiver = HostInputReceiver::new(receiver, injector.clone());

        assert_eq!(host_receiver.local_addr().unwrap(), addr_receiver);

        let events = vec![
            InputEvent::MouseMoveRelative { dx: 12, dy: -8 },
            InputEvent::MouseButton {
                button: MouseButton::Left,
                state: ElementState::Pressed,
            },
            InputEvent::KeyboardKey {
                keycode: 30, // KEY_A
                state: ElementState::Pressed,
                modifiers: KeyModifiers::empty(),
            },
        ];

        sender.send_events(events.clone(), 100_000).await.unwrap();

        let count = host_receiver.step().await.unwrap();
        assert_eq!(count, 3);

        let binding = injector.lock().unwrap();
        let received = &binding.received_events;
        assert_eq!(received.len(), 3);
        assert_eq!(*received, events);
    }
}

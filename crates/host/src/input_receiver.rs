//! Host-side input receiver service connecting low-latency transport channel to InputInjector.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

use linux_quest_input::InputInjector;
use linux_quest_protocol::InputEvent;
use linux_quest_transport::{InputChannelReceiver, TransportError};

use crate::link_status::SharedLinkTracker;
use crate::HostError;

/// Host-side service that listens on the low-latency input channel
/// and hands all received `InputEvent`s directly to an `InputInjector`.
pub struct HostInputReceiver<I: InputInjector> {
    receiver: InputChannelReceiver,
    injector: Arc<Mutex<I>>,
    running: Arc<AtomicBool>,
    link: Option<SharedLinkTracker>,
    keyframe_requests: Option<Arc<AtomicBool>>,
}

impl<I: InputInjector> HostInputReceiver<I> {
    pub fn new(receiver: InputChannelReceiver, injector: Arc<Mutex<I>>) -> Self {
        Self {
            receiver,
            injector,
            running: Arc::new(AtomicBool::new(false)),
            link: None,
            keyframe_requests: None,
        }
    }

    /// Sets `flag` when the client sends [`InputEvent::KeyframeRequest`] (it lost video); the
    /// video sender clears it and forces an IDR. Without this the request is dropped.
    pub fn with_keyframe_requests(mut self, flag: Arc<AtomicBool>) -> Self {
        self.keyframe_requests = Some(flag);
        self
    }

    /// Reports each received packet's sender to the shared link tracker.
    pub fn with_link_tracker(mut self, link: SharedLinkTracker) -> Self {
        self.link = Some(link);
        self
    }

    pub fn running_flag(&self) -> Arc<AtomicBool> {
        self.running.clone()
    }

    pub fn local_addr(&self) -> Result<SocketAddr, TransportError> {
        self.receiver.local_addr()
    }

    pub fn injector(&self) -> Arc<Mutex<I>> {
        self.injector.clone()
    }
    /// Process a single incoming input event batch from the transport channel,
    /// injecting each event into the `InputInjector`.
    /// Returns the number of events injected.
    pub async fn step(&mut self) -> Result<usize, HostError> {
        let events = self.receiver.recv_events().await?;
        if let (Some(link), Some(peer)) = (&self.link, self.receiver.remote_addr()) {
            link.lock().input_packet(peer, std::time::Instant::now());
        }
        let (keyframe_requests, events): (Vec<_>, Vec<_>) = events
            .into_iter()
            .partition(|e| matches!(e, InputEvent::KeyframeRequest));
        if !keyframe_requests.is_empty() {
            if let Some(flag) = &self.keyframe_requests {
                flag.store(true, Ordering::Release);
            }
        }
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
        if let Ok(mut inj) = self.injector.lock() {
            let _ = inj.release_all();
        }
        Ok(())
    }
}

impl<I: InputInjector> Drop for HostInputReceiver<I> {
    fn drop(&mut self) {
        if let Ok(mut inj) = self.injector.lock() {
            let _ = inj.release_all();
        }
    }
}

impl HostInputReceiver<linux_quest_input::AutoInputInjector> {
    /// Creates a new `HostInputReceiver` wired with an `AutoInputInjector`.
    /// The `AutoInputInjector` dynamically uses `UinputInjector` if `/dev/uinput` is accessible,
    /// or falls back to `MockInputInjector` / logging if unavailable.
    pub fn new_auto(receiver: InputChannelReceiver) -> Self {
        let injector = Arc::new(Mutex::new(linux_quest_input::AutoInputInjector::new()));
        Self::new(receiver, injector)
    }

    /// Binds a UDP socket to `bind_addr` and initializes a `HostInputReceiver` with `AutoInputInjector`.
    pub async fn bind_auto(bind_addr: SocketAddr) -> Result<Self, HostError> {
        let socket = tokio::net::UdpSocket::bind(bind_addr)
            .await
            .map_err(|e| HostError::Transport(TransportError::Io(e)))?;
        let receiver = InputChannelReceiver::new(socket, None, true);
        Ok(Self::new_auto(receiver))
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
            InputEvent::TextCommit {
                text: "résumé 東京".into(),
            },
        ];

        sender.send_events(events.clone(), 100_000).await.unwrap();

        let count = host_receiver.step().await.unwrap();
        assert_eq!(count, 4);

        let binding = injector.lock().unwrap();
        let received = &binding.received_events;
        assert_eq!(received.len(), 4);
        assert_eq!(*received, events);
    }
    #[tokio::test]
    async fn test_host_input_receiver_with_auto_injector() {
        let s_sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let s_receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let addr_sender = s_sender.local_addr().unwrap();
        let addr_receiver = s_receiver.local_addr().unwrap();

        let mut sender = InputChannelSender::new(s_sender, Some(addr_receiver), 4);
        let receiver = InputChannelReceiver::new(s_receiver, Some(addr_sender), true);

        // uinput when the user can open /dev/uinput (a desktop), the mock fallback otherwise.
        let uinput_openable = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .is_ok();
        let mut host_receiver = HostInputReceiver::new_auto(receiver);
        assert_eq!(
            host_receiver.injector().lock().unwrap().is_uinput(),
            uinput_openable
        );

        let events = vec![InputEvent::MouseMoveRelative { dx: 5, dy: -3 }];
        sender.send_events(events, 100_000).await.unwrap();

        let count = host_receiver.step().await.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn test_host_input_receiver_keyframe_request_sets_flag_and_is_not_injected() {
        let s_sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let s_receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let addr_sender = s_sender.local_addr().unwrap();
        let addr_receiver = s_receiver.local_addr().unwrap();

        let mut sender = InputChannelSender::new(s_sender, Some(addr_receiver), 4);
        let receiver = InputChannelReceiver::new(s_receiver, Some(addr_sender), true);

        let flag = Arc::new(AtomicBool::new(false));
        let injector = Arc::new(Mutex::new(MockInputInjector::new()));
        let mut host_receiver =
            HostInputReceiver::new(receiver, injector.clone()).with_keyframe_requests(flag.clone());

        let normal = InputEvent::MouseMoveRelative { dx: 3, dy: 4 };
        sender
            .send_events(vec![InputEvent::KeyframeRequest, normal.clone()], 100_000)
            .await
            .unwrap();

        let count = host_receiver.step().await.unwrap();
        assert_eq!(count, 1);
        assert!(flag.load(Ordering::Acquire));

        let binding = injector.lock().unwrap();
        assert_eq!(binding.received_events, vec![normal]);
    }

    #[tokio::test]
    async fn test_host_input_receiver_keyframe_request_without_flag_is_dropped() {
        let s_sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let s_receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let addr_sender = s_sender.local_addr().unwrap();
        let addr_receiver = s_receiver.local_addr().unwrap();

        let mut sender = InputChannelSender::new(s_sender, Some(addr_receiver), 4);
        let receiver = InputChannelReceiver::new(s_receiver, Some(addr_sender), true);

        let injector = Arc::new(Mutex::new(MockInputInjector::new()));
        let mut host_receiver = HostInputReceiver::new(receiver, injector.clone());

        sender
            .send_events(vec![InputEvent::KeyframeRequest], 100_000)
            .await
            .unwrap();

        let count = host_receiver.step().await.unwrap();
        assert_eq!(count, 0);
        assert!(injector.lock().unwrap().received_events.is_empty());
    }

    #[tokio::test]
    async fn test_host_input_receiver_bind_auto() {
        let host_receiver = HostInputReceiver::bind_auto("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        assert!(host_receiver.local_addr().is_ok());
    }
}

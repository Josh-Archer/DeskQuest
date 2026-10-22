//! Live controls of the running video streams, for the daemon's IPC server: pause and resume
//! sending, and ask the desktop capture to let the user pick another screen.

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};

use super::PeerProvider;

/// How long [`VideoControl::choose_screen`] waits by default: the portal's own limit for the
/// user to answer the share dialog, plus a little.
pub const CHOOSE_SCREEN_TIMEOUT: Duration = Duration::from_secs(320);

/// A request to the desktop capture to show the screen picker again.
#[derive(Debug)]
pub struct ReselectRequest {
    pub reply: oneshot::Sender<Result<(), String>>,
}

/// What stream 0 shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureKind {
    /// No video sender runs.
    None,
    /// A real screen through the screen-cast portal.
    Screen,
    /// The synthetic test pattern (`--test-pattern`, no capture backend, or standing in while
    /// a lost screen capture is restored).
    TestPattern,
}

/// Shared by the video senders and the IPC server.
#[derive(Debug)]
pub struct VideoControl {
    paused: AtomicBool,
    reselect: Mutex<Option<mpsc::UnboundedSender<ReselectRequest>>>,
    streams: AtomicU16,
    capture: Mutex<CaptureKind>,
}

impl Default for VideoControl {
    fn default() -> Self {
        Self {
            paused: AtomicBool::new(false),
            reselect: Mutex::new(None),
            streams: AtomicU16::new(0),
            capture: Mutex::new(CaptureKind::None),
        }
    }
}

impl VideoControl {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Stops (or resumes) sending video to the headset. Capture keeps running, so resuming is
    /// immediate; the encoder does no work while paused.
    pub fn set_paused(&self, paused: bool) {
        let was = self.paused.swap(paused, Ordering::SeqCst);
        if was != paused {
            tracing::info!(
                paused,
                "video to the headset {}",
                if paused { "paused" } else { "resumed" }
            );
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// Records what the senders that started are.
    pub fn set_running(&self, streams: u16, capture: CaptureKind) {
        self.streams.store(streams, Ordering::SeqCst);
        *self.capture.lock() = capture;
    }

    /// Records what stream 0 shows now (the test pattern stands in while a lost screen is
    /// being restored).
    pub fn set_capture_kind(&self, capture: CaptureKind) {
        *self.capture.lock() = capture;
    }

    pub fn running_streams(&self) -> u16 {
        self.streams.load(Ordering::SeqCst)
    }

    pub fn capture_kind(&self) -> CaptureKind {
        *self.capture.lock()
    }

    /// Called by the desktop capture: requests to pick another screen arrive here.
    pub fn register_reselect(&self) -> mpsc::UnboundedReceiver<ReselectRequest> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.reselect.lock() = Some(tx);
        rx
    }

    /// True when a desktop capture that can show the screen picker is running.
    pub fn can_choose_screen(&self) -> bool {
        self.reselect
            .lock()
            .as_ref()
            .is_some_and(|tx| !tx.is_closed())
    }

    /// Shows the screen-cast portal's picker (GNOME's share dialog) so the user can pick what
    /// stream 0 shows. Returns once they picked, or with why not (cancelled, no capture).
    pub async fn choose_screen(&self, timeout: Duration) -> Result<(), String> {
        let tx = self.reselect.lock().clone().ok_or_else(|| {
            "no screen capture is running (the daemon sends a test pattern or no video)".to_string()
        })?;
        let (reply, answer) = oneshot::channel();
        tx.send(ReselectRequest { reply })
            .map_err(|_| "the screen capture has stopped".to_string())?;
        match tokio::time::timeout(timeout, answer).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("the screen capture stopped before answering".into()),
            Err(_) => Err(format!(
                "no screen was chosen within {}s",
                timeout.as_secs()
            )),
        }
    }
}

/// The headset to send video to, or none while video is paused.
pub struct PausablePeers<P> {
    pub peers: P,
    pub control: Arc<VideoControl>,
}

impl<P: PeerProvider> PeerProvider for PausablePeers<P> {
    fn video_peer(&self, now: Instant) -> Option<IpAddr> {
        if self.control.is_paused() {
            None
        } else {
            self.peers.video_peer(now)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Always(IpAddr);

    impl PeerProvider for Always {
        fn video_peer(&self, _now: Instant) -> Option<IpAddr> {
            Some(self.0)
        }
    }

    #[test]
    fn paused_video_has_no_peer() {
        let control = VideoControl::new();
        let peers = PausablePeers {
            peers: Always("192.168.1.20".parse().unwrap()),
            control: control.clone(),
        };
        assert!(peers.video_peer(Instant::now()).is_some());
        control.set_paused(true);
        assert!(control.is_paused());
        assert_eq!(peers.video_peer(Instant::now()), None);
        control.set_paused(false);
        assert!(peers.video_peer(Instant::now()).is_some());
    }

    #[tokio::test]
    async fn choose_screen_needs_a_capture_and_returns_its_answer() {
        let control = VideoControl::new();
        assert!(!control.can_choose_screen());
        assert!(control
            .choose_screen(Duration::from_millis(10))
            .await
            .unwrap_err()
            .contains("no screen capture"));

        let mut requests = control.register_reselect();
        assert!(control.can_choose_screen());
        let capture = tokio::spawn(async move {
            let request = requests.recv().await.unwrap();
            request
                .reply
                .send(Err("the user cancelled".into()))
                .unwrap();
            requests
        });
        let answer = control.choose_screen(Duration::from_secs(5)).await;
        assert_eq!(answer, Err("the user cancelled".into()));

        // The capture went away: requests fail instead of hanging.
        drop(capture.await.unwrap());
        assert!(!control.can_choose_screen());
        assert!(control.choose_screen(Duration::from_secs(5)).await.is_err());
    }

    #[tokio::test]
    async fn an_unanswered_request_times_out() {
        let control = VideoControl::new();
        let _requests = control.register_reselect();
        let err = control
            .choose_screen(Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(err.contains("within"), "{err}");
    }
}

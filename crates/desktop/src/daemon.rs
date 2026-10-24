//! Talking to the daemon over its IPC socket, off the GTK main thread.
//!
//! Requests run on a small tokio runtime of their own; the UI awaits the returned handles from
//! `glib::spawn_future_local`, so nothing blocks the main loop.

use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;

use linux_quest_display::VirtualMonitor;
use linux_quest_host::tui::model::DashboardSnapshot;
use linux_quest_host::{
    send_daemon_request, DaemonRequest, DaemonResponse, DaemonStatus, HeadsetReport,
};
use linux_quest_protocol::{HeadsetSettingsPatch, ScreenPlacement};

/// Longest an ordinary request may take.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Choosing a screen waits for the user to answer GNOME's dialog.
pub const CHOOSE_SCREEN_TIMEOUT: Duration = Duration::from_secs(330);

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("lq-ipc")
            .enable_all()
            .build()
            .expect("tokio runtime for daemon requests")
    })
}

/// Runs `fut` on the request runtime. Await the result from the GTK main loop.
pub fn spawn<F, T>(fut: F) -> impl Future<Output = T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let handle = runtime().spawn(fut);
    async move {
        handle
            .await
            .unwrap_or_else(|e| panic!("daemon request task failed: {e}"))
    }
}

/// Why a request did not get the answer it wanted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonError {
    /// The socket did not answer: the daemon is not running.
    NotRunning,
    TimedOut,
    /// The daemon answered using an incompatible IPC schema.
    Incompatible,
    /// The daemon answered with an error.
    Refused(String),
    /// The daemon answered with something else (it is probably another version).
    Unexpected,
    /// The connection failed after it was made.
    Broken(String),
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRunning => write!(f, "the DeskQuest daemon is not running"),
            Self::TimedOut => write!(f, "the DeskQuest daemon did not answer in time"),
            Self::Incompatible => write!(f, "the DeskQuest app and daemon use incompatible versions; update both and restart the host service"),
            Self::Refused(why) => write!(f, "{why}"),
            Self::Unexpected => write!(
                f,
                "the daemon answered unexpectedly; is it a different version?"
            ),
            Self::Broken(why) => write!(f, "the daemon connection failed: {why}"),
        }
    }
}

/// Sends `req` and waits up to `timeout` for the answer.
pub async fn request(req: DaemonRequest, timeout: Duration) -> Result<DaemonResponse, DaemonError> {
    match tokio::time::timeout(timeout, send_daemon_request(req)).await {
        Err(_) => Err(DaemonError::TimedOut),
        Ok(Err(e)) => Err(request_error(e)),
        Ok(Ok(DaemonResponse::Error(why))) => Err(DaemonError::Refused(why)),
        Ok(Ok(resp)) => Ok(resp),
    }
}

fn request_error(e: Box<dyn std::error::Error + Send + Sync>) -> DaemonError {
    if e.downcast_ref::<bincode::ErrorKind>().is_some()
        || e.downcast_ref::<bincode::Error>().is_some()
    {
        return DaemonError::Incompatible;
    }
    let not_running = e.downcast_ref::<std::io::Error>().is_some_and(|io| {
        matches!(
            io.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
        )
    });
    if not_running {
        DaemonError::NotRunning
    } else {
        tracing::debug!(error = %e, "daemon request failed");
        DaemonError::Broken(e.to_string())
    }
}

/// Everything the pages show, fetched together once per refresh.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub status: Option<DaemonStatus>,
    pub dashboard: Option<DashboardSnapshot>,
    pub headset: Option<HeadsetReport>,
    pub monitors: Vec<VirtualMonitor>,
}

/// When an older daemon (without the Status request) was last seen.
static LEGACY_SEEN: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
/// How long to stop asking an older daemon for what it does not know.
const LEGACY_BACKOFF: Duration = Duration::from_secs(30);

fn legacy_daemon() -> bool {
    LEGACY_SEEN
        .lock()
        .ok()
        .and_then(|seen| *seen)
        .is_some_and(|at| at.elapsed() < LEGACY_BACKOFF)
}

fn mark_legacy_daemon() {
    if let Ok(mut seen) = LEGACY_SEEN.lock() {
        *seen = Some(std::time::Instant::now());
    }
}

/// Fetches a [`Snapshot`]. `Err` only when the daemon does not answer at all.
///
/// The dashboard snapshot comes first because every daemon version answers it. An older
/// daemon drops the connection on requests it does not know (Status, GetHeadset), so those
/// are optional.
pub async fn snapshot() -> Result<Snapshot, DaemonError> {
    let dashboard = match request(DaemonRequest::DashboardSnapshot, REQUEST_TIMEOUT).await? {
        DaemonResponse::DashboardSnapshot(snapshot) => Some(*snapshot),
        _ => None,
    };
    let status = if legacy_daemon() {
        None
    } else {
        match request(DaemonRequest::Status, REQUEST_TIMEOUT).await {
            Ok(DaemonResponse::Status(status)) => Some(*status),
            Err(DaemonError::Broken(_)) => {
                // An older daemon drops requests it does not know, and logs each one.
                mark_legacy_daemon();
                None
            }
            _ => None,
        }
    };
    let headset = match status {
        Some(_) => match request(DaemonRequest::GetHeadset, REQUEST_TIMEOUT).await {
            Ok(DaemonResponse::Headset(report)) => report.map(|r| *r),
            _ => None,
        },
        None => None,
    };
    let monitors = match request(DaemonRequest::List, REQUEST_TIMEOUT).await {
        Ok(DaemonResponse::Monitors(monitors)) => monitors,
        _ => Vec::new(),
    };
    Ok(Snapshot {
        status,
        dashboard,
        headset,
        monitors,
    })
}

fn headset_answer(resp: DaemonResponse) -> Result<Option<HeadsetReport>, DaemonError> {
    match resp {
        DaemonResponse::Headset(report) => Ok(report.map(|r| *r)),
        _ => Err(DaemonError::Unexpected),
    }
}

fn success(resp: DaemonResponse) -> Result<String, DaemonError> {
    match resp {
        DaemonResponse::Success(msg) => Ok(msg),
        _ => Err(DaemonError::Unexpected),
    }
}

/// Places the headset's screens; answers with what it applied.
pub async fn set_layout(
    screens: Vec<ScreenPlacement>,
) -> Result<Option<HeadsetReport>, DaemonError> {
    headset_answer(request(DaemonRequest::SetHeadsetLayout { screens }, REQUEST_TIMEOUT).await?)
}

/// Changes only the settings `patch` carries; answers with what the headset applied.
pub async fn patch_settings(
    patch: HeadsetSettingsPatch,
) -> Result<Option<HeadsetReport>, DaemonError> {
    headset_answer(
        request(
            DaemonRequest::PatchHeadsetSettings { patch },
            REQUEST_TIMEOUT,
        )
        .await?,
    )
}

pub async fn set_bitrate(mbps: u32, adaptive: bool) -> Result<String, DaemonError> {
    success(
        request(
            DaemonRequest::SetBitrate { mbps, adaptive },
            REQUEST_TIMEOUT,
        )
        .await?,
    )
}

pub async fn set_streaming(enabled: bool) -> Result<String, DaemonError> {
    success(request(DaemonRequest::SetStreaming { enabled }, REQUEST_TIMEOUT).await?)
}

/// Shows GNOME's share dialog; returns once the user picked or cancelled.
pub async fn choose_screen() -> Result<String, DaemonError> {
    success(request(DaemonRequest::ChooseScreen, CHOOSE_SCREEN_TIMEOUT).await?)
}

/// Sets the number of virtual monitors and their mode.
pub async fn set_monitor_count(
    count: u32,
    width: u32,
    height: u32,
    refresh: u32,
) -> Result<Vec<VirtualMonitor>, DaemonError> {
    let req = DaemonRequest::Toggle {
        count,
        width,
        height,
        refresh,
        dpi: 96,
        layout: linux_quest_display::DisplayLayoutMode::Horizontal,
    };
    match request(req, REQUEST_TIMEOUT).await? {
        DaemonResponse::Monitors(monitors) => Ok(monitors),
        _ => Err(DaemonError::Unexpected),
    }
}

pub async fn remove_monitor(id: u32) -> Result<String, DaemonError> {
    success(request(DaemonRequest::Remove { id }, REQUEST_TIMEOUT).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incompatible_response_is_not_reported_as_a_stopped_daemon() {
        // A response from a daemon with a different enum/schema cannot be decoded.
        fn decode_like_ipc() -> Result<DaemonResponse, Box<dyn std::error::Error + Send + Sync>> {
            Ok(bincode::deserialize(&99u32.to_le_bytes())?)
        }
        assert_eq!(
            request_error(decode_like_ipc().unwrap_err()),
            DaemonError::Incompatible
        );
        let error = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert_eq!(request_error(Box::new(error)), DaemonError::NotRunning);
        let error = std::io::Error::from(std::io::ErrorKind::ConnectionReset);
        assert!(matches!(
            request_error(Box::new(error)),
            DaemonError::Broken(_)
        ));
    }
}

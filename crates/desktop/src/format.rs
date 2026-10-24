//! Text the Status page and the window show, from the daemon's answers. Pure, so it is tested
//! without GTK.

use linux_quest_host::tui::model::DashboardSnapshot;
use linux_quest_host::{DaemonCapture, DaemonStatus, HeadsetReport};
use linux_quest_protocol::ClientConnectionMode;

/// How the headset is connected.
pub fn connection(status: &DaemonStatus) -> String {
    connection_mode(status.link)
}

/// `mode` in words.
pub fn connection_mode(mode: ClientConnectionMode) -> String {
    match mode {
        ClientConnectionMode::UsbAdb => "Connected over USB",
        ClientConnectionMode::Wifi => "Connected over Wi-Fi",
        ClientConnectionMode::Disconnected => "Not connected",
    }
    .to_string()
}

/// Whether video flows, and why not.
pub fn streaming(status: &DaemonStatus) -> String {
    if status.video_streams == 0 {
        "No video yet (screen capture may be waiting for permission)".to_string()
    } else if !status.streaming_enabled {
        "Paused".to_string()
    } else if status.link == ClientConnectionMode::Disconnected {
        "Waiting for the headset".to_string()
    } else {
        let screens = match status.video_streams {
            1 => "1 screen".to_string(),
            n => format!("{n} screens"),
        };
        format!("Streaming {screens}")
    }
}

/// What the desktop screen shows.
pub fn capture(status: &DaemonStatus) -> String {
    match status.capture {
        DaemonCapture::Screen => "Your screen (GNOME screen sharing)",
        DaemonCapture::TestPattern => "Test pattern",
        DaemonCapture::None => "Nothing",
    }
    .to_string()
}

/// Frames per second the headset renders.
pub fn fps(headset: Option<&HeadsetReport>) -> String {
    match headset {
        Some(h) if h.status.stats.render_fps > 0.0 => format!(
            "{:.0} fps at {:.0} Hz",
            h.status.stats.render_fps, h.status.settings.refresh_rate_hz
        ),
        Some(h) => format!("— at {:.0} Hz", h.status.settings.refresh_rate_hz),
        None => "—".to_string(),
    }
}

/// Latency the headset estimates, and its decode time.
pub fn latency(snapshot: Option<&DashboardSnapshot>) -> String {
    let Some(s) = snapshot else {
        return "—".to_string();
    };
    let total = s.video_latency.total_motion_to_photon_us;
    let decode = s.video_latency.client_decode_duration_us;
    match (total, decode) {
        (0, 0) => "—".to_string(),
        (0, d) => format!("decode {:.1} ms", f64::from(d) / 1000.0),
        (t, 0) => format!(
            "{:.0} ms est. (software, not glass-to-glass)",
            f64::from(t) / 1000.0
        ),
        (t, d) => format!(
            "{:.0} ms est. (software, not glass-to-glass), decode {:.1} ms",
            f64::from(t) / 1000.0,
            f64::from(d) / 1000.0
        ),
    }
}

/// Bitrate sent, against the ceiling.
pub fn bitrate(status: &DaemonStatus, snapshot: Option<&DashboardSnapshot>) -> String {
    let sent = snapshot.map_or(0.0, |s| s.client.bitrate_mbps);
    let mode = if status.adaptive_bitrate {
        "adaptive, up to"
    } else {
        "fixed at"
    };
    if status.bitrate_mbps == 0 {
        return "—".to_string();
    }
    format!("{sent:.1} Mbit/s ({mode} {} Mbit/s)", status.bitrate_mbps)
}

/// Packet loss the headset reports.
pub fn loss(snapshot: Option<&DashboardSnapshot>) -> String {
    let Some(s) = snapshot else {
        return "—".to_string();
    };
    let c = &s.client;
    if c.packets_recv + c.packets_lost == 0 {
        return "—".to_string();
    }
    format!(
        "{:.2} % ({} lost, {} recovered by FEC)",
        c.packet_loss_rate * 100.0,
        c.packets_lost,
        c.fec_recoveries
    )
}

/// `3725` seconds as `1 h 02 min`.
pub fn uptime(secs: u64) -> String {
    let (h, m) = (secs / 3600, secs % 3600 / 60);
    if h > 0 {
        format!("{h} h {m:02} min")
    } else if m > 0 {
        format!("{m} min")
    } else {
        format!("{secs} s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_quest_host::ipc::build_dashboard_snapshot;
    use linux_quest_host::link_status::{LinkTracker, DEFAULT_INPUT_LIVENESS};

    fn status() -> DaemonStatus {
        DaemonStatus {
            version: "0.1.0".into(),
            uptime_secs: 0,
            streaming_enabled: true,
            video_streams: 3,
            capture: DaemonCapture::Screen,
            can_choose_screen: true,
            bitrate_mbps: 30,
            adaptive_bitrate: true,
            link: ClientConnectionMode::Wifi,
            headset_connected: true,
            display_backend: "mock".into(),
        }
    }

    fn snapshot() -> DashboardSnapshot {
        build_dashboard_snapshot(
            vec![],
            LinkTracker::new(DEFAULT_INPUT_LIVENESS).status(std::time::Instant::now()),
            0,
        )
    }

    #[test]
    fn streaming_says_why_video_does_not_flow() {
        let mut s = status();
        assert_eq!(streaming(&s), "Streaming 3 screens");
        s.streaming_enabled = false;
        assert_eq!(streaming(&s), "Paused");
        s.streaming_enabled = true;
        s.link = ClientConnectionMode::Disconnected;
        assert_eq!(streaming(&s), "Waiting for the headset");
        assert_eq!(connection(&s), "Not connected");
        s.video_streams = 0;
        assert!(streaming(&s).starts_with("No video"));
    }

    #[test]
    fn numbers_read_naturally_and_unknowns_are_dashes() {
        let mut snap = snapshot();
        assert_eq!(latency(Some(&snap)), "—");
        assert_eq!(loss(Some(&snap)), "—");
        snap.video_latency.total_motion_to_photon_us = 31_400;
        snap.video_latency.client_decode_duration_us = 2_500;
        assert_eq!(
            latency(Some(&snap)),
            "31 ms est. (software, not glass-to-glass), decode 2.5 ms"
        );
        snap.client.packets_recv = 9_990;
        snap.client.packets_lost = 10;
        snap.client.packet_loss_rate = 0.001;
        snap.client.fec_recoveries = 4;
        assert_eq!(loss(Some(&snap)), "0.10 % (10 lost, 4 recovered by FEC)");
        snap.client.bitrate_mbps = 27.44;
        assert_eq!(
            bitrate(&status(), Some(&snap)),
            "27.4 Mbit/s (adaptive, up to 30 Mbit/s)"
        );
        assert_eq!(fps(None), "—");
        assert_eq!(uptime(42), "42 s");
        assert_eq!(uptime(3725), "1 h 02 min");
        assert_eq!(capture(&status()), "Your screen (GNOME screen sharing)");
    }
}

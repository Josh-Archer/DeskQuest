//! Smoke test of the settings channel against a running daemon and headset.
//!
//! Reads the headset's layout and settings, moves the desktop screen 10° to the left and back,
//! and toggles the performance overlay and back, printing what the headset reported after
//! each step and how long it took. Everything ends as it started.
//!
//! ```text
//! cargo run -p linux-quest-desktop --example headset_roundtrip
//! ```

use std::time::{Duration, Instant};

use linux_quest_host::{send_daemon_request, DaemonRequest, DaemonResponse, HeadsetReport};
use linux_quest_protocol::{HeadsetSettings, ScreenPlacement};

async fn ask(req: DaemonRequest) -> Result<(Option<HeadsetReport>, Duration), String> {
    let started = Instant::now();
    match send_daemon_request(req).await.map_err(|e| e.to_string())? {
        DaemonResponse::Headset(report) => Ok((report.map(|r| *r), started.elapsed())),
        DaemonResponse::Error(e) => Err(e),
        other => Err(format!("unexpected answer: {other:?}")),
    }
}

fn describe(s: &ScreenPlacement) -> String {
    format!(
        "display {}: {:+.1}° right, {:.2} m away, {:.2} m up, {:.2} x {:.2} m, {:?}",
        s.display_id,
        -s.yaw_rad().to_degrees(),
        s.horizontal_distance_m(),
        s.position[1],
        s.width_m,
        s.height_m,
        s.curve
    )
}

fn settings(s: &HeadsetSettings) -> String {
    format!(
        "bitrate {} Mbit/s ({}), {:?}, {:.0} Hz, curve {:?}, overlay {}, passthrough {}",
        s.bitrate_mbps,
        if s.adaptive_bitrate {
            "adaptive"
        } else {
            "fixed"
        },
        s.codec,
        s.refresh_rate_hz,
        s.curve,
        s.hud,
        s.passthrough
    )
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let (report, took) = ask(DaemonRequest::GetHeadset).await?;
    let report = report.ok_or("no headset on the settings channel")?;
    println!(
        "headset {} (app {}) via {}, reported {} ms ago; read in {took:?}",
        report.status.device, report.status.app_version, report.peer, report.age_ms
    );
    for s in &report.status.screens {
        println!("  {}", describe(s));
    }
    println!("  showing {:?}", report.status.showing);
    println!("  {}", settings(&report.status.settings));

    let original = *report.status.screen(0).ok_or("no desktop screen")?;
    let turned = original.moved_around_user(
        original.yaw_rad() + 10f32.to_radians(),
        original.horizontal_distance_m(),
    );
    let (after, took) = ask(DaemonRequest::SetHeadsetLayout {
        screens: vec![turned],
    })
    .await?;
    let moved = *after
        .as_ref()
        .and_then(|r| r.status.screen(0))
        .ok_or("no desktop screen after the move")?;
    println!(
        "moved 10° left, answered in {took:?}:\n  {}",
        describe(&moved)
    );
    let (after, took) = ask(DaemonRequest::SetHeadsetLayout {
        screens: vec![original],
    })
    .await?;
    let back = *after
        .as_ref()
        .and_then(|r| r.status.screen(0))
        .ok_or("no desktop screen after moving back")?;
    println!("moved back, answered in {took:?}:\n  {}", describe(&back));

    // The bitrate in a settings request also becomes the host's: send the host's own, as the
    // app does, so only the overlay changes.
    let status = match send_daemon_request(DaemonRequest::Status)
        .await
        .map_err(|e| e.to_string())?
    {
        DaemonResponse::Status(status) => *status,
        other => return Err(format!("unexpected answer: {other:?}")),
    };
    let before = HeadsetSettings {
        bitrate_mbps: status.bitrate_mbps,
        adaptive_bitrate: status.adaptive_bitrate,
        ..report.status.settings
    };
    let toggled = HeadsetSettings {
        hud: !before.hud,
        ..before
    };
    let (after, took) = ask(DaemonRequest::SetHeadsetSettings { settings: toggled }).await?;
    println!(
        "overlay {} -> {}, answered in {took:?}:\n  {}",
        before.hud,
        toggled.hud,
        after.map_or("no report".into(), |r| settings(&r.status.settings))
    );
    let (after, took) = ask(DaemonRequest::SetHeadsetSettings { settings: before }).await?;
    println!(
        "overlay back, answered in {took:?}:\n  {}",
        after.map_or("no report".into(), |r| settings(&r.status.settings))
    );
    Ok(())
}

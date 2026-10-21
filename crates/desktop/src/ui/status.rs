//! Status page: is the headset connected, and how is the stream doing.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use super::Shell;
use crate::daemon::{self, Snapshot};
use crate::format;

pub struct StatusPage {
    page: adw::PreferencesPage,
    connection: adw::ActionRow,
    device: adw::ActionRow,
    channel: adw::ActionRow,
    streaming: adw::ActionRow,
    stream_switch: adw::SwitchRow,
    capture: adw::ActionRow,
    fps: adw::ActionRow,
    latency: adw::ActionRow,
    bitrate: adw::ActionRow,
    loss: adw::ActionRow,
    daemon: adw::ActionRow,
    /// Set while the page writes the switch, so its handler does not send.
    updating: Rc<Cell<bool>>,
}

fn row(title: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).subtitle("—").build();
    row.add_css_class("property");
    row
}

impl StatusPage {
    pub fn new(shell: Shell) -> Self {
        let page = adw::PreferencesPage::new();

        let headset = adw::PreferencesGroup::builder().title("Headset").build();
        let connection = row("Connection");
        let device = row("Device");
        let channel = row("Settings channel");
        channel.set_tooltip_text(Some(
            "Whether the headset app answers the host's layout and settings requests",
        ));
        for r in [&connection, &device, &channel] {
            headset.add(r);
        }

        let stream = adw::PreferencesGroup::builder().title("Stream").build();
        let streaming = row("State");
        let stream_switch = adw::SwitchRow::builder()
            .title("Send Video to the Headset")
            .subtitle("Turn off to pause the stream; capture keeps running so it resumes at once")
            .active(true)
            .build();
        let capture = row("Desktop screen shows");
        let fps = row("Frame rate");
        let latency = row("Latency");
        let bitrate = row("Bitrate");
        let loss = row("Packet loss");
        for r in [&streaming, &capture, &fps, &latency, &bitrate, &loss] {
            stream.add(r);
        }
        stream.add(&stream_switch);

        let host = adw::PreferencesGroup::builder().title("Host").build();
        let daemon_row = row("Daemon");
        host.add(&daemon_row);

        page.add(&headset);
        page.add(&stream);
        page.add(&host);

        let updating = Rc::new(Cell::new(false));
        {
            let updating = updating.clone();
            stream_switch.connect_active_notify(move |switch| {
                if updating.get() {
                    return;
                }
                let enabled = switch.is_active();
                let shell = shell.clone();
                glib::spawn_future_local(async move {
                    match daemon::spawn(daemon::set_streaming(enabled)).await {
                        Ok(message) => shell.toast(&message),
                        Err(e) => shell.toast(&e.to_string()),
                    }
                    shell.refresh();
                });
            });
        }

        Self {
            page,
            connection,
            device,
            channel,
            streaming,
            stream_switch,
            capture,
            fps,
            latency,
            bitrate,
            loss,
            daemon: daemon_row,
            updating,
        }
    }

    pub fn widget(&self) -> &adw::PreferencesPage {
        &self.page
    }

    pub fn update(&self, snapshot: Option<&Snapshot>) {
        let Some(snap) = snapshot else {
            for r in [
                &self.connection,
                &self.device,
                &self.channel,
                &self.streaming,
                &self.capture,
                &self.fps,
                &self.latency,
                &self.bitrate,
                &self.loss,
            ] {
                r.set_subtitle("—");
            }
            self.daemon.set_subtitle("Not running");
            self.stream_switch.set_sensitive(false);
            return;
        };
        let Some(status) = snap.status.as_ref() else {
            // An older daemon: only its dashboard snapshot is there.
            let d = snap.dashboard.as_ref();
            self.connection
                .set_subtitle(&d.map_or("—".to_string(), |d| {
                    format::connection_mode(d.client.mode)
                }));
            self.streaming.set_subtitle(match d {
                Some(d) if d.streaming => "Streaming",
                Some(_) => "Not streaming",
                None => "—",
            });
            self.bitrate.set_subtitle(&d.map_or("—".to_string(), |d| {
                format!("{:.1} Mbit/s", d.client.bitrate_mbps)
            }));
            self.latency.set_subtitle(&format::latency(d));
            self.loss.set_subtitle(&format::loss(d));
            for r in [&self.device, &self.channel, &self.capture, &self.fps] {
                r.set_subtitle("—");
            }
            self.daemon.set_subtitle(&format!(
                "An older version{}: restart it from this build for headset layout, settings \
                 and screen choice",
                d.map_or(String::new(), |d| format!(
                    ", up {}",
                    format::uptime(d.uptime_secs)
                ))
            ));
            self.stream_switch.set_sensitive(false);
            return;
        };
        let headset = snap.headset.as_ref();
        let dashboard = snap.dashboard.as_ref();

        self.connection.set_subtitle(&format::connection(status));
        self.device.set_subtitle(&match headset {
            Some(h) => format!("{} (app {})", h.status.device, h.status.app_version),
            None => "—".to_string(),
        });
        self.channel.set_subtitle(&match headset {
            Some(h) => format!("Connected from {}", h.peer),
            None if status.headset_connected => "Connected".to_string(),
            None => "Not connected: layout and settings cannot be changed".to_string(),
        });
        self.streaming.set_subtitle(&format::streaming(status));
        self.capture.set_subtitle(&format::capture(status));
        self.fps.set_subtitle(&format::fps(headset));
        self.latency.set_subtitle(&format::latency(dashboard));
        self.bitrate
            .set_subtitle(&format::bitrate(status, dashboard));
        self.loss.set_subtitle(&format::loss(dashboard));
        self.daemon.set_subtitle(&format!(
            "Version {}, up {}, virtual monitors via {}",
            status.version,
            format::uptime(status.uptime_secs),
            status.display_backend
        ));

        self.updating.set(true);
        self.stream_switch.set_sensitive(status.video_streams > 0);
        self.stream_switch.set_active(status.streaming_enabled);
        self.updating.set(false);
    }
}

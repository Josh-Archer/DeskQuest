//! `linux-quest`: the desktop app for the Linux Quest host.
//!
//! It shows whether the headset is connected and how the stream is doing, lets you arrange the
//! headset's screens around you from above, change the headset's settings, and pick which
//! screen to share. It only talks to the daemon (`linux-quest-host daemon`) over its IPC
//! socket; the daemon does the work.
//!
//! Development aids, read from the environment:
//! - `LINUX_QUEST_PAGE=status|screens|settings` opens that page.
//! - `LINUX_QUEST_SCREENSHOT=<file.png>` renders the window into that file once the first data
//!   arrived, then quits. It draws the app itself, so no screen-sharing permission is needed.

mod daemon;
mod format;
mod topdown;
mod ui;

use adw::prelude::*;

/// Application id (also the D-Bus name that keeps the app single-instance).
pub const APP_ID: &str = "org.linuxquest.Desktop";

fn main() -> gtk::glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(ui::activate);
    // The app takes no command-line options; GApplication would reject unknown ones.
    app.run_with_args::<&str>(&[])
}

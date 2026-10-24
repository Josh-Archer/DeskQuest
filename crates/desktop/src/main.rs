//! `deskquest`: the desktop app for the DeskQuest host.
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
mod headset;
mod topdown;
mod ui;

use adw::prelude::*;

/// Application id (also the D-Bus name that keeps the app single-instance).
pub const APP_ID: &str = "org.deskquest.Desktop";

fn main() -> gtk::glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();
    gtk::gio::resources_register_include!("deskquest.gresource")
        .expect("could not register the desktop icon resource");
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| {
        gtk::Window::set_default_icon_name(APP_ID);
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::IconTheme::for_display(&display).add_resource_path("/org/deskquest/Desktop/icons");
            let css = gtk::CssProvider::new();
            css.load_from_resource("/org/deskquest/Desktop/theme.css");
            gtk::style_context_add_provider_for_display(
                &display,
                &css,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
            // Newer Adwaita uses CSS variables; Ubuntu 24.04's GTK uses named colours.
            if gtk::minor_version() >= 16 {
                let css = gtk::CssProvider::new();
                css.load_from_resource("/org/deskquest/Desktop/theme-modern.css");
                gtk::style_context_add_provider_for_display(
                    &display,
                    &css,
                    gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
                );
            }
        }
    });
    app.connect_activate(ui::activate);
    // The app takes no command-line options; GApplication would reject unknown ones.
    app.run_with_args::<&str>(&[])
}

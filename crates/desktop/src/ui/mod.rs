//! The window: a header bar with the page switcher and "Choose screen…", a banner while the
//! daemon is not running, and the three pages. Everything is refreshed from the daemon once a
//! second.

mod screens;
mod settings;
mod status;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;

use crate::daemon::{self, DaemonError, Snapshot};

/// How often the pages are refreshed.
const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// A refresh callback that is filled in once the window exists.
type RefreshSlot = Rc<RefCell<Option<Rc<dyn Fn()>>>>;

/// What pages use to report back to the window.
#[derive(Clone)]
pub struct Shell {
    toasts: adw::ToastOverlay,
    refresh_now: Rc<dyn Fn()>,
}

impl Shell {
    /// Shows a short message at the bottom of the window.
    pub fn toast(&self, message: &str) {
        let toast = adw::Toast::new(message);
        toast.set_timeout(4);
        self.toasts.add_toast(toast);
    }

    /// Refreshes every page now instead of at the next tick.
    pub fn refresh(&self) {
        (self.refresh_now)();
    }
}

struct Window {
    window: adw::ApplicationWindow,
    banner: adw::Banner,
    choose: gtk::Button,
    status: status::StatusPage,
    screens: Rc<screens::ScreensPage>,
    settings: Rc<settings::SettingsPage>,
    fetching: Cell<bool>,
    /// The screen picker is showing.
    choosing: Cell<bool>,
    last: RefCell<Option<Snapshot>>,
    /// Screenshot to take once data arrived (development aid).
    screenshot: RefCell<Option<std::path::PathBuf>>,
}

/// Builds the window, or presents the existing one.
pub fn activate(app: &adw::Application) {
    if let Some(window) = app.active_window() {
        window.present();
        return;
    }

    let toasts = adw::ToastOverlay::new();
    let stack = adw::ViewStack::new();
    let refresh_slot: RefreshSlot = Rc::new(RefCell::new(None));
    let shell = {
        let slot = refresh_slot.clone();
        Shell {
            toasts: toasts.clone(),
            refresh_now: Rc::new(move || {
                if let Some(refresh) = slot.borrow().as_ref() {
                    refresh();
                }
            }),
        }
    };

    let status = status::StatusPage::new(shell.clone());
    let screens = screens::ScreensPage::new(shell.clone());
    let settings = settings::SettingsPage::new(shell.clone());
    stack.add_titled_with_icon(
        status.widget(),
        Some("status"),
        "Status",
        "network-wireless-symbolic",
    );
    stack.add_titled_with_icon(
        screens.widget(),
        Some("screens"),
        "Screens",
        "video-display-symbolic",
    );
    stack.add_titled_with_icon(
        settings.widget(),
        Some("settings"),
        "Quest Settings",
        "preferences-system-symbolic",
    );

    let switcher = adw::ViewSwitcher::builder()
        .stack(&stack)
        .policy(adw::ViewSwitcherPolicy::Wide)
        .build();
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&switcher));
    let choose = gtk::Button::builder()
        .label("Choose Screen…")
        .tooltip_text("Pick the screen or window the headset shows (GNOME asks you which)")
        .build();
    choose.add_css_class("suggested-action");
    header.pack_start(&choose);

    let banner = adw::Banner::new("The Linux Quest daemon is not running");
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&banner);
    toasts.set_child(Some(&stack));
    toasts.set_vexpand(true);
    content.append(&toasts);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&content));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Linux Quest")
        .default_width(1100)
        .default_height(760)
        .content(&toolbar)
        .build();
    window.set_size_request(720, 520);

    if let Ok(page) = std::env::var("LINUX_QUEST_PAGE") {
        stack.set_visible_child_name(&page);
    }

    let this = Rc::new(Window {
        window: window.clone(),
        banner,
        choose,
        status,
        screens,
        settings,
        fetching: Cell::new(false),
        choosing: Cell::new(false),
        last: RefCell::new(None),
        screenshot: RefCell::new(
            std::env::var_os("LINUX_QUEST_SCREENSHOT").map(std::path::PathBuf::from),
        ),
    });

    {
        let weak = Rc::downgrade(&this);
        *refresh_slot.borrow_mut() = Some(Rc::new(move || {
            if let Some(this) = weak.upgrade() {
                this.refresh();
            }
        }));
    }
    {
        let weak = Rc::downgrade(&this);
        let shell = shell.clone();
        this.choose.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.choose_screen(&shell);
            }
        });
    }
    {
        let shell = shell.clone();
        this.banner
            .connect_button_clicked(move |_| start_service(&shell));
    }

    let weak = Rc::downgrade(&this);
    glib::timeout_add_local(REFRESH_INTERVAL, move || match weak.upgrade() {
        Some(this) => {
            this.refresh();
            glib::ControlFlow::Continue
        }
        None => glib::ControlFlow::Break,
    });
    this.refresh();
    window.present();
    if this.screenshot.borrow().is_some() {
        let weak = Rc::downgrade(&this);
        glib::timeout_add_local_once(Duration::from_secs(8), move || {
            if let Some(this) = weak.upgrade() {
                this.take_screenshot_if_asked();
            }
        });
    }
}

impl Window {
    /// Fetches everything from the daemon and updates the pages (one fetch at a time).
    fn refresh(self: &Rc<Self>) {
        if self.fetching.replace(true) {
            return;
        }
        let this = self.clone();
        glib::spawn_future_local(async move {
            let result = daemon::spawn(daemon::snapshot()).await;
            this.fetching.set(false);
            this.apply(result);
        });
    }

    fn apply(self: &Rc<Self>, result: Result<Snapshot, DaemonError>) {
        match result {
            Ok(snapshot) => {
                self.banner.set_revealed(false);
                self.status.update(Some(&snapshot));
                self.screens.update(Some(&snapshot));
                self.settings.update(Some(&snapshot));
                let can_choose = snapshot
                    .status
                    .as_ref()
                    .is_some_and(|s| s.can_choose_screen);
                self.choose
                    .set_sensitive(can_choose && !self.choosing.get());
                self.choose.set_tooltip_text(Some(if can_choose {
                    "Pick the screen or window the headset shows (GNOME asks you which)"
                } else {
                    "The daemon is not sharing a screen (it streams a test pattern or no video)"
                }));
                *self.last.borrow_mut() = Some(snapshot);
                self.take_screenshot_if_asked();
            }
            Err(e) => {
                tracing::warn!(error = %e, "no answer from the daemon");
                let installed = service_installed();
                self.banner.set_title(&if installed {
                    format!("{e}. Start it as a service?")
                } else {
                    format!(
                        "{e}. Install it with `linux-quest-host service install --enable --start`"
                    )
                });
                self.banner
                    .set_button_label(installed.then_some("Start Service"));
                self.banner.set_revealed(true);
                self.status.update(None);
                self.screens.update(None);
                self.settings.update(None);
                self.choose.set_sensitive(false);
                *self.last.borrow_mut() = None;
            }
        }
    }

    fn choose_screen(self: &Rc<Self>, shell: &Shell) {
        if self.choosing.replace(true) {
            return;
        }
        self.choose.set_sensitive(false);
        shell.toast("Pick a screen or window in the dialog GNOME shows");
        let this = self.clone();
        let shell = shell.clone();
        glib::spawn_future_local(async move {
            let result = daemon::spawn(daemon::choose_screen()).await;
            this.choosing.set(false);
            match result {
                Ok(message) => shell.toast(&message),
                Err(e) => shell.toast(&e.to_string()),
            }
            this.refresh();
        });
    }

    /// Renders the window to the requested PNG and quits (development aid). Called with the
    /// first data; without any, after a few seconds.
    fn take_screenshot_if_asked(self: &Rc<Self>) {
        let Some(path) = self.screenshot.borrow_mut().take() else {
            return;
        };
        let this = self.clone();
        // Give the pages a moment to lay out with the data.
        glib::timeout_add_local_once(Duration::from_millis(800), move || {
            match render_to_png(this.window.upcast_ref(), &path) {
                Ok(()) => tracing::info!(path = %path.display(), "screenshot saved"),
                Err(e) => tracing::error!(error = %e, "screenshot failed"),
            }
            this.window.close();
        });
    }
}

/// Draws `widget` into a PNG with the widget's own renderer.
fn render_to_png(widget: &gtk::Widget, path: &std::path::Path) -> Result<(), String> {
    let (width, height) = (widget.width(), widget.height());
    if width <= 0 || height <= 0 {
        return Err("the window has no size yet".into());
    }
    let paintable = gtk::WidgetPaintable::new(Some(widget));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, f64::from(width), f64::from(height));
    let node = snapshot.to_node().ok_or("nothing was drawn")?;
    let renderer = widget
        .native()
        .and_then(|n| n.renderer())
        .ok_or("the window has no renderer")?;
    let bounds = gtk::graphene::Rect::new(0.0, 0.0, width as f32, height as f32);
    let texture = renderer.render_texture(&node, Some(&bounds));
    texture.save_to_png(path).map_err(|e| e.to_string())
}

/// Whether a user unit for the daemon is installed (by `service install` or a package).
fn service_installed() -> bool {
    let user = linux_quest_host::service::user_unit_dir()
        .map(|d| d.join(linux_quest_host::service::DAEMON_UNIT));
    let system =
        std::path::Path::new("/usr/lib/systemd/user").join(linux_quest_host::service::DAEMON_UNIT);
    user.is_some_and(|p| p.exists()) || system.exists()
}

/// Starts the daemon's user service. Only offered while the daemon does not answer, so it
/// cannot start a second daemon next to a running one.
fn start_service(shell: &Shell) {
    let shell = shell.clone();
    glib::spawn_future_local(async move {
        let result = daemon::spawn(async {
            tokio::process::Command::new("systemctl")
                .args(["--user", "start", linux_quest_host::service::DAEMON_UNIT])
                .output()
                .await
        })
        .await;
        match result {
            Ok(out) if out.status.success() => shell.toast("Starting the daemon…"),
            Ok(out) => shell.toast(&format!(
                "Could not start the service: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Err(e) => shell.toast(&format!("Could not run systemctl: {e}")),
        }
        shell.refresh();
    });
}

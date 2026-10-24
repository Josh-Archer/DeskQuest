//! Quest Settings page: what the headset shows and how the stream is encoded.
//!
//! Changes are sent a moment after the last edit. With a headset on the settings channel they
//! go to the headset (and the bitrate also to the host's encoder), and the page then shows
//! what the headset applied. Without one, only the bitrate can change.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use linux_quest_protocol::{HeadsetCodec, HeadsetSettings, HeadsetSettingsPatch, ScreenCurve};

use super::Shell;
use crate::daemon::{self, Snapshot};

/// Wait after the last edit before sending.
const SEND_DELAY: Duration = Duration::from_millis(350);
const DEFAULT_RATES: [f32; 3] = [72.0, 90.0, 120.0];

pub struct SettingsPage {
    page: adw::PreferencesPage,
    offline: adw::PreferencesGroup,
    bitrate: adw::SpinRow,
    adaptive: adw::SwitchRow,
    codec: adw::ComboRow,
    refresh: adw::ComboRow,
    refresh_model: gtk::StringList,
    curved: adw::SwitchRow,
    radius: adw::SpinRow,
    hud: adw::SwitchRow,
    passthrough: adw::SwitchRow,
    headset_rows: Vec<gtk::Widget>,
    state: RefCell<State>,
    updating: Cell<bool>,
    /// What the rows show, so a refresh with the same values does not rewrite them.
    shown: Cell<Option<HeadsetSettings>>,
    shell: Shell,
}

#[derive(Default)]
struct State {
    /// What the headset last reported.
    reported: Option<HeadsetSettings>,
    rates: Vec<f32>,
    /// Bumped on each edit; a send happens only for the newest one.
    edit: u64,
    /// An edit was made and not yet answered; reports do not overwrite the rows meanwhile.
    dirty: bool,
}

impl SettingsPage {
    pub fn new(shell: Shell) -> Rc<Self> {
        let page = adw::PreferencesPage::new();

        let offline = adw::PreferencesGroup::builder()
            .description(
                "The headset is not on the settings channel. Start the DeskQuest app on the \
                 headset; it reaches the host over Wi-Fi or the USB cable. Until then only the \
                 host's bitrate can change.",
            )
            .build();

        let video = adw::PreferencesGroup::builder().title("Video").build();
        let bitrate = adw::SpinRow::with_range(5.0, 200.0, 5.0);
        bitrate.set_title("Bitrate");
        bitrate.set_subtitle("Mbit/s for all screens together");
        let adaptive = adw::SwitchRow::builder()
            .title("Adapt to the Network")
            .subtitle("Lower the bitrate when packets are lost or delayed")
            .build();
        let codec = adw::ComboRow::builder()
            .title("Preferred Codec")
            .subtitle("The headset's preference; the host's encoder picks the codec it streams")
            .model(&gtk::StringList::new(&["AV1", "HEVC"]))
            .build();
        video.add(&bitrate);
        video.add(&adaptive);
        video.add(&codec);

        let display = adw::PreferencesGroup::builder().title("Display").build();
        let refresh_model = gtk::StringList::new(&[]);
        let refresh = adw::ComboRow::builder()
            .title("Refresh Rate")
            .model(&refresh_model)
            .build();
        let curved = adw::SwitchRow::builder()
            .title("Curved Screens")
            .subtitle("Bend every screen around you; flat screens when off")
            .build();
        let radius = adw::SpinRow::with_range(0.5, 5.0, 0.1);
        radius.set_title("Curve Radius");
        radius.set_subtitle("Meters; larger is flatter");
        radius.set_digits(1);
        let hud = adw::SwitchRow::builder()
            .title("Performance Overlay")
            .subtitle("Frame rate and latency in view")
            .build();
        let passthrough = adw::SwitchRow::builder()
            .title("Passthrough")
            .subtitle("See your room behind the screens")
            .build();
        for r in [
            refresh.upcast_ref::<gtk::Widget>(),
            curved.upcast_ref(),
            radius.upcast_ref(),
            hud.upcast_ref(),
            passthrough.upcast_ref(),
        ] {
            display.add(r);
        }

        page.add(&offline);
        page.add(&video);
        page.add(&display);

        let headset_rows: Vec<gtk::Widget> = vec![
            codec.clone().upcast(),
            refresh.clone().upcast(),
            curved.clone().upcast(),
            radius.clone().upcast(),
            hud.clone().upcast(),
            passthrough.clone().upcast(),
        ];

        let this = Rc::new(Self {
            page,
            offline,
            bitrate,
            adaptive,
            codec,
            refresh,
            refresh_model,
            curved,
            radius,
            hud,
            passthrough,
            headset_rows,
            state: RefCell::new(State {
                rates: Vec::new(),
                ..Default::default()
            }),
            updating: Cell::new(false),
            shown: Cell::new(None),
            shell,
        });
        this.set_rates(&DEFAULT_RATES);
        this.connect_edits();
        this
    }

    pub fn widget(&self) -> &adw::PreferencesPage {
        &self.page
    }

    fn connect_edits(self: &Rc<Self>) {
        let on_edit = {
            let weak = Rc::downgrade(self);
            move || {
                if let Some(this) = weak.upgrade() {
                    this.edited();
                }
            }
        };
        let e = on_edit.clone();
        self.bitrate.connect_value_notify(move |_| e());
        let e = on_edit.clone();
        self.adaptive.connect_active_notify(move |_| e());
        let e = on_edit.clone();
        self.codec.connect_selected_notify(move |_| e());
        let e = on_edit.clone();
        self.refresh.connect_selected_notify(move |_| e());
        let e = on_edit.clone();
        self.curved.connect_active_notify(move |_| e());
        let e = on_edit.clone();
        self.radius.connect_value_notify(move |_| e());
        let e = on_edit.clone();
        self.hud.connect_active_notify(move |_| e());
        let e = on_edit;
        self.passthrough.connect_active_notify(move |_| e());
    }

    fn set_rates(&self, rates: &[f32]) {
        let mut state = self.state.borrow_mut();
        if state.rates == rates {
            return;
        }
        state.rates = rates.to_vec();
        let labels: Vec<String> = rates.iter().map(|r| format!("{r:.0} Hz")).collect();
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        self.refresh_model
            .splice(0, self.refresh_model.n_items(), &labels);
    }

    /// The settings the rows show now.
    fn read_rows(&self, base: Option<HeadsetSettings>) -> HeadsetSettings {
        let rates = self.state.borrow().rates.clone();
        let refresh = rates
            .get(self.refresh.selected() as usize)
            .copied()
            .or(base.map(|b| b.refresh_rate_hz))
            .unwrap_or(90.0);
        HeadsetSettings {
            bitrate_mbps: self.bitrate.value().round() as u32,
            adaptive_bitrate: self.adaptive.is_active(),
            codec: if self.codec.selected() == 1 {
                HeadsetCodec::Hevc
            } else {
                HeadsetCodec::Av1
            },
            refresh_rate_hz: refresh,
            curve: if self.curved.is_active() {
                ScreenCurve::Curved {
                    radius_m: self.radius.value() as f32,
                }
            } else {
                ScreenCurve::Flat
            },
            hud: self.hud.is_active(),
            passthrough: self.passthrough.is_active(),
        }
    }

    fn edited(self: &Rc<Self>) {
        self.radius
            .set_sensitive(self.curved.is_active() && self.curved.is_sensitive());
        if self.updating.get() {
            return;
        }
        let edit = {
            let mut state = self.state.borrow_mut();
            state.edit += 1;
            state.dirty = true;
            state.edit
        };
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(SEND_DELAY, move || {
            if let Some(this) = weak.upgrade() {
                if this.state.borrow().edit == edit {
                    this.send(edit);
                }
            }
        });
    }

    fn send(self: &Rc<Self>, edit: u64) {
        let reported = self.state.borrow().reported;
        let wanted = self.read_rows(reported);
        let this = self.clone();
        glib::spawn_future_local(async move {
            let result = match reported {
                // Send only what the user changed, so one row cannot reset the others (the
                // headset's bitrate in particular) to stale or host-side values.
                Some(reported) => daemon::spawn(daemon::patch_settings(
                    HeadsetSettingsPatch::between(&reported, &wanted),
                ))
                .await
                .map(|report| report.map(|r| r.status.settings)),
                // No headset: only the host's bitrate can change.
                None => daemon::spawn(daemon::set_bitrate(
                    wanted.bitrate_mbps,
                    wanted.adaptive_bitrate,
                ))
                .await
                .map(|_| None),
            };
            let newest = this.state.borrow().edit == edit;
            if newest {
                this.state.borrow_mut().dirty = false;
            }
            match result {
                Ok(Some(applied)) if newest => {
                    this.show(&applied);
                    if applied != wanted {
                        this.shell
                            .toast("The headset adjusted some values to what it supports");
                    }
                }
                Ok(_) => {}
                Err(e) => this.shell.toast(&format!("Settings not applied: {e}")),
            }
            this.shell.refresh();
        });
    }

    /// Writes `settings` into the rows without sending them.
    fn show(&self, settings: &HeadsetSettings) {
        if self.shown.get() == Some(*settings) {
            return;
        }
        self.shown.set(Some(*settings));
        self.updating.set(true);
        self.bitrate.set_value(f64::from(settings.bitrate_mbps));
        self.adaptive.set_active(settings.adaptive_bitrate);
        self.codec.set_selected(match settings.codec {
            HeadsetCodec::Av1 => 0,
            HeadsetCodec::Hevc => 1,
        });
        let rates = self.state.borrow().rates.clone();
        if let Some(i) = rates
            .iter()
            .position(|r| (r - settings.refresh_rate_hz).abs() < 0.5)
        {
            self.refresh.set_selected(i as u32);
        }
        match settings.curve {
            ScreenCurve::Flat => self.curved.set_active(false),
            ScreenCurve::Curved { radius_m } => {
                self.curved.set_active(true);
                self.radius.set_value(f64::from(radius_m));
            }
        }
        self.hud.set_active(settings.hud);
        self.passthrough.set_active(settings.passthrough);
        self.updating.set(false);
        self.radius
            .set_sensitive(self.curved.is_active() && self.curved.is_sensitive());
    }

    pub fn update(&self, snapshot: Option<&Snapshot>) {
        let status = snapshot.and_then(|s| s.status.as_ref());
        let headset = snapshot.and_then(|s| s.headset.as_ref());
        let settings = headset.map(|h| h.status.settings);
        let dirty = self.state.borrow().dirty;
        self.state.borrow_mut().reported = settings;

        self.page.set_sensitive(status.is_some());
        self.offline.set_visible(headset.is_none());
        for row in &self.headset_rows {
            row.set_sensitive(headset.is_some());
        }
        if dirty {
            return;
        }
        if let Some(h) = headset {
            let rates = if h.status.refresh_rates_hz.is_empty() {
                DEFAULT_RATES.to_vec()
            } else {
                h.status.refresh_rates_hz.clone()
            };
            self.updating.set(true);
            self.set_rates(&rates);
            self.updating.set(false);
        }
        match (settings, status) {
            (Some(settings), Some(status)) => {
                // The host's encoder is what really streams: show its bitrate.
                let mut shown = settings;
                if status.bitrate_mbps > 0 {
                    shown.bitrate_mbps = status.bitrate_mbps;
                    shown.adaptive_bitrate = status.adaptive_bitrate;
                }
                self.show(&shown);
            }
            (None, Some(status)) if status.bitrate_mbps > 0 => {
                self.shown.set(None);
                self.updating.set(true);
                self.bitrate.set_value(f64::from(status.bitrate_mbps));
                self.adaptive.set_active(status.adaptive_bitrate);
                self.updating.set(false);
            }
            _ => {}
        }
        self.radius
            .set_sensitive(self.curved.is_active() && headset.is_some());
    }
}

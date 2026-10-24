//! Screens page: the headset's screens seen from above, arranged around you.
//!
//! Drag a screen to move it around you (it keeps facing you), drag one of its ends to resize
//! it, or scroll over it to scale it. The sidebar edits the selected screen exactly. Changes
//! go to the headset while you drag (one request at a time, newest wins), and the page then
//! shows what the headset applied. Below that, the host's virtual monitors can be added,
//! removed or given another resolution and refresh rate.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::glib;
use linux_quest_display::VirtualMonitor;
use linux_quest_protocol::{ScreenCurve, ScreenPlacement};

use super::Shell;
use crate::daemon::{self, Snapshot};
use crate::headset::{self, Identity, Model, UsbDetection};
use crate::topdown::{self, Part, View};

/// Pixels around the drawing kept free.
const PAD: f64 = 56.0;
/// How close (in pixels) the pointer must be to a screen to grab it.
const GRAB_PX: f64 = 14.0;
/// After a local edit, reports from the headset are not shown for this long, so a report
/// that crossed the edit does not make the screen jump back.
const EDIT_SETTLE: Duration = Duration::from_millis(1500);
/// Adwaita's blue, for the selected screen.
const ACCENT: (f64, f64, f64) = (0.0, 100.0 / 255.0, 237.0 / 255.0);

const RESOLUTIONS: [(u32, u32); 3] = [(1920, 1080), (2560, 1440), (3840, 2160)];
const MONITOR_RATES: [u32; 4] = [60, 72, 90, 120];
/// The host creates at most this many virtual monitors.
const MAX_MONITORS: usize = 3;

struct Drag {
    part: Part,
    start: (f64, f64),
    original: ScreenPlacement,
}

#[derive(Default)]
struct State {
    screens: Vec<ScreenPlacement>,
    showing: Vec<u32>,
    selected: Option<u32>,
    hovered: Option<u32>,
    pointer: Option<(f64, f64)>,
    drag: Option<Drag>,
    /// The view is kept still while dragging.
    frozen_view: Option<View>,
    pending: Option<ScreenPlacement>,
    in_flight: bool,
    last_edit: Option<Instant>,
    connected: bool,
    monitors: Vec<VirtualMonitor>,
    backend: String,
    headset: Option<Identity>,
}

#[derive(PartialEq)]
struct MonitorList {
    monitors: Vec<VirtualMonitor>,
    backend: String,
    daemon_running: bool,
}

struct Rows {
    group: adw::PreferencesGroup,
    angle: adw::SpinRow,
    distance: adw::SpinRow,
    height: adw::SpinRow,
    tilt: adw::SpinRow,
    width: adw::SpinRow,
    curved: adw::SwitchRow,
    radius: adw::SpinRow,
    video: adw::ActionRow,
}

pub struct ScreensPage {
    root: adw::BreakpointBin,
    selector: gtk::DropDown,
    screen_names: gtk::StringList,
    screen_ids: RefCell<Vec<u32>>,
    stack: gtk::Stack,
    empty: adw::StatusPage,
    empty_preview: headset::Preview,
    device_button: gtk::Button,
    heading: gtk::Label,
    usb: RefCell<UsbDetection>,
    area: gtk::DrawingArea,
    rows: Rows,
    monitors_group: adw::PreferencesGroup,
    monitor_rows: RefCell<Vec<adw::ActionRow>>,
    shown_monitors: RefCell<Option<MonitorList>>,
    resolution: adw::ComboRow,
    monitor_rate: adw::ComboRow,
    add_monitor: gtk::Button,
    apply_mode: gtk::Button,
    state: RefCell<State>,
    updating: Cell<bool>,
    /// What the sidebar shows, so a refresh with the same values does not rewrite the rows
    /// (and what the user is typing).
    shown: Cell<Option<(ScreenPlacement, bool)>>,
    shell: Shell,
}

fn spin(title: &str, subtitle: &str, min: f64, max: f64, step: f64, digits: u32) -> adw::SpinRow {
    let row = adw::SpinRow::with_range(min, max, step);
    row.set_title(title);
    row.set_subtitle(subtitle);
    row.set_digits(digits);
    row
}

impl ScreensPage {
    pub fn new(shell: Shell) -> Rc<Self> {
        let area = gtk::DrawingArea::builder()
            .hexpand(true)
            .vexpand(true)
            .focusable(true)
            .build();
        let hint = gtk::Label::builder()
            .label(
                "Seen from above, the headset marks your layout origin. Drag a screen to move it, drag \
                 an end to resize it, scroll over it to scale it.",
            )
            .wrap(true)
            .justify(gtk::Justification::Center)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        hint.add_css_class("dim-label");
        let canvas = gtk::Box::new(gtk::Orientation::Vertical, 0);
        canvas.append(&area);
        canvas.append(&hint);

        let empty = adw::StatusPage::builder()
            .icon_name("video-display-symbolic")
            .title("No Headset Layout")
            .description(
                "Start the DeskQuest app on the headset. Its screens appear here once it \
                 reports them to the host.",
            )
            .build();
        let empty_preview = headset::Preview::new();
        empty_preview.widget().set_visible(false);
        empty.set_child(Some(empty_preview.widget()));
        let stack = gtk::Stack::new();
        stack.add_named(&canvas, Some("canvas"));
        stack.add_named(&empty, Some("empty"));
        stack.set_visible_child_name("empty");
        stack.set_hexpand(true);

        // Sidebar: the selected screen, then the host's virtual monitors.
        let group = adw::PreferencesGroup::builder().title("Screen").build();
        let rows = Rows {
            angle: spin(
                "Direction",
                "Degrees to the right of straight ahead (negative: left)",
                -120.0,
                120.0,
                1.0,
                0,
            ),
            distance: spin("Distance", "Meters from you", 0.3, 5.0, 0.05, 2),
            height: spin(
                "Height",
                "Center above the floor, meters",
                0.2,
                3.0,
                0.05,
                2,
            ),
            tilt: spin("Tilt", "Degrees up (negative: down)", -60.0, 60.0, 1.0, 0),
            width: spin(
                "Width",
                "Meters; the height follows the picture's shape",
                0.2,
                5.0,
                0.05,
                2,
            ),
            curved: adw::SwitchRow::builder().title("Curved").build(),
            radius: spin(
                "Curve Radius",
                "Meters; larger is flatter",
                0.5,
                5.0,
                0.1,
                1,
            ),
            video: adw::ActionRow::builder()
                .title("Video")
                .subtitle("—")
                .build(),
            group,
        };
        rows.video.add_css_class("property");
        for w in [
            rows.angle.upcast_ref::<gtk::Widget>(),
            rows.distance.upcast_ref(),
            rows.height.upcast_ref(),
            rows.tilt.upcast_ref(),
            rows.width.upcast_ref(),
            rows.curved.upcast_ref(),
            rows.radius.upcast_ref(),
            rows.video.upcast_ref(),
        ] {
            rows.group.add(w);
        }

        let monitors_group = adw::PreferencesGroup::builder()
            .title("Virtual Monitors")
            .build();
        let resolution = adw::ComboRow::builder()
            .title("Resolution")
            .model(&gtk::StringList::new(&[
                "1920 × 1080",
                "2560 × 1440",
                "3840 × 2160",
            ]))
            .selected(1)
            .build();
        let monitor_rate = adw::ComboRow::builder()
            .title("Refresh Rate")
            .model(&gtk::StringList::new(&[
                "60 Hz", "72 Hz", "90 Hz", "120 Hz",
            ]))
            .selected(2)
            .build();
        let add_monitor = gtk::Button::with_label("Add Monitor");
        add_monitor.add_css_class("pill");
        let apply_mode = gtk::Button::with_label("Apply to All");
        apply_mode.set_tooltip_text(Some(
            "Give every virtual monitor this resolution and refresh rate",
        ));
        let buttons = gtk::Box::builder()
            .spacing(12)
            .halign(gtk::Align::Center)
            .margin_top(12)
            .build();
        buttons.append(&apply_mode);
        buttons.append(&add_monitor);
        let mode_group = adw::PreferencesGroup::new();
        mode_group.add(&resolution);
        mode_group.add(&monitor_rate);

        let sidebar_page = adw::PreferencesPage::new();
        sidebar_page.add(&rows.group);
        sidebar_page.add(&monitors_group);
        sidebar_page.add(&mode_group);
        let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
        sidebar.append(&sidebar_page);
        sidebar_page.set_vexpand(true);
        sidebar.append(&buttons);
        buttons.set_margin_bottom(18);
        sidebar.add_css_class("layout-sidebar");

        let screen_names = gtk::StringList::new(&[]);
        let selector = gtk::DropDown::builder()
            .model(&screen_names)
            .tooltip_text("Select the screen to arrange")
            .build();
        selector.set_sensitive(false);
        let heading = gtk::Label::builder()
            .label("Headset Layout")
            .halign(gtk::Align::Start)
            .hexpand(true)
            .build();
        heading.add_css_class("heading");
        let controls = gtk::ToggleButton::builder()
            .label("Controls")
            .tooltip_text("Show screen settings and virtual monitors")
            .active(true)
            .visible(false)
            .build();
        let tools = gtk::Box::builder()
            .spacing(12)
            .margin_top(16)
            .margin_bottom(12)
            .margin_start(20)
            .margin_end(20)
            .build();
        let device_button = gtk::Button::builder()
            .label("Headset")
            .tooltip_text("Inspect the detected headset in 3D")
            .visible(false)
            .build();
        device_button.add_css_class("flat");
        if let Some(label) = device_button.child().and_downcast::<gtk::Label>() {
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            label.set_max_width_chars(22);
        }
        // The device button provides the model name, leaving room for the selector on
        // compact windows without adding another row of controls.
        tools.append(&heading);
        device_button.set_hexpand(true);
        device_button.set_halign(gtk::Align::Start);
        tools.append(&device_button);
        tools.append(&selector);
        tools.append(&controls);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&tools);
        content.append(&stack);
        stack.set_vexpand(true);

        let close = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Hide layout controls")
            .visible(false)
            .build();
        close.add_css_class("flat");
        let sidebar_title = gtk::Label::builder()
            .label("Layout Controls")
            .halign(gtk::Align::Start)
            .hexpand(true)
            .build();
        sidebar_title.add_css_class("heading");
        let sidebar_header = gtk::Box::builder()
            .spacing(12)
            .margin_top(12)
            .margin_bottom(6)
            .margin_start(24)
            .margin_end(12)
            .build();
        sidebar_header.append(&sidebar_title);
        sidebar_header.append(&close);
        sidebar.prepend(&sidebar_header);
        let split = adw::OverlaySplitView::builder()
            .content(&content)
            .sidebar(&sidebar)
            .sidebar_position(gtk::PackType::End)
            .min_sidebar_width(360.0)
            .max_sidebar_width(400.0)
            .sidebar_width_fraction(0.36)
            .pin_sidebar(true)
            .show_sidebar(true)
            .build();
        controls
            .bind_property("active", &split, "show-sidebar")
            .bidirectional()
            .sync_create()
            .build();
        let weak_split = split.downgrade();
        close.connect_clicked(move |_| {
            if let Some(split) = weak_split.upgrade() {
                split.set_show_sidebar(false);
            }
        });
        let compact = adw::Breakpoint::new(
            adw::BreakpointCondition::parse("max-width: 920px").expect("layout breakpoint"),
        );
        compact.add_setter(&split, "collapsed", Some(&true.to_value()));
        compact.add_setter(&split, "show-sidebar", Some(&false.to_value()));
        compact.add_setter(&controls, "visible", Some(&true.to_value()));
        compact.add_setter(&close, "visible", Some(&true.to_value()));
        let root = adw::BreakpointBin::builder()
            .child(&split)
            .width_request(360)
            .height_request(300)
            .build();
        root.add_breakpoint(compact);

        let this = Rc::new(Self {
            root,
            selector,
            screen_names,
            screen_ids: RefCell::new(Vec::new()),
            stack,
            empty,
            empty_preview,
            device_button,
            heading,
            usb: RefCell::new(UsbDetection::None),
            area,
            rows,
            monitors_group,
            monitor_rows: RefCell::new(Vec::new()),
            shown_monitors: RefCell::new(None),
            resolution,
            monitor_rate,
            add_monitor,
            apply_mode,
            state: RefCell::new(State::default()),
            updating: Cell::new(false),
            shown: Cell::new(None),
            shell,
        });
        let weak = Rc::downgrade(&this);
        this.selector.connect_selected_notify(move |selector| {
            if let Some(this) = weak.upgrade() {
                let id = this
                    .screen_ids
                    .borrow()
                    .get(selector.selected() as usize)
                    .copied();
                if let Some(id) = id {
                    this.state.borrow_mut().selected = Some(id);
                    this.show_selected();
                    this.area.queue_draw();
                }
            }
        });
        let weak = Rc::downgrade(&this);
        this.device_button.connect_clicked(move |button| {
            if let Some(this) = weak.upgrade() {
                if let Some(identity) = this.state.borrow().headset.as_ref() {
                    headset::show_details(button, identity);
                }
            }
        });
        this.connect_canvas();
        this.connect_rows();
        this.connect_monitor_buttons();
        this.show_selected();
        this
    }

    pub fn widget(&self) -> &adw::BreakpointBin {
        &self.root
    }

    pub fn set_usb_detection(&self, detection: UsbDetection) {
        *self.usb.borrow_mut() = detection;
    }

    // ---- data in -------------------------------------------------------------------------

    pub fn update(self: &Rc<Self>, snapshot: Option<&Snapshot>) {
        let headset = snapshot.and_then(|s| s.headset.as_ref());
        let identity = Identity::detected(snapshot, &self.usb.borrow());
        self.device_button.set_visible(identity.is_some());
        self.heading.set_visible(identity.is_none());
        if let Some(identity) = &identity {
            self.device_button
                .set_label(&format!("{} · 3D", identity.name));
            self.device_button
                .set_tooltip_text(Some(&format!("{} · View 3D model", identity.connection)));
            self.empty_preview.set_model(identity.model);
            self.empty_preview.widget().set_visible(true);
            self.empty.set_icon_name(None);
            self.empty.set_title(&format!("{} detected", identity.name));
            self.empty.set_description(Some(if identity.app_connected {
                "Waiting for the headset to report its screen layout."
            } else {
                "USB is available. Start DeskQuest on the headset and connect the host to see its screen layout."
            }));
        } else {
            self.empty_preview.widget().set_visible(false);
            self.empty.set_icon_name(Some("video-display-symbolic"));
            self.empty.set_title("No Headset Layout");
            self.empty.set_description(Some(if *self.usb.borrow() == UsbDetection::Multiple {
                "Multiple Quest headsets are visible over USB. Connect one through its DeskQuest app to select its layout."
            } else {
                "Start DeskQuest on the headset. Its model and screens appear here when it connects."
            }));
        }
        {
            let mut st = self.state.borrow_mut();
            st.connected = headset.is_some();
            st.headset = identity;
            let settled = st.drag.is_none()
                && !st.in_flight
                && st.pending.is_none()
                && st.last_edit.is_none_or(|t| t.elapsed() >= EDIT_SETTLE);
            if let Some(h) = headset {
                if settled {
                    st.screens = h.status.screens.clone();
                }
                st.showing = h.status.showing.clone();
            } else {
                st.screens.clear();
                st.showing.clear();
            }
            if st
                .selected
                .is_none_or(|id| !st.screens.iter().any(|s| s.display_id == id))
            {
                st.selected = st.screens.first().map(|s| s.display_id);
            }
            st.monitors = snapshot.map(|s| s.monitors.clone()).unwrap_or_default();
            st.backend = snapshot
                .and_then(|s| s.status.as_ref())
                .map(|s| s.display_backend.clone())
                .unwrap_or_else(|| "the host display backend".to_string());
        }
        let connected = self.state.borrow().connected;
        self.stack
            .set_visible_child_name(if connected { "canvas" } else { "empty" });
        self.show_selected();
        self.show_monitors(snapshot.is_some());
        self.area.queue_draw();
    }

    // ---- sending -------------------------------------------------------------------------

    /// Shows `placement` right away and sends it to the headset.
    fn push(self: &Rc<Self>, placement: ScreenPlacement) {
        let send_now = {
            let mut st = self.state.borrow_mut();
            if let Some(s) = st
                .screens
                .iter_mut()
                .find(|s| s.display_id == placement.display_id)
            {
                *s = placement;
            }
            st.pending = Some(placement);
            st.last_edit = Some(Instant::now());
            !st.in_flight
        };
        self.area.queue_draw();
        if send_now {
            self.send_next();
        }
    }

    fn send_next(self: &Rc<Self>) {
        let Some(placement) = ({
            let mut st = self.state.borrow_mut();
            let next = st.pending.take();
            st.in_flight = next.is_some();
            next
        }) else {
            return;
        };
        let this = self.clone();
        glib::spawn_future_local(async move {
            let result = daemon::spawn(daemon::set_layout(vec![placement])).await;
            let more = {
                let mut st = this.state.borrow_mut();
                st.in_flight = false;
                if let (Ok(Some(report)), None, None) = (&result, &st.pending, &st.drag) {
                    st.screens = report.status.screens.clone();
                }
                st.pending.is_some()
            };
            if let Err(e) = result {
                this.shell.toast(&format!("Screen not moved: {e}"));
            }
            this.show_selected();
            this.area.queue_draw();
            if more {
                this.send_next();
            }
        });
    }

    fn selected_placement(&self) -> Option<ScreenPlacement> {
        let st = self.state.borrow();
        st.selected
            .and_then(|id| st.screens.iter().find(|s| s.display_id == id).copied())
    }

    // ---- canvas --------------------------------------------------------------------------

    fn view(&self, width: f64, height: f64) -> View {
        let st = self.state.borrow();
        st.frozen_view
            .unwrap_or_else(|| View::fit(width, height, PAD, &st.screens))
    }

    fn current_view(&self) -> View {
        self.view(f64::from(self.area.width()), f64::from(self.area.height()))
    }

    fn connect_canvas(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.area.set_draw_func(move |area, cr, w, h| {
            if let Some(this) = weak.upgrade() {
                this.draw(area, cr, f64::from(w), f64::from(h));
            }
        });

        let click = gtk::GestureClick::new();
        let weak = Rc::downgrade(self);
        click.connect_pressed(move |_, _, x, y| {
            if let Some(this) = weak.upgrade() {
                let view = this.current_view();
                let hit = {
                    let st = this.state.borrow();
                    topdown::hit(&view, &st.screens, (x, y), GRAB_PX)
                        .map(|(i, _)| st.screens[i].display_id)
                };
                if let Some(id) = hit {
                    this.state.borrow_mut().selected = Some(id);
                    this.show_selected();
                    this.area.queue_draw();
                }
                this.area.grab_focus();
            }
        });
        self.area.add_controller(click);

        let drag = gtk::GestureDrag::new();
        let weak = Rc::downgrade(self);
        drag.connect_drag_begin(move |gesture, x, y| {
            let Some(this) = weak.upgrade() else { return };
            let view = this.current_view();
            let mut st = this.state.borrow_mut();
            match topdown::hit(&view, &st.screens, (x, y), GRAB_PX) {
                Some((i, part)) => {
                    let original = st.screens[i];
                    st.selected = Some(original.display_id);
                    st.drag = Some(Drag {
                        part,
                        start: (x, y),
                        original,
                    });
                    st.frozen_view = Some(view);
                }
                None => {
                    gesture.set_state(gtk::EventSequenceState::Denied);
                }
            }
        });
        let weak = Rc::downgrade(self);
        drag.connect_drag_update(move |_, dx, dy| {
            let Some(this) = weak.upgrade() else { return };
            let next = {
                let st = this.state.borrow();
                let (Some(d), Some(view)) = (&st.drag, &st.frozen_view) else {
                    return;
                };
                let world = view.to_world((d.start.0 + dx, d.start.1 + dy));
                match d.part {
                    Part::Body => topdown::drag_to(&d.original, world),
                    Part::Edge => topdown::resize_to(&d.original, world),
                }
            };
            this.push(next);
            this.show_selected();
        });
        let weak = Rc::downgrade(self);
        drag.connect_drag_end(move |_, _, _| {
            if let Some(this) = weak.upgrade() {
                let mut st = this.state.borrow_mut();
                st.drag = None;
                st.frozen_view = None;
                st.last_edit = Some(Instant::now());
                drop(st);
                this.area.queue_draw();
            }
        });
        self.area.add_controller(drag);

        let motion = gtk::EventControllerMotion::new();
        let weak = Rc::downgrade(self);
        motion.connect_motion(move |_, x, y| {
            let Some(this) = weak.upgrade() else { return };
            let view = this.current_view();
            let (hovered, cursor) = {
                let mut st = this.state.borrow_mut();
                st.pointer = Some((x, y));
                let hit = topdown::hit(&view, &st.screens, (x, y), GRAB_PX);
                let cursor = match (&st.drag, hit) {
                    (Some(d), _) if d.part == Part::Edge => "ew-resize",
                    (Some(_), _) => "grabbing",
                    (None, Some((_, Part::Edge))) => "ew-resize",
                    (None, Some((_, Part::Body))) => "grab",
                    (None, None) => "default",
                };
                (hit.map(|(i, _)| st.screens[i].display_id), cursor)
            };
            this.area.set_cursor_from_name(Some(cursor));
            let changed = this.state.borrow().hovered != hovered;
            if changed {
                this.state.borrow_mut().hovered = hovered;
                this.area.queue_draw();
            }
        });
        let weak = Rc::downgrade(self);
        motion.connect_leave(move |_| {
            if let Some(this) = weak.upgrade() {
                let mut st = this.state.borrow_mut();
                st.pointer = None;
                st.hovered = None;
                drop(st);
                this.area.queue_draw();
            }
        });
        self.area.add_controller(motion);

        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        let weak = Rc::downgrade(self);
        scroll.connect_scroll(move |_, _, dy| {
            let Some(this) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let view = this.current_view();
            let target = {
                let st = this.state.borrow();
                st.pointer
                    .and_then(|p| topdown::hit(&view, &st.screens, p, GRAB_PX))
                    .map(|(i, _)| st.screens[i])
            };
            let Some(screen) = target else {
                return glib::Propagation::Proceed;
            };
            let factor = (1.0 - 0.06 * dy as f32).clamp(0.5, 1.5);
            this.state.borrow_mut().selected = Some(screen.display_id);
            this.push(topdown::scaled(&screen, factor));
            this.show_selected();
            glib::Propagation::Stop
        });
        self.area.add_controller(scroll);
    }

    fn draw(&self, area: &gtk::DrawingArea, cr: &gtk::cairo::Context, w: f64, h: f64) {
        let fg = area.color();
        let (r, g, b) = (
            f64::from(fg.red()),
            f64::from(fg.green()),
            f64::from(fg.blue()),
        );
        let view = self.view(w, h);
        let st = self.state.borrow();
        let user = view.to_canvas((0.0, 0.0));

        // What the headset can see straight ahead, roughly.
        let fov = 55f64.to_radians();
        let reach = w.max(h) * 2.0;
        cr.move_to(user.0, user.1);
        cr.line_to(user.0 - reach * fov.sin(), user.1 - reach * fov.cos());
        cr.line_to(user.0 + reach * fov.sin(), user.1 - reach * fov.cos());
        cr.close_path();
        cr.set_source_rgba(r, g, b, 0.04);
        let _ = cr.fill();

        // A ring every meter.
        cr.set_line_width(1.0);
        cr.select_font_face(
            "Sans",
            gtk::cairo::FontSlant::Normal,
            gtk::cairo::FontWeight::Normal,
        );
        cr.set_font_size(11.0);
        for m in 1..=5 {
            let radius = view.px_per_m * f64::from(m);
            if radius > w.max(h) * 1.5 {
                break;
            }
            cr.new_sub_path();
            cr.arc(user.0, user.1, radius, 0.0, std::f64::consts::TAU);
            cr.set_source_rgba(r, g, b, 0.12);
            let _ = cr.stroke();
            cr.set_source_rgba(r, g, b, 0.45);
            cr.move_to(user.0 + 4.0, user.1 - radius - 4.0);
            let _ = cr.show_text(&format!("{m} m"));
        }

        // A fixed layout origin, not a claim of live head-pose tracking.
        let model = st
            .headset
            .as_ref()
            .map_or(Model::Generic, |identity| identity.model);
        headset::draw_origin(cr, model, user.0, user.1, area.scale_factor());

        // The screens, selected one last so it is on top.
        let mut order: Vec<&ScreenPlacement> = st.screens.iter().collect();
        order.sort_by_key(|s| Some(s.display_id) == st.selected);
        for s in order {
            let selected = Some(s.display_id) == st.selected;
            let hovered = Some(s.display_id) == st.hovered;
            let showing = st.showing.contains(&s.display_id);
            let pts: Vec<(f64, f64)> = topdown::footprint(s)
                .into_iter()
                .map(|p| view.to_canvas(p))
                .collect();
            cr.set_line_cap(gtk::cairo::LineCap::Round);
            cr.set_line_join(gtk::cairo::LineJoin::Round);
            cr.set_line_width(if selected { 11.0 } else { 9.0 });
            if showing {
                cr.set_dash(&[], 0.0);
            } else {
                cr.set_dash(&[2.0, 14.0], 0.0);
            }
            if selected {
                cr.set_source_rgba(ACCENT.0, ACCENT.1, ACCENT.2, 1.0);
            } else {
                cr.set_source_rgba(r, g, b, if hovered { 0.75 } else { 0.5 });
            }
            cr.move_to(pts[0].0, pts[0].1);
            for p in &pts[1..] {
                cr.line_to(p.0, p.1);
            }
            let _ = cr.stroke();
            cr.set_dash(&[], 0.0);

            // Resize handles on the selected screen.
            if selected {
                for end in [pts[0], pts[pts.len() - 1]] {
                    cr.new_sub_path();
                    cr.arc(end.0, end.1, 7.0, 0.0, std::f64::consts::TAU);
                    cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
                    let _ = cr.fill_preserve();
                    cr.set_source_rgba(ACCENT.0, ACCENT.1, ACCENT.2, 1.0);
                    cr.set_line_width(2.5);
                    let _ = cr.stroke();
                }
            }

            // The name, on the far side of the screen.
            let c = view.to_canvas(topdown::centre(s));
            let away = (c.0 - user.0, c.1 - user.1);
            let len = away.0.hypot(away.1).max(1.0);
            let at = (c.0 + away.0 / len * 22.0, c.1 + away.1 / len * 22.0);
            let name = topdown::display_name(s.display_id);
            cr.select_font_face(
                "Sans",
                gtk::cairo::FontSlant::Normal,
                if selected {
                    gtk::cairo::FontWeight::Bold
                } else {
                    gtk::cairo::FontWeight::Normal
                },
            );
            cr.set_font_size(13.0);
            if let Ok(ext) = cr.text_extents(&name) {
                cr.move_to(at.0 - ext.width() / 2.0, at.1 + ext.height() / 2.0);
                cr.set_source_rgba(r, g, b, if selected { 1.0 } else { 0.8 });
                let _ = cr.show_text(&name);
            }
        }
    }

    // ---- sidebar -------------------------------------------------------------------------

    fn show_selected(&self) {
        let (ids, selected_id) = {
            let st = self.state.borrow();
            (
                st.screens
                    .iter()
                    .map(|screen| screen.display_id)
                    .collect::<Vec<_>>(),
                st.selected,
            )
        };
        if *self.screen_ids.borrow() != ids {
            let labels: Vec<_> = ids.iter().map(|&id| topdown::display_name(id)).collect();
            let labels: Vec<_> = labels.iter().map(String::as_str).collect();
            // Drop the borrow before the model emits selection notifications.
            *self.screen_ids.borrow_mut() = ids.clone();
            self.screen_names
                .splice(0, self.screen_names.n_items(), &labels);
        }
        self.selector.set_sensitive(!ids.is_empty());
        self.selector.set_visible(!ids.is_empty());
        let index = ids
            .iter()
            .position(|&id| Some(id) == selected_id)
            .map_or(gtk::INVALID_LIST_POSITION, |index| index as u32);
        if self.selector.selected() != index {
            self.selector.set_selected(index);
        }
        let selected = self.selected_placement();
        let showing = {
            let st = self.state.borrow();
            selected.is_some_and(|s| st.showing.contains(&s.display_id))
        };
        let rows = &self.rows;
        rows.group.set_visible(selected.is_some());
        let Some(s) = selected else {
            self.shown.set(None);
            return;
        };
        if self.shown.get() == Some((s, showing)) {
            return;
        }
        self.shown.set(Some((s, showing)));
        rows.group.set_title(&topdown::display_name(s.display_id));
        self.updating.set(true);
        rows.angle
            .set_value(f64::from(topdown::angle_right_deg(&s)));
        rows.distance
            .set_value(f64::from(s.horizontal_distance_m()));
        rows.height.set_value(f64::from(s.position[1]));
        rows.tilt.set_value(f64::from(s.pitch_rad().to_degrees()));
        rows.width.set_value(f64::from(s.width_m));
        match s.curve {
            ScreenCurve::Flat => rows.curved.set_active(false),
            ScreenCurve::Curved { radius_m } => {
                rows.curved.set_active(true);
                rows.radius.set_value(f64::from(radius_m));
            }
        }
        rows.radius.set_sensitive(s.curve.is_curved());
        self.updating.set(false);
        rows.video.set_subtitle(if showing {
            "Showing its stream"
        } else {
            "No video yet: it shows once the host sends this stream"
        });
    }

    fn connect_rows(self: &Rc<Self>) {
        type Edit = fn(&ScreensPage, &ScreenPlacement) -> ScreenPlacement;
        let bind_spin = |row: &adw::SpinRow, edit: Edit| {
            let weak = Rc::downgrade(self);
            row.connect_value_notify(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.edit_selected(edit);
                }
            });
        };
        bind_spin(&self.rows.angle, |p, s| {
            topdown::with_angle(s, p.rows.angle.value() as f32)
        });
        bind_spin(&self.rows.distance, |p, s| {
            topdown::with_distance(s, p.rows.distance.value() as f32)
        });
        bind_spin(&self.rows.height, |p, s| {
            topdown::with_height(s, p.rows.height.value() as f32)
        });
        bind_spin(&self.rows.tilt, |p, s| {
            topdown::with_tilt(s, p.rows.tilt.value() as f32)
        });
        bind_spin(&self.rows.width, |p, s| {
            topdown::with_width(s, p.rows.width.value() as f32)
        });
        bind_spin(&self.rows.radius, |p, s| ScreenPlacement {
            curve: ScreenCurve::Curved {
                radius_m: p.rows.radius.value() as f32,
            },
            ..*s
        });
        let weak = Rc::downgrade(self);
        self.rows.curved.connect_active_notify(move |_| {
            if let Some(this) = weak.upgrade() {
                this.edit_selected(|p, s| ScreenPlacement {
                    curve: if p.rows.curved.is_active() {
                        ScreenCurve::Curved {
                            radius_m: p.rows.radius.value() as f32,
                        }
                    } else {
                        ScreenCurve::Flat
                    },
                    ..*s
                });
            }
        });
    }

    fn edit_selected(self: &Rc<Self>, edit: fn(&ScreensPage, &ScreenPlacement) -> ScreenPlacement) {
        if self.updating.get() {
            return;
        }
        let Some(current) = self.selected_placement() else {
            return;
        };
        let next = edit(self, &current);
        self.rows.radius.set_sensitive(next.curve.is_curved());
        if next != current {
            self.push(next);
        }
    }

    // ---- virtual monitors ----------------------------------------------------------------

    fn chosen_mode(&self) -> (u32, u32, u32) {
        let (w, h) = RESOLUTIONS[(self.resolution.selected() as usize).min(RESOLUTIONS.len() - 1)];
        let rate =
            MONITOR_RATES[(self.monitor_rate.selected() as usize).min(MONITOR_RATES.len() - 1)];
        (w, h, rate)
    }

    fn show_monitors(self: &Rc<Self>, daemon_running: bool) {
        let (monitors, backend) = {
            let st = self.state.borrow();
            (st.monitors.clone(), st.backend.clone())
        };
        let shown = MonitorList {
            monitors: monitors.clone(),
            backend: backend.clone(),
            daemon_running,
        };
        if self.shown_monitors.borrow().as_ref() == Some(&shown) {
            return;
        }
        *self.shown_monitors.borrow_mut() = Some(shown);
        // Retain unchanged row widgets so a telemetry tick cannot take keyboard focus
        // away from a monitor's remove button.
        let mutter = backend.to_lowercase().contains("mutter");
        self.monitors_group
            .set_description(Some(&if !daemon_running {
                "The daemon is not running.".to_string()
            } else if mutter {
                "Extra monitors the host streams to the headset. With GNOME they are added to your \
             desktop like real monitors."
                    .to_string()
            } else {
                format!("Extra monitors the host streams to the headset ({backend}).")
            }));

        let mut rows = self.monitor_rows.borrow_mut();
        for row in rows.drain(..) {
            self.monitors_group.remove(&row);
        }
        if monitors.is_empty() && daemon_running {
            let row = adw::ActionRow::builder()
                .title("No virtual monitors")
                .subtitle("The headset shows the screen you share")
                .build();
            self.monitors_group.add(&row);
            rows.push(row);
        }
        for m in &monitors {
            let row = adw::ActionRow::builder()
                .title(&m.name)
                .subtitle(format!(
                    "{} × {} at {} Hz, {}",
                    m.width, m.height, m.refresh_rate, m.connector
                ))
                .build();
            let remove = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text("Remove this monitor")
                .valign(gtk::Align::Center)
                .build();
            remove.add_css_class("flat");
            let id = m.id;
            let weak = Rc::downgrade(self);
            remove.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.remove_monitor(id);
                }
            });
            row.add_suffix(&remove);
            self.monitors_group.add(&row);
            rows.push(row);
        }
        self.add_monitor
            .set_sensitive(daemon_running && monitors.len() < MAX_MONITORS);
        self.apply_mode
            .set_sensitive(daemon_running && !monitors.is_empty());
    }

    fn connect_monitor_buttons(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.add_monitor.connect_clicked(move |button| {
            if let Some(this) = weak.upgrade() {
                this.confirm_add_monitor(button);
            }
        });
        let weak = Rc::downgrade(self);
        self.apply_mode.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                let count = this.state.borrow().monitors.len() as u32;
                this.set_monitor_count(count, "Monitors changed");
            }
        });
    }

    fn confirm_add_monitor(self: &Rc<Self>, button: &gtk::Button) {
        let (w, h, rate) = self.chosen_mode();
        let backend = self.state.borrow().backend.clone();
        let dialog = adw::AlertDialog::new(
            Some("Add a Virtual Monitor?"),
            Some(&format!(
                "DeskQuest creates a {w} × {h} monitor at {rate} Hz through {backend}. Your \
                 desktop gets a new monitor, and the headset can show it."
            )),
        );
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("add", "Add Monitor");
        dialog.set_response_appearance("add", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            if response != "add" {
                return;
            }
            if let Some(this) = weak.upgrade() {
                let count = this.state.borrow().monitors.len() as u32 + 1;
                this.set_monitor_count(count, "Monitor added");
            }
        });
        dialog.present(Some(button));
    }

    fn set_monitor_count(self: &Rc<Self>, count: u32, done: &'static str) {
        if count == 0 {
            return;
        }
        let (w, h, rate) = self.chosen_mode();
        let shell = self.shell.clone();
        glib::spawn_future_local(async move {
            match daemon::spawn(daemon::set_monitor_count(count, w, h, rate)).await {
                Ok(_) => shell.toast(done),
                Err(e) => shell.toast(&format!("Monitors not changed: {e}")),
            }
            shell.refresh();
        });
    }

    fn remove_monitor(self: &Rc<Self>, id: u32) {
        let shell = self.shell.clone();
        glib::spawn_future_local(async move {
            match daemon::spawn(daemon::remove_monitor(id)).await {
                Ok(message) => shell.toast(&message),
                Err(e) => shell.toast(&format!("Monitor not removed: {e}")),
            }
            shell.refresh();
        });
    }
}

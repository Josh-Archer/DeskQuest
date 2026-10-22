//! Interactive mesh preview, rendered only when it is exposed or edited.
use super::{mesh, Identity, Model, Orbit, Rect};
use adw::prelude::*;
use gtk::glib;
use std::cell::Cell;
use std::rc::Rc;

pub struct Preview {
    area: gtk::DrawingArea,
    model: Rc<Cell<Model>>,
    orbit: Rc<Cell<Orbit>>,
}

impl Preview {
    pub fn new() -> Self {
        let area = gtk::DrawingArea::builder()
            .content_width(420)
            .content_height(260)
            .hexpand(true)
            .vexpand(true)
            .build();
        area.set_tooltip_text(Some("Drag to rotate the model; scroll to zoom"));
        let model = Rc::new(Cell::new(Model::Generic));
        let orbit = Rc::new(Cell::new(Orbit::default()));
        let camera = orbit.clone();
        let current_model = model.clone();
        area.set_draw_func(move |_, cr, width, height| {
            mesh::draw(
                cr,
                current_model.get(),
                Rect {
                    x: 12.,
                    y: 12.,
                    width: f64::from(width) - 24.,
                    height: f64::from(height) - 24.,
                },
                camera.get(),
            );
        });
        let drag = gtk::GestureDrag::new();
        let start = Rc::new(Cell::new(Orbit::default()));
        let origin = start.clone();
        let camera = orbit.clone();
        drag.connect_drag_begin(move |_, _, _| origin.set(camera.get()));
        let camera = orbit.clone();
        let weak = area.downgrade();
        drag.connect_drag_update(move |_, x, y| {
            let original = start.get();
            camera.set(Orbit {
                yaw: original.yaw + x as f32 * 0.012,
                pitch: (original.pitch + y as f32 * 0.012).clamp(-1.35, 1.35),
                ..original
            });
            if let Some(area) = weak.upgrade() {
                area.queue_draw();
            }
        });
        area.add_controller(drag);
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        let camera = orbit.clone();
        let weak = area.downgrade();
        scroll.connect_scroll(move |_, _, delta| {
            let old = camera.get();
            camera.set(Orbit {
                zoom: (old.zoom * (1.0 - delta as f32 * 0.08)).clamp(0.6, 1.8),
                ..old
            });
            if let Some(area) = weak.upgrade() {
                area.queue_draw();
            }
            glib::Propagation::Stop
        });
        area.add_controller(scroll);
        Self { area, model, orbit }
    }

    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.area
    }
    pub fn set_model(&self, model: Model) {
        if self.model.replace(model) != model {
            self.area.queue_draw();
        }
    }
    pub fn reset(&self) {
        self.orbit.set(Orbit::default());
        self.area.queue_draw();
    }
}

pub fn show_details(parent: &impl IsA<gtk::Widget>, identity: &Identity) {
    let preview = Rc::new(Preview::new());
    preview.set_model(identity.model);
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(
        &identity.name,
        &identity.connection,
    )));
    let reset = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Reset model view")
        .build();
    let model = preview.clone();
    reset.connect_clicked(move |_| model.reset());
    header.pack_start(&reset);
    let note = gtk::Label::builder()
        .label(identity.preview_note())
        .wrap(true)
        .margin_start(20)
        .margin_end(20)
        .margin_bottom(18)
        .build();
    note.add_css_class("dim-label");
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(preview.widget());
    content.append(&note);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&content));
    adw::Dialog::builder()
        .title(&identity.name)
        .content_width(600)
        .content_height(440)
        .child(&toolbar)
        .build()
        .present(Some(parent));
}

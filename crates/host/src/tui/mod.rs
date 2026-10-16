pub mod data_source;
pub mod model;
pub mod render;
pub mod terminal;

pub use data_source::{IpcTelemetrySource, MockTelemetrySource, TelemetrySource};
pub use model::{
    AudioLatencyModel, AudioStatusModel, ClientConnectionMode, ClientStatusModel, DashboardModel,
    DashboardSnapshot, DisplayMetricModel, DisplaySnapshot, LatencyWaterfallModel, RollingWindow,
    VideoWaterfallStage,
};
pub use render::render_dashboard;
pub use terminal::{
    handle_key_action, init_terminal, install_panic_hook, restore_terminal,
    run_dashboard_with_source,
};

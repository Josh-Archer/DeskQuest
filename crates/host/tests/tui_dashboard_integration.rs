use ratatui::backend::TestBackend;
use ratatui::Terminal;
use std::sync::Arc;

use linux_quest_display::backend::mock::MockDisplayBackend;
use linux_quest_display::{DisplayLayoutMode, VirtualMonitorConfig};
use linux_quest_host::tui::data_source::{MockTelemetrySource, TelemetrySource};
use linux_quest_host::tui::model::{
    ClientConnectionMode, DashboardModel, DashboardSnapshot, DisplaySnapshot,
    LatencyWaterfallModel, RollingWindow, VideoWaterfallStage,
};
use linux_quest_host::tui::render::render_dashboard;
use linux_quest_host::tui::terminal::handle_key_action;
use linux_quest_host::MultiDisplayHost;
use linux_quest_protocol::{
    AudioLatencyBreakdown, AudioStreamTelemetry, ClientTransportTelemetry, FrameLatencyBreakdown,
};

fn buffer_to_string(buffer: &ratatui::buffer::Buffer) -> String {
    let mut s = String::new();
    for y in buffer.area.top()..buffer.area.bottom() {
        for x in buffer.area.left()..buffer.area.right() {
            s.push_str(buffer[(x, y)].symbol());
        }
        s.push('\n');
    }
    s
}

#[test]
fn test_rolling_window_rolling_eviction_and_stats() {
    let mut window = RollingWindow::<u64>::new(4);
    assert!(window.is_empty());
    assert_eq!(window.capacity(), 4);

    // Push 4 values: 10, 20, 30, 40
    window.push(10);
    window.push(20);
    window.push(30);
    window.push(40);

    assert_eq!(window.len(), 4);
    assert_eq!(window.min(), Some(10));
    assert_eq!(window.max(), Some(40));
    assert_eq!(window.average(), 25.0);
    assert_eq!(window.as_sparkline(), vec![10, 20, 30, 40]);

    // Push 5th value: evicts 10 -> [20, 30, 40, 50]
    window.push(50);
    assert_eq!(window.len(), 4);
    assert_eq!(window.min(), Some(20));
    assert_eq!(window.max(), Some(50));
    assert_eq!(window.average(), 35.0);
    assert_eq!(window.as_sparkline(), vec![20, 30, 40, 50]);

    // Test clear
    window.clear();
    assert!(window.is_empty());
    assert_eq!(window.len(), 0);
    assert_eq!(window.average(), 0.0);
}

#[test]
fn test_latency_waterfall_stage_calculations_and_proportions() {
    let mut waterfall = LatencyWaterfallModel::new(20);

    let mut frame = FrameLatencyBreakdown::new(42, 500_000);
    frame.capture_duration_us = 1000; // 1.0 ms
    frame.encode_duration_us = 2500; // 2.5 ms
    frame.host_queue_duration_us = 500; // 0.5 ms
    frame.transport_duration_us = 1000; // 1.0 ms
    frame.client_decode_duration_us = 3000; // 3.0 ms
    frame.client_render_duration_us = 1000; // 1.0 ms
    frame.compute_total(); // 9.0 ms (9000 us)

    waterfall.push(frame);

    assert_eq!(waterfall.total_ms(), 9.0);
    assert_eq!(waterfall.capture_ms(), 1.0);
    assert_eq!(waterfall.encode_ms(), 2.5);
    assert_eq!(waterfall.queue_ms(), 0.5);
    assert_eq!(waterfall.transport_ms(), 1.0);
    assert_eq!(waterfall.decode_ms(), 3.0);
    assert_eq!(waterfall.render_ms(), 1.0);

    // Percentage checks
    let cap_pct = waterfall.stage_percentage(VideoWaterfallStage::Capture);
    let enc_pct = waterfall.stage_percentage(VideoWaterfallStage::Encode);
    let dec_pct = waterfall.stage_percentage(VideoWaterfallStage::ClientDecode);

    assert!((cap_pct - (1000.0 / 9000.0 * 100.0)).abs() < 0.01);
    assert!((enc_pct - (2500.0 / 9000.0 * 100.0)).abs() < 0.01);
    assert!((dec_pct - (3000.0 / 9000.0 * 100.0)).abs() < 0.01);

    // Sparkline conversion (in tenths of a ms: 9000us = 90 tenths of a ms)
    assert_eq!(waterfall.sparkline_total_tenths_ms(), vec![90]);
}

#[test]
fn test_ratatui_render_all_dashboard_components() {
    let backend = TestBackend::new(140, 36);
    let mut terminal = Terminal::new(backend).unwrap();

    let mut model = DashboardModel::new(50);
    let snapshot = DashboardSnapshot {
        uptime_secs: 3665, // 01:01:05
        streaming: true,
        displays: vec![
            DisplaySnapshot {
                id: 1,
                name: "Quest-Virtual-Center".into(),
                width: 3840,
                height: 2160,
                refresh_rate: 90,
                fps: 89.8,
                bitrate_mbps: 120.5,
                target_bitrate_mbps: 125.0,
                codec: "AV1".into(),
                is_virtual: true,
            },
            DisplaySnapshot {
                id: 2,
                name: "Quest-Virtual-Left".into(),
                width: 1920,
                height: 1080,
                refresh_rate: 72,
                fps: 71.9,
                bitrate_mbps: 35.0,
                target_bitrate_mbps: 35.0,
                codec: "AV1".into(),
                is_virtual: true,
            },
        ],
        video_latency: FrameLatencyBreakdown {
            frame_id: 1000,
            capture_start_us: 100_000,
            capture_duration_us: 1100,
            encode_duration_us: 2700,
            host_queue_duration_us: 250,
            transport_duration_us: 750,
            client_decode_duration_us: 3200,
            client_render_duration_us: 600,
            total_motion_to_photon_us: 8600,
        },
        audio_latency: AudioLatencyBreakdown {
            frame_id: 4000,
            capture_start_us: 100_000,
            capture_duration_us: 280,
            encode_duration_us: 390,
            transport_duration_us: 750,
            jitter_buffer_delay_us: 2500,
            decode_duration_us: 190,
            playout_buffer_duration_us: 450,
            total_latency_us: 4560,
        },
        client: ClientTransportTelemetry {
            mode: ClientConnectionMode::UsbAdb,
            rtt_us: 950,
            smoothed_rtt_us: 920,
            packet_loss_rate: 0.0002,
            packets_sent: 100_000,
            packets_recv: 99_980,
            packets_lost: 20,
            fec_recoveries: 20,
            bitrate_mbps: 155.5,
        },
        audio: AudioStreamTelemetry {
            active: true,
            codec: "Opus (Low Delay)".into(),
            sample_rate: 48000,
            channels: 2,
            frame_size_ms: 2.5,
            bitrate_kbps: 128,
            jitter_us: 320,
            buffer_delay_us: 2500,
            buffer_health_pct: 100.0,
            plc_count: 0,
        },
    };

    model.apply_snapshot(snapshot);

    terminal
        .draw(|f| {
            render_dashboard(f, &model);
        })
        .unwrap();

    let buffer = terminal.backend().buffer();
    let rendered = buffer_to_string(buffer);

    // Verify Title & Uptime
    assert!(rendered.contains("LINUX-QUEST HOST DASHBOARD"));
    assert!(rendered.contains("Uptime: 01:01:05"));
    assert!(rendered.contains("STREAMING LIVE"));
    assert!(rendered.contains("USB 3.0 ADB TETHER"));

    // Verify Virtual Displays
    assert!(rendered.contains("Active Virtual Displays"));
    assert!(rendered.contains("Quest-Virtual-Center"));
    assert!(rendered.contains("3840x2160"));
    assert!(rendered.contains("89.8"));
    assert!(rendered.contains("120.5/125.0"));
    assert!(rendered.contains("Quest-Virtual-Left"));
    assert!(rendered.contains("1920x1080"));

    // Verify Video Latency Waterfall
    assert!(rendered.contains("Real-Time Latency Waterfall Breakdown"));
    assert!(rendered.contains("Capture"));
    assert!(rendered.contains("Encode"));
    assert!(rendered.contains("Queue"));
    assert!(rendered.contains("Transport"));
    assert!(rendered.contains("Decode"));
    assert!(rendered.contains("Render"));
    assert!(rendered.contains("Motion-to-Photon: 8.60 ms"));
    assert!(rendered.contains("Sub-10ms USB Budget Met"));

    // Verify Audio Latency & Pipeline Status
    assert!(rendered.contains("Audio Latency:"));
    assert!(rendered.contains("JitterBuf"));
    assert!(rendered.contains("Playout"));
    assert!(rendered.contains("PipeWire + Opus"));
    assert!(rendered.contains("Active (Streaming)"));
    assert!(rendered.contains("48000Hz, 2ch, 2.5ms"));
    assert!(rendered.contains("128 kbps"));

    // Verify Client Stats
    assert!(rendered.contains("Meta Quest 3"));
    assert!(rendered.contains("0.92 ms"));
    assert!(rendered.contains("Packet Loss: 0.02%"));
    assert!(rendered.contains("FEC Recoveries: 20 packets"));

    // Verify Footer Hotkeys
    assert!(rendered.contains("Quit Dashboard"));
    assert!(rendered.contains("Reset Statistics"));
    assert!(rendered.contains("Pause/Resume"));
}

#[tokio::test]
async fn test_mock_telemetry_source_multi_frame_progression() {
    let mut source = MockTelemetrySource::new();
    let mut model = DashboardModel::new(30);

    for expected_frame in 1..=15 {
        let snap = source.fetch_snapshot().await.expect("Fetch failed");
        assert_eq!(snap.video_latency.frame_id, expected_frame);
        model.apply_snapshot(snap);
    }

    assert_eq!(model.frame_counter, 15);
    assert_eq!(model.video_latency.total_history.len(), 15);
    assert!(model.video_latency.avg_total_ms() > 5.0);
    assert!(model.video_latency.avg_total_ms() < 15.0);

    // Test resetting statistics
    model.reset_stats();
    assert_eq!(model.video_latency.total_history.len(), 0);
    assert_eq!(model.audio_latency.total_history.len(), 0);
    assert_eq!(model.client.rtt_history.len(), 0);
}

#[tokio::test]
async fn test_display_host_snapshot_generation() {
    let backend = MockDisplayBackend::new();
    let host = Arc::new(MultiDisplayHost::new(Box::new(backend)));

    let cfg = VirtualMonitorConfig::new(1, "Quest-Virtual", 1920, 1080, 60);
    host.toggle_monitors(1, &cfg, DisplayLayoutMode::Horizontal)
        .await
        .expect("Toggle monitors failed");

    // Request snapshot via direct logic matching IPC server
    let monitors = host.list_monitors().await;
    assert_eq!(monitors.len(), 1);

    let displays: Vec<DisplaySnapshot> = monitors
        .into_iter()
        .map(|m| DisplaySnapshot {
            id: m.id as u16,
            name: m.name,
            width: m.width,
            height: m.height,
            refresh_rate: m.refresh_rate,
            fps: m.refresh_rate as f32,
            bitrate_mbps: 75.0,
            target_bitrate_mbps: 75.0,
            codec: "AV1".into(),
            is_virtual: m.is_virtual,
        })
        .collect();

    let snap = DashboardSnapshot {
        uptime_secs: 10,
        streaming: true,
        displays,
        video_latency: FrameLatencyBreakdown::default(),
        audio_latency: AudioLatencyBreakdown::default(),
        client: ClientTransportTelemetry::default(),
        audio: AudioStreamTelemetry::default(),
    };

    let serialized = bincode::serialize(&snap).expect("Serialize failed");
    let deserialized: DashboardSnapshot =
        bincode::deserialize(&serialized).expect("Deserialize failed");

    assert_eq!(deserialized.displays.len(), 1);
    assert_eq!(deserialized.displays[0].id, 1);
    assert_eq!(deserialized.displays[0].width, 1920);
    assert_eq!(deserialized.displays[0].height, 1080);
}

#[test]
fn test_keyboard_event_navigation_and_control() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut model = DashboardModel::new(10);
    let mut paused = false;

    // Quit keys
    assert!(handle_key_action(
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        &mut model,
        &mut paused
    ));
    assert!(handle_key_action(
        KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::NONE),
        &mut model,
        &mut paused
    ));
    assert!(handle_key_action(
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        &mut model,
        &mut paused
    ));
    assert!(handle_key_action(
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        &mut model,
        &mut paused
    ));

    // Pause toggle
    assert!(!handle_key_action(
        KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        &mut model,
        &mut paused
    ));
    assert!(paused);

    assert!(!handle_key_action(
        KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        &mut model,
        &mut paused
    ));
    assert!(!paused);

    // Reset stats
    assert!(!handle_key_action(
        KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
        &mut model,
        &mut paused
    ));

    // Unhandled key
    assert!(!handle_key_action(
        KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
        &mut model,
        &mut paused
    ));
}

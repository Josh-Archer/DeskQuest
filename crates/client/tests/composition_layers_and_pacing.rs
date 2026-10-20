use linux_quest_client::config::{ClientConfig, DisplayMode};
use linux_quest_client::decoder::DecoderStats;
use linux_quest_client::hud::DebugHud;
use linux_quest_client::openxr::{
    DesktopLayerConfig, FrameLoopEngine, FramePacingMetrics, OpenXrContext,
};

#[test]
fn test_desktop_layer_config_defaults_and_validation() {
    let mut config = DesktopLayerConfig::default();
    assert_eq!(config.mode, DisplayMode::CurvedCylinder);
    assert_eq!(config.quad_size, (1.6, 0.9));
    assert_eq!(config.distance, 1.2);
    // A gentle curve: a 3 m radius over 0.6 rad keeps the 1.8 m wide screen of the old
    // 1.5 m / 1.2 rad default with half the bend.
    assert_eq!(config.cylinder_radius, 3.0);
    assert_eq!(config.cylinder_central_angle, 0.6);
    assert!((config.cylinder_radius * config.cylinder_central_angle - 1.8).abs() < 1e-4);
    let client = ClientConfig::default();
    assert_eq!(client.cylinder_radius, config.cylinder_radius);
    assert_eq!(client.cylinder_central_angle, config.cylinder_central_angle);
    assert!((config.cylinder_aspect_ratio - (16.0 / 9.0)).abs() < 1e-4);

    // Flat quad mode
    config.mode = DisplayMode::FlatQuad;
    config.quad_size = (2.0, 1.125);
    config.distance = 1.0;
    assert_eq!(config.quad_size.0, 2.0);
    assert_eq!(config.quad_size.1, 1.125);
}

#[test]
fn test_frame_loop_engine_pacing_metrics() {
    let client_config = ClientConfig::default();
    let xr_context = OpenXrContext::new_simulated(&client_config);
    let mut frame_loop = FrameLoopEngine::new();

    assert_eq!(frame_loop.frame_counter(), 0);

    // Run 5 pacing steps
    for i in 1..=5 {
        let metrics = frame_loop
            .wait_frame_pacing(&xr_context)
            .expect("Pacing failed");
        assert_eq!(frame_loop.frame_counter(), i);
        assert!(metrics.predicted_display_time > 0);
        // Budget for default 90Hz is ~11.11ms = 11111us
        assert!(
            metrics.motion_to_photon_latency_us > 10_000
                && metrics.motion_to_photon_latency_us < 12_000
        );
    }

    assert_eq!(frame_loop.frame_counter(), 5);
}

#[test]
fn test_frame_loop_engine_different_refresh_budgets() {
    // 72 Hz
    let config_72 = ClientConfig {
        target_refresh_rate: 72.0,
        ..Default::default()
    };
    let xr_72 = OpenXrContext::new_simulated(&config_72);
    let mut loop_72 = FrameLoopEngine::new();
    let m_72 = loop_72.wait_frame_pacing(&xr_72).unwrap();
    // 1000000 / 72 ≈ 13888 us
    assert!(m_72.motion_to_photon_latency_us > 13_000 && m_72.motion_to_photon_latency_us < 14_500);

    // 120 Hz
    let config_120 = ClientConfig {
        target_refresh_rate: 120.0,
        ..Default::default()
    };
    let xr_120 = OpenXrContext::new_simulated(&config_120);
    let mut loop_120 = FrameLoopEngine::new();
    let m_120 = loop_120.wait_frame_pacing(&xr_120).unwrap();
    // 1000000 / 120 ≈ 8333 us
    assert!(m_120.motion_to_photon_latency_us > 8_000 && m_120.motion_to_photon_latency_us < 9_000);
}

#[test]
fn test_debug_hud_rendering_and_telemetry() {
    let mut hud = DebugHud::new(512, 256);
    assert_eq!(hud.width(), 512);
    assert_eq!(hud.height(), 256);

    let config = ClientConfig::default();
    let pacing = FramePacingMetrics {
        motion_to_photon_latency_us: 11_111,
        wait_frame_duration_us: 500,
        render_duration_us: 200,
        predicted_display_time: 1_000_000,
        should_render: true,
    };
    let stats = DecoderStats {
        last_latency_us: 2800,
        decoder_name: "c2.qti.av1.decoder.low_latency".to_string(),
        frames_decoded: 100,
        frames_rendered: 100,
        frames_dropped: 0,
        ..Default::default()
    };

    // Update with sample metrics
    hud.update(&config, &pacing, &stats, 1500, 12, 0);

    let text = hud.formatted_text();
    assert!(text.contains("90Hz"));
    assert!(text.contains("M2P: 14.4ms"));
    assert!(text.contains("Dec: 2.8ms"));
    assert!(text.contains("c2.qti.av1.decoder.low_latency"));
    assert!(text.contains("Packets: 1500"));

    // Render RGBA buffer
    let rgba = hud.render_to_rgba();
    assert_eq!(rgba.len(), (512 * 256 * 4) as usize);

    // Top accent bar should be cyan (R:0, G:220, B:255, A:255)
    assert_eq!(rgba[0], 0);
    assert_eq!(rgba[1], 220);
    assert_eq!(rgba[2], 255);
    assert_eq!(rgba[3], 255);

    // Bottom pixel should be dark background (R:16, G:18, B:24, A:200)
    let bottom_idx = ((255 * 512 + 256) * 4) as usize;
    assert_eq!(rgba[bottom_idx], 16);
    assert_eq!(rgba[bottom_idx + 1], 18);
    assert_eq!(rgba[bottom_idx + 2], 24);
    assert_eq!(rgba[bottom_idx + 3], 200);
}

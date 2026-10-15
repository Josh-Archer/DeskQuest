use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Cell, Gauge, Paragraph, Row, Sparkline, Table},
    Frame,
};

use crate::tui::model::{ClientConnectionMode, DashboardModel};

/// Main render function drawing the complete dashboard onto a ratatui Frame.
pub fn render_dashboard(frame: &mut Frame, model: &DashboardModel) {
    let size = frame.area();

    // Top-level vertical layout: Header, Content, Footer
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(24),   // Main content
            Constraint::Length(1), // Footer hotkey bar
        ])
        .split(size);

    render_header(frame, chunks[0], model);
    render_main_content(frame, chunks[1], model);
    render_footer(frame, chunks[2]);
}

fn render_header(frame: &mut Frame, area: Rect, model: &DashboardModel) {
    let (status_badge, status_style) = if model.streaming {
        (
            "[● STREAMING LIVE]",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (
            "[○ IDLE / READY]",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    };

    let (conn_badge, conn_style) = match model.client.connection_mode {
        ClientConnectionMode::UsbAdb => (
            "[● USB 3.0 ADB TETHER]",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        ClientConnectionMode::Wifi => (
            "[● WI-FI 6E TRANSPORT]",
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        ),
        ClientConnectionMode::Disconnected => (
            "[○ NO CLIENT CONNECTED]",
            Style::default().fg(Color::DarkGray),
        ),
    };

    let hours = model.uptime_secs / 3600;
    let mins = (model.uptime_secs % 3600) / 60;
    let secs = model.uptime_secs % 60;
    let uptime_str = format!("Uptime: {:02}:{:02}:{:02}", hours, mins, secs);

    let header_line = Line::from(vec![
        Span::styled(
            " LINUX-QUEST HOST DASHBOARD ",
            Style::default()
                .fg(Color::White)
                .bg(Color::Rgb(30, 60, 120))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(status_badge, status_style),
        Span::raw("  "),
        Span::styled(conn_badge, conn_style),
        Span::raw("  "),
        Span::styled(
            format!("Frames: {}", model.frame_counter),
            Style::default().fg(Color::Gray),
        ),
        Span::raw("  "),
        Span::styled(uptime_str, Style::default().fg(Color::Gray)),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Rgb(60, 90, 150)));

    let paragraph = Paragraph::new(header_line)
        .block(block)
        .alignment(Alignment::Left);
    frame.render_widget(paragraph, area);
}

fn render_main_content(frame: &mut Frame, area: Rect, model: &DashboardModel) {
    let main_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(9), // Top row: Client link status & Virtual Displays
            Constraint::Min(12),   // Middle row: Latency Waterfall Breakdown
            Constraint::Length(8), // Bottom row: Audio stream status & Sparkline overview
        ])
        .split(area);

    render_top_row(frame, main_rows[0], model);
    render_waterfall_section(frame, main_rows[1], model);
    render_bottom_row(frame, main_rows[2], model);
}

fn render_top_row(frame: &mut Frame, area: Rect, model: &DashboardModel) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(45), // Client status
            Constraint::Percentage(55), // Active virtual displays
        ])
        .split(area);

    render_client_status(frame, cols[0], model);
    render_virtual_displays(frame, cols[1], model);
}

fn render_client_status(frame: &mut Frame, area: Rect, model: &DashboardModel) {
    let block = Block::default()
        .title(" Quest 3 Client & Transport Link ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan));

    let loss_pct = model.client.loss_percentage();
    let loss_color = if loss_pct < 0.1 {
        Color::Green
    } else if loss_pct < 1.0 {
        Color::Yellow
    } else {
        Color::Red
    };

    let rtt_color = if model.client.rtt_ms < 2.0 {
        Color::Green
    } else if model.client.rtt_ms < 5.0 {
        Color::Yellow
    } else {
        Color::Red
    };

    let text = vec![
        Line::from(vec![
            Span::styled("Device: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                &model.client.device_name,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" | Mode: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                model.client.connection_mode.to_string(),
                Style::default().fg(Color::Cyan),
            ),
        ]),
        Line::from(vec![
            Span::styled("Round-Trip (RTT): ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{:.2} ms", model.client.rtt_ms),
                Style::default().fg(rtt_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" | Throughput: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{:.1} Mbps", model.client.bitrate_mbps),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("Packet Loss: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{:.2}%", loss_pct),
                Style::default().fg(loss_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" ({} lost)", model.client.packets_lost),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(" | FEC Scheme: ", Style::default().fg(Color::DarkGray)),
            Span::styled(&model.client.fec_scheme, Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("FEC Recoveries: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{} packets", model.client.fec_recoveries),
                Style::default()
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" | Total Tx/Rx: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!(
                    "{} / {}",
                    model.client.packets_sent, model.client.packets_recv
                ),
                Style::default().fg(Color::Gray),
            ),
        ]),
    ];

    let paragraph = Paragraph::new(text).block(block);
    frame.render_widget(paragraph, area);
}

fn render_virtual_displays(frame: &mut Frame, area: Rect, model: &DashboardModel) {
    let block = Block::default()
        .title(" Active Virtual Displays (Wayland / DRM) ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Magenta));

    if model.displays.is_empty() {
        let empty_msg = Paragraph::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "No active virtual displays provisioned.",
                Style::default().fg(Color::DarkGray),
            ),
        ]))
        .block(block);
        frame.render_widget(empty_msg, area);
        return;
    }

    let header = Row::new(vec![
        Cell::from("ID").style(Style::default().fg(Color::Yellow)),
        Cell::from("Name").style(Style::default().fg(Color::Yellow)),
        Cell::from("Resolution").style(Style::default().fg(Color::Yellow)),
        Cell::from("Target").style(Style::default().fg(Color::Yellow)),
        Cell::from("FPS").style(Style::default().fg(Color::Yellow)),
        Cell::from("Bitrate").style(Style::default().fg(Color::Yellow)),
        Cell::from("Codec").style(Style::default().fg(Color::Yellow)),
    ])
    .bottom_margin(1);

    let rows: Vec<Row> = model
        .displays
        .iter()
        .map(|disp| {
            let fps_color = if (disp.fps - disp.refresh_rate as f64).abs() < 2.0 {
                Color::Green
            } else {
                Color::Yellow
            };

            Row::new(vec![
                Cell::from(format!("{}", disp.display_id)),
                Cell::from(disp.name.clone()),
                Cell::from(format!("{}x{}", disp.width, disp.height)),
                Cell::from(format!("{} Hz", disp.refresh_rate)),
                Cell::from(format!("{:.1}", disp.fps)).style(Style::default().fg(fps_color)),
                Cell::from(format!("{:.1} Mbps", disp.bitrate_mbps)),
                Cell::from(disp.codec.clone()).style(Style::default().fg(Color::Cyan)),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(3),
            Constraint::Min(22),
            Constraint::Length(11),
            Constraint::Length(8),
            Constraint::Length(7),
            Constraint::Length(12),
            Constraint::Length(6),
        ],
    )
    .header(header)
    .block(block);

    frame.render_widget(table, area);
}

fn render_waterfall_section(frame: &mut Frame, area: Rect, model: &DashboardModel) {
    let block = Block::default()
        .title(" Real-Time Latency Waterfall Breakdown ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Green));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let waterfall_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // Video waterfall breakdown line
            Constraint::Length(2), // Video latency budget gauge
            Constraint::Length(2), // Video stats line
            Constraint::Length(2), // Audio waterfall breakdown line
            Constraint::Min(2),    // Latency sparkline
        ])
        .split(inner);

    // 1. Video Waterfall Breakdown Line
    let v = &model.video_latency;
    let video_stages = Line::from(vec![
        Span::styled(
            "Video Latency: ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("[Capture: {:.1}ms]", v.capture_ms()),
            Style::default().fg(Color::Blue),
        ),
        Span::styled(" ➔ ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("[Encode: {:.1}ms]", v.encode_ms()),
            Style::default().fg(Color::Yellow),
        ),
        Span::styled(" ➔ ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("[Queue: {:.1}ms]", v.queue_ms()),
            Style::default().fg(Color::Magenta),
        ),
        Span::styled(" ➔ ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("[Transport: {:.1}ms]", v.transport_ms()),
            Style::default().fg(Color::Cyan),
        ),
        Span::styled(" ➔ ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("[Decode: {:.1}ms]", v.decode_ms()),
            Style::default().fg(Color::Green),
        ),
        Span::styled(" ➔ ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("[Render: {:.1}ms]", v.render_ms()),
            Style::default().fg(Color::Rgb(180, 180, 180)),
        ),
    ]);
    frame.render_widget(Paragraph::new(video_stages), waterfall_chunks[0]);

    // 2. Video Latency Budget Gauge
    let total_ms = v.total_ms();
    let target_ms = 15.0; // 15ms Wi-Fi / 10ms USB target
    let ratio = (total_ms / target_ms).clamp(0.0, 1.0);
    let gauge_color = if total_ms <= 10.0 {
        Color::Green
    } else if total_ms <= 15.0 {
        Color::Yellow
    } else {
        Color::Red
    };

    let gauge_label = format!(
        "Motion-to-Photon: {:.2} ms / {:.1} ms budget ({:.1}% of budget)",
        total_ms,
        target_ms,
        ratio * 100.0
    );

    let gauge = Gauge::default()
        .gauge_style(Style::default().fg(gauge_color).bg(Color::Rgb(40, 40, 40)))
        .ratio(ratio)
        .label(gauge_label);
    frame.render_widget(gauge, waterfall_chunks[1]);

    // 3. Video Latency Statistics Line
    let stats_line = Line::from(vec![
        Span::styled("Window Stats: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("Avg: {:.2} ms  |  ", v.avg_total_ms()),
            Style::default().fg(Color::White),
        ),
        Span::styled(
            format!("Min: {:.2} ms  |  ", v.min_total_ms()),
            Style::default().fg(Color::Green),
        ),
        Span::styled(
            format!("Max: {:.2} ms  |  ", v.max_total_ms()),
            Style::default().fg(Color::Red),
        ),
        Span::styled(
            format!("P95: {:.2} ms  |  ", v.p95_total_ms()),
            Style::default().fg(Color::Yellow),
        ),
        Span::styled(
            if total_ms <= 10.0 {
                "● Sub-10ms USB Budget Met"
            } else if total_ms <= 15.0 {
                "● Sub-15ms Wi-Fi Budget Met"
            } else {
                "▲ Latency Exceeds Budget"
            },
            Style::default()
                .fg(gauge_color)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(Paragraph::new(stats_line), waterfall_chunks[2]);

    // 4. Audio Waterfall Line
    let a = &model.audio_latency;
    let audio_stages = Line::from(vec![
        Span::styled(
            "Audio Latency: ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("[Capture: {:.2}ms]", a.capture_ms()),
            Style::default().fg(Color::Blue),
        ),
        Span::styled(" ➔ ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("[Encode: {:.2}ms]", a.encode_ms()),
            Style::default().fg(Color::Yellow),
        ),
        Span::styled(" ➔ ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("[Transport: {:.2}ms]", a.transport_ms()),
            Style::default().fg(Color::Cyan),
        ),
        Span::styled(" ➔ ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("[JitterBuf: {:.2}ms]", a.jitter_buffer_ms()),
            Style::default().fg(Color::Magenta),
        ),
        Span::styled(" ➔ ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("[Decode: {:.2}ms]", a.decode_ms()),
            Style::default().fg(Color::Green),
        ),
        Span::styled(" ➔ ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("[Playout: {:.2}ms]", a.playout_ms()),
            Style::default().fg(Color::Rgb(180, 180, 180)),
        ),
        Span::styled(" ➔ ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!(
                "Total: {:.2} ms (Avg: {:.2} ms)",
                a.total_ms(),
                a.avg_total_ms()
            ),
            Style::default()
                .fg(Color::LightGreen)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(Paragraph::new(audio_stages), waterfall_chunks[3]);

    // 5. Latency Sparkline
    let sparkline_data = v.sparkline_total_tenths_ms();
    let sparkline = Sparkline::default()
        .block(
            Block::default()
                .title(" Motion-to-Photon History (0.1ms units) ")
                .borders(Borders::TOP)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .data(&sparkline_data)
        .style(Style::default().fg(gauge_color));
    frame.render_widget(sparkline, waterfall_chunks[4]);
}

fn render_bottom_row(frame: &mut Frame, area: Rect, model: &DashboardModel) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(50), // Audio pipeline status
            Constraint::Percentage(50), // Trends overview
        ])
        .split(area);

    render_audio_status(frame, cols[0], model);
    render_trends_overview(frame, cols[1], model);
}

fn render_audio_status(frame: &mut Frame, area: Rect, model: &DashboardModel) {
    let block = Block::default()
        .title(" Audio Streaming Pipeline (PipeWire + Opus) ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Yellow));

    let audio = &model.audio;
    let (status_text, status_color) = if audio.active {
        ("Active (Streaming)", Color::Green)
    } else {
        ("Inactive / Muted", Color::Yellow)
    };

    let text = vec![
        Line::from(vec![
            Span::styled("Pipeline Status: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                status_text,
                Style::default()
                    .fg(status_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" | Codec: ", Style::default().fg(Color::DarkGray)),
            Span::styled(&audio.codec, Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("Audio Format: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!(
                    "{}Hz, {}ch, {:.1}ms frames",
                    audio.sample_rate, audio.channels, audio.frame_size_ms
                ),
                Style::default().fg(Color::White),
            ),
            Span::styled(" | Bitrate: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{} kbps", audio.bitrate_kbps),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("Jitter: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{:.2} ms", audio.jitter_ms()),
                Style::default().fg(Color::Cyan),
            ),
            Span::styled(" | Buffer Target: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{:.2} ms", audio.buffer_delay_ms()),
                Style::default().fg(Color::White),
            ),
            Span::styled(" | Health: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{:.1}%", audio.buffer_health_pct),
                Style::default().fg(Color::Green),
            ),
        ]),
        Line::from(vec![
            Span::styled("PLC Concealments: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{} events", audio.plc_count),
                Style::default().fg(Color::LightGreen),
            ),
        ]),
    ];

    let paragraph = Paragraph::new(text).block(block);
    frame.render_widget(paragraph, area);
}

fn render_trends_overview(frame: &mut Frame, area: Rect, model: &DashboardModel) {
    let block = Block::default()
        .title(" Audio Latency History ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan));

    let audio_sparkline_data = model.audio_latency.sparkline_tenths_ms();
    let sparkline = Sparkline::default()
        .block(block)
        .data(&audio_sparkline_data)
        .style(Style::default().fg(Color::Cyan));

    frame.render_widget(sparkline, area);
}

fn render_footer(frame: &mut Frame, area: Rect) {
    let hints = Line::from(vec![
        Span::styled(
            " [q / Esc] ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("Quit Dashboard  |  "),
        Span::styled(
            " [r] ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("Reset Statistics  |  "),
        Span::styled(
            " [Space] ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("Pause/Resume  |  "),
        Span::styled(" linux-quest-host ", Style::default().fg(Color::DarkGray)),
    ]);

    let paragraph = Paragraph::new(hints).alignment(Alignment::Center);
    frame.render_widget(paragraph, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::model::{DashboardSnapshot, DisplaySnapshot};
    use linux_quest_protocol::{
        AudioLatencyBreakdown, AudioStreamTelemetry, ClientConnectionMode,
        ClientTransportTelemetry, FrameLatencyBreakdown,
    };
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn test_render_dashboard_snapshot_assertions() {
        let backend = TestBackend::new(140, 36);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut model = DashboardModel::new(30);
        let snap = DashboardSnapshot {
            uptime_secs: 125,
            streaming: true,
            displays: vec![
                DisplaySnapshot {
                    id: 1,
                    name: "Quest-Virtual-1".into(),
                    width: 2560,
                    height: 1440,
                    refresh_rate: 90,
                    fps: 89.9,
                    bitrate_mbps: 78.4,
                    codec: "AV1".into(),
                    is_virtual: true,
                },
                DisplaySnapshot {
                    id: 2,
                    name: "Quest-Virtual-2".into(),
                    width: 1920,
                    height: 1080,
                    refresh_rate: 60,
                    fps: 59.9,
                    bitrate_mbps: 34.2,
                    codec: "AV1".into(),
                    is_virtual: true,
                },
            ],
            video_latency: FrameLatencyBreakdown {
                frame_id: 100,
                capture_start_us: 1000,
                capture_duration_us: 1150,
                encode_duration_us: 2850,
                host_queue_duration_us: 300,
                transport_duration_us: 800,
                client_decode_duration_us: 3100,
                client_render_duration_us: 600,
                total_motion_to_photon_us: 8800,
            },
            audio_latency: AudioLatencyBreakdown {
                frame_id: 200,
                capture_start_us: 1000,
                capture_duration_us: 300,
                encode_duration_us: 400,
                transport_duration_us: 800,
                jitter_buffer_delay_us: 2500,
                decode_duration_us: 200,
                playout_buffer_duration_us: 500,
                total_latency_us: 4700,
            },
            client: ClientTransportTelemetry {
                mode: ClientConnectionMode::UsbAdb,
                rtt_us: 1100,
                smoothed_rtt_us: 1050,
                packet_loss_rate: 0.0001,
                packets_sent: 50000,
                packets_recv: 49995,
                packets_lost: 5,
                fec_recoveries: 5,
                bitrate_mbps: 112.6,
            },
            audio: AudioStreamTelemetry {
                active: true,
                codec: "Opus".into(),
                sample_rate: 48000,
                channels: 2,
                frame_size_ms: 2.5,
                bitrate_kbps: 128,
                jitter_us: 400,
                buffer_delay_us: 2500,
                buffer_health_pct: 100.0,
                plc_count: 0,
            },
        };

        model.apply_snapshot(snap);

        terminal
            .draw(|f| {
                render_dashboard(f, &model);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let rendered_text = buffer_to_string(buffer);

        // Assert all critical dashboard sections and strings are rendered in the buffer
        assert!(rendered_text.contains("LINUX-QUEST HOST DASHBOARD"));
        assert!(rendered_text.contains("STREAMING LIVE"));
        assert!(rendered_text.contains("USB 3.0 ADB TETHER"));
        assert!(rendered_text.contains("Meta Quest 3"));
        assert!(rendered_text.contains("Active Virtual Displays"));
        assert!(rendered_text.contains("Quest-Virtual-1"));
        assert!(rendered_text.contains("2560x1440"));
        assert!(rendered_text.contains("AV1"));
        assert!(rendered_text.contains("Real-Time Latency Waterfall Breakdown"));
        assert!(rendered_text.contains("Capture"));
        assert!(rendered_text.contains("Encode"));
        assert!(rendered_text.contains("Queue"));
        assert!(rendered_text.contains("Transport"));
        assert!(rendered_text.contains("Decode"));
        assert!(rendered_text.contains("Motion-to-Photon"));
        assert!(rendered_text.contains("Audio Latency"));
        assert!(rendered_text.contains("PipeWire + Opus"));
        assert!(rendered_text.contains("Quit Dashboard"));
    }

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
    fn test_render_empty_displays_and_wifi() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut model = DashboardModel::new(10);
        model.client.connection_mode = ClientConnectionMode::Wifi;
        model.client.rtt_ms = 4.2;

        terminal
            .draw(|f| {
                render_dashboard(f, &model);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let rendered_text = buffer_to_string(buffer);

        assert!(rendered_text.contains("WI-FI 6E TRANSPORT"));
        assert!(rendered_text.contains("No active virtual displays provisioned."));
    }
}

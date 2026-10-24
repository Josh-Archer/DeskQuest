use linux_quest_capture::DisplayCapture;
use linux_quest_encoder::VideoEncoder;
use linux_quest_input::InputInjector;
use linux_quest_protocol::{
    Packet, PacketHeader, PacketType, ProtocolError, FLAG_INTRA_REFRESH, FLAG_KEYFRAME,
    FLAG_LAST_CHUNK,
};
use linux_quest_transport::TransportEndpoint;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use thiserror::Error;

pub mod adb_link;
pub mod advertise;
pub mod audio_pipeline;
pub mod clipboard_service;
pub mod codec_select;
pub mod congestion;
pub mod headset;
pub mod input_receiver;
pub mod ipc;
pub mod link_status;
pub mod multi_display;
pub mod service;
pub mod session_registry;
pub mod tray;
pub mod tui;
pub mod video_sender;
pub mod window_panels;

pub use audio_pipeline::HostAudioPipeline;
pub use clipboard_service::{HostClipboardService, HOST_CLIPBOARD_ORIGIN_ID};
pub use headset::{HeadsetError, HeadsetHub};
pub use input_receiver::HostInputReceiver;
pub use ipc::{
    daemon_pid_path, daemon_runtime_dir, daemon_socket_path, run_ipc_server, run_ipc_server_with,
    send_daemon_request, send_daemon_request_to, DaemonCapture, DaemonRequest, DaemonResponse,
    DaemonStatus, HeadsetReport, IpcContext,
};
pub use multi_display::MultiDisplayHost;
pub use window_panels::{WindowPanelManager, WindowSource, X11WindowSource};

#[derive(Error, Debug)]
pub enum HostError {
    #[error("Capture failure: {0}")]
    Capture(#[from] linux_quest_capture::CaptureError),

    #[error("Encoder failure: {0}")]
    Encoder(#[from] linux_quest_encoder::EncoderError),

    #[error("Audio encoder failure: {0}")]
    AudioEncoder(#[from] linux_quest_encoder::AudioEncoderError),

    #[error("Transport failure: {0}")]
    Transport(#[from] linux_quest_transport::TransportError),

    #[error("Protocol error: {0}")]
    Protocol(#[from] ProtocolError),

    #[error("Input error: {0}")]
    Input(#[from] linux_quest_input::InputError),

    #[error("Display error: {0}")]
    Display(#[from] linux_quest_display::DisplayError),

    #[error("Bincode error: {0}")]
    Bincode(#[from] Box<bincode::ErrorKind>),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("IPC error: {0}")]
    Ipc(String),

    #[error("Dashboard error: {0}")]
    Dashboard(String),

    #[error("Session is not running")]
    NotRunning,
}

/// Host session managing the streaming lifecycle for a single display.
pub struct HostStreamSession<C, E, T, I>
where
    C: DisplayCapture,
    E: VideoEncoder,
    T: TransportEndpoint,
    I: InputInjector,
{
    capture: C,
    encoder: E,
    transport: T,
    input: I,
    display_host: Option<Arc<MultiDisplayHost>>,
    window_panels: Option<WindowPanelManager<Box<dyn WindowSource>>>,
    stream_id: u16,
    sequence: u32,
    running: Arc<AtomicBool>,
    /// Set once the client sent a clock-sync ping, proving it understands frame-timing
    /// reports (see `linux_quest_protocol::timing`). Older clients never get them.
    timing_opt_in: bool,
}

/// Host wall clock in microseconds since the Unix epoch (the clock frame timings use).
pub fn host_now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

impl<C, E, T, I> HostStreamSession<C, E, T, I>
where
    C: DisplayCapture,
    E: VideoEncoder,
    T: TransportEndpoint,
    I: InputInjector,
{
    pub fn new(capture: C, encoder: E, transport: T, input: I, stream_id: u16) -> Self {
        Self {
            capture,
            encoder,
            transport,
            input,
            display_host: None,
            window_panels: None,
            stream_id,
            sequence: 0,
            running: Arc::new(AtomicBool::new(false)),
            timing_opt_in: false,
        }
    }

    /// Whether the client opted in to per-frame timing reports.
    pub fn timing_opt_in(&self) -> bool {
        self.timing_opt_in
    }

    pub fn with_display_host(mut self, host: Arc<MultiDisplayHost>) -> Self {
        self.display_host = Some(host);
        self
    }

    /// Enables window listing, panel selection, and window-aware pointer routing for this
    /// session. The source is injectable so tests never invoke desktop tools or hardware.
    pub fn with_window_panels<S: WindowSource + 'static>(
        mut self,
        source: S,
        desktop_size: (u32, u32),
    ) -> Self {
        self.window_panels = Some(WindowPanelManager::new(Box::new(source), desktop_size));
        self
    }

    pub fn running_flag(&self) -> Arc<AtomicBool> {
        self.running.clone()
    }

    pub fn encoder(&self) -> &E {
        &self.encoder
    }

    pub fn encoder_mut(&mut self) -> &mut E {
        &mut self.encoder
    }

    /// Initialize capture and encoder backends.
    pub async fn init(
        &mut self,
        config: linux_quest_encoder::EncoderConfig,
    ) -> Result<(), HostError> {
        self.capture.init().await?;
        if let Err(e) = self.encoder.init(config).await {
            self.running.store(false, Ordering::SeqCst);
            return Err(HostError::Encoder(e));
        }
        self.running.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Process one frame: Capture -> Hardware Encode -> Packetize -> Transport Send.
    pub async fn step_stream_frame(&mut self) -> Result<u64, HostError> {
        if !self.running.load(Ordering::Relaxed) {
            return Err(HostError::NotRunning);
        }
        let raw_frame = self.capture.capture_frame().await?;
        let capture_us = host_now_us();
        let encode_start_us = host_now_us();
        let encoded = self.encoder.encode(&raw_frame).await?;
        let encode_end_us = host_now_us();

        let total_chunks = encoded.chunks.len();
        for (idx, chunk) in encoded.chunks.into_iter().enumerate() {
            let serialized_payload = chunk.serialize()?;
            self.sequence += 1;

            let mut flags = 0;
            if encoded.is_keyframe {
                flags |= FLAG_KEYFRAME;
            }
            if encoded.is_intra_refresh {
                flags |= FLAG_INTRA_REFRESH;
            }
            if idx + 1 == total_chunks {
                flags |= FLAG_LAST_CHUNK;
            }

            let header = PacketHeader::new(
                PacketType::VideoFrameChunk,
                self.stream_id,
                self.sequence,
                raw_frame.pts_us,
                &serialized_payload,
            )
            .with_flags(flags);

            let packet = Packet::new(header, serialized_payload);
            self.transport.send_packet(packet).await?;
        }

        if self.timing_opt_in {
            let timing = linux_quest_protocol::HostFrameTiming {
                frame_id: encoded.frame_id,
                stream_id: self.stream_id,
                capture_us,
                encode_start_us,
                encode_end_us,
                send_us: host_now_us(),
            };
            let payload = bytes::Bytes::from(timing.encode());
            self.sequence += 1;
            let header = PacketHeader::new(
                PacketType::TelemetryReport,
                self.stream_id,
                self.sequence,
                timing.send_us,
                &payload,
            );
            self.transport
                .send_packet(Packet::new(header, payload))
                .await?;
        }

        Ok(encoded.frame_id)
    }

    /// Process an incoming packet from the Quest client.
    pub async fn handle_incoming_packet(&mut self, packet: Packet) -> Result<(), HostError> {
        match packet.header.packet_type {
            PacketType::Ping => {
                let t2 = host_now_us();
                let payload = match linux_quest_protocol::timing::decode_clock_sync_request(
                    &packet.payload,
                ) {
                    Some(t1) => {
                        self.timing_opt_in = true;
                        bytes::Bytes::from(
                            linux_quest_protocol::timing::encode_clock_sync_response(
                                t1,
                                t2,
                                host_now_us(),
                            ),
                        )
                    }
                    None => packet.payload,
                };
                let pong_header = PacketHeader::new(
                    PacketType::Pong,
                    self.stream_id,
                    self.sequence + 1,
                    packet.header.timestamp_us,
                    &payload,
                );
                self.transport
                    .send_packet(Packet::new(pong_header, payload))
                    .await?;
            }
            PacketType::ReferencePictureInvalidation => {
                if packet.payload.len() >= 8 {
                    let pts_us = u64::from_be_bytes(packet.payload[..8].try_into().unwrap());
                    self.encoder.invalidate_reference_picture(pts_us);
                }
            }
            PacketType::InputEvent => {
                if let Ok(batch) = linux_quest_protocol::InputBatch::decode(&packet.payload) {
                    for event in batch.events {
                        let event = self
                            .window_panels
                            .as_mut()
                            .map_or(event.clone(), |panels| panels.map_input(event));
                        self.input.inject_event(event)?;
                    }
                } else if let Ok(event) = bincode::deserialize(&packet.payload) {
                    self.input.inject_event(event)?;
                }
            }
            PacketType::DisplayConfig => {
                let reply = match bincode::deserialize::<linux_quest_protocol::DisplayConfigMessage>(
                    &packet.payload,
                ) {
                    Ok(msg) => {
                        tracing::info!(msg = ?msg, "Received DisplayConfig command from client");
                        if let Some(host) = &self.display_host {
                            match msg {
                                linux_quest_protocol::DisplayConfigMessage::SetMonitorCount {
                                    count,
                                    width,
                                    height,
                                    refresh_rate,
                                    dpi,
                                    layout_mode,
                                } => {
                                    let layout = match layout_mode {
                                        1 => linux_quest_display::DisplayLayoutMode::Vertical,
                                        2 => linux_quest_display::DisplayLayoutMode::Grid,
                                        _ => linux_quest_display::DisplayLayoutMode::Horizontal,
                                    };
                                    let base_cfg = linux_quest_display::VirtualMonitorConfig::new(
                                        1,
                                        "Quest-Virtual",
                                        width,
                                        height,
                                        refresh_rate,
                                    )
                                    .with_dpi(dpi);
                                    match host.toggle_monitors(count, &base_cfg, layout).await {
                                        Ok(monitors) => {
                                            let display_infos: Vec<
                                                linux_quest_protocol::DisplayInfo,
                                            > = monitors
                                                .into_iter()
                                                .map(|m| linux_quest_protocol::DisplayInfo {
                                                    display_id: m.id as u16,
                                                    name: m.name,
                                                    width: m.width,
                                                    height: m.height,
                                                    refresh_rate: m.refresh_rate,
                                                    dpi: m.dpi,
                                                    is_virtual: m.is_virtual,
                                                    x: m.x,
                                                    y: m.y,
                                                })
                                                .collect();
                                            Some(linux_quest_protocol::DisplayConfigMessage::ActiveMonitors(
                                                display_infos,
                                            ))
                                        }
                                        Err(e) => {
                                            Some(linux_quest_protocol::DisplayConfigMessage::Error(
                                                e.to_string(),
                                            ))
                                        }
                                    }
                                }
                                linux_quest_protocol::DisplayConfigMessage::ActiveMonitors(_)
                                | linux_quest_protocol::DisplayConfigMessage::MonitorAdded(_)
                                | linux_quest_protocol::DisplayConfigMessage::MonitorRemoved {
                                    ..
                                }
                                | linux_quest_protocol::DisplayConfigMessage::WindowList(_)
                                | linux_quest_protocol::DisplayConfigMessage::ActiveWindowPanels(_)
                                | linux_quest_protocol::DisplayConfigMessage::Error(_) => None,
                                linux_quest_protocol::DisplayConfigMessage::ListWindows => {
                                    Some(match self.window_panels.as_mut() {
                                        Some(panels) => match panels.list_windows() {
                                            Ok(windows) => linux_quest_protocol::DisplayConfigMessage::WindowList(windows),
                                            Err(e) => linux_quest_protocol::DisplayConfigMessage::Error(e.to_string()),
                                        },
                                        None => linux_quest_protocol::DisplayConfigMessage::Error(
                                            "Window panel support is not initialized on this session".into(),
                                        ),
                                    })
                                }
                                linux_quest_protocol::DisplayConfigMessage::SetWindowPanels { window_ids } => {
                                    Some(match self.window_panels.as_mut() {
                                        Some(panels) => match panels.set_panels(&window_ids) {
                                            Ok(displays) => linux_quest_protocol::DisplayConfigMessage::ActiveWindowPanels(displays),
                                            Err(e) => linux_quest_protocol::DisplayConfigMessage::Error(e.to_string()),
                                        },
                                        None => linux_quest_protocol::DisplayConfigMessage::Error(
                                            "Window panel support is not initialized on this session".into(),
                                        ),
                                    })
                                }
                            }
                        } else {
                            Some(linux_quest_protocol::DisplayConfigMessage::Error(
                                "Display host not initialized on stream session".into(),
                            ))
                        }
                    }
                    Err(e) => Some(linux_quest_protocol::DisplayConfigMessage::Error(format!(
                        "Failed to deserialize DisplayConfig message: {e}"
                    ))),
                };

                if let Some(reply_msg) = reply {
                    let reply_bytes = bincode::serialize(&reply_msg)?;
                    self.sequence += 1;
                    let reply_header = PacketHeader::new(
                        PacketType::DisplayConfig,
                        self.stream_id,
                        self.sequence,
                        packet.header.timestamp_us,
                        &reply_bytes,
                    );
                    self.transport
                        .send_packet(Packet::new(reply_header, reply_bytes.into()))
                        .await?;
                }
            }
            PacketType::Disconnect => {
                self.running.store(false, Ordering::SeqCst);
                let _ = self.input.release_all();
            }
            _ => (),
        }
        Ok(())
    }
}

impl<C, E, T, I> Drop for HostStreamSession<C, E, T, I>
where
    C: DisplayCapture,
    E: VideoEncoder,
    T: TransportEndpoint,
    I: InputInjector,
{
    fn drop(&mut self) {
        let _ = self.input.release_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_quest_capture::SyntheticCapture;
    use linux_quest_encoder::{EncoderConfig, MockVideoEncoder};
    use linux_quest_input::MockInputInjector;
    use linux_quest_protocol::{ElementState, InputEvent, MouseButton};
    use linux_quest_transport::{LoopbackEndpoint, PacedUdpEndpoint, TcpEndpoint};

    #[tokio::test]
    async fn test_host_stream_single_frame() {
        let capture = SyntheticCapture::new(0, 1920, 1080, 60);
        let encoder = MockVideoEncoder::new(EncoderConfig::default());
        let (ep_host, mut ep_client) = LoopbackEndpoint::create_pair();
        let input = MockInputInjector::new();

        let mut session = HostStreamSession::new(capture, encoder, ep_host, input, 0);
        session
            .init(EncoderConfig::default())
            .await
            .expect("HostStreamSession init failed");

        let frame_id = session
            .step_stream_frame()
            .await
            .expect("Step stream frame failed");
        assert_eq!(frame_id, 1);

        // Client receives packets
        let packet = ep_client.recv_packet().await.expect("Recv failed");
        assert_eq!(packet.header.packet_type, PacketType::VideoFrameChunk);
    }

    #[tokio::test]
    async fn test_host_stream_not_running_guard() {
        let capture = SyntheticCapture::new(0, 1920, 1080, 60);
        let encoder = MockVideoEncoder::new(EncoderConfig::default());
        let (ep_host, _) = LoopbackEndpoint::create_pair();
        let input = MockInputInjector::new();

        let mut session = HostStreamSession::new(capture, encoder, ep_host, input, 0);
        // Before calling init(), step_stream_frame should return NotRunning error
        let err = session
            .step_stream_frame()
            .await
            .expect_err("Should fail when not running");
        match err {
            HostError::NotRunning => (),
            other => panic!("Expected HostError::NotRunning, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_host_handle_input_event() {
        let capture = SyntheticCapture::new(0, 1920, 1080, 60);
        let encoder = MockVideoEncoder::new(EncoderConfig::default());
        let (ep_host, _) = LoopbackEndpoint::create_pair();
        let input = MockInputInjector::new();

        let mut session = HostStreamSession::new(capture, encoder, ep_host, input, 0);

        let input_event = InputEvent::MouseButton {
            button: MouseButton::Right,
            state: ElementState::Pressed,
        };
        let payload = bytes::Bytes::from(bincode::serialize(&input_event).unwrap());
        let header = PacketHeader::new(PacketType::InputEvent, 0, 1, 1000, &payload);
        let packet = Packet::new(header, payload);

        session
            .handle_incoming_packet(packet)
            .await
            .expect("Handle input failed");
        assert_eq!(session.input.received_events.len(), 1);
        assert_eq!(session.input.received_events[0], input_event);
    }

    #[tokio::test]
    async fn test_host_stream_over_tcp_endpoint() {
        let capture = SyntheticCapture::new(0, 1280, 720, 60);
        let encoder = MockVideoEncoder::new(EncoderConfig::default());
        let (ep_host, mut ep_client) = TcpEndpoint::create_connected_pair().await.unwrap();
        let input = MockInputInjector::new();

        let mut session = HostStreamSession::new(capture, encoder, ep_host, input, 0);
        session
            .init(EncoderConfig::default())
            .await
            .expect("HostStreamSession init failed");

        let frame_id = session
            .step_stream_frame()
            .await
            .expect("Step stream frame failed");
        assert_eq!(frame_id, 1);

        let packet = ep_client.recv_packet().await.expect("Recv failed");
        assert_eq!(packet.header.packet_type, PacketType::VideoFrameChunk);
    }

    #[tokio::test]
    async fn clock_sync_ping_gets_host_timestamps_and_enables_frame_timing() {
        use linux_quest_protocol::timing;
        let capture = SyntheticCapture::new(0, 1280, 720, 60);
        let encoder = MockVideoEncoder::new(EncoderConfig::default());
        let (ep_host, mut ep_client) = LoopbackEndpoint::create_pair();
        let mut session =
            HostStreamSession::new(capture, encoder, ep_host, MockInputInjector::new(), 0);
        session.init(EncoderConfig::default()).await.unwrap();

        // Legacy client: no timing reports, plain pings echoed unchanged.
        session.step_stream_frame().await.unwrap();
        let mut kinds = Vec::new();
        while let Ok(Ok(p)) = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            ep_client.recv_packet(),
        )
        .await
        {
            kinds.push(p.header.packet_type);
        }
        assert!(!kinds.contains(&PacketType::TelemetryReport));
        assert!(!session.timing_opt_in());

        let before = host_now_us();
        let req = bytes::Bytes::from(timing::encode_clock_sync_request(77));
        let hdr = PacketHeader::new(PacketType::Ping, 0, 1, 77, &req);
        session
            .handle_incoming_packet(Packet::new(hdr, req))
            .await
            .unwrap();
        let pong = ep_client.recv_packet().await.unwrap();
        assert_eq!(pong.header.packet_type, PacketType::Pong);
        let (t1, t2, t3) = timing::decode_clock_sync_response(&pong.payload).unwrap();
        assert_eq!(t1, 77);
        assert!(t2 >= before && t3 >= t2);
        assert!(session.timing_opt_in());

        let frame_id = session.step_stream_frame().await.unwrap();
        let mut timing_report = None;
        while let Ok(Ok(p)) = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            ep_client.recv_packet(),
        )
        .await
        {
            if p.header.packet_type == PacketType::TelemetryReport {
                timing_report = linux_quest_protocol::HostFrameTiming::decode(&p.payload);
            }
        }
        let t = timing_report.expect("frame timing report");
        assert_eq!(t.frame_id, frame_id);
        assert!(t.capture_us <= t.encode_start_us);
        assert!(t.encode_start_us <= t.encode_end_us);
        assert!(t.encode_end_us <= t.send_us);
    }

    #[tokio::test]
    async fn test_host_stream_over_paced_udp_endpoint() {
        let capture = SyntheticCapture::new(0, 1280, 720, 60);
        let encoder = MockVideoEncoder::new(EncoderConfig::default());
        let (ep_host, mut ep_client) = PacedUdpEndpoint::create_connected_pair(100).await.unwrap();
        let input = MockInputInjector::new();

        let mut session = HostStreamSession::new(capture, encoder, ep_host, input, 0);
        session
            .init(EncoderConfig::default())
            .await
            .expect("HostStreamSession init failed");

        let frame_id = session
            .step_stream_frame()
            .await
            .expect("Step stream frame failed");
        assert_eq!(frame_id, 1);

        let packet = ep_client.recv_packet().await.expect("Recv failed");
        assert_eq!(packet.header.packet_type, PacketType::VideoFrameChunk);
    }

    #[tokio::test]
    async fn test_host_handle_display_config_packet() {
        let capture = SyntheticCapture::new(0, 1920, 1080, 60);
        let encoder = MockVideoEncoder::new(EncoderConfig::default());
        let (ep_host, mut ep_client) = LoopbackEndpoint::create_pair();
        let input = MockInputInjector::new();

        let mock_backend = linux_quest_display::backend::mock::MockDisplayBackend::new();
        let host = Arc::new(MultiDisplayHost::new(Box::new(mock_backend)));

        let mut session =
            HostStreamSession::new(capture, encoder, ep_host, input, 0).with_display_host(host);

        let req = linux_quest_protocol::DisplayConfigMessage::SetMonitorCount {
            count: 2,
            width: 1920,
            height: 1080,
            refresh_rate: 60,
            dpi: 120,
            layout_mode: 0,
        };
        let payload = bytes::Bytes::from(bincode::serialize(&req).unwrap());
        let header = PacketHeader::new(PacketType::DisplayConfig, 0, 1, 1000, &payload);
        let packet = Packet::new(header, payload);

        session
            .handle_incoming_packet(packet)
            .await
            .expect("Handle DisplayConfig failed");

        let reply_packet = ep_client.recv_packet().await.expect("Recv reply failed");
        assert_eq!(reply_packet.header.packet_type, PacketType::DisplayConfig);
        let reply: linux_quest_protocol::DisplayConfigMessage =
            bincode::deserialize(&reply_packet.payload).expect("Deserialize reply failed");

        match reply {
            linux_quest_protocol::DisplayConfigMessage::ActiveMonitors(monitors) => {
                assert_eq!(monitors.len(), 2);
                assert_eq!(monitors[0].display_id, 1);
                assert_eq!(monitors[0].dpi, 120);
                assert_eq!(monitors[1].display_id, 2);
                assert_eq!(monitors[1].dpi, 120);
            }
            _ => panic!("Expected ActiveMonitors response"),
        }
    }

    #[tokio::test]
    async fn window_picker_messages_return_mock_windows_and_panel_descriptors() {
        use crate::window_panels::mock::MockWindowSource;
        use linux_quest_protocol::{DisplayConfigMessage, WindowInfo, WINDOW_STREAM_ID_BASE};

        let capture = SyntheticCapture::new(0, 1920, 1080, 60);
        let encoder = MockVideoEncoder::new(EncoderConfig::default());
        let (ep_host, mut ep_client) = LoopbackEndpoint::create_pair();
        let input = MockInputInjector::new();
        let display_host = Arc::new(MultiDisplayHost::new(Box::new(
            linux_quest_display::backend::mock::MockDisplayBackend::new(),
        )));
        let source = MockWindowSource {
            windows: vec![
                WindowInfo {
                    window_id: 10,
                    title: "Editor".into(),
                    app_name: "editor".into(),
                    width: 1200,
                    height: 800,
                    x: 100,
                    y: 50,
                },
                WindowInfo {
                    window_id: 11,
                    title: "Browser".into(),
                    app_name: "browser".into(),
                    width: 1600,
                    height: 900,
                    x: 1300,
                    y: 50,
                },
            ],
            focused: vec![],
        };
        let mut session = HostStreamSession::new(capture, encoder, ep_host, input, 0)
            .with_display_host(display_host)
            .with_window_panels(source, (3000, 2000));

        for (sequence, request, expected) in [
            (
                1,
                DisplayConfigMessage::ListWindows,
                DisplayConfigMessage::WindowList(vec![
                    WindowInfo {
                        window_id: 10,
                        title: "Editor".into(),
                        app_name: "editor".into(),
                        width: 1200,
                        height: 800,
                        x: 100,
                        y: 50,
                    },
                    WindowInfo {
                        window_id: 11,
                        title: "Browser".into(),
                        app_name: "browser".into(),
                        width: 1600,
                        height: 900,
                        x: 1300,
                        y: 50,
                    },
                ]),
            ),
            (
                2,
                DisplayConfigMessage::SetWindowPanels {
                    window_ids: vec![10, 11],
                },
                DisplayConfigMessage::ActiveWindowPanels(vec![
                    linux_quest_protocol::DisplayInfo {
                        display_id: WINDOW_STREAM_ID_BASE,
                        name: "Editor".into(),
                        width: 1200,
                        height: 800,
                        refresh_rate: 60,
                        dpi: 96,
                        is_virtual: false,
                        x: 100,
                        y: 50,
                    },
                    linux_quest_protocol::DisplayInfo {
                        display_id: WINDOW_STREAM_ID_BASE + 1,
                        name: "Browser".into(),
                        width: 1600,
                        height: 900,
                        refresh_rate: 60,
                        dpi: 96,
                        is_virtual: false,
                        x: 1300,
                        y: 50,
                    },
                ]),
            ),
        ] {
            let payload = bytes::Bytes::from(bincode::serialize(&request).unwrap());
            let packet = Packet::new(
                PacketHeader::new(PacketType::DisplayConfig, 0, sequence, 1000, &payload),
                payload,
            );
            session.handle_incoming_packet(packet).await.unwrap();
            let response = ep_client.recv_packet().await.unwrap();
            let response: DisplayConfigMessage = bincode::deserialize(&response.payload).unwrap();
            assert_eq!(response, expected);
        }
    }

    #[tokio::test]
    async fn test_host_audio_pipeline_multi_frame() {
        let source = linux_quest_capture::SineAudioSource::new_default();
        let enc_cfg = linux_quest_encoder::AudioEncoderConfig::default();
        let encoder = linux_quest_encoder::AutoAudioEncoder::new(enc_cfg.clone());
        let (ep_host, mut ep_client) = LoopbackEndpoint::create_pair();

        let mut pipeline = HostAudioPipeline::new(source, encoder, ep_host, 1);
        pipeline
            .init(enc_cfg)
            .await
            .expect("Audio pipeline init failed");

        // Stream 5 audio frames
        for expected_id in 1..=5 {
            let fid = pipeline
                .step_stream_audio_frame()
                .await
                .expect("Step audio failed");
            assert_eq!(fid, expected_id);

            let pkt = ep_client
                .recv_packet()
                .await
                .expect("Client recv audio failed");
            assert_eq!(pkt.header.packet_type, PacketType::AudioFrame);
            assert_eq!(pkt.header.stream_id, 1);
            assert_eq!(pkt.header.sequence, expected_id as u32);

            let audio = linux_quest_protocol::AudioPacket::deserialize(&pkt.payload)
                .expect("Deserialize audio failed");
            assert_eq!(audio.meta.frame_id, expected_id);
            assert_eq!(audio.meta.sample_rate, 48000);
            assert_eq!(audio.meta.channels, 2);
            assert_eq!(audio.meta.samples_per_channel, 120);
            assert_eq!(audio.duration_us(), 2500);
        }
    }

    #[tokio::test]
    async fn test_host_audio_pipeline_ping_pong() {
        let source = linux_quest_capture::SineAudioSource::new_default();
        let encoder = linux_quest_encoder::MockAudioEncoder::new(
            linux_quest_encoder::AudioEncoderConfig::default(),
        );
        let (ep_host, mut ep_client) = LoopbackEndpoint::create_pair();

        let mut pipeline = HostAudioPipeline::new(source, encoder, ep_host, 1);

        let ping_payload = bytes::Bytes::from_static(b"audio-ping");
        let ping_hdr = PacketHeader::new(PacketType::Ping, 1, 1, 12345, &ping_payload);
        let ping_pkt = Packet::new(ping_hdr, ping_payload.clone());

        pipeline
            .handle_incoming_packet(ping_pkt)
            .await
            .expect("Handle ping failed");

        let pong_pkt = ep_client.recv_packet().await.expect("Recv pong failed");
        assert_eq!(pong_pkt.header.packet_type, PacketType::Pong);
        assert_eq!(pong_pkt.payload, ping_payload);
    }
}

//! A stand-in daemon for trying the desktop app without a headset or GPU.
//!
//! It runs the daemon's real IPC server and control service with mock hardware (no capture,
//! no encoder, a mock display backend), and a simulated headset (the real headset runtime with
//! a simulated OpenXR session) that connects to that control service over loopback, showing
//! three screens. It binds no fixed port, captures nothing and never opens the screen-share
//! dialog. "Choose Screen…" pretends the user picked a screen after a second.
//!
//! ```text
//! export LINUX_QUEST_RUNTIME_DIR=$(mktemp -d)
//! cargo run -p linux-quest-desktop --example demo_daemon &
//! cargo run -p linux-quest-desktop
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use parking_lot::Mutex;

use linux_quest_client::host_control::{HostLink, LinkConfig, STATUS_INTERVAL};
use linux_quest_client::{ClientConfig, QuestClientRuntime};
use linux_quest_display::backend::mock::MockDisplayBackend;
use linux_quest_host::congestion::RateCoordinator;
use linux_quest_host::ipc::RUNTIME_DIR_ENV;
use linux_quest_host::link_status::{new_shared_link_tracker, DEFAULT_INPUT_LIVENESS};
use linux_quest_host::video_sender::{CaptureKind, VideoControl};
use linux_quest_host::{
    run_ipc_server_with, HeadsetHub, HostClipboardService, IpcContext, MultiDisplayHost,
    HOST_CLIPBOARD_ORIGIN_ID,
};
use linux_quest_input::MockClipboardBackend;
use linux_quest_protocol::packet::{Packet, PacketHeader, PacketType};
use linux_quest_protocol::video::{VideoChunk, VideoChunkMeta, VideoCodec};
use linux_quest_protocol::ClipboardSyncEngine;

const STREAMS: u16 = 3;

fn video_packet(stream_id: u16, frame_id: u64) -> Packet {
    let (width, height) = if stream_id == 0 {
        (3840, 2160)
    } else {
        (1920, 1080)
    };
    let meta = VideoChunkMeta {
        frame_id,
        chunk_index: 0,
        total_chunks: 1,
        codec: VideoCodec::Av1,
        is_keyframe: frame_id % 60 == 1,
        is_intra_refresh: false,
        width,
        height,
        fps: 60,
        pts_us: frame_id * 16_666,
    };
    let bytes = VideoChunk::new(meta, Bytes::from_static(&[0x0A, 0x02, 0x11, 0x22]))
        .serialize()
        .expect("chunk");
    let header = PacketHeader::new(
        PacketType::VideoFrameChunk,
        stream_id,
        frame_id as u32,
        frame_id * 16_666,
        &bytes,
    );
    Packet::new(header, bytes)
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();
    let Some(dir) = std::env::var_os(RUNTIME_DIR_ENV).filter(|d| !d.is_empty()) else {
        eprintln!(
            "Set {RUNTIME_DIR_ENV} to a private empty directory first, so this demo does not \
             take the real daemon's socket:\n  export {RUNTIME_DIR_ENV}=$(mktemp -d)"
        );
        std::process::exit(2);
    };
    println!(
        "demo daemon socket in {}",
        std::path::Path::new(&dir).display()
    );

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let link = new_shared_link_tracker(DEFAULT_INPUT_LIVENESS);
    let hub = Arc::new(HeadsetHub::new());

    // The control service the simulated headset talks to, on an ephemeral loopback port.
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .expect("loopback listener");
    let control_addr = listener.local_addr().expect("address");
    let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
    let (_in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut service = HostClipboardService::new(
        Arc::new(Mutex::new(MockClipboardBackend::new())),
        Arc::new(Mutex::new(ClipboardSyncEngine::new(
            HOST_CLIPBOARD_ORIGIN_ID,
        ))),
        out_tx,
        in_rx,
    )
    .with_listener(listener)
    .with_headset_hub(hub.clone());
    rt.spawn(async move {
        let _ = service.run().await;
    });

    // The headset "streams over Wi-Fi": its input keepalives come from a LAN address.
    {
        let link = link.clone();
        rt.spawn(async move {
            let peer = "192.168.1.50:48442".parse().expect("address");
            loop {
                link.lock().input_packet(peer, Instant::now());
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        });
    }

    // Three video streams at an adaptive 30 Mbit/s, sending a little under their targets.
    let rate = Arc::new(RateCoordinator::new(30_000, true));
    let streams: Vec<_> = (0..STREAMS).map(|id| rate.register_stream(id)).collect();
    linux_quest_host::congestion::install_global(rate.clone());
    rt.spawn(async move {
        let mut tick = 0u32;
        loop {
            tick = tick.wrapping_add(1);
            for (i, s) in streams.iter().enumerate() {
                let wobble = 0.9 + 0.05 * (((tick + i as u32) % 3) as f64);
                s.set_actual_kbps((f64::from(s.target_kbps()) * wobble) as u32);
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });

    // "Choose Screen…": the user picks a screen after a second.
    let video = VideoControl::new();
    video.set_running(STREAMS, CaptureKind::Screen);
    let mut picks = video.register_reselect();
    rt.spawn(async move {
        while let Some(request) = picks.recv().await {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let _ = request.reply.send(Ok(()));
        }
    });

    let ipc = IpcContext {
        host: Arc::new(MultiDisplayHost::new(Box::new(MockDisplayBackend::new()))),
        link,
        headset: hub,
        video,
        rate: Some(rate),
    };
    let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel(1);
    let server = rt.spawn(async move {
        if let Err(e) = run_ipc_server_with(ipc, shutdown_rx).await {
            eprintln!("IPC server: {e}");
        }
    });

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        rt.spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            stop.store(true, Ordering::SeqCst);
        });
    }

    // The simulated headset: the real runtime, fed a frame per stream about 60 times a second.
    let link_config = LinkConfig {
        retry_interval: Duration::from_millis(200),
        ..LinkConfig::new(vec![control_addr])
    };
    let headset_link = HostLink::spawn(link_config).expect("headset link");
    let mut headset = QuestClientRuntime::new(ClientConfig::default()).expect("headset runtime");
    headset.set_device_name("Simulated Quest 3");
    let mut frame_id = 0u64;
    let mut next_status = Instant::now();
    while !stop.load(Ordering::SeqCst) {
        frame_id += 1;
        let now = Instant::now();
        for stream in 0..STREAMS {
            headset.ingest_packet(video_packet(stream, frame_id), now);
        }
        let _ = headset.step_frame_at(now + Duration::from_millis(10));
        while let Some(command) = headset_link.try_command() {
            headset.apply_host_command(command);
        }
        if headset_link.is_connected() && (headset.take_host_status_dirty() || now >= next_status) {
            headset_link.publish(headset.headset_status());
            next_status = now + STATUS_INTERVAL;
        }
        std::thread::sleep(Duration::from_millis(16));
    }

    headset_link.stop();
    let _ = shutdown_tx.send(());
    let _ = rt.block_on(server);
    println!("demo daemon stopped");
}

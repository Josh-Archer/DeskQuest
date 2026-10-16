//! End-to-end integration tests for bidirectional clipboard text synchronization
//! between Linux host and Quest client over reliable TCP control channel.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::mpsc;

use linux_quest_client::{ClientClipboardSync, ClipboardManager, MockClipboardManager};
use linux_quest_host::{HostClipboardService, HOST_CLIPBOARD_ORIGIN_ID};
use linux_quest_input::{ClipboardBackend, ClipboardWatcherConfig, MockClipboardBackend};
use linux_quest_protocol::{chunk_clipboard_text, ClipboardMessage, ClipboardSyncEngine};
use linux_quest_transport::{ControlChannel, ControlMessage};

const CLIENT_ORIGIN_ID: u64 = 2;

#[tokio::test]
async fn test_bidirectional_clipboard_sync_host_to_client_and_back() {
    let (mut client_ctrl, mut server_ctrl) = ControlChannel::create_connected_pair().await.unwrap();

    let host_backend = Arc::new(Mutex::new(MockClipboardBackend::new()));
    let host_engine = Arc::new(Mutex::new(ClipboardSyncEngine::new(
        HOST_CLIPBOARD_ORIGIN_ID,
    )));

    let (host_out_tx, mut host_out_rx) = mpsc::unbounded_channel::<ControlMessage>();
    let (_host_in_tx, host_in_rx) = mpsc::unbounded_channel::<ClipboardMessage>();

    let mut host_service =
        HostClipboardService::new(host_backend.clone(), host_engine, host_out_tx, host_in_rx)
            .with_watcher_config(ClipboardWatcherConfig {
                poll_interval: Duration::from_millis(5),
                debounce_duration: Duration::ZERO,
            });

    let client_backend = Arc::new(Mutex::new(MockClipboardManager::new()));
    let mut client_sync = ClientClipboardSync::new(client_backend.clone(), CLIENT_ORIGIN_ID)
        .with_timing(Duration::from_millis(5), Duration::ZERO);

    // --- Phase 1: Host copies text -> syncs to Client ---
    host_backend
        .lock()
        .set_text("Host initial copied text: fn main() {}")
        .unwrap();

    let host_outgoing = host_service
        .step_local()
        .unwrap()
        .expect("Host should produce outgoing messages");
    assert_eq!(host_outgoing.len(), 1);

    let host_msg = host_out_rx.recv().await.unwrap();
    // Send over TCP control channel from server to client
    server_ctrl.send_message(&host_msg).await.unwrap();

    let client_recv = client_ctrl.recv_message().await.unwrap();
    match client_recv {
        ControlMessage::Clipboard(msg) => {
            let applied = client_sync.handle_incoming(msg).unwrap();
            assert!(
                applied,
                "Client should accept and apply host clipboard text"
            );
        }
        other => panic!("Expected Clipboard message, got {:?}", other),
    }

    assert_eq!(
        client_backend.lock().get_text().unwrap(),
        "Host initial copied text: fn main() {}"
    );

    // Client poll must be suppressed to break echo loop!
    let client_echo = client_sync.poll_local().unwrap();
    assert!(
        client_echo.is_none(),
        "Echo loop error: client re-emitted host text!"
    );

    // --- Phase 2: Client copies text -> syncs to Host ---
    client_backend
        .lock()
        .set_text("Quest client text: println!(\"In VR!\");")
        .unwrap();

    let client_outgoing = client_sync
        .poll_local()
        .unwrap()
        .expect("Client should produce outgoing messages");
    assert_eq!(client_outgoing.len(), 1);

    let client_msg = ControlMessage::Clipboard(client_outgoing[0].clone());
    client_ctrl.send_message(&client_msg).await.unwrap();

    let server_recv = server_ctrl.recv_message().await.unwrap();
    match server_recv {
        ControlMessage::Clipboard(msg) => {
            let applied = host_service.handle_incoming(msg).unwrap();
            assert!(
                applied,
                "Host should accept and apply client clipboard text"
            );
        }
        other => panic!("Expected Clipboard message, got {:?}", other),
    }

    assert_eq!(
        host_backend.lock().get_text().unwrap(),
        "Quest client text: println!(\"In VR!\");"
    );

    // Host poll must be suppressed to break echo loop!
    let host_echo = host_service.step_local().unwrap();
    assert!(
        host_echo.is_none(),
        "Echo loop error: host re-emitted client text!"
    );
}

#[tokio::test]
async fn test_large_clipboard_chunking_and_reassembly_across_control_channel() {
    let (mut client_ctrl, mut server_ctrl) = ControlChannel::create_connected_pair().await.unwrap();

    let client_backend = Arc::new(Mutex::new(MockClipboardManager::new()));
    let mut client_sync = ClientClipboardSync::new(client_backend.clone(), CLIENT_ORIGIN_ID);

    // Create a 150 KiB text with multi-byte unicode emojis
    let base_text = "🦀 Linux Quest Spatial Multi-Monitor Streaming with Ultra-Low Latency! 🚀\n";
    let repeat_count = (150 * 1024) / base_text.len() + 1;
    let large_text = base_text.repeat(repeat_count);

    let chunks = chunk_clipboard_text(
        HOST_CLIPBOARD_ORIGIN_ID,
        1,
        &large_text,
        32 * 1024, // 32KB chunks
    )
    .unwrap();
    assert!(chunks.len() >= 5);

    // Stream all chunks across the TCP control channel
    for chunk in chunks {
        let msg = ControlMessage::Clipboard(chunk);
        server_ctrl.send_message(&msg).await.unwrap();

        let received = client_ctrl.recv_message().await.unwrap();
        if let ControlMessage::Clipboard(clip_msg) = received {
            let _ = client_sync.handle_incoming(clip_msg).unwrap();
        }
    }

    // Client should now have fully reassembled the complete large text
    let reassembled_text = client_backend.lock().get_text().unwrap();
    assert_eq!(reassembled_text.len(), large_text.len());
    assert_eq!(reassembled_text, large_text);
}

#[tokio::test]
async fn test_concurrent_clipboard_conflict_resolution_convergence() {
    let mut host_engine = ClipboardSyncEngine::new(HOST_CLIPBOARD_ORIGIN_ID);
    let mut client_engine = ClipboardSyncEngine::new(CLIENT_ORIGIN_ID);

    // Host writes text A at version 1
    let host_msgs = host_engine
        .process_local_change("Concurrent Write from Host")
        .unwrap()
        .unwrap();

    // Client writes text B at version 1
    let client_msgs = client_engine
        .process_local_change("Concurrent Write from Client")
        .unwrap()
        .unwrap();

    // In-flight messages cross on the network:
    // Host receives client's message (origin 2 > origin 1 -> Client wins tie-break)
    let host_action = host_engine
        .process_incoming_message(client_msgs[0].clone())
        .unwrap();
    assert_eq!(
        host_action,
        linux_quest_protocol::SyncAction::ApplyToLocal("Concurrent Write from Client".to_string())
    );

    // Client receives host's message (origin 1 < origin 2 -> Host loses tie-break)
    let client_action = client_engine
        .process_incoming_message(host_msgs[0].clone())
        .unwrap();
    assert_eq!(
        client_action,
        linux_quest_protocol::SyncAction::ConflictLost
    );

    // Both converge to the identical winner text!
    assert_eq!(
        host_engine.current_text(),
        Some("Concurrent Write from Client")
    );
    assert_eq!(
        client_engine.current_text(),
        Some("Concurrent Write from Client")
    );
}

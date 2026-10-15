//! Clipboard synchronization protocol types, chunking, reassembly, and sync engine.
//!
//! Provides bidirectional UTF-8 text synchronization primitives between Linux host
//! and Meta Quest OpenXR client with size limits, chunking for large transfers,
//! origin tracking, monotonic versioning, echo loop suppression, and last-writer conflict resolution.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;

/// Maximum allowed clipboard text size in bytes (4 MiB).
pub const MAX_CLIPBOARD_TEXT_SIZE: usize = 4 * 1024 * 1024;

/// Default maximum size per clipboard chunk in bytes (64 KiB).
pub const DEFAULT_CLIPBOARD_CHUNK_SIZE: usize = 64 * 1024;

/// Protocol message representing a clipboard text snapshot or chunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardMessage {
    /// Identifier of the origin node (e.g. host = 1, client = 2).
    pub origin_id: u64,
    /// Monotonically increasing version counter from this origin.
    pub version: u64,
    /// 0-based index of this chunk.
    pub chunk_index: u32,
    /// Total number of chunks representing this version of clipboard text.
    pub total_chunks: u32,
    /// Total length of the full reassembled clipboard text in bytes.
    pub total_bytes: u32,
    /// UTF-8 text payload for this chunk.
    pub text: String,
}

impl ClipboardMessage {
    /// Creates a single, unchunked clipboard message.
    pub fn single(origin_id: u64, version: u64, text: String) -> Result<Self, ProtocolError> {
        let total_bytes = text.len();
        if total_bytes > MAX_CLIPBOARD_TEXT_SIZE {
            return Err(ProtocolError::ClipboardTooLarge {
                size: total_bytes,
                limit: MAX_CLIPBOARD_TEXT_SIZE,
            });
        }

        Ok(Self {
            origin_id,
            version,
            chunk_index: 0,
            total_chunks: 1,
            total_bytes: total_bytes as u32,
            text,
        })
    }

    /// Returns `true` if this message represents a single, self-contained clipboard text.
    pub fn is_single(&self) -> bool {
        self.total_chunks <= 1
    }

    /// Validates the structural integrity and bounds of this message.
    pub fn is_valid(&self) -> bool {
        if self.total_chunks == 0 || self.chunk_index >= self.total_chunks {
            return false;
        }
        if self.total_bytes as usize > MAX_CLIPBOARD_TEXT_SIZE {
            return false;
        }
        if self.is_single() && self.text.len() != self.total_bytes as usize {
            return false;
        }
        if self.text.len() > self.total_bytes as usize {
            return false;
        }
        true
    }
}

/// Splits a UTF-8 string into a sequence of `ClipboardMessage` chunks, each <= `max_chunk_size` bytes.
///
/// Guaranteed never to split across multi-byte UTF-8 character boundaries.
pub fn chunk_clipboard_text(
    origin_id: u64,
    version: u64,
    text: &str,
    max_chunk_size: usize,
) -> Result<Vec<ClipboardMessage>, ProtocolError> {
    let total_bytes = text.len();
    if total_bytes > MAX_CLIPBOARD_TEXT_SIZE {
        return Err(ProtocolError::ClipboardTooLarge {
            size: total_bytes,
            limit: MAX_CLIPBOARD_TEXT_SIZE,
        });
    }

    if text.is_empty() {
        return Ok(vec![ClipboardMessage {
            origin_id,
            version,
            chunk_index: 0,
            total_chunks: 1,
            total_bytes: 0,
            text: String::new(),
        }]);
    }

    let chunk_limit = if max_chunk_size == 0 {
        DEFAULT_CLIPBOARD_CHUNK_SIZE
    } else {
        max_chunk_size
    };

    let mut slices = Vec::new();
    let mut start = 0;

    while start < total_bytes {
        let mut end = (start + chunk_limit).min(total_bytes);
        // Step backwards to the nearest valid UTF-8 character boundary
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        // If a single character is wider than chunk_limit, step forward to include it
        if end == start {
            end = (start + 1).min(total_bytes);
            while end < total_bytes && !text.is_char_boundary(end) {
                end += 1;
            }
        }
        slices.push(&text[start..end]);
        start = end;
    }

    let total_chunks = slices.len() as u32;
    let messages = slices
        .into_iter()
        .enumerate()
        .map(|(idx, s)| ClipboardMessage {
            origin_id,
            version,
            chunk_index: idx as u32,
            total_chunks,
            total_bytes: total_bytes as u32,
            text: s.to_string(),
        })
        .collect();

    Ok(messages)
}

/// Reassembled clipboard data produced by `ClipboardReassembler`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledClipboard {
    pub origin_id: u64,
    pub version: u64,
    pub text: String,
}

#[derive(Debug)]
struct ReassemblySlot {
    total_chunks: u32,
    total_bytes: u32,
    received_count: u32,
    chunks: Vec<Option<String>>,
}

impl ReassemblySlot {
    fn new(total_chunks: u32, total_bytes: u32) -> Self {
        let count = total_chunks as usize;
        let mut chunks = Vec::with_capacity(count);
        chunks.resize_with(count, || None);
        Self {
            total_chunks,
            total_bytes,
            received_count: 0,
            chunks,
        }
    }

    fn insert(&mut self, chunk_index: u32, text: String) -> Result<bool, ProtocolError> {
        let idx = chunk_index as usize;
        if idx >= self.chunks.len() {
            return Err(ProtocolError::InvalidClipboardChunk {
                index: chunk_index,
                total: self.total_chunks,
            });
        }

        if self.chunks[idx].is_none() {
            self.chunks[idx] = Some(text);
            self.received_count += 1;
        }

        Ok(self.received_count == self.total_chunks)
    }

    fn assemble(self) -> Result<String, ProtocolError> {
        let mut full_text = String::with_capacity(self.total_bytes as usize);
        for chunk in self.chunks {
            match chunk {
                Some(part) => full_text.push_str(&part),
                None => {
                    return Err(ProtocolError::ClipboardReassemblyMismatch {
                        expected: self.total_bytes as usize,
                        actual: full_text.len(),
                    });
                }
            }
        }
        if full_text.len() != self.total_bytes as usize {
            return Err(ProtocolError::ClipboardReassemblyMismatch {
                expected: self.total_bytes as usize,
                actual: full_text.len(),
            });
        }
        Ok(full_text)
    }
}

/// Reassembly status of an ingested clipboard chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReassemblyResult {
    /// Full clipboard text has been successfully reassembled.
    Complete(AssembledClipboard),
    /// Chunk accepted; waiting for remaining chunks to arrive.
    Pending,
    /// Chunk was discarded because a newer version has already been processed for this origin.
    Stale,
}

impl ReassemblyResult {
    /// Returns `true` if reassembly is complete.
    pub fn is_complete(&self) -> bool {
        matches!(self, ReassemblyResult::Complete(_))
    }

    /// Returns `true` if chunk was accepted and more chunks are expected.
    pub fn is_pending(&self) -> bool {
        matches!(self, ReassemblyResult::Pending)
    }

    /// Returns `true` if chunk was discarded as stale.
    pub fn is_stale(&self) -> bool {
        matches!(self, ReassemblyResult::Stale)
    }

    /// Converts this result into `Some(AssembledClipboard)` if complete.
    pub fn into_assembled(self) -> Option<AssembledClipboard> {
        match self {
            ReassemblyResult::Complete(assembled) => Some(assembled),
            _ => None,
        }
    }
}

/// Ingests and reassembles multi-chunk clipboard messages.
#[derive(Debug)]
pub struct ClipboardReassembler {
    active_slots: HashMap<(u64, u64), ReassemblySlot>,
    highest_versions: HashMap<u64, u64>,
    max_active_slots: usize,
}

impl Default for ClipboardReassembler {
    fn default() -> Self {
        Self::new(32)
    }
}

impl ClipboardReassembler {
    pub fn new(max_active_slots: usize) -> Self {
        Self {
            active_slots: HashMap::new(),
            highest_versions: HashMap::new(),
            max_active_slots,
        }
    }

    /// Feeds a chunk into the reassembler.
    pub fn ingest_chunk(
        &mut self,
        chunk: ClipboardMessage,
    ) -> Result<ReassemblyResult, ProtocolError> {
        if !chunk.is_valid() {
            return Err(ProtocolError::InvalidClipboardChunk {
                index: chunk.chunk_index,
                total: chunk.total_chunks,
            });
        }

        let origin_id = chunk.origin_id;
        let version = chunk.version;

        // Discard chunks from superseded versions for this origin
        if let Some(&highest) = self.highest_versions.get(&origin_id) {
            if version < highest {
                return Ok(ReassemblyResult::Stale);
            }
        }

        // Fast path for single-chunk messages
        if chunk.is_single() {
            self.highest_versions.insert(origin_id, version);
            self.prune_stale_versions(origin_id, version);
            return Ok(ReassemblyResult::Complete(AssembledClipboard {
                origin_id,
                version,
                text: chunk.text,
            }));
        }

        // Enforce maximum in-flight active reassembly slots
        if !self.active_slots.contains_key(&(origin_id, version))
            && self.active_slots.len() >= self.max_active_slots
        {
            if let Some(&oldest_key) = self.active_slots.keys().next() {
                self.active_slots.remove(&oldest_key);
            }
        }

        let slot = self
            .active_slots
            .entry((origin_id, version))
            .or_insert_with(|| ReassemblySlot::new(chunk.total_chunks, chunk.total_bytes));

        let complete = slot.insert(chunk.chunk_index, chunk.text)?;
        if complete {
            if let Some(slot) = self.active_slots.remove(&(origin_id, version)) {
                let full_text = slot.assemble()?;
                self.highest_versions.insert(origin_id, version);
                self.prune_stale_versions(origin_id, version);
                return Ok(ReassemblyResult::Complete(AssembledClipboard {
                    origin_id,
                    version,
                    text: full_text,
                }));
            }
        }

        Ok(ReassemblyResult::Pending)
    }

    fn prune_stale_versions(&mut self, origin_id: u64, latest_version: u64) {
        self.active_slots
            .retain(|&(orig, ver), _| orig != origin_id || ver >= latest_version);
    }
}

/// Action to be taken by the caller after processing an incoming clipboard message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncAction {
    /// Apply this reassembled text to the local clipboard backend.
    ApplyToLocal(String),
    /// Chunk received; waiting for remaining chunks to finish reassembly.
    Reassembling,
    /// Message was dropped because it originated from our own node (echo loop suppressed).
    EchoSuppressed,
    /// Message was dropped because an equal or newer version already won (conflict lost).
    ConflictLost,
    /// Message was dropped because it was stale or redundant.
    Ignored,
}

/// Shared clipboard synchronization engine.
///
/// Implements deduplication, echo loop suppression, and conflict resolution by last-writer version.
#[derive(Debug)]
pub struct ClipboardSyncEngine {
    origin_id: u64,
    local_version: u64,
    active_origin: u64,
    active_version: u64,
    last_synced_text: Option<String>,
    suppressed_echo_text: Option<String>,
    reassembler: ClipboardReassembler,
    chunk_size: usize,
}

impl ClipboardSyncEngine {
    /// Creates a new synchronization engine for the specified local origin ID.
    pub fn new(origin_id: u64) -> Self {
        Self {
            origin_id,
            local_version: 0,
            active_origin: 0,
            active_version: 0,
            last_synced_text: None,
            suppressed_echo_text: None,
            reassembler: ClipboardReassembler::default(),
            chunk_size: DEFAULT_CLIPBOARD_CHUNK_SIZE,
        }
    }

    /// Configures the maximum chunk size for outgoing messages.
    pub fn with_chunk_size(mut self, chunk_size: usize) -> Self {
        self.chunk_size = chunk_size;
        self
    }

    /// Evaluates a local clipboard change.
    ///
    /// - If the text matches `last_synced_text`, it is deduplicated (returns `None`).
    /// - If the text matches `suppressed_echo_text`, it is recognized as the result of a previous
    ///   remote write and suppressed (returns `None`), breaking the echo loop.
    /// - Otherwise, increments the local monotonic version and generates outgoing `ClipboardMessage`s.
    pub fn process_local_change(
        &mut self,
        text: &str,
    ) -> Result<Option<Vec<ClipboardMessage>>, ProtocolError> {
        // Deduplication: no-op if text matches current synchronized state
        if let Some(ref current) = self.last_synced_text {
            if current == text {
                return Ok(None);
            }
        }

        // Echo suppression: local backend saw the text written by incoming remote update
        if let Some(ref suppressed) = self.suppressed_echo_text {
            if suppressed == text {
                self.suppressed_echo_text = None;
                self.last_synced_text = Some(text.to_string());
                return Ok(None);
            }
        }

        if text.len() > MAX_CLIPBOARD_TEXT_SIZE {
            return Err(ProtocolError::ClipboardTooLarge {
                size: text.len(),
                limit: MAX_CLIPBOARD_TEXT_SIZE,
            });
        }

        // Monotonic version increment (advancing past any remote version seen)
        self.local_version = self.local_version.max(self.active_version) + 1;
        self.active_version = self.local_version;
        self.active_origin = self.origin_id;
        self.last_synced_text = Some(text.to_string());
        self.suppressed_echo_text = None;

        let messages =
            chunk_clipboard_text(self.origin_id, self.local_version, text, self.chunk_size)?;
        Ok(Some(messages))
    }

    /// Ingests an incoming `ClipboardMessage` from the network transport.
    ///
    /// - Suppresses echo messages whose `origin_id` matches this node.
    /// - Reassembles chunked messages.
    /// - Resolves conflicts using last-writer version: higher version wins; ties broken by higher origin ID.
    pub fn process_incoming_message(
        &mut self,
        msg: ClipboardMessage,
    ) -> Result<SyncAction, ProtocolError> {
        // Echo suppression: drop packets originating from our own node
        if msg.origin_id == self.origin_id {
            return Ok(SyncAction::EchoSuppressed);
        }

        match self.reassembler.ingest_chunk(msg)? {
            ReassemblyResult::Pending => Ok(SyncAction::Reassembling),
            ReassemblyResult::Stale => Ok(SyncAction::ConflictLost),
            ReassemblyResult::Complete(assembled) => {
                if self.is_newer(
                    assembled.version,
                    assembled.origin_id,
                    self.active_version,
                    self.active_origin,
                ) {
                    // Remote wins conflict resolution
                    self.active_version = assembled.version;
                    self.active_origin = assembled.origin_id;
                    self.local_version = self.local_version.max(assembled.version);
                    self.last_synced_text = Some(assembled.text.clone());
                    self.suppressed_echo_text = Some(assembled.text.clone());
                    Ok(SyncAction::ApplyToLocal(assembled.text))
                } else {
                    Ok(SyncAction::ConflictLost)
                }
            }
        }
    }

    /// Sets explicit echo suppression for `text` (useful when a backend writes externally).
    pub fn mark_suppression(&mut self, text: &str) {
        self.suppressed_echo_text = Some(text.to_string());
        self.last_synced_text = Some(text.to_string());
    }

    /// Current synchronized text, if any.
    pub fn current_text(&self) -> Option<&str> {
        self.last_synced_text.as_deref()
    }

    /// Version of the currently active clipboard text.
    pub fn active_version(&self) -> u64 {
        self.active_version
    }

    /// Origin of the currently active clipboard text.
    pub fn active_origin(&self) -> u64 {
        self.active_origin
    }

    /// Local monotonic version counter.
    pub fn local_version(&self) -> u64 {
        self.local_version
    }

    /// Local node's origin ID.
    pub fn origin_id(&self) -> u64 {
        self.origin_id
    }

    /// Resets all sync engine state to clean baseline.
    pub fn reset(&mut self) {
        self.local_version = 0;
        self.active_origin = 0;
        self.active_version = 0;
        self.last_synced_text = None;
        self.suppressed_echo_text = None;
        self.reassembler = ClipboardReassembler::default();
    }

    /// Determines if `(v1, o1)` is strictly newer than `(v2, o2)`.
    ///
    /// Rules:
    /// 1. Higher version wins.
    /// 2. If versions are identical, higher origin ID wins as deterministic tie-breaker.
    fn is_newer(&self, v1: u64, o1: u64, v2: u64, o2: u64) -> bool {
        if v1 > v2 {
            true
        } else if v1 == v2 {
            o1 > o2
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_clipboard_message() {
        let msg = ClipboardMessage::single(1, 42, "Hello Quest 3".to_string()).unwrap();
        assert!(msg.is_single());
        assert!(msg.is_valid());
        assert_eq!(msg.origin_id, 1);
        assert_eq!(msg.version, 42);
        assert_eq!(msg.text, "Hello Quest 3");
        assert_eq!(msg.total_bytes, 13);
    }

    #[test]
    fn test_chunk_small_text() {
        let text = "Simple text";
        let chunks = chunk_clipboard_text(1, 10, text, 1024).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_index, 0);
        assert_eq!(chunks[0].total_chunks, 1);
        assert_eq!(chunks[0].text, text);
    }

    #[test]
    fn test_chunk_large_text_and_reassemble() {
        let text = "A".repeat(150_000);
        let chunk_size = 32 * 1024;
        let chunks = chunk_clipboard_text(1, 5, &text, chunk_size).unwrap();
        assert!(chunks.len() > 1);

        let mut reassembler = ClipboardReassembler::default();
        let mut final_result = None;

        for chunk in chunks {
            if let Some(assembled) = reassembler.ingest_chunk(chunk).unwrap().into_assembled() {
                final_result = Some(assembled);
            }
        }

        let assembled = final_result.expect("Should assemble complete text");
        assert_eq!(assembled.origin_id, 1);
        assert_eq!(assembled.version, 5);
        assert_eq!(assembled.text, text);
    }

    #[test]
    fn test_chunk_utf8_multibyte_boundary_safety() {
        // Emoji 🦀 is 4 bytes: [0xF0, 0x9F, 0xA6, 0x80]
        // Repeat emoji 20 times: 80 bytes.
        let crab = "🦀";
        let text = crab.repeat(20);
        // Force chunk size to 7 bytes (which falls inside a 4-byte crab boundary)
        let chunk_size = 7;
        let chunks = chunk_clipboard_text(1, 1, &text, chunk_size).unwrap();

        for chunk in &chunks {
            // Verify that every single chunk is valid UTF-8 and valid message
            assert!(chunk.is_valid());
            // Make sure no replacement characters or invalid slices
            assert!(!chunk.text.contains('\u{FFFD}'));
        }

        let mut reassembler = ClipboardReassembler::default();
        let mut assembled_opt = None;
        for chunk in chunks {
            if let Some(assembled) = reassembler.ingest_chunk(chunk).unwrap().into_assembled() {
                assembled_opt = Some(assembled);
            }
        }

        let assembled = assembled_opt.expect("Must reassemble successfully");
        assert_eq!(assembled.text, text);
    }

    #[test]
    fn test_oversized_text_rejection() {
        let oversized = "X".repeat(MAX_CLIPBOARD_TEXT_SIZE + 1);
        let err = chunk_clipboard_text(1, 1, &oversized, 64 * 1024).unwrap_err();
        match err {
            ProtocolError::ClipboardTooLarge { size, limit } => {
                assert_eq!(size, MAX_CLIPBOARD_TEXT_SIZE + 1);
                assert_eq!(limit, MAX_CLIPBOARD_TEXT_SIZE);
            }
            other => panic!("Unexpected error: {:?}", other),
        }
    }

    #[test]
    fn test_reassembler_out_of_order_chunks() {
        let text = "FirstPart-SecondPart-ThirdPart";
        let chunks = chunk_clipboard_text(2, 100, text, 10).unwrap();
        assert_eq!(chunks.len(), 3);

        let mut reassembler = ClipboardReassembler::default();
        // Ingest chunks in order: 2, 0, 1
        assert!(reassembler
            .ingest_chunk(chunks[2].clone())
            .unwrap()
            .is_pending());
        assert!(reassembler
            .ingest_chunk(chunks[0].clone())
            .unwrap()
            .is_pending());
        let res = reassembler
            .ingest_chunk(chunks[1].clone())
            .unwrap()
            .into_assembled()
            .expect("Should assemble on last chunk");

        assert_eq!(res.text, text);
        assert_eq!(res.version, 100);
        assert_eq!(res.origin_id, 2);
    }

    #[test]
    fn test_reassembler_duplicate_chunk_does_not_corrupt() {
        let text = "Part1-Part2";
        let chunks = chunk_clipboard_text(1, 1, text, 6).unwrap();
        assert_eq!(chunks.len(), 2);

        let mut reassembler = ClipboardReassembler::default();
        assert!(reassembler
            .ingest_chunk(chunks[0].clone())
            .unwrap()
            .is_pending());
        // Duplicate chunk 0
        assert!(reassembler
            .ingest_chunk(chunks[0].clone())
            .unwrap()
            .is_pending());
        // Now chunk 1
        let res = reassembler
            .ingest_chunk(chunks[1].clone())
            .unwrap()
            .into_assembled()
            .expect("Assembled");
        assert_eq!(res.text, text);
    }

    #[test]
    fn test_reassembler_stale_version_discard() {
        let mut reassembler = ClipboardReassembler::default();
        let msg_v2 = ClipboardMessage::single(1, 2, "v2-text".to_string()).unwrap();
        let msg_v1 = ClipboardMessage::single(1, 1, "v1-text".to_string()).unwrap();

        assert!(reassembler.ingest_chunk(msg_v2).unwrap().is_complete());
        // Now ingest older v1 chunk from same origin
        assert!(reassembler.ingest_chunk(msg_v1).unwrap().is_stale());
    }

    #[test]
    fn test_sync_engine_deduplication() {
        let mut engine = ClipboardSyncEngine::new(1);

        let msgs1 = engine.process_local_change("Hello").unwrap();
        assert!(msgs1.is_some());
        assert_eq!(engine.current_text(), Some("Hello"));

        // Same text copied again: deduplicated!
        let msgs2 = engine.process_local_change("Hello").unwrap();
        assert!(msgs2.is_none());
    }

    #[test]
    fn test_sync_engine_echo_suppression() {
        let mut host_engine = ClipboardSyncEngine::new(1);
        let mut client_engine = ClipboardSyncEngine::new(2);

        // Host copies "CopiedOnHost"
        let host_msgs = host_engine
            .process_local_change("CopiedOnHost")
            .unwrap()
            .unwrap();
        assert_eq!(host_msgs.len(), 1);

        // Client receives host message
        let action = client_engine
            .process_incoming_message(host_msgs[0].clone())
            .unwrap();
        assert_eq!(action, SyncAction::ApplyToLocal("CopiedOnHost".to_string()));

        // Client backend writes "CopiedOnHost", and watcher then polls "CopiedOnHost" locally
        let client_local = client_engine.process_local_change("CopiedOnHost").unwrap();
        // MUST BE SUPPRESSED to prevent echo loop!
        assert!(
            client_local.is_none(),
            "Echo loop suppression failed: client re-emitted remote text!"
        );
    }

    #[test]
    fn test_sync_engine_self_echo_suppression() {
        let mut engine = ClipboardSyncEngine::new(1);
        let msgs = engine.process_local_change("SelfEcho").unwrap().unwrap();

        // Suppose message is reflected back to the same node
        let action = engine.process_incoming_message(msgs[0].clone()).unwrap();
        assert_eq!(action, SyncAction::EchoSuppressed);
    }

    #[test]
    fn test_sync_engine_conflict_resolution_higher_version_wins() {
        let mut engine = ClipboardSyncEngine::new(1);
        // Local state at version 1
        let _ = engine.process_local_change("LocalText").unwrap();
        assert_eq!(engine.active_version(), 1);

        // Remote sends version 2 from origin 2
        let remote_msg = ClipboardMessage::single(2, 2, "RemoteNewerText".to_string()).unwrap();
        let action = engine.process_incoming_message(remote_msg).unwrap();
        assert_eq!(
            action,
            SyncAction::ApplyToLocal("RemoteNewerText".to_string())
        );
        assert_eq!(engine.current_text(), Some("RemoteNewerText"));

        // Remote sends stale version 1 from origin 2
        let stale_msg = ClipboardMessage::single(2, 1, "RemoteStaleText".to_string()).unwrap();
        let action2 = engine.process_incoming_message(stale_msg).unwrap();
        assert_eq!(action2, SyncAction::ConflictLost);
        assert_eq!(engine.current_text(), Some("RemoteNewerText"));
    }

    #[test]
    fn test_sync_engine_conflict_resolution_tie_breaker() {
        let mut host_engine = ClipboardSyncEngine::new(1);
        let mut client_engine = ClipboardSyncEngine::new(2);

        // Both generate a copy at the exact same version 1
        let host_msgs = host_engine
            .process_local_change("HostTie")
            .unwrap()
            .unwrap();
        let client_msgs = client_engine
            .process_local_change("ClientTie")
            .unwrap()
            .unwrap();

        // Host receives client message: origin 2 > origin 1, client wins
        let host_action = host_engine
            .process_incoming_message(client_msgs[0].clone())
            .unwrap();
        assert_eq!(
            host_action,
            SyncAction::ApplyToLocal("ClientTie".to_string())
        );

        // Client receives host message: origin 1 < origin 2, host loses
        let client_action = client_engine
            .process_incoming_message(host_msgs[0].clone())
            .unwrap();
        assert_eq!(client_action, SyncAction::ConflictLost);

        // Both nodes converge to "ClientTie"!
        assert_eq!(host_engine.current_text(), Some("ClientTie"));
        assert_eq!(client_engine.current_text(), Some("ClientTie"));
    }
}

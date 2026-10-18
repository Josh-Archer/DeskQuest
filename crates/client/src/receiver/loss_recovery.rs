//! Keeps the decoder on a valid reference chain after video loss.
//!
//! A frame that FEC could not repair never leaves the reassembler, so the decoder would get the
//! frames after it, which reference the missing one, and show corruption until the next intra
//! refresh repaints the picture. [`LossRecovery`] notices the gap (or a frame the decoder
//! refused), holds back non-key frames, and asks for a keyframe until one arrives.

use std::time::{Duration, Instant};

/// How often a keyframe request is repeated while waiting for one. The host rate limits
/// client requests to 250 ms, so this only covers a lost request datagram.
pub const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(200);

/// What to do with a reassembled frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    /// Queue it into the decoder.
    Decode,
    /// Drop it: the decoder has no valid reference for it.
    Skip,
}

/// Loss state between the reassembler and the decoder.
#[derive(Debug, Clone)]
pub struct LossRecovery {
    last_frame_id: Option<u64>,
    need_keyframe: bool,
    last_request: Option<Instant>,
    frames_skipped: u64,
    requests: u64,
}

impl Default for LossRecovery {
    fn default() -> Self {
        Self::new()
    }
}

impl LossRecovery {
    /// Starts out waiting for a keyframe: a decoder cannot start from a P-frame.
    pub fn new() -> Self {
        Self {
            last_frame_id: None,
            need_keyframe: true,
            last_request: None,
            frames_skipped: 0,
            requests: 0,
        }
    }

    /// Decides whether a complete frame goes to the decoder. A frame id that is not the
    /// successor of the previous one (a frame was lost, or the host restarted its stream)
    /// starts a resync.
    pub fn admit(&mut self, frame_id: u64, is_keyframe: bool) -> Admit {
        if let Some(last) = self.last_frame_id {
            if frame_id != last.wrapping_add(1) && !self.need_keyframe {
                tracing::debug!(last, frame_id, "video frame gap; waiting for a keyframe");
                self.need_keyframe = true;
            }
        }
        self.last_frame_id = Some(frame_id);
        if is_keyframe {
            if self.need_keyframe {
                tracing::info!(
                    frame_id,
                    skipped = self.frames_skipped,
                    "video resynced on a keyframe"
                );
            }
            self.need_keyframe = false;
            return Admit::Decode;
        }
        if self.need_keyframe {
            self.frames_skipped += 1;
            Admit::Skip
        } else {
            Admit::Decode
        }
    }

    /// The decoder refused a frame, so its reference chain is broken.
    pub fn decoder_refused(&mut self) {
        self.need_keyframe = true;
    }

    /// True when a keyframe request should be sent now (waiting for one, and the last request
    /// is at least [`KEYFRAME_REQUEST_INTERVAL`] old). Records the request.
    pub fn take_request(&mut self, now: Instant) -> bool {
        if !self.need_keyframe {
            return false;
        }
        if self
            .last_request
            .is_some_and(|last| now.saturating_duration_since(last) < KEYFRAME_REQUEST_INTERVAL)
        {
            return false;
        }
        self.last_request = Some(now);
        self.requests += 1;
        true
    }

    /// Whether non-key frames are currently being held back.
    pub fn waiting_for_keyframe(&self) -> bool {
        self.need_keyframe
    }

    /// Frames dropped while waiting for a keyframe.
    pub fn frames_skipped(&self) -> u64 {
        self.frames_skipped
    }

    /// Keyframe requests issued.
    pub fn requests(&self) -> u64 {
        self.requests
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_waiting_and_skips_until_the_first_keyframe() {
        let mut r = LossRecovery::new();
        assert!(r.waiting_for_keyframe());
        assert_eq!(r.admit(5, false), Admit::Skip);
        assert_eq!(r.admit(6, true), Admit::Decode);
        assert_eq!(r.admit(7, false), Admit::Decode);
        assert_eq!(r.frames_skipped(), 1);
    }

    #[test]
    fn a_gap_skips_p_frames_until_the_next_keyframe() {
        let mut r = LossRecovery::new();
        r.admit(1, true);
        assert_eq!(r.admit(2, false), Admit::Decode);
        // Frame 3 was lost.
        assert_eq!(r.admit(4, false), Admit::Skip);
        assert_eq!(r.admit(5, false), Admit::Skip);
        assert_eq!(r.admit(6, true), Admit::Decode);
        assert_eq!(r.admit(7, false), Admit::Decode);
    }

    #[test]
    fn a_host_restart_with_lower_ids_resyncs() {
        let mut r = LossRecovery::new();
        r.admit(9000, true);
        r.admit(9001, false);
        assert_eq!(r.admit(0, false), Admit::Skip);
        assert_eq!(r.admit(1, true), Admit::Decode);
    }

    #[test]
    fn decoder_refusal_waits_for_a_keyframe() {
        let mut r = LossRecovery::new();
        r.admit(1, true);
        r.decoder_refused();
        assert_eq!(r.admit(2, false), Admit::Skip);
        assert_eq!(r.admit(3, true), Admit::Decode);
    }

    #[test]
    fn requests_are_rate_limited_and_stop_once_resynced() {
        let mut r = LossRecovery::new();
        let t0 = Instant::now();
        assert!(r.take_request(t0));
        assert!(!r.take_request(t0 + Duration::from_millis(50)));
        assert!(r.take_request(t0 + KEYFRAME_REQUEST_INTERVAL));
        r.admit(1, true);
        assert!(!r.take_request(t0 + Duration::from_secs(5)));
        assert_eq!(r.requests(), 2);
    }
}

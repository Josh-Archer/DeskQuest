//! Per-stream keyframe requests, from the input receiver to the video senders.
//!
//! The client asks for an IDR on the input channel ([`InputEvent::KeyframeRequest`] means stream
//! 0, [`InputEvent::KeyframeRequestFor`] names any stream). With several video streams, a
//! request must reach only the stream that lost video, so each stream has its own flag: one bit
//! of a shared word, indexed by the stream id.
//!
//! [`InputEvent::KeyframeRequest`]: linux_quest_protocol::InputEvent::KeyframeRequest
//! [`InputEvent::KeyframeRequestFor`]: linux_quest_protocol::InputEvent::KeyframeRequestFor

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// One "keyframe wanted" flag per video stream. Shared (`Arc`) between the input receiver, which
/// sets flags, and each stream's send loop, which takes its own.
#[derive(Debug, Default)]
pub struct KeyframeRequests {
    bits: AtomicU32,
}

impl KeyframeRequests {
    /// Streams with an id at or above this have no flag; requests for them are ignored. The
    /// daemon sends at most three streams.
    pub const MAX_STREAMS: u16 = 32;

    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Asks for a keyframe on `stream_id`. Returns `false` (and does nothing) for a stream id
    /// beyond [`Self::MAX_STREAMS`].
    pub fn request(&self, stream_id: u16) -> bool {
        match Self::mask(stream_id) {
            Some(mask) => {
                self.bits.fetch_or(mask, Ordering::AcqRel);
                true
            }
            None => false,
        }
    }

    /// Whether a keyframe is wanted on `stream_id`, without consuming the request.
    pub fn is_requested(&self, stream_id: u16) -> bool {
        Self::mask(stream_id).is_some_and(|mask| self.bits.load(Ordering::Acquire) & mask != 0)
    }

    /// Consumes the request for `stream_id`: true if one was pending. Other streams' requests
    /// are left alone.
    pub fn take(&self, stream_id: u16) -> bool {
        Self::mask(stream_id)
            .is_some_and(|mask| self.bits.fetch_and(!mask, Ordering::AcqRel) & mask != 0)
    }

    fn mask(stream_id: u16) -> Option<u32> {
        (stream_id < Self::MAX_STREAMS).then(|| 1u32 << stream_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_per_stream_and_consumed_once() {
        let flags = KeyframeRequests::new();
        assert!(!flags.take(0));
        assert!(!flags.take(1));

        assert!(flags.request(1));
        assert!(flags.is_requested(1));
        assert!(!flags.is_requested(0));
        assert!(!flags.take(0), "stream 0 did not ask");
        assert!(flags.take(1));
        assert!(!flags.take(1), "consumed");

        // Several streams can ask at once; each takes only its own.
        flags.request(0);
        flags.request(2);
        assert!(flags.take(2));
        assert!(flags.is_requested(0));
        assert!(flags.take(0));
        assert!(!flags.is_requested(0) && !flags.is_requested(2));
    }

    #[test]
    fn out_of_range_streams_are_ignored() {
        let flags = KeyframeRequests::new();
        assert!(!flags.request(KeyframeRequests::MAX_STREAMS));
        assert!(!flags.request(u16::MAX));
        assert!(!flags.is_requested(KeyframeRequests::MAX_STREAMS));
        assert!(!flags.take(KeyframeRequests::MAX_STREAMS));
        for id in 0..KeyframeRequests::MAX_STREAMS {
            assert!(!flags.take(id), "an ignored request set nothing ({id})");
        }
    }
}

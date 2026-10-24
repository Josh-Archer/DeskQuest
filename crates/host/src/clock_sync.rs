//! Clock sync and frame timing for the production daemon path (#102).
//!
//! `HostStreamSession` answers clock-sync pings itself. The daemon splits the same job: the
//! client's `LQCK` ping arrives on the input channel (UDP or the USB tunnel), but the reply has
//! to reach the client on the video path, where it already listens for `Pong` and
//! `TelemetryReport`. [`ClockSyncRelay`] carries the request from the input receiver to the
//! stream 0 video sender. The host receive time `t2` is taken when the ping arrives and the send
//! time `t3` right before the `Pong` leaves, so time spent waiting in the relay is not counted
//! as network delay.
//!
//! The first valid request also opts the client in to per-frame `LQFT` timing reports, so old
//! clients never get them.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use bytes::Bytes;
use linux_quest_protocol::timing::{decode_clock_sync_request, encode_clock_sync_response};

use crate::host_now_us;

/// Requests older than this many are dropped; the client pings every 500 ms, so a backlog only
/// builds up while no video is flowing.
const MAX_PENDING: usize = 8;

/// Pending clock-sync requests `(t1, t2)` and the frame-timing opt-in.
#[derive(Debug, Default)]
pub struct ClockSyncRelay {
    pending: Mutex<VecDeque<(u64, u64)>>,
    opted_in: AtomicBool,
}

impl ClockSyncRelay {
    /// Records a `Ping` payload received now. Returns `false` for ordinary pings.
    pub fn on_ping(&self, payload: &[u8]) -> bool {
        self.on_ping_at(payload, host_now_us())
    }

    /// Like [`Self::on_ping`] with an explicit host receive time.
    pub fn on_ping_at(&self, payload: &[u8], t2_us: u64) -> bool {
        let Some(t1) = decode_clock_sync_request(payload) else {
            return false;
        };
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if pending.len() == MAX_PENDING {
            pending.pop_front();
        }
        pending.push_back((t1, t2_us));
        self.opted_in.store(true, Ordering::Release);
        true
    }

    /// Whether a client has asked for clock sync, i.e. understands frame-timing reports.
    pub fn opted_in(&self) -> bool {
        self.opted_in.load(Ordering::Acquire)
    }

    /// Takes every pending request and returns the `Pong` payloads, stamped with `t3` = now.
    pub fn take_responses(&self) -> Vec<Bytes> {
        let drained: Vec<_> = {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.drain(..).collect()
        };
        drained
            .into_iter()
            .map(|(t1, t2)| {
                let t3 = host_now_us().max(t2);
                Bytes::from(encode_clock_sync_response(t1, t2, t3))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_quest_protocol::timing::{decode_clock_sync_response, encode_clock_sync_request};

    #[test]
    fn ordinary_ping_is_ignored_and_does_not_opt_in() {
        let relay = ClockSyncRelay::default();
        assert!(!relay.on_ping(b"hello"));
        assert!(!relay.opted_in());
        assert!(relay.take_responses().is_empty());
    }

    #[test]
    fn clock_sync_ping_yields_one_pong_with_host_times_and_opts_in() {
        let relay = ClockSyncRelay::default();
        assert!(relay.on_ping_at(&encode_clock_sync_request(42), 1_000));
        assert!(relay.opted_in());
        let pongs = relay.take_responses();
        assert_eq!(pongs.len(), 1);
        let (t1, t2, t3) = decode_clock_sync_response(&pongs[0]).unwrap();
        assert_eq!((t1, t2), (42, 1_000));
        assert!(t3 >= t2);
        assert!(relay.take_responses().is_empty());
    }

    #[test]
    fn backlog_is_bounded() {
        let relay = ClockSyncRelay::default();
        for t1 in 0..20 {
            relay.on_ping_at(&encode_clock_sync_request(t1), 5);
        }
        let pongs = relay.take_responses();
        assert_eq!(pongs.len(), MAX_PENDING);
        let first = decode_clock_sync_response(&pongs[0]).unwrap().0;
        assert_eq!(first, 20 - MAX_PENDING as u64);
    }
}

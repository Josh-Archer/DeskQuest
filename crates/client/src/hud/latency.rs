//! Client half of measured latency: clock calibration against the host and per-frame stage
//! timestamps. See `linux_quest_protocol::timing` for the wire format and math.

use std::collections::{BTreeMap, HashMap};

use linux_quest_protocol::timing::{decode_clock_sync_response, encode_clock_sync_request};
use linux_quest_protocol::{ClientFrameTiming, ClockSync, HostFrameTiming, MeasuredFrameLatency};

/// Frames tracked at once; older entries are dropped.
const MAX_TRACKED: usize = 64;
/// A measurement older than this (client µs) is stale and the HUD falls back to estimates.
pub const MEASURED_STALE_US: u64 = 2_000_000;

/// Client wall clock in microseconds since the Unix epoch.
pub fn client_now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

#[derive(Default)]
struct Entry {
    host: Option<HostFrameTiming>,
    client: ClientFrameTiming,
}

/// Collects host/client stage timestamps per frame and produces measured latencies.
#[derive(Default)]
pub struct LatencyTracker {
    clock: ClockSync,
    frames: BTreeMap<u64, Entry>,
    pts_to_frame: HashMap<u64, u64>,
    latest: Option<(MeasuredFrameLatency, u64)>,
}

impl LatencyTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Payload for a clock-sync `Ping` sent at client time `now_us`.
    pub fn clock_sync_request(&self, now_us: u64) -> Vec<u8> {
        encode_clock_sync_request(now_us)
    }

    /// Handles a `Pong` payload received at `now_us`. Returns true if it calibrated the clock.
    pub fn on_pong(&mut self, payload: &[u8], now_us: u64) -> bool {
        match decode_clock_sync_response(payload) {
            Some((t1, t2, t3)) => self.clock.add(t1, t2, t3, now_us).is_some(),
            None => false,
        }
    }

    pub fn clock(&self) -> &ClockSync {
        &self.clock
    }

    fn entry(&mut self, frame_id: u64) -> &mut Entry {
        while self.frames.len() >= MAX_TRACKED && !self.frames.contains_key(&frame_id) {
            if let Some((&old, _)) = self.frames.iter().next() {
                self.frames.remove(&old);
                self.pts_to_frame.retain(|_, f| *f != old);
            }
        }
        self.frames.entry(frame_id).or_default()
    }

    /// Host stage timestamps for a frame (from a frame-timing telemetry report).
    pub fn on_host_timing(&mut self, timing: HostFrameTiming) {
        self.entry(timing.frame_id).host = Some(timing);
        self.try_finish(timing.frame_id);
    }

    /// Last chunk of `frame_id` arrived.
    pub fn on_receive(&mut self, frame_id: u64, now_us: u64) {
        self.entry(frame_id).client.receive_us = now_us;
    }

    /// `frame_id` (carrying `pts_us`) was queued to the decoder.
    pub fn on_decode_start(&mut self, frame_id: u64, pts_us: u64, now_us: u64) {
        self.entry(frame_id).client.decode_start_us = now_us;
        self.pts_to_frame.insert(pts_us, frame_id);
    }

    /// A decoded buffer with `pts_us` came out of the decoder.
    pub fn on_decode_end(&mut self, pts_us: u64, now_us: u64) {
        if let Some(&id) = self.pts_to_frame.get(&pts_us) {
            if let Some(e) = self.frames.get_mut(&id) {
                e.client.decode_end_us = now_us;
            }
        }
    }

    /// The decoded buffer with `pts_us` was released to the compositor for display.
    pub fn on_present(&mut self, pts_us: u64, now_us: u64) {
        if let Some(id) = self.pts_to_frame.remove(&pts_us) {
            if let Some(e) = self.frames.get_mut(&id) {
                e.client.present_us = now_us;
            }
            self.try_finish(id);
        }
    }

    fn try_finish(&mut self, frame_id: u64) {
        let Some(e) = self.frames.get(&frame_id) else {
            return;
        };
        let (Some(host), true) = (e.host, e.client.present_us != 0) else {
            return;
        };
        let m = MeasuredFrameLatency::combine(&host, &e.client, &self.clock);
        self.latest = Some((m, e.client.present_us));
        // Everything up to this frame is done or superseded.
        self.frames = self.frames.split_off(&(frame_id + 1));
        self.pts_to_frame.retain(|_, f| *f > frame_id);
    }

    /// The newest measured frame, if fresh at `now_us`.
    pub fn latest(&self, now_us: u64) -> Option<MeasuredFrameLatency> {
        self.latest
            .filter(|(_, at)| now_us.saturating_sub(*at) <= MEASURED_STALE_US)
            .map(|(m, _)| m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_quest_protocol::timing::encode_clock_sync_response;

    const OFF: u64 = 1_000_000; // host = client + OFF

    fn host(frame_id: u64) -> HostFrameTiming {
        HostFrameTiming {
            frame_id,
            stream_id: 0,
            capture_us: OFF + 10_000,
            encode_start_us: OFF + 10_500,
            encode_end_us: OFF + 13_500,
            send_us: OFF + 14_000,
        }
    }

    fn run_frame(t: &mut LatencyTracker, id: u64, host_first: bool) {
        if host_first {
            t.on_host_timing(host(id));
        }
        t.on_receive(id, 16_000);
        t.on_decode_start(id, 777, 17_000);
        t.on_decode_end(777, 19_500);
        t.on_present(777, 25_000);
        if !host_first {
            t.on_host_timing(host(id));
        }
    }

    #[test]
    fn timestamps_propagate_to_measured_end_to_end_after_calibration() {
        let mut t = LatencyTracker::new();
        let req = t.clock_sync_request(0);
        assert_eq!(&req[..4], b"LQCK");
        assert!(t.on_pong(&encode_clock_sync_response(0, OFF + 500, OFF + 500), 1_000));
        run_frame(&mut t, 5, true);
        let m = t.latest(25_000).expect("measured");
        assert_eq!(m.frame_id, 5);
        assert_eq!(m.encode_us, Some(3_000));
        assert_eq!(m.network_us, Some(2_000));
        assert_eq!(m.decode_us, Some(2_500));
        assert_eq!(m.present_us, Some(5_500));
        assert_eq!(m.capture_to_present_us, Some(15_000));
    }

    #[test]
    fn host_report_arriving_after_present_still_completes() {
        let mut t = LatencyTracker::new();
        t.on_pong(&encode_clock_sync_response(0, OFF + 500, OFF + 500), 1_000);
        run_frame(&mut t, 9, false);
        assert_eq!(
            t.latest(25_000).unwrap().capture_to_present_us,
            Some(15_000)
        );
    }

    #[test]
    fn without_calibration_cross_clock_stages_are_absent() {
        let mut t = LatencyTracker::new();
        // Old host echoed the request: ignored.
        assert!(!t.on_pong(&t.clock_sync_request(0), 10));
        run_frame(&mut t, 1, true);
        let m = t.latest(25_000).unwrap();
        assert_eq!(m.capture_to_present_us, None);
        assert_eq!(m.network_us, None);
        assert_eq!(m.decode_us, Some(2_500));
    }

    #[test]
    fn stale_measurements_expire_and_tracking_is_bounded() {
        let mut t = LatencyTracker::new();
        run_frame(&mut t, 1, true);
        assert!(t.latest(25_000 + MEASURED_STALE_US + 1).is_none());
        for id in 100..(100 + 3 * MAX_TRACKED as u64) {
            t.on_receive(id, 1);
        }
        assert!(t.frames.len() <= MAX_TRACKED);
    }
}

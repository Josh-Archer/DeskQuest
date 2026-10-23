//! Measured per-frame latency: host/headset clock calibration and stage timestamps.
//!
//! Both extensions ride on existing packet types so older peers are unaffected:
//!
//! * **Clock sync** uses `Ping`/`Pong`. A request payload is `CLOCK_SYNC_MAGIC` + version +
//!   `t1` (client send time). A new host answers with a `Pong` whose payload appends `t2`
//!   (host receive) and `t3` (host send). An old host just echoes the request, which the
//!   client recognizes (too short) and ignores, so the HUD keeps showing estimates.
//! * **Frame timing** uses `TelemetryReport` with a `FRAME_TIMING_MAGIC` prefix. The host only
//!   sends it after it has seen a clock-sync request, i.e. to clients that understand it.
//!
//! All times are microseconds on the sender's own clock (host: wall clock epoch; client:
//! whatever monotonic/epoch clock it uses consistently). Offsets are `host - client`.

use serde::{Deserialize, Serialize};

/// Prefix of a clock-sync `Ping`/`Pong` payload.
pub const CLOCK_SYNC_MAGIC: [u8; 4] = *b"LQCK";
/// Prefix of a frame-timing `TelemetryReport` payload.
pub const FRAME_TIMING_MAGIC: [u8; 4] = *b"LQFT";
/// Version byte following either magic.
pub const TIMING_EXT_VERSION: u8 = 1;

const REQ_LEN: usize = 4 + 1 + 8;
const RESP_LEN: usize = REQ_LEN + 16;

/// Encodes a clock-sync request carrying the client send time `t1_us`.
pub fn encode_clock_sync_request(t1_us: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(REQ_LEN);
    v.extend_from_slice(&CLOCK_SYNC_MAGIC);
    v.push(TIMING_EXT_VERSION);
    v.extend_from_slice(&t1_us.to_be_bytes());
    v
}

/// Parses a clock-sync request; returns `t1_us`. `None` for ordinary pings.
pub fn decode_clock_sync_request(payload: &[u8]) -> Option<u64> {
    if payload.len() < REQ_LEN
        || payload[..4] != CLOCK_SYNC_MAGIC
        || payload[4] != TIMING_EXT_VERSION
    {
        return None;
    }
    Some(u64::from_be_bytes(payload[5..13].try_into().ok()?))
}

/// Encodes the host's reply: the client's `t1`, host receive `t2`, host send `t3`.
pub fn encode_clock_sync_response(t1_us: u64, t2_us: u64, t3_us: u64) -> Vec<u8> {
    let mut v = encode_clock_sync_request(t1_us);
    v.extend_from_slice(&t2_us.to_be_bytes());
    v.extend_from_slice(&t3_us.to_be_bytes());
    v
}

/// Parses a clock-sync reply into `(t1, t2, t3)`. An echoed request from an old host
/// (no host timestamps) yields `None`.
pub fn decode_clock_sync_response(payload: &[u8]) -> Option<(u64, u64, u64)> {
    let t1 = decode_clock_sync_request(payload)?;
    if payload.len() < RESP_LEN {
        return None;
    }
    let t2 = u64::from_be_bytes(payload[13..21].try_into().ok()?);
    let t3 = u64::from_be_bytes(payload[21..29].try_into().ok()?);
    Some((t1, t2, t3))
}

/// One NTP-style exchange: `t1` client send, `t2` host receive, `t3` host send, `t4` client receive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockSample {
    /// Estimated `host - client` clock offset in microseconds.
    pub offset_us: i64,
    /// Round-trip network time, excluding host processing, in microseconds.
    pub rtt_us: u64,
}

impl ClockSample {
    /// NTP math: offset = ((t2 - t1) + (t3 - t4)) / 2, rtt = (t4 - t1) - (t3 - t2).
    /// Returns `None` for inconsistent samples (negative round trip).
    pub fn from_timestamps(t1: u64, t2: u64, t3: u64, t4: u64) -> Option<Self> {
        let (t1, t2, t3, t4) = (t1 as i128, t2 as i128, t3 as i128, t4 as i128);
        let rtt = (t4 - t1) - (t3 - t2);
        if rtt < 0 || t3 < t2 || t4 < t1 {
            return None;
        }
        let offset = ((t2 - t1) + (t3 - t4)) / 2;
        Some(Self {
            offset_us: offset as i64,
            rtt_us: rtt as u64,
        })
    }
}

/// Keeps a window of recent samples and trusts the one with the lowest RTT, whose offset
/// error is bounded by rtt/2 (the usual NTP filter).
#[derive(Debug, Clone)]
pub struct ClockSync {
    samples: std::collections::VecDeque<ClockSample>,
    window: usize,
}

impl Default for ClockSync {
    fn default() -> Self {
        Self::new(16)
    }
}

impl ClockSync {
    pub fn new(window: usize) -> Self {
        Self {
            samples: std::collections::VecDeque::with_capacity(window.max(1)),
            window: window.max(1),
        }
    }

    /// Adds an exchange; returns the computed sample if it was valid.
    pub fn add(&mut self, t1: u64, t2: u64, t3: u64, t4: u64) -> Option<ClockSample> {
        let s = ClockSample::from_timestamps(t1, t2, t3, t4)?;
        if self.samples.len() == self.window {
            self.samples.pop_front();
        }
        self.samples.push_back(s);
        Some(s)
    }

    /// The best (lowest-RTT) sample, if calibrated.
    pub fn best(&self) -> Option<ClockSample> {
        self.samples.iter().min_by_key(|s| s.rtt_us).copied()
    }

    pub fn is_calibrated(&self) -> bool {
        !self.samples.is_empty()
    }

    /// Converts a host timestamp into the client's clock.
    pub fn host_to_client_us(&self, host_us: u64) -> Option<u64> {
        let b = self.best()?;
        Some((host_us as i128 - b.offset_us as i128).max(0) as u64)
    }
}

/// Host-side stage timestamps for one frame (host clock, microseconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HostFrameTiming {
    pub frame_id: u64,
    pub stream_id: u16,
    /// Frame captured (compositor buffer acquired).
    pub capture_us: u64,
    /// Encoder input submitted.
    pub encode_start_us: u64,
    /// Encoded bitstream ready.
    pub encode_end_us: u64,
    /// Last chunk handed to the transport.
    pub send_us: u64,
}

impl HostFrameTiming {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(48);
        v.extend_from_slice(&FRAME_TIMING_MAGIC);
        v.push(TIMING_EXT_VERSION);
        v.extend_from_slice(&bincode::serialize(self).unwrap_or_default());
        v
    }

    /// Parses a frame-timing telemetry payload; `None` for anything else.
    pub fn decode(payload: &[u8]) -> Option<Self> {
        if payload.len() < 5
            || payload[..4] != FRAME_TIMING_MAGIC
            || payload[4] != TIMING_EXT_VERSION
        {
            return None;
        }
        bincode::deserialize(&payload[5..]).ok()
    }
}

/// Client-side stage timestamps for one frame (client clock, microseconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClientFrameTiming {
    /// Last chunk of the frame received from the transport.
    pub receive_us: u64,
    /// Frame queued to the decoder.
    pub decode_start_us: u64,
    /// Decoded frame dequeued.
    pub decode_end_us: u64,
    /// Frame released for presentation (compositor submit).
    pub present_us: u64,
}

/// Per-stage durations for a frame, each either measured or absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MeasuredFrameLatency {
    pub frame_id: u64,
    /// capture → encode start (host queueing), host clock only.
    pub capture_to_encode_us: Option<u64>,
    /// encode start → encode end, host clock only.
    pub encode_us: Option<u64>,
    /// encode end → last chunk sent, host clock only.
    pub send_us: Option<u64>,
    /// host send → client receive; crosses clocks, needs calibration.
    pub network_us: Option<u64>,
    /// receive → decode start (jitter buffer / reassembly), client clock only.
    pub jitter_us: Option<u64>,
    /// decode start → decode end, client clock only.
    pub decode_us: Option<u64>,
    /// decode end → present, client clock only.
    pub present_us: Option<u64>,
    /// capture → present across both clocks; needs calibration. Still excludes panel
    /// scanout, so it is not optical glass-to-glass.
    pub capture_to_present_us: Option<u64>,
    /// Calibration RTT used (bounds the cross-clock error at ±rtt/2).
    pub clock_rtt_us: Option<u64>,
}

fn span(a: u64, b: u64) -> Option<u64> {
    (a != 0 && b != 0 && b >= a).then(|| b - a)
}

impl MeasuredFrameLatency {
    /// Combines host and client timestamps. Cross-clock stages are only filled when `clock`
    /// is calibrated; a zero timestamp means "not recorded".
    pub fn combine(host: &HostFrameTiming, client: &ClientFrameTiming, clock: &ClockSync) -> Self {
        let to_client = |h: u64| {
            if h == 0 {
                None
            } else {
                clock.host_to_client_us(h)
            }
        };
        let send_c = to_client(host.send_us);
        let cap_c = to_client(host.capture_us);
        Self {
            frame_id: host.frame_id,
            capture_to_encode_us: span(host.capture_us, host.encode_start_us),
            encode_us: span(host.encode_start_us, host.encode_end_us),
            send_us: span(host.encode_end_us, host.send_us),
            // Clamp tiny negatives (offset error) to zero rather than dropping the sample.
            network_us: send_c
                .filter(|_| client.receive_us != 0)
                .map(|s| client.receive_us.saturating_sub(s)),
            jitter_us: span(client.receive_us, client.decode_start_us),
            decode_us: span(client.decode_start_us, client.decode_end_us),
            present_us: span(client.decode_end_us, client.present_us),
            capture_to_present_us: cap_c
                .filter(|_| client.present_us != 0)
                .map(|c| client.present_us.saturating_sub(c)),
            clock_rtt_us: clock.best().map(|b| b.rtt_us),
        }
    }

    /// True when the end-to-end (capture → present) figure is measured.
    pub fn is_end_to_end_measured(&self) -> bool {
        self.capture_to_present_us.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ntp_offset_and_rtt_symmetric_path() {
        // Host clock is 1_000_000us ahead; 2ms each way; host holds 500us.
        let off = 1_000_000u64;
        let t1 = 10_000;
        let t2 = t1 + 2_000 + off;
        let t3 = t2 + 500;
        let t4 = t1 + 2_000 + 500 + 2_000;
        let s = ClockSample::from_timestamps(t1, t2, t3, t4).unwrap();
        assert_eq!(s.offset_us, 1_000_000);
        assert_eq!(s.rtt_us, 4_000);
    }

    #[test]
    fn ntp_negative_offset_and_asymmetry_error_bounded() {
        // Host behind client by 3s; 1ms up, 3ms down -> offset error = (1-3)/2 = -1ms.
        let t1 = 5_000_000u64;
        let t2 = t1 + 1_000 - 3_000_000;
        let t3 = t2 + 100;
        let t4 = t1 + 1_000 + 100 + 3_000;
        let s = ClockSample::from_timestamps(t1, t2, t3, t4).unwrap();
        assert_eq!(s.rtt_us, 4_000);
        let err = s.offset_us - (-3_000_000);
        assert_eq!(err, -1_000);
        assert!(err.unsigned_abs() <= s.rtt_us / 2);
    }

    #[test]
    fn inconsistent_sample_rejected() {
        assert!(ClockSample::from_timestamps(100, 50, 40, 200).is_none());
        assert!(ClockSample::from_timestamps(100, 0, 10_000, 200).is_none());
    }

    #[test]
    fn clock_sync_prefers_min_rtt_and_windows() {
        let mut c = ClockSync::new(3);
        assert!(!c.is_calibrated());
        assert_eq!(c.host_to_client_us(5), None);
        c.add(0, 5_100, 5_100, 10_000); // rtt 10ms, offset 100
        c.add(0, 1_100, 1_100, 2_000); // rtt 2ms, offset 100
        c.add(0, 3_100, 3_100, 6_000); // rtt 6ms
        assert_eq!(c.best().unwrap().rtt_us, 2_000);
        assert_eq!(c.best().unwrap().offset_us, 100);
        assert_eq!(c.host_to_client_us(1_100), Some(1_000));
        // Push the good sample out of the window.
        c.add(0, 4_100, 4_100, 8_000);
        c.add(0, 3_600, 3_600, 7_000);
        c.add(0, 2_600, 2_600, 5_000);
        assert_eq!(c.best().unwrap().rtt_us, 5_000);
    }

    #[test]
    fn clock_sync_wire_roundtrip_and_old_host_echo() {
        let req = encode_clock_sync_request(42);
        assert_eq!(decode_clock_sync_request(&req), Some(42));
        // Old host echoes the request verbatim: not a usable response.
        assert_eq!(decode_clock_sync_response(&req), None);
        let resp = encode_clock_sync_response(42, 100, 150);
        assert_eq!(decode_clock_sync_response(&resp), Some((42, 100, 150)));
        // Ordinary pings are not clock-sync requests.
        assert_eq!(decode_clock_sync_request(&[0u8; 8]), None);
        assert_eq!(decode_clock_sync_request(b"LQCK\x02aaaaaaaa"), None);
    }

    #[test]
    fn host_frame_timing_roundtrip() {
        let h = HostFrameTiming {
            frame_id: 9,
            stream_id: 1,
            capture_us: 1,
            encode_start_us: 2,
            encode_end_us: 3,
            send_us: 4,
        };
        assert_eq!(HostFrameTiming::decode(&h.encode()), Some(h));
        assert_eq!(HostFrameTiming::decode(b"junk"), None);
    }

    fn host() -> HostFrameTiming {
        // Host clock = client + 1_000_000.
        HostFrameTiming {
            frame_id: 7,
            stream_id: 0,
            capture_us: 1_010_000,
            encode_start_us: 1_010_500,
            encode_end_us: 1_013_500,
            send_us: 1_014_000,
        }
    }

    fn client() -> ClientFrameTiming {
        ClientFrameTiming {
            receive_us: 16_000,
            decode_start_us: 17_000,
            decode_end_us: 19_500,
            present_us: 25_000,
        }
    }

    #[test]
    fn combine_uncalibrated_keeps_only_single_clock_stages() {
        let m = MeasuredFrameLatency::combine(&host(), &client(), &ClockSync::default());
        assert_eq!(m.capture_to_encode_us, Some(500));
        assert_eq!(m.encode_us, Some(3_000));
        assert_eq!(m.send_us, Some(500));
        assert_eq!(m.network_us, None);
        assert_eq!(m.jitter_us, Some(1_000));
        assert_eq!(m.decode_us, Some(2_500));
        assert_eq!(m.present_us, Some(5_500));
        assert_eq!(m.capture_to_present_us, None);
        assert!(!m.is_end_to_end_measured());
    }

    #[test]
    fn combine_calibrated_propagates_timestamps_end_to_end() {
        let mut clock = ClockSync::default();
        clock.add(0, 1_000_500, 1_000_500, 1_000).unwrap(); // offset 1_000_000, rtt 1ms
        let m = MeasuredFrameLatency::combine(&host(), &client(), &clock);
        assert_eq!(m.network_us, Some(2_000));
        assert_eq!(m.capture_to_present_us, Some(15_000));
        assert_eq!(m.clock_rtt_us, Some(1_000));
        // Stages add up to the end-to-end figure.
        let sum: u64 = [
            m.capture_to_encode_us,
            m.encode_us,
            m.send_us,
            m.network_us,
            m.jitter_us,
            m.decode_us,
            m.present_us,
        ]
        .iter()
        .map(|v| v.unwrap())
        .sum();
        assert_eq!(sum, 15_000);
    }

    #[test]
    fn combine_missing_timestamps_are_absent_not_zero() {
        let mut c = client();
        c.present_us = 0;
        let mut clock = ClockSync::default();
        clock.add(0, 1_000_500, 1_000_500, 1_000);
        let m = MeasuredFrameLatency::combine(&host(), &c, &clock);
        assert_eq!(m.present_us, None);
        assert_eq!(m.capture_to_present_us, None);
    }
}

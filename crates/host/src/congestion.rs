//! Adaptive bitrate and congestion control driven by client feedback.
//!
//! The client reports loss, one-way delay trend, jitter and the rate it actually received
//! ([`NetworkFeedback`], sent on the input channel). [`CongestionController`] turns those into
//! a target video bitrate and FEC parity ratio for the whole link: multiplicative decrease on
//! loss or rising delay, then a fast multiplicative probe back up to the configured ceiling
//! (`--bitrate`) once the path is clean, so a dip recovers in a few seconds. The USB/ADB and
//! Wi-Fi links use different [`CongestionConfig`]s: USB stays near the ceiling and reacts
//! slowly, Wi-Fi reacts quickly and adds FEC when loss appears.
//!
//! [`RateCoordinator`] owns the controller for a session, splits the link target across the
//! monitor streams with the focused panel weighted higher ([`split_rate`]), and publishes each
//! stream's target through a [`StreamRate`] that its video sender polls lock-free. The sender
//! writes back the rate it measured, which the dashboard reads via
//! [`RateCoordinator::snapshot`].
//!
//! Everything takes `now` as an argument, so the policy is tested without clocks or sockets.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use linux_quest_protocol::NetworkFeedback;
use parking_lot::Mutex;

use crate::link_status::{LinkTransport, SharedLinkTracker};

/// Weight of the focused panel relative to an unfocused one when splitting the rate.
pub const FOCUS_WEIGHT: u32 = 3;

/// Which link the controller is tuned for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkPolicy {
    /// Shared radio: react fast, probe quickly, add FEC when loss shows up.
    Wifi,
    /// ADB reverse over USB 3: near-fixed high rate, only trim for sustained trouble.
    Usb,
}

impl LinkPolicy {
    pub fn from_transport(transport: Option<LinkTransport>) -> Self {
        match transport {
            Some(LinkTransport::UsbAdb) => Self::Usb,
            _ => Self::Wifi,
        }
    }
}

/// Tunables of one [`LinkPolicy`].
#[derive(Debug, Clone, PartialEq)]
pub struct CongestionConfig {
    pub max_kbps: u32,
    pub min_kbps: u32,
    /// Loss fraction (0..1) at or above which the path counts as congested.
    pub loss_threshold: f32,
    /// Delay growth (µs per second) at or above which queues are considered to be building.
    pub delay_trend_threshold_us_per_s: i32,
    /// Jitter at or above which the rate is held (neither raised nor, on its own, cut).
    pub jitter_hold_us: u32,
    /// Shortest time between two decreases.
    pub decrease_cooldown: Duration,
    /// Time after a decrease before probing upward again.
    pub increase_hold: Duration,
    /// Shortest time between two increases.
    pub increase_interval: Duration,
    /// Multiplier per increase step.
    pub increase_factor: f64,
    /// No feedback for this long is treated as a stall and the rate is cut.
    pub feedback_timeout: Duration,
    pub base_fec_ratio: f64,
    /// Highest parity ratio loss may raise FEC to. Equal to the base to keep FEC fixed.
    pub max_fec_ratio: f64,
}

impl CongestionConfig {
    pub fn for_policy(policy: LinkPolicy, max_kbps: u32) -> Self {
        let max_kbps = max_kbps.max(1);
        match policy {
            LinkPolicy::Wifi => Self {
                max_kbps,
                min_kbps: (max_kbps / 10).max(1_000).min(max_kbps),
                loss_threshold: 0.02,
                delay_trend_threshold_us_per_s: 5_000,
                jitter_hold_us: 10_000,
                decrease_cooldown: Duration::from_millis(200),
                increase_hold: Duration::from_millis(500),
                increase_interval: Duration::from_millis(200),
                increase_factor: 1.15,
                feedback_timeout: Duration::from_millis(1_500),
                base_fec_ratio: 0.25,
                max_fec_ratio: 0.5,
            },
            LinkPolicy::Usb => Self {
                max_kbps,
                min_kbps: (max_kbps / 10 * 7).max(1).min(max_kbps),
                loss_threshold: 0.05,
                delay_trend_threshold_us_per_s: 20_000,
                jitter_hold_us: 30_000,
                decrease_cooldown: Duration::from_millis(500),
                increase_hold: Duration::from_secs(2),
                increase_interval: Duration::from_millis(500),
                increase_factor: 1.05,
                feedback_timeout: Duration::from_secs(5),
                base_fec_ratio: 0.25,
                max_fec_ratio: 0.25,
            },
        }
    }
}

/// What the controller wants the encoder and packetizer to use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Decision {
    pub target_kbps: u32,
    pub fec_ratio: f64,
}

/// Feedback-driven rate controller for one link.
#[derive(Debug)]
pub struct CongestionController {
    cfg: CongestionConfig,
    target_kbps: u32,
    fec_ratio: f64,
    loss_ewma: f32,
    last_decrease: Option<Instant>,
    last_increase: Option<Instant>,
    last_feedback: Option<Instant>,
}

impl CongestionController {
    /// Starts at the ceiling: a fresh link is assumed good until the client says otherwise.
    pub fn new(cfg: CongestionConfig) -> Self {
        Self {
            target_kbps: cfg.max_kbps,
            fec_ratio: cfg.base_fec_ratio,
            cfg,
            loss_ewma: 0.0,
            last_decrease: None,
            last_increase: None,
            last_feedback: None,
        }
    }

    pub fn decision(&self) -> Decision {
        Decision {
            target_kbps: self.target_kbps,
            fec_ratio: self.fec_ratio,
        }
    }

    pub fn config(&self) -> &CongestionConfig {
        &self.cfg
    }

    /// Switches to another policy's tunables, keeping the current target within the new range.
    pub fn set_config(&mut self, cfg: CongestionConfig) {
        self.target_kbps = self.target_kbps.clamp(cfg.min_kbps, cfg.max_kbps);
        self.fec_ratio = self.fec_ratio.clamp(cfg.base_fec_ratio, cfg.max_fec_ratio);
        self.cfg = cfg;
    }

    fn since(now: Instant, then: Option<Instant>) -> Option<Duration> {
        then.map(|t| now.saturating_duration_since(t))
    }

    fn can_decrease(&self, now: Instant) -> bool {
        Self::since(now, self.last_decrease).is_none_or(|d| d >= self.cfg.decrease_cooldown)
    }

    fn decrease_to(&mut self, kbps: u32, now: Instant) {
        self.target_kbps = kbps.clamp(self.cfg.min_kbps, self.cfg.max_kbps);
        self.last_decrease = Some(now);
    }

    /// Folds one client report in and returns the new decision.
    pub fn on_feedback(&mut self, f: &NetworkFeedback, now: Instant) -> Decision {
        self.last_feedback = Some(now);
        let loss = f32::from(f.loss_permille) / 1000.0;
        self.loss_ewma = 0.5 * self.loss_ewma + 0.5 * loss;
        // Rounded to whole percent so a decayed loss average settles back on the base ratio.
        let wanted = (self.cfg.base_fec_ratio + 2.0 * f64::from(self.loss_ewma)) * 100.0;
        self.fec_ratio =
            (wanted.round() / 100.0).clamp(self.cfg.base_fec_ratio, self.cfg.max_fec_ratio);

        let congested = loss >= self.cfg.loss_threshold
            || f.delay_trend_us_per_s >= self.cfg.delay_trend_threshold_us_per_s;
        if congested {
            if self.can_decrease(now) {
                // If the client received less than we sent, that is the capacity estimate.
                let basis = if f.received_kbps > 0 && f.received_kbps < self.target_kbps {
                    f.received_kbps
                } else {
                    self.target_kbps
                };
                let factor = if loss >= 0.10 { 0.7 } else { 0.85 };
                let lowered = (f64::from(basis) * factor).round() as u32;
                self.decrease_to(lowered.min(self.target_kbps), now);
            }
        } else if f.jitter_us < self.cfg.jitter_hold_us
            && Self::since(now, self.last_decrease).is_none_or(|d| d >= self.cfg.increase_hold)
            && Self::since(now, self.last_increase).is_none_or(|d| d >= self.cfg.increase_interval)
        {
            let stepped = (f64::from(self.target_kbps) * self.cfg.increase_factor).round() as u32;
            let at_least = self
                .target_kbps
                .saturating_add((self.cfg.max_kbps / 100).max(1));
            self.target_kbps = stepped.max(at_least).min(self.cfg.max_kbps);
            self.last_increase = Some(now);
        }
        self.decision()
    }

    /// Call periodically. If the client has gone quiet the path is presumed stalled and the
    /// rate is cut step by step until reports resume.
    pub fn on_tick(&mut self, now: Instant) -> Decision {
        if let Some(silent) = Self::since(now, self.last_feedback) {
            if silent >= self.cfg.feedback_timeout && self.can_decrease(now) {
                self.decrease_to((f64::from(self.target_kbps) * 0.7).round() as u32, now);
            }
        }
        self.decision()
    }
}

/// Divides `total_kbps` over `streams`, the focused one weighted [`FOCUS_WEIGHT`] times an
/// unfocused one. The shares sum to `total_kbps` exactly (leftover kbit/s go to the heaviest
/// streams first), and the result is in the order of `streams`.
pub fn split_rate(total_kbps: u32, streams: &[u16], focused: Option<u16>) -> Vec<u32> {
    if streams.is_empty() {
        return Vec::new();
    }
    let weight = |id: u16| if Some(id) == focused { FOCUS_WEIGHT } else { 1 };
    let total_weight: u64 = streams.iter().map(|&id| u64::from(weight(id))).sum();
    let mut shares: Vec<u32> = streams
        .iter()
        .map(|&id| (u64::from(total_kbps) * u64::from(weight(id)) / total_weight) as u32)
        .collect();
    let mut leftover = total_kbps - shares.iter().sum::<u32>();
    let mut order: Vec<usize> = (0..streams.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(weight(streams[i])));
    for i in order.into_iter().cycle() {
        if leftover == 0 {
            break;
        }
        shares[i] += 1;
        leftover -= 1;
    }
    shares
}

/// One stream's published rate. The coordinator writes `target`/`fec`; the stream's video
/// sender reads them and writes `actual`.
#[derive(Debug)]
pub struct StreamRate {
    stream_id: u16,
    target_kbps: AtomicU32,
    fec_permille: AtomicU32,
    actual_kbps: AtomicU32,
}

impl StreamRate {
    fn new(stream_id: u16, target_kbps: u32, fec_ratio: f64) -> Self {
        Self {
            stream_id,
            target_kbps: AtomicU32::new(target_kbps),
            fec_permille: AtomicU32::new(permille(fec_ratio)),
            actual_kbps: AtomicU32::new(0),
        }
    }

    pub fn stream_id(&self) -> u16 {
        self.stream_id
    }

    pub fn target_kbps(&self) -> u32 {
        self.target_kbps.load(Ordering::Relaxed)
    }

    pub fn fec_ratio(&self) -> f64 {
        f64::from(self.fec_permille.load(Ordering::Relaxed)) / 1000.0
    }

    pub fn actual_kbps(&self) -> u32 {
        self.actual_kbps.load(Ordering::Relaxed)
    }

    /// Called by the video sender with the video rate it measured.
    pub fn set_actual_kbps(&self, kbps: u32) {
        self.actual_kbps.store(kbps, Ordering::Relaxed);
    }

    fn publish(&self, target_kbps: u32, fec_ratio: f64) {
        self.target_kbps.store(target_kbps, Ordering::Relaxed);
        self.fec_permille
            .store(permille(fec_ratio), Ordering::Relaxed);
    }
}

fn permille(ratio: f64) -> u32 {
    (ratio * 1000.0).round().clamp(0.0, 1000.0) as u32
}

/// Per-stream numbers for the dashboard.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StreamRateSnapshot {
    pub stream_id: u16,
    pub target_kbps: u32,
    pub actual_kbps: u32,
    pub fec_ratio: f64,
}

struct CoordinatorState {
    controller: CongestionController,
    policy: LinkPolicy,
    streams: Vec<Arc<StreamRate>>,
    focused: Option<u16>,
}

/// Session-wide rate state shared by the input receiver (feedback in), the video senders
/// (targets out) and the dashboard.
pub struct RateCoordinator {
    state: Mutex<CoordinatorState>,
    max_kbps: u32,
    adaptive: bool,
    link: Option<SharedLinkTracker>,
}

impl RateCoordinator {
    /// `max_kbps` is the configured ceiling. With `adaptive` false (`--fixed-bitrate`) the
    /// targets never move and feedback is ignored.
    pub fn new(max_kbps: u32, adaptive: bool) -> Self {
        let policy = LinkPolicy::Wifi;
        Self {
            state: Mutex::new(CoordinatorState {
                controller: CongestionController::new(CongestionConfig::for_policy(
                    policy, max_kbps,
                )),
                policy,
                streams: Vec::new(),
                focused: None,
            }),
            max_kbps,
            adaptive,
            link: None,
        }
    }

    /// Chooses the policy from the live link: USB/ADB or Wi-Fi.
    pub fn with_link(mut self, link: SharedLinkTracker) -> Self {
        self.link = Some(link);
        self
    }

    pub fn max_kbps(&self) -> u32 {
        self.max_kbps
    }

    /// Adds a stream and re-splits the current rate over all streams.
    pub fn register_stream(&self, stream_id: u16) -> Arc<StreamRate> {
        let mut st = self.state.lock();
        let decision = st.controller.decision();
        let rate = Arc::new(StreamRate::new(
            stream_id,
            decision.target_kbps,
            decision.fec_ratio,
        ));
        st.streams.push(rate.clone());
        Self::publish(&st, decision);
        rate
    }

    fn publish(st: &CoordinatorState, decision: Decision) {
        let ids: Vec<u16> = st.streams.iter().map(|s| s.stream_id).collect();
        let shares = split_rate(decision.target_kbps, &ids, st.focused);
        for (stream, share) in st.streams.iter().zip(shares) {
            stream.publish(share, decision.fec_ratio);
        }
    }

    fn sync_policy(&self, st: &mut CoordinatorState, now: Instant) {
        let Some(link) = &self.link else { return };
        let policy = LinkPolicy::from_transport(link.lock().status(now).transport);
        if policy != st.policy {
            st.policy = policy;
            st.controller
                .set_config(CongestionConfig::for_policy(policy, self.max_kbps));
        }
    }

    /// Handles one client report.
    pub fn on_feedback(&self, feedback: &NetworkFeedback, now: Instant) {
        let mut st = self.state.lock();
        st.focused = (feedback.focused_stream != linux_quest_protocol::NO_FOCUSED_STREAM)
            .then_some(feedback.focused_stream);
        if !self.adaptive {
            Self::publish(&st, st.controller.decision());
            return;
        }
        self.sync_policy(&mut st, now);
        let decision = st.controller.on_feedback(feedback, now);
        Self::publish(&st, decision);
    }

    /// Periodic stall check; call a few times a second.
    pub fn tick(&self, now: Instant) {
        if !self.adaptive {
            return;
        }
        let mut st = self.state.lock();
        self.sync_policy(&mut st, now);
        let decision = st.controller.on_tick(now);
        Self::publish(&st, decision);
    }

    pub fn snapshot(&self) -> Vec<StreamRateSnapshot> {
        self.state
            .lock()
            .streams
            .iter()
            .map(|s| StreamRateSnapshot {
                stream_id: s.stream_id,
                target_kbps: s.target_kbps(),
                actual_kbps: s.actual_kbps(),
                fec_ratio: s.fec_ratio(),
            })
            .collect()
    }
}

static GLOBAL: parking_lot::RwLock<Option<Arc<RateCoordinator>>> = parking_lot::RwLock::new(None);

/// Makes `coordinator` the one the dashboard reports on.
pub fn install_global(coordinator: Arc<RateCoordinator>) {
    *GLOBAL.write() = Some(coordinator);
}

/// Rates of the installed coordinator; empty when no video session was set up.
pub fn global_snapshot() -> Vec<StreamRateSnapshot> {
    GLOBAL
        .read()
        .as_ref()
        .map_or_else(Vec::new, |c| c.snapshot())
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_quest_protocol::NO_FOCUSED_STREAM;

    fn fb(loss_permille: u16, trend: i32, jitter_us: u32, received_kbps: u32) -> NetworkFeedback {
        NetworkFeedback {
            loss_permille,
            delay_trend_us_per_s: trend,
            jitter_us,
            received_kbps,
            focused_stream: NO_FOCUSED_STREAM,
        }
    }

    fn wifi(max: u32) -> CongestionController {
        CongestionController::new(CongestionConfig::for_policy(LinkPolicy::Wifi, max))
    }

    const STEP: Duration = Duration::from_millis(100);

    /// Simulated bottleneck: loss above capacity, delay growing while over it.
    fn netem(target_kbps: u32, cap_kbps: u32) -> NetworkFeedback {
        if target_kbps <= cap_kbps {
            fb(0, 0, 1_000, target_kbps)
        } else {
            let loss = (1.0 - f64::from(cap_kbps) / f64::from(target_kbps)) * 1000.0;
            fb(loss as u16, 20_000, 3_000, cap_kbps)
        }
    }

    #[test]
    fn clean_link_stays_at_the_ceiling() {
        let mut c = wifi(30_000);
        let t0 = Instant::now();
        for i in 0..50 {
            let d = c.on_feedback(&fb(0, 0, 500, 30_000), t0 + STEP * i);
            assert_eq!(d.target_kbps, 30_000);
            assert_eq!(d.fec_ratio, 0.25);
        }
    }

    #[test]
    fn loss_cuts_toward_the_received_rate() {
        let mut c = wifi(30_000);
        let d = c.on_feedback(&fb(150, 0, 500, 12_000), Instant::now());
        assert_eq!(d.target_kbps, 8_400, "12 Mbit/s received * 0.7");
    }

    #[test]
    fn rising_delay_alone_backs_off() {
        let mut c = wifi(30_000);
        let d = c.on_feedback(&fb(0, 9_000, 500, 0), Instant::now());
        assert_eq!(d.target_kbps, 25_500);
    }

    #[test]
    fn decreases_are_rate_limited_by_the_cooldown() {
        let mut c = wifi(30_000);
        let t0 = Instant::now();
        c.on_feedback(&fb(100, 0, 0, 0), t0);
        let d = c.on_feedback(&fb(100, 0, 0, 0), t0 + Duration::from_millis(50));
        assert_eq!(
            d.target_kbps, 21_000,
            "second report inside the cooldown is ignored"
        );
        let d = c.on_feedback(&fb(100, 0, 0, 0), t0 + Duration::from_millis(250));
        assert!(d.target_kbps < 21_000);
    }

    #[test]
    fn never_drops_below_the_floor() {
        let mut c = wifi(30_000);
        let t0 = Instant::now();
        for i in 0..100 {
            c.on_feedback(&fb(500, 50_000, 0, 100), t0 + STEP * i);
        }
        assert_eq!(c.decision().target_kbps, 3_000);
    }

    #[test]
    fn high_jitter_holds_the_rate() {
        let mut c = wifi(30_000);
        let t0 = Instant::now();
        c.on_feedback(&fb(100, 0, 0, 10_000), t0);
        let low = c.decision().target_kbps;
        for i in 1..40 {
            c.on_feedback(&fb(0, 0, 40_000, 0), t0 + STEP * i);
        }
        assert_eq!(c.decision().target_kbps, low);
    }

    #[test]
    fn degrades_gracefully_under_a_bandwidth_cap_and_recovers_in_seconds() {
        let mut c = wifi(30_000);
        let t0 = Instant::now();
        let mut now = t0;
        let report = |c: &mut CongestionController, now: Instant, cap: u32| {
            let target = c.decision().target_kbps;
            c.on_feedback(&netem(target, cap), now);
        };
        // Settle on a clean link, then cap to 8 Mbit/s for 5 s.
        for _ in 0..10 {
            now += STEP;
            report(&mut c, now, 100_000);
        }
        let mut worst_overshoot = 0u32;
        for step in 0..50 {
            now += STEP;
            report(&mut c, now, 8_000);
            if step >= 20 {
                worst_overshoot =
                    worst_overshoot.max(c.decision().target_kbps.saturating_sub(8_000));
            }
        }
        // After 2 s the target hugs the cap (probing above it is brief) and never collapsed
        // to the floor.
        assert!(worst_overshoot <= 3_000, "overshoot {worst_overshoot}");
        assert!(c.decision().target_kbps >= 3_000);
        // Cap lifted: back to full rate within 6 s.
        let lifted = now;
        let mut recovered_at = None;
        for _ in 0..100 {
            now += STEP;
            report(&mut c, now, 100_000);
            if c.decision().target_kbps == 30_000 {
                recovered_at = Some(now - lifted);
                break;
            }
        }
        let took = recovered_at.expect("never recovered");
        assert!(took <= Duration::from_secs(6), "recovery took {took:?}");
    }

    #[test]
    fn recovers_from_a_sudden_deep_cut_within_a_few_seconds() {
        let mut c = wifi(30_000);
        let t0 = Instant::now();
        c.on_feedback(&fb(400, 30_000, 0, 1_000), t0);
        assert_eq!(c.decision().target_kbps, 3_000, "floor");
        let mut now = t0;
        let mut elapsed = Duration::ZERO;
        while c.decision().target_kbps < 30_000 {
            now += STEP;
            elapsed += STEP;
            c.on_feedback(&fb(0, 0, 500, 3_000), now);
            assert!(
                elapsed < Duration::from_secs(8),
                "stuck at {}",
                c.decision().target_kbps
            );
        }
        assert!(elapsed <= Duration::from_secs(6), "{elapsed:?}");
    }

    #[test]
    fn fec_rises_with_loss_and_falls_back() {
        let mut c = wifi(30_000);
        let t0 = Instant::now();
        let d = c.on_feedback(&fb(100, 0, 0, 0), t0);
        assert!(d.fec_ratio > 0.25 && d.fec_ratio <= 0.5, "{}", d.fec_ratio);
        let mut now = t0;
        for _ in 0..10 {
            now += STEP;
            c.on_feedback(&fb(0, 0, 0, 0), now);
        }
        assert_eq!(c.decision().fec_ratio, 0.25);
        // Loss never pushes FEC past the cap.
        for _ in 0..10 {
            now += STEP;
            c.on_feedback(&fb(1000, 0, 0, 0), now);
        }
        assert_eq!(c.decision().fec_ratio, 0.5);
    }

    #[test]
    fn usb_policy_stays_high_and_keeps_fec_fixed() {
        let cfg = CongestionConfig::for_policy(LinkPolicy::Usb, 100_000);
        assert_eq!(cfg.min_kbps, 70_000);
        let mut c = CongestionController::new(cfg);
        let t0 = Instant::now();
        // Mild loss below the USB threshold is ignored.
        let d = c.on_feedback(&fb(30, 0, 0, 0), t0);
        assert_eq!(d.target_kbps, 100_000);
        // Heavy trouble trims but never below 70 %.
        let mut now = t0;
        for _ in 0..40 {
            now += Duration::from_millis(600);
            c.on_feedback(&fb(300, 50_000, 0, 10_000), now);
        }
        assert_eq!(c.decision().target_kbps, 70_000);
        assert_eq!(c.decision().fec_ratio, 0.25);
    }

    #[test]
    fn silence_is_treated_as_a_stall_and_resumes_cleanly() {
        let mut c = wifi(30_000);
        let t0 = Instant::now();
        c.on_feedback(&fb(0, 0, 0, 30_000), t0);
        assert_eq!(
            c.on_tick(t0 + Duration::from_millis(500)).target_kbps,
            30_000
        );
        let d = c.on_tick(t0 + Duration::from_secs(2));
        assert_eq!(d.target_kbps, 21_000);
        let d = c.on_tick(t0 + Duration::from_millis(2_100));
        assert_eq!(d.target_kbps, 21_000, "cooldown");
        let d = c.on_tick(t0 + Duration::from_secs(3));
        assert_eq!(d.target_kbps, 14_700);
        // Without any feedback ever, ticks do nothing.
        let mut fresh = wifi(30_000);
        assert_eq!(
            fresh.on_tick(t0 + Duration::from_secs(60)).target_kbps,
            30_000
        );
    }

    #[test]
    fn split_gives_the_focused_panel_priority_and_conserves_the_total() {
        assert_eq!(split_rate(30_000, &[0, 1, 2], None), vec![10_000; 3]);
        let shares = split_rate(30_000, &[0, 1, 2], Some(1));
        assert_eq!(shares, vec![6_000, 18_000, 6_000]);
        let odd = split_rate(10_001, &[4, 5, 6], Some(6));
        assert_eq!(odd.iter().sum::<u32>(), 10_001);
        assert!(odd[2] > odd[0] && odd[2] > odd[1]);
        assert_eq!(split_rate(5_000, &[7], Some(9)), vec![5_000]);
        assert!(split_rate(5_000, &[], None).is_empty());
        // A focus id that is not a stream changes nothing.
        assert_eq!(split_rate(9_000, &[0, 1], Some(5)), vec![4_500, 4_500]);
    }

    #[test]
    fn coordinator_publishes_split_targets_and_follows_focus() {
        let rc = RateCoordinator::new(30_000, true);
        let a = rc.register_stream(0);
        let b = rc.register_stream(1);
        assert_eq!((a.target_kbps(), b.target_kbps()), (15_000, 15_000));

        let now = Instant::now();
        let mut f = fb(0, 0, 0, 30_000);
        f.focused_stream = 1;
        rc.on_feedback(&f, now);
        assert_eq!((a.target_kbps(), b.target_kbps()), (7_500, 22_500));

        // Congestion lowers the total; the focused panel keeps the larger share.
        let mut lossy = fb(100, 0, 0, 12_000);
        lossy.focused_stream = 1;
        rc.on_feedback(&lossy, now + Duration::from_millis(300));
        assert_eq!(a.target_kbps() + b.target_kbps(), 8_400);
        assert!(b.target_kbps() > a.target_kbps());
        assert!(a.fec_ratio() > 0.25);

        b.set_actual_kbps(6_000);
        let snap = rc.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[1].actual_kbps, 6_000);
        assert_eq!(snap[1].target_kbps, b.target_kbps());
    }

    #[test]
    fn fixed_bitrate_ignores_feedback_but_still_follows_focus() {
        let rc = RateCoordinator::new(20_000, false);
        let a = rc.register_stream(0);
        let b = rc.register_stream(1);
        let mut f = fb(500, 90_000, 0, 1_000);
        f.focused_stream = 0;
        rc.on_feedback(&f, Instant::now());
        rc.tick(Instant::now() + Duration::from_secs(30));
        assert_eq!(a.target_kbps() + b.target_kbps(), 20_000);
        assert_eq!(a.fec_ratio(), 0.25);
    }

    #[test]
    fn coordinator_picks_the_policy_from_the_link() {
        use crate::link_status::{new_shared_link_tracker, DEFAULT_INPUT_LIVENESS};
        let link = new_shared_link_tracker(DEFAULT_INPUT_LIVENESS);
        let rc = RateCoordinator::new(100_000, true).with_link(link.clone());
        let s = rc.register_stream(0);
        let now = Instant::now();
        // No transport: Wi-Fi tuning, so loss cuts hard.
        rc.on_feedback(&fb(100, 0, 0, 0), now);
        assert_eq!(
            s.target_kbps(),
            70_000,
            "Wi-Fi tuning cuts by 30 % on heavy loss"
        );
        assert_eq!(
            LinkPolicy::from_transport(Some(LinkTransport::UsbAdb)),
            LinkPolicy::Usb
        );
        assert_eq!(LinkPolicy::from_transport(None), LinkPolicy::Wifi);
    }
}

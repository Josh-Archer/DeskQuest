//! Hand-off between the PipeWire thread (producer) and `capture_frame().await` (consumer).
//!
//! The queue is bounded and latest-wins: when the consumer is slower than the compositor the
//! oldest waiting frame is dropped, so the encoder always sees the freshest picture and no
//! backlog (or run of stale DMA-BUFs) builds up.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::Notify;

use super::error::ScreenCastError;
use crate::{CaptureError, CaptureInfo, RawFrame};

/// Counters describing what the capture path did. Cheap to read; meant for logs and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureStats {
    /// Frames accepted into the queue.
    pub frames_queued: u64,
    /// Frames handed to the consumer.
    pub frames_delivered: u64,
    /// Frames dropped because a newer one arrived before the consumer took them.
    pub frames_dropped: u64,
    /// Buffers the producer marked empty / corrupted / without damage, not forwarded.
    pub frames_unchanged: u64,
    /// Formats negotiated so far (1 = initial, more = renegotiations).
    pub negotiations: u64,
}

struct HubState {
    queue: VecDeque<RawFrame>,
    info: Option<CaptureInfo>,
    ended: Option<ScreenCastError>,
    stats: CaptureStats,
}

/// Shared state between the PipeWire thread and the consumer. Cloning the `Arc` is how both
/// sides hold it.
pub struct CaptureHub {
    capacity: usize,
    state: Mutex<HubState>,
    notify: Notify,
    generation: AtomicU64,
}

impl CaptureHub {
    /// `capacity` is the number of frames that may wait for the consumer (minimum 1).
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            capacity: capacity.max(1),
            state: Mutex::new(HubState {
                queue: VecDeque::new(),
                info: None,
                ended: None,
                stats: CaptureStats::default(),
            }),
            notify: Notify::new(),
            generation: AtomicU64::new(0),
        })
    }

    fn lock(&self) -> MutexGuard<'_, HubState> {
        // A panic while holding the lock must not poison capture for good.
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Queues a frame, evicting the oldest waiting one if the queue is full. Frames pushed
    /// after the stream ended are discarded.
    pub fn push(&self, frame: RawFrame) {
        {
            let mut st = self.lock();
            if st.ended.is_some() {
                return;
            }
            if st.queue.len() >= self.capacity {
                st.queue.pop_front();
                st.stats.frames_dropped += 1;
            }
            st.queue.push_back(frame);
            st.stats.frames_queued += 1;
        }
        self.notify.notify_waiters();
        self.notify.notify_one();
    }

    /// Records a buffer that carried no new picture.
    pub fn note_unchanged(&self) {
        self.lock().stats.frames_unchanged += 1;
    }

    /// Publishes the negotiated stream parameters (also on every re-negotiation).
    pub fn set_info(&self, info: CaptureInfo) {
        {
            let mut st = self.lock();
            st.info = Some(info);
            st.stats.negotiations += 1;
        }
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_waiters();
        self.notify.notify_one();
    }

    /// The most recently negotiated parameters, if any.
    pub fn info(&self) -> Option<CaptureInfo> {
        self.lock().info
    }

    /// Increases every time [`set_info`](Self::set_info) is called.
    pub fn info_generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub fn stats(&self) -> CaptureStats {
        self.lock().stats
    }

    /// Marks the stream as finished. The first reason wins; queued frames are discarded
    /// because they would be stale by the time a restart delivers new ones.
    pub fn end(&self, reason: ScreenCastError) {
        {
            let mut st = self.lock();
            if st.ended.is_none() {
                st.ended = Some(reason);
            }
            st.queue.clear();
        }
        self.notify.notify_waiters();
        self.notify.notify_one();
    }

    /// Why the stream ended, if it did.
    pub fn ended(&self) -> Option<ScreenCastError> {
        self.lock().ended.clone()
    }

    /// Forgets the previous run so the hub can serve a restarted capture.
    pub fn reset(&self) {
        let mut st = self.lock();
        st.queue.clear();
        st.info = None;
        st.ended = None;
        st.stats = CaptureStats::default();
    }

    /// Waits for the next frame. Returns the end reason as an error once the stream is over.
    pub async fn next_frame(&self) -> Result<RawFrame, CaptureError> {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            // Register interest before checking state so a push in between is not missed.
            notified.as_mut().enable();
            {
                let mut st = self.lock();
                if let Some(reason) = st.ended.clone() {
                    return Err(CaptureError::from(reason));
                }
                if let Some(frame) = st.queue.pop_front() {
                    st.stats.frames_delivered += 1;
                    return Ok(frame);
                }
            }
            notified.await;
        }
    }

    /// Waits until a format has been negotiated, the stream ended, or `timeout` passed.
    pub async fn wait_for_info(&self, timeout: Duration) -> Result<CaptureInfo, ScreenCastError> {
        let wait = async {
            loop {
                let notified = self.notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                {
                    let st = self.lock();
                    if let Some(reason) = st.ended.clone() {
                        return Err(reason);
                    }
                    if let Some(info) = st.info {
                        return Ok(info);
                    }
                }
                notified.await;
            }
        };
        match tokio::time::timeout(timeout, wait).await {
            Ok(r) => r,
            Err(_) => Err(ScreenCastError::NegotiationTimeout(timeout)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PixelFormat;
    use bytes::Bytes;

    fn frame(pts_us: u64) -> RawFrame {
        RawFrame {
            display_id: 0,
            width: 4,
            height: 2,
            stride: 16,
            format: PixelFormat::Bgra8,
            pts_us,
            dma_buf: None,
            data: Bytes::from_static(&[0; 32]),
        }
    }

    fn info(w: u32, h: u32, fps: u32) -> CaptureInfo {
        CaptureInfo {
            width: w,
            height: h,
            fps,
            format: PixelFormat::Bgra8,
            dmabuf: false,
            modifier: None,
        }
    }

    #[tokio::test]
    async fn latest_wins_drops_the_oldest_frames() {
        let hub = CaptureHub::new(2);
        for pts in 1..=5 {
            hub.push(frame(pts));
        }
        assert_eq!(hub.next_frame().await.unwrap().pts_us, 4);
        assert_eq!(hub.next_frame().await.unwrap().pts_us, 5);
        let s = hub.stats();
        assert_eq!(s.frames_queued, 5);
        assert_eq!(s.frames_dropped, 3);
        assert_eq!(s.frames_delivered, 2);
    }

    #[tokio::test]
    async fn capacity_is_at_least_one() {
        let hub = CaptureHub::new(0);
        hub.push(frame(1));
        hub.push(frame(2));
        assert_eq!(hub.next_frame().await.unwrap().pts_us, 2);
    }

    #[tokio::test]
    async fn next_frame_wakes_when_a_frame_arrives() {
        let hub = CaptureHub::new(1);
        let producer = Arc::clone(&hub);
        let t = tokio::spawn(async move { producer.next_frame().await.map(|f| f.pts_us) });
        tokio::time::sleep(Duration::from_millis(20)).await;
        hub.push(frame(42));
        assert_eq!(t.await.unwrap().unwrap(), 42);
    }

    #[tokio::test]
    async fn end_reports_the_reason_and_discards_stale_frames() {
        let hub = CaptureHub::new(4);
        hub.push(frame(1));
        hub.end(ScreenCastError::SessionClosed);
        hub.end(ScreenCastError::NodeRemoved); // first reason wins
        hub.push(frame(2)); // ignored
        let err = hub.next_frame().await.unwrap_err();
        assert!(matches!(
            err,
            CaptureError::ScreenCast(ScreenCastError::SessionClosed)
        ));
        assert_eq!(hub.ended(), Some(ScreenCastError::SessionClosed));
    }

    #[tokio::test]
    async fn end_wakes_a_blocked_consumer() {
        let hub = CaptureHub::new(1);
        let waiter = Arc::clone(&hub);
        let t = tokio::spawn(async move { waiter.next_frame().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        hub.end(ScreenCastError::NodeRemoved);
        assert!(t.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn info_propagates_and_renegotiation_bumps_the_generation() {
        let hub = CaptureHub::new(1);
        assert!(hub.info().is_none());
        assert_eq!(hub.info_generation(), 0);
        let waiter = Arc::clone(&hub);
        let t = tokio::spawn(async move { waiter.wait_for_info(Duration::from_secs(5)).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        hub.set_info(info(2560, 1440, 144));
        assert_eq!(t.await.unwrap().unwrap(), info(2560, 1440, 144));
        hub.set_info(info(1920, 1080, 60));
        assert_eq!(hub.info().unwrap().width, 1920);
        assert_eq!(hub.info_generation(), 2);
        assert_eq!(hub.stats().negotiations, 2);
    }

    #[tokio::test]
    async fn wait_for_info_times_out_and_surfaces_end() {
        let hub = CaptureHub::new(1);
        let err = hub
            .wait_for_info(Duration::from_millis(30))
            .await
            .unwrap_err();
        assert!(matches!(err, ScreenCastError::NegotiationTimeout(_)));
        hub.end(ScreenCastError::Stream("boom".into()));
        let err = hub.wait_for_info(Duration::from_secs(5)).await.unwrap_err();
        assert_eq!(err, ScreenCastError::Stream("boom".into()));
    }

    #[tokio::test]
    async fn reset_allows_a_restart() {
        let hub = CaptureHub::new(1);
        hub.set_info(info(1, 1, 1));
        hub.push(frame(1));
        hub.end(ScreenCastError::SessionClosed);
        hub.reset();
        assert!(hub.info().is_none());
        assert!(hub.ended().is_none());
        assert_eq!(hub.stats(), CaptureStats::default());
        hub.push(frame(9));
        assert_eq!(hub.next_frame().await.unwrap().pts_us, 9);
    }

    #[test]
    fn unchanged_buffers_are_counted() {
        let hub = CaptureHub::new(1);
        hub.note_unchanged();
        hub.note_unchanged();
        assert_eq!(hub.stats().frames_unchanged, 2);
    }
}

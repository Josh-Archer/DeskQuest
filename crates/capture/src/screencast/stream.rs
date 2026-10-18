//! The PipeWire side: one thread that owns the main loop, connects to the portal's PipeWire
//! remote, negotiates a video format and pushes frames into the [`CaptureHub`].
//!
//! Ownership is strictly thread-confined (the `pipewire` crate's objects are `!Send`); the
//! only things crossing threads are the hub (`Arc`), a quit channel and the join handle.
//! Dropping [`StreamHandle`] asks the loop to quit and joins the thread, so a stopped capture
//! leaves nothing running and nothing leaked.
//!
//! # Buffer lifetime
//!
//! * Memory buffers (memfd / memptr) are copied once into a `Bytes` and the PipeWire buffer
//!   is requeued immediately.
//! * DMA-BUF buffers are **not** copied: each plane's fd is `dup`ed into the `RawFrame` and
//!   the PipeWire buffer is requeued immediately. The fd refers to the producer's buffer
//!   pool, so the compositor may render into it again after a few frames. Consumers must
//!   import or encode the frame promptly (the hub's latest-wins queue keeps that window
//!   short). If tearing is ever observed, set `OfferConfig::dmabuf = false` to force SHM.

use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bytes::Bytes;
use pipewire as pw;
use pw::context::ContextRc;
use pw::main_loop::MainLoopRc;
use pw::properties::properties;
use pw::spa::buffer::meta::{MetaHeader, MetaHeaderFlags, MetaVideoDamage};
use pw::spa::buffer::{Data, DataType};
use pw::spa::param::ParamType;
use pw::spa::pod::Pod;
use pw::spa::utils::Direction;
use pw::stream::{StreamFlags, StreamRc, StreamState};

use super::error::ScreenCastError;
use super::format::{build_offers, fixate_modifier, NegotiatedFormat, OfferConfig};
use super::hub::CaptureHub;
use super::pod;
use crate::{CaptureInfo, DmaBufPlane, RawFrame};

/// Inputs of the stream thread.
pub struct StreamParams {
    /// The portal's PipeWire socket. `None` connects to the default PipeWire daemon.
    pub fd: Option<OwnedFd>,
    /// The node to capture; `None` lets the session manager pick (tests only).
    pub node_id: Option<u32>,
    pub display_id: u16,
    pub offer: OfferConfig,
    pub hub: Arc<CaptureHub>,
}

/// Owns the PipeWire thread. Dropping it stops the thread.
pub struct StreamHandle {
    quit: Mutex<Option<pw::channel::Sender<()>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    finished: Arc<AtomicBool>,
}

impl StreamHandle {
    /// True while the PipeWire thread is still running.
    pub fn is_running(&self) -> bool {
        !self.finished.load(Ordering::SeqCst)
    }

    /// Stops the main loop and joins the thread. Idempotent.
    pub fn stop(&self) {
        if let Some(tx) = self.quit.lock().unwrap_or_else(|p| p.into_inner()).take() {
            // If the loop already exited the receiver is gone and this fails harmlessly.
            let _ = tx.send(());
        }
        if let Some(t) = self.thread.lock().unwrap_or_else(|p| p.into_inner()).take() {
            if t.thread().id() != std::thread::current().id() {
                let _ = t.join();
            }
        }
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Starts the PipeWire thread and waits until the stream is connected (not until a format is
/// negotiated; that is published through the hub).
pub fn spawn(params: StreamParams) -> Result<StreamHandle, ScreenCastError> {
    let (ready_tx, ready_rx) = mpsc::channel::<Result<pw::channel::Sender<()>, ScreenCastError>>();
    let finished = Arc::new(AtomicBool::new(false));
    let thread_finished = Arc::clone(&finished);
    let hub = Arc::clone(&params.hub);

    let thread = std::thread::Builder::new()
        .name("lq-pipewire".into())
        .spawn(move || {
            let result = run(params, &ready_tx);
            if let Err(e) = result {
                // Setup failures are reported through `ready_tx`; later ones through the hub.
                let _ = ready_tx.send(Err(e));
            }
            thread_finished.store(true, Ordering::SeqCst);
        })
        .map_err(|e| ScreenCastError::Stream(format!("cannot start PipeWire thread: {e}")))?;

    match ready_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(quit)) => Ok(StreamHandle {
            quit: Mutex::new(Some(quit)),
            thread: Mutex::new(Some(thread)),
            finished,
        }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            hub.end(ScreenCastError::Stream(
                "PipeWire thread did not start".into(),
            ));
            Err(ScreenCastError::Stream(
                "PipeWire thread did not start in time".into(),
            ))
        }
    }
}

fn stream_err(what: &str, e: impl std::fmt::Display) -> ScreenCastError {
    ScreenCastError::Stream(format!("{what}: {e}"))
}

/// State shared by the stream callbacks (all on the PipeWire thread).
struct UserData {
    hub: Arc<CaptureHub>,
    mainloop: MainLoopRc,
    display_id: u16,
    offer: OfferConfig,
    format: Option<NegotiatedFormat>,
    started: Instant,
    streaming_seen: bool,
    damage_seen: bool,
    first_frame_logged: bool,
}

impl UserData {
    fn fail(&mut self, err: ScreenCastError) {
        tracing::warn!("screencast: {err}");
        self.hub.end(err);
        self.mainloop.quit();
    }
}

fn run(
    params: StreamParams,
    ready: &mpsc::Sender<Result<pw::channel::Sender<()>, ScreenCastError>>,
) -> Result<(), ScreenCastError> {
    pw::init();
    let mainloop = MainLoopRc::new(None).map_err(|e| stream_err("main loop", e))?;
    let context = ContextRc::new(&mainloop, None).map_err(|e| stream_err("context", e))?;
    let core = match params.fd {
        Some(fd) => context.connect_fd_rc(fd, None),
        None => context.connect_rc(None),
    }
    .map_err(|e| stream_err("cannot connect to the PipeWire remote", e))?;

    let (quit_tx, quit_rx) = pw::channel::channel::<()>();
    let _quit = quit_rx.attach(mainloop.loop_(), {
        let ml = mainloop.clone();
        move |_| ml.quit()
    });

    let hub = params.hub;
    let core_hub = Arc::clone(&hub);
    let core_ml = mainloop.clone();
    let _core_listener = core
        .add_listener_local()
        .error(move |id, seq, res, msg| {
            tracing::warn!("pipewire core error id={id} seq={seq} res={res}: {msg}");
            if id == pw::core::PW_ID_CORE && res == -libc::EPIPE {
                core_hub.end(ScreenCastError::Stream(
                    "connection to PipeWire was lost".into(),
                ));
                core_ml.quit();
            }
        })
        .register();

    // PipeWire does not tell a stream that its peer node left (the stream just goes to
    // Paused), so watch the registry for the node we capture disappearing.
    let registry = core
        .get_registry_rc()
        .map_err(|e| stream_err("cannot get registry", e))?;
    let target = params.node_id;
    let reg_hub = Arc::clone(&hub);
    let reg_ml = mainloop.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global_remove(move |id| {
            if Some(id) == target {
                tracing::warn!("screencast: PipeWire node {id} was removed");
                reg_hub.end(ScreenCastError::NodeRemoved);
                reg_ml.quit();
            }
        })
        .register();

    let stream = StreamRc::new(
        core.clone(),
        "linux-quest-capture",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| stream_err("cannot create stream", e))?;

    let user = UserData {
        hub: Arc::clone(&hub),
        mainloop: mainloop.clone(),
        display_id: params.display_id,
        offer: params.offer.clone(),
        format: None,
        started: Instant::now(),
        streaming_seen: false,
        damage_seen: false,
        first_frame_logged: false,
    };

    let _listener = stream
        .add_local_listener_with_user_data(user)
        .state_changed(on_state_changed)
        .param_changed(on_param_changed)
        .process(on_process)
        .register()
        .map_err(|e| stream_err("cannot register stream listener", e))?;

    let offers = build_offers(&params.offer);
    let pods = pod::enum_format_pods(&offers, &params.offer);
    let mut refs: Vec<&Pod> = pods.iter().filter_map(|b| pod::as_pod(b)).collect();
    stream
        .connect(
            Direction::Input,
            params.node_id,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut refs,
        )
        .map_err(|e| stream_err("cannot connect stream", e))?;

    if ready.send(Ok(quit_tx)).is_err() {
        // The spawner gave up; nobody will ever stop us.
        return Ok(());
    }
    mainloop.run();
    // `_listener`, `stream`, `_core_listener`, `core`, `context`, `mainloop` drop in reverse
    // order here; the stream is disconnected explicitly first.
    // Record why we stopped before disconnecting: the disconnect itself would otherwise look
    // like the node going away. If a failure already ended the hub, that reason stays.
    hub.end(ScreenCastError::Stopped);
    let _ = stream.disconnect();
    Ok(())
}

fn on_state_changed(
    _stream: &pw::stream::Stream,
    ud: &mut UserData,
    old: StreamState,
    new: StreamState,
) {
    tracing::debug!("screencast stream state {old:?} -> {new:?}");
    match new {
        StreamState::Error(msg) => ud.fail(ScreenCastError::Stream(msg)),
        StreamState::Streaming => ud.streaming_seen = true,
        StreamState::Unconnected if ud.streaming_seen || ud.format.is_some() => {
            // The node we were reading from went away (monitor unplugged, sharing stopped).
            ud.fail(ScreenCastError::NodeRemoved);
        }
        _ => {}
    }
}

fn on_param_changed(stream: &pw::stream::Stream, ud: &mut UserData, id: u32, param: Option<&Pod>) {
    if id != ParamType::Format.as_raw() {
        return;
    }
    let Some(param) = param else {
        // Format cleared (stream paused for renegotiation or torn down).
        ud.format = None;
        return;
    };
    let neg = match pod::parse_format(param.as_bytes()) {
        Ok(n) => n,
        Err(e) => return ud.fail(ScreenCastError::Stream(e.to_string())),
    };

    if neg.needs_fixation() {
        let Some(modifier) = fixate_modifier(&neg.modifier_choices, &ud.offer.modifiers) else {
            return ud.fail(ScreenCastError::Stream(
                "producer offered no DMA-BUF modifier".into(),
            ));
        };
        tracing::debug!(
            "screencast: fixating DMA-BUF modifier {modifier:#x} out of {:x?}",
            neg.modifier_choices
        );
        let bytes = pod::fixated_format_pod(&neg, modifier);
        if let Some(p) = pod::as_pod(&bytes) {
            if let Err(e) = stream.update_params(&mut [p]) {
                ud.fail(ScreenCastError::Stream(format!(
                    "cannot fixate format: {e}"
                )));
            }
        }
        return;
    }

    let fps = neg.fps(ud.offer.preferred_fps);
    let info = CaptureInfo {
        width: neg.width,
        height: neg.height,
        fps,
        format: neg.kind.pixel_format(),
        dmabuf: neg.is_dmabuf(),
        modifier: neg.modifier,
    };
    tracing::info!(
        "screencast: negotiated {:?} {}x{} @ {} fps via {}{}",
        neg.kind,
        neg.width,
        neg.height,
        fps,
        if info.dmabuf { "DMA-BUF" } else { "SHM" },
        neg.modifier
            .map(|m| format!(" (modifier {m:#x})"))
            .unwrap_or_default()
    );

    let buffers = pod::buffers_pod(&neg);
    let metas = pod::meta_pods();
    let mut params: Vec<&Pod> = std::iter::once(&buffers)
        .chain(metas.iter())
        .filter_map(|b| pod::as_pod(b))
        .collect();
    if let Err(e) = stream.update_params(&mut params) {
        return ud.fail(ScreenCastError::Stream(format!(
            "cannot set buffer params: {e}"
        )));
    }
    ud.format = Some(neg);
    ud.hub.set_info(info);
}

fn dup_fd(fd: i32) -> Option<OwnedFd> {
    // SAFETY: plain fcntl on an fd owned by the PipeWire buffer for the duration of the call.
    let new = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    // SAFETY: `new` is a fresh descriptor that nothing else owns.
    (new >= 0).then(|| unsafe { OwnedFd::from_raw_fd(new) })
}

enum Payload {
    Dma(Vec<DmaBufPlane>),
    Memory(Bytes),
    /// Producer marked the buffer as carrying no new picture.
    Unchanged,
    /// Could not read the buffer; skip it.
    Unusable(&'static str),
}

fn read_payload(datas: &mut [Data], fmt: &NegotiatedFormat) -> (u32, Payload) {
    let default_stride = fmt.default_stride();
    let Some(first) = datas.first() else {
        return (default_stride, Payload::Unusable("buffer has no data"));
    };
    let stride = match first.chunk().stride() {
        s if s > 0 => s as u32,
        _ => default_stride,
    };

    if first.type_() == DataType::DmaBuf {
        let modifier = fmt.modifier.unwrap_or(0);
        let mut planes = Vec::with_capacity(datas.len());
        for d in datas.iter() {
            let Some(fd) = dup_fd(d.fd()) else {
                return (stride, Payload::Unusable("cannot dup DMA-BUF fd"));
            };
            let plane_stride = match d.chunk().stride() {
                s if s > 0 => s as u32,
                _ => stride,
            };
            planes.push(DmaBufPlane::new(
                fd,
                plane_stride,
                d.chunk().offset(),
                modifier,
            ));
        }
        return (stride, Payload::Dma(planes));
    }

    let mut out: Vec<u8> = Vec::new();
    for d in datas.iter_mut() {
        let (offset, size) = (d.chunk().offset() as usize, d.chunk().size() as usize);
        let Some(mapped) = d.data() else {
            return (stride, Payload::Unusable("buffer is not mapped"));
        };
        if size == 0 {
            return (stride, Payload::Unchanged);
        }
        let end = offset.saturating_add(size).min(mapped.len());
        let start = offset.min(end);
        out.extend_from_slice(&mapped[start..end]);
    }
    (stride, Payload::Memory(Bytes::from(out)))
}

fn on_process(stream: &pw::stream::Stream, ud: &mut UserData) {
    // Latest wins: take the newest queued buffer, the older ones go straight back.
    let mut latest = None;
    while let Some(b) = stream.dequeue_buffer() {
        latest = Some(b);
    }
    let Some(mut buf) = latest else { return };
    let Some(fmt) = ud.format.clone() else { return };

    let (pts_ns, corrupted) = buf
        .find_meta::<MetaHeader>()
        .map(|h| (h.pts(), h.flags().contains(MetaHeaderFlags::CORRUPTED)))
        .unwrap_or((0, false));
    let no_damage = buf
        .find_meta::<MetaVideoDamage>()
        .map(|d| d.iter().next().is_none());
    if no_damage.is_some() && !ud.damage_seen {
        ud.damage_seen = true;
        tracing::info!("screencast: producer supplies video damage metadata");
    }

    let (stride, payload) = read_payload(buf.datas_mut(), &fmt);
    // `buf` is requeued when it drops at the end of this function.

    if corrupted {
        ud.hub.note_unchanged();
        return;
    }
    // An empty damage list on a frame after the first means nothing changed on screen.
    if no_damage == Some(true) && ud.first_frame_logged {
        ud.hub.note_unchanged();
        return;
    }

    let pts_us = if pts_ns > 0 {
        (pts_ns / 1000) as u64
    } else {
        ud.started.elapsed().as_micros() as u64
    };
    let (dma_buf, data) = match payload {
        Payload::Dma(planes) => (Some(planes), Bytes::new()),
        Payload::Memory(b) => (None, b),
        Payload::Unchanged => {
            ud.hub.note_unchanged();
            return;
        }
        Payload::Unusable(why) => {
            tracing::warn!("screencast: dropping unusable buffer: {why}");
            return;
        }
    };
    if !ud.first_frame_logged {
        ud.first_frame_logged = true;
        tracing::info!(
            "screencast: first frame {}x{} stride {} via {}",
            fmt.width,
            fmt.height,
            stride,
            if dma_buf.is_some() { "DMA-BUF" } else { "SHM" }
        );
    }
    ud.hub.push(RawFrame {
        display_id: ud.display_id,
        width: fmt.width,
        height: fmt.height,
        stride,
        format: fmt.kind.pixel_format(),
        pts_us,
        dma_buf,
        data,
    });
}

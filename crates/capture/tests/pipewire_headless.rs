//! End-to-end test of the PipeWire half against a real (headless) PipeWire daemon with a
//! synthetic video source, bypassing the portal with a fake one.
//!
//! Needs a running `pipewire` and a session manager (`wireplumber`) on `$XDG_RUNTIME_DIR`,
//! so it is ignored by default:
//!
//! ```text
//! export XDG_RUNTIME_DIR=$(mktemp -d); chmod 700 $XDG_RUNTIME_DIR
//! pipewire & dbus-run-session -- wireplumber &
//! cargo test -p linux-quest-capture --features pipewire-screencast \
//!     --test pipewire_headless -- --ignored --test-threads=1
//! ```
//!
//! It exercises the SHM path only: DMA-BUF needs a GPU and a compositor.
#![cfg(feature = "pipewire-screencast")]

use std::io::Cursor;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use async_trait::async_trait;
use linux_quest_capture::screencast::portal::{
    PortalStream, ScreenCastPortal, SelectOptions, SessionHandle, StartResponse,
};
use linux_quest_capture::screencast::{ScreenCastConfig, ScreenCastError};
use linux_quest_capture::{CaptureError, DisplayCapture, PipeWireCapture, PixelFormat};
use pipewire as pw;
use pw::context::ContextRc;
use pw::main_loop::MainLoopRc;
use pw::properties::properties;
use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pw::spa::param::video::VideoFormat;
use pw::spa::param::ParamType;
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{Object, Pod, Property, Value};
use pw::spa::utils::{Direction, Fraction, Id, Rectangle, SpaTypes};
use pw::stream::{StreamFlags, StreamRc};
use tokio::sync::oneshot;

fn socket_path() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?).join("pipewire-0");
    p.exists().then_some(p)
}

fn serialize(obj: Object) -> Vec<u8> {
    PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(obj))
        .unwrap()
        .0
        .into_inner()
}

fn video_format(w: u32, h: u32, fps: u32) -> Vec<u8> {
    serialize(Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: vec![
            Property::new(
                FormatProperties::MediaType.as_raw(),
                Value::Id(Id(MediaType::Video.as_raw())),
            ),
            Property::new(
                FormatProperties::MediaSubtype.as_raw(),
                Value::Id(Id(MediaSubtype::Raw.as_raw())),
            ),
            Property::new(
                FormatProperties::VideoFormat.as_raw(),
                Value::Id(Id(VideoFormat::BGRx.as_raw())),
            ),
            Property::new(
                FormatProperties::VideoSize.as_raw(),
                Value::Rectangle(Rectangle {
                    width: w,
                    height: h,
                }),
            ),
            Property::new(
                FormatProperties::VideoFramerate.as_raw(),
                Value::Fraction(Fraction { num: fps, denom: 1 }),
            ),
        ],
    })
}

fn buffers(w: u32, h: u32) -> Vec<u8> {
    use pw::spa::pod::{ChoiceValue, Value};
    use pw::spa::sys as s;
    use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags};
    let size = (w * h * 4) as i32;
    let mask = (1 << s::SPA_DATA_MemFd) | (1 << s::SPA_DATA_MemPtr);
    serialize(Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: ParamType::Buffers.as_raw(),
        properties: vec![
            Property::new(
                s::SPA_PARAM_BUFFERS_buffers,
                Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: 4,
                        min: 2,
                        max: 8,
                    },
                ))),
            ),
            Property::new(s::SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
            Property::new(s::SPA_PARAM_BUFFERS_size, Value::Int(size)),
            Property::new(s::SPA_PARAM_BUFFERS_stride, Value::Int((w * 4) as i32)),
            Property::new(
                s::SPA_PARAM_BUFFERS_dataType,
                Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Flags {
                        default: mask,
                        flags: vec![mask],
                    },
                ))),
            ),
        ],
    })
}

/// A synthetic video source node: BGRx frames whose first pixel carries a counter. Switches
/// to `second` resolution when `switch` is raised.
struct Producer {
    stop: Arc<AtomicBool>,
    switch: Arc<AtomicBool>,
    node_id: u32,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Producer {
    fn start(first: (u32, u32), second: (u32, u32), fps: u32) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let switch = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let (t_stop, t_switch) = (Arc::clone(&stop), Arc::clone(&switch));
        let thread = std::thread::spawn(move || {
            pw::init();
            let ml = MainLoopRc::new(None).unwrap();
            let ctx = ContextRc::new(&ml, None).unwrap();
            let core = ctx.connect_rc(None).unwrap();
            let stream = StreamRc::new(
                core,
                "lq-test-source",
                properties! {
                    *pw::keys::MEDIA_CLASS => "Video/Source",
                    *pw::keys::NODE_NAME => "lq-test-source",
                    *pw::keys::MEDIA_TYPE => "Video",
                    *pw::keys::MEDIA_CATEGORY => "Capture",
                },
            )
            .unwrap();
            let size = Arc::new((AtomicU32::new(first.0), AtomicU32::new(first.1)));
            let counter = Arc::new(AtomicU32::new(0));
            let mut node_tx = Some(tx);
            let (sz_pc, sz_proc, cnt) = (size.clone(), size.clone(), counter.clone());
            let _l = stream
                .add_local_listener_with_user_data(())
                .state_changed(move |stream, _, _, new| {
                    if matches!(new, pw::stream::StreamState::Paused) {
                        if let Some(tx) = node_tx.take() {
                            let _ = tx.send(stream.node_id());
                        }
                    }
                })
                .param_changed(move |stream, _, id, param| {
                    if id != ParamType::Format.as_raw() || param.is_none() {
                        return;
                    }
                    let (w, h) = (
                        sz_pc.0.load(Ordering::SeqCst),
                        sz_pc.1.load(Ordering::SeqCst),
                    );
                    let b = buffers(w, h);
                    let p = Pod::from_bytes(&b).unwrap();
                    stream.update_params(&mut [p]).unwrap();
                })
                .process(move |stream, _| {
                    let Some(mut buf) = stream.dequeue_buffer() else {
                        return;
                    };
                    let (w, h) = (
                        sz_proc.0.load(Ordering::SeqCst) as usize,
                        sz_proc.1.load(Ordering::SeqCst) as usize,
                    );
                    let n = cnt.fetch_add(1, Ordering::SeqCst);
                    let d = &mut buf.datas_mut()[0];
                    if let Some(px) = d.data() {
                        let len = (w * h * 4).min(px.len());
                        px[..len].fill(0x40);
                        if len >= 4 {
                            px[..4].copy_from_slice(&n.to_le_bytes());
                        }
                    }
                    let c = d.chunk_mut();
                    *c.offset_mut() = 0;
                    *c.stride_mut() = (w * 4) as i32;
                    *c.size_mut() = (w * h * 4) as u32;
                })
                .register()
                .unwrap();
            let f = video_format(first.0, first.1, fps);
            stream
                .connect(
                    Direction::Output,
                    None,
                    StreamFlags::MAP_BUFFERS,
                    &mut [Pod::from_bytes(&f).unwrap()],
                )
                .unwrap();

            let lp = ml.loop_();
            // Poll the control flags from the loop thread.
            let switched = std::cell::Cell::new(false);
            let control_timer = lp.add_timer({
                let (ml, stream) = (ml.clone(), stream.clone());
                move |_| {
                    if t_stop.load(Ordering::SeqCst) {
                        ml.quit();
                    }
                    if t_switch.load(Ordering::SeqCst) && !switched.replace(true) {
                        size.0.store(second.0, Ordering::SeqCst);
                        size.1.store(second.1, Ordering::SeqCst);
                        let f = video_format(second.0, second.1, fps);
                        let _ = stream.update_params(&mut [Pod::from_bytes(&f).unwrap()]);
                    }
                }
            });
            control_timer
                .update_timer(
                    Some(Duration::from_millis(20)),
                    Some(Duration::from_millis(20)),
                )
                .into_result()
                .unwrap();
            ml.run();
            let _ = stream.disconnect();
        });
        let node_id = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("producer node");
        Self {
            stop,
            switch,
            node_id,
            thread: Some(thread),
        }
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Producer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Portal stand-in: hands out a fresh connection to the local PipeWire daemon and the
/// producer's node id, like `OpenPipeWireRemote` + `Start` would.
struct LocalPipeWirePortal {
    socket: PathBuf,
    node_id: u32,
    close_tx: Vec<oneshot::Sender<()>>,
}

#[async_trait]
impl ScreenCastPortal for LocalPipeWirePortal {
    async fn version(&mut self) -> Result<u32, ScreenCastError> {
        Ok(5)
    }
    async fn available_cursor_modes(&mut self) -> Result<u32, ScreenCastError> {
        Ok(2)
    }
    async fn create_session(&mut self) -> Result<SessionHandle, ScreenCastError> {
        Ok(SessionHandle("/local".into()))
    }
    async fn select_sources(
        &mut self,
        _: &SessionHandle,
        _: &SelectOptions,
    ) -> Result<(), ScreenCastError> {
        Ok(())
    }
    async fn start(&mut self, _: &SessionHandle) -> Result<StartResponse, ScreenCastError> {
        Ok(StartResponse {
            streams: vec![PortalStream {
                node_id: self.node_id,
                size: None,
                position: None,
                source_type: Some(1),
            }],
            restore_token: None,
        })
    }
    async fn open_pipewire_remote(
        &mut self,
        _: &SessionHandle,
    ) -> Result<OwnedFd, ScreenCastError> {
        UnixStream::connect(&self.socket)
            .map(OwnedFd::from)
            .map_err(|e| ScreenCastError::Io(e.to_string()))
    }
    async fn watch_closed(
        &mut self,
        _: &SessionHandle,
    ) -> Result<oneshot::Receiver<()>, ScreenCastError> {
        let (tx, rx) = oneshot::channel();
        self.close_tx.push(tx);
        Ok(rx)
    }
    async fn close_session(&mut self, _: &SessionHandle) {}
}

fn config() -> ScreenCastConfig {
    ScreenCastConfig {
        persist_token: false,
        negotiation_timeout: Duration::from_secs(15),
        ..Default::default()
    }
}

#[tokio::test]
#[ignore = "needs a headless pipewire + wireplumber session; see the file header"]
async fn streams_synthetic_frames_resizes_restarts_and_reports_node_removal() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "linux_quest_capture=debug".into()),
        )
        .with_test_writer()
        .try_init();
    let Some(socket) = socket_path() else {
        panic!("no $XDG_RUNTIME_DIR/pipewire-0: start pipewire and wireplumber first");
    };
    let mut producer = Producer::start((640, 360), (800, 450), 30);
    let portal = |node_id| LocalPipeWirePortal {
        socket: socket.clone(),
        node_id,
        close_tx: vec![],
    };

    let mut cap = PipeWireCapture::with_config(0, 1920, 1080, 30, config());
    cap.init_with_portal(portal(producer.node_id))
        .await
        .expect("init");
    let info = cap.capture_info().expect("info after init");
    assert_eq!((info.width, info.height), (640, 360));
    assert_eq!(info.fps, 30);
    assert_eq!(info.format, PixelFormat::Bgra8);
    assert!(!info.dmabuf);

    let mut last_pts = 0;
    for _ in 0..10 {
        let f = tokio::time::timeout(Duration::from_secs(5), cap.capture_frame())
            .await
            .expect("frame in time")
            .expect("frame");
        assert_eq!((f.width, f.height), (640, 360));
        assert!(f.stride >= 640 * 4);
        assert!(f.dma_buf.is_none());
        assert!(f.data.len() >= 640 * 360 * 4);
        assert!(f.pts_us >= last_pts);
        last_pts = f.pts_us;
    }
    assert!(cap.stats().frames_delivered >= 10);

    // Resolution change: the format is re-negotiated and frames follow.
    let generation = cap.info_generation();
    producer.switch.store(true, Ordering::SeqCst);
    let mut resized = false;
    for _ in 0..200 {
        let f = tokio::time::timeout(Duration::from_secs(5), cap.capture_frame())
            .await
            .expect("frame in time")
            .expect("frame");
        if (f.width, f.height) == (800, 450) {
            resized = true;
            break;
        }
    }
    assert!(resized, "frames at the new size never arrived");
    assert!(cap.info_generation() > generation);
    assert_eq!(cap.capture_info().unwrap().width, 800);

    // Clean stop, then a restart on the same object.
    cap.stop().await;
    assert!(!cap.is_running());
    assert!(cap.capture_info().is_none());
    cap.init_with_portal(portal(producer.node_id))
        .await
        .expect("restart");
    assert!(cap.capture_frame().await.is_ok());

    // The source disappears: the consumer is told, not left hanging.
    producer.shutdown();
    let end = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Err(e) = cap.capture_frame().await {
                return e;
            }
        }
    })
    .await
    .expect("end of stream reported");
    assert!(matches!(end, CaptureError::ScreenCast(_)), "{end:?}");
    cap.stop().await;
}

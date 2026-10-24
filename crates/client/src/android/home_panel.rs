//! Android Home panel: the same transport and hardware decoder, without an XR session.
//! The manifest puts this activity in its own process; android-activity owns ndk-context
//! in the immersive process, while this process initializes it from the Java activity.

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use jni::{
    objects::{GlobalRef, JClass, JObject, JString},
    sys::{jboolean, jfloat, jint, jstring},
    JNIEnv,
};
use linux_quest_protocol::{
    packet::PacketType, video::VideoCodec, ElementState, InputEvent, KeyModifiers, MouseButton,
};
use parking_lot::Mutex;
use serde::Serialize;

use crate::{
    config::{ClientVideoCodec, TransportMode},
    decoder::{create_decoder, HardwareVideoDecoder},
    input_sender::{
        input_sender_config, CombinedInputSource, InputSenderControl, InputSenderThread,
        SharedInputEvents,
    },
    receiver::{
        loss_recovery::{Admit, LossRecovery},
        ClientReceiver,
    },
};

static APP_CONTEXT: Mutex<Option<GlobalRef>> = Mutex::new(None);
static WORKER: Mutex<Option<Worker>> = Mutex::new(None);
static STATUS: Mutex<PanelStatus> = Mutex::new(PanelStatus {
    message: String::new(),
    width: 1920,
    height: 1080,
    frames: 0,
    input_ready: false,
});

#[derive(Serialize)]
struct PanelStatus {
    message: String,
    width: u32,
    height: u32,
    frames: u64,
    input_ready: bool,
}

#[derive(Default)]
struct PanelInput {
    events: SharedInputEvents,
    control: Mutex<Option<InputSenderControl>>,
    keys: Mutex<BTreeMap<u32, KeyModifiers>>,
}

impl PanelInput {
    fn push(&self, event: InputEvent) {
        if let InputEvent::KeyboardKey {
            keycode,
            state,
            modifiers,
        } = &event
        {
            let mut keys = self.keys.lock();
            if *state == ElementState::Pressed {
                keys.insert(*keycode, *modifiers);
            } else {
                keys.remove(keycode);
            }
        }
        self.events.push(event);
        if let Some(control) = &*self.control.lock() {
            control.notify();
        }
    }

    fn release(&self) {
        for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
            self.events.push(InputEvent::MouseButton {
                button,
                state: ElementState::Released,
            });
        }
        for (keycode, modifiers) in std::mem::take(&mut *self.keys.lock()) {
            self.events.push(InputEvent::KeyboardKey {
                keycode,
                modifiers,
                state: ElementState::Released,
            });
        }
        if let Some(control) = &*self.control.lock() {
            control.notify();
        }
    }
}

struct Worker {
    running: Arc<AtomicBool>,
    input: Arc<PanelInput>,
    thread: JoinHandle<()>,
}

// ANativeWindow_fromSurface acquires a reference. Keep it until the decoder is destroyed.
struct SurfaceWindow(usize);
impl Drop for SurfaceWindow {
    fn drop(&mut self) {
        unsafe {
            ndk_sys::ANativeWindow_release(self.0 as *mut _);
        }
    }
}

fn stop() {
    let worker = WORKER.lock().take();
    if let Some(worker) = worker {
        worker.input.release();
        worker.running.store(false, Ordering::Release);
        let _ = worker.thread.join();
    }
    STATUS.lock().input_ready = false;
}

fn run(
    window: SurfaceWindow,
    running: Arc<AtomicBool>,
    input: Arc<PanelInput>,
    source: CombinedInputSource,
) {
    let (mut config, explicit) = super::launch_config();
    if crate::discovery::autoconnect::should_autodiscover(
        explicit,
        super::usb_tunnel_present(&config),
    ) {
        STATUS.lock().message = "Looking for your desktop on Wi-Fi…".into();
        config = super::autodiscover_config(config);
    }
    if !running.load(Ordering::Acquire) {
        return;
    }
    let flags = source.requester();
    let (tx, rx) = std::sync::mpsc::sync_channel(super::PACKET_CHANNEL_DEPTH);
    let (sender, tunnel, receiver_thread) = match config.transport_mode {
        TransportMode::Udp => {
            let sender = input_sender_config(&config).and_then(|cfg| {
                InputSenderThread::spawn(cfg, Box::new(source))
                    .map_err(|error| tracing::warn!(?error, "Home input sender failed"))
                    .ok()
            });
            let control = sender.as_ref().map(|s| s.control().clone());
            let receiver =
                super::transport::spawn_udp_receiver(&config, running.clone(), control, tx);
            (sender, None, Some(receiver))
        }
        TransportMode::AdbReverseTcp => {
            let input = crate::usb_tunnel::TunnelInput {
                source: Arc::new(std::sync::Mutex::new(source)),
                keyframes: flags.clone(),
            };
            let tunnel = crate::usb_tunnel::UsbTunnel::spawn(
                crate::usb_tunnel::UsbTunnelConfig::new(config.host_addr),
                Some(input),
                move |packet| {
                    let _ = tx.try_send(packet);
                },
            )
            .map_err(|error| tracing::warn!(?error, "Home USB tunnel failed"))
            .ok();
            (None, tunnel, None)
        }
    };
    let control = sender.as_ref().map(|s| s.control().clone());
    *input.control.lock() = control.clone();
    let mut receiver = ClientReceiver::for_stream(&config, 0);
    receiver.set_focused_stream(Some(0));
    let mut loss = LossRecovery::new();
    let mut decoder: Option<Box<dyn HardwareVideoDecoder>> = None;
    let mut format = None;
    let mut last_video = Instant::now();
    let mut stalled = true;
    let audio_decoder = crate::audio::OpusAudioDecoder::new(48_000, 2).ok();
    let mut audio = audio_decoder.and_then(|decoder| {
        let mut pipeline = crate::audio::ClientAudioPipeline::new(
            Default::default(),
            Default::default(),
            decoder,
            crate::audio::AAudioSink::new(),
        );
        pipeline.start(Default::default()).ok().map(|_| pipeline)
    });
    STATUS.lock().message = format!("Connecting to {}…", config.host_addr);

    while running.load(Ordering::Acquire) {
        let now = Instant::now();
        // Bound each drain so a busy sender cannot starve decoding or activity shutdown.
        for _ in 0..512 {
            let packet = match rx.try_recv() {
                Ok(p) => p,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    STATUS.lock().message = "Video transport stopped. Tap Reconnect.".into();
                    running.store(false, Ordering::Release);
                    break;
                }
            };
            if packet.header.packet_type == PacketType::AudioFrame {
                if let Some(audio) = &mut audio {
                    let _ = audio.ingest_packet(packet, now);
                }
            } else {
                receiver.ingest_packet(packet, now);
            }
        }
        match receiver.pop_ready_frames(now) {
            Ok(frames) => {
                for frame in frames {
                    if loss.admit(frame.meta.frame_id, frame.meta.is_keyframe) == Admit::Skip {
                        continue;
                    }
                    let codec = match frame.meta.codec {
                        VideoCodec::Av1 => ClientVideoCodec::Av1,
                        VideoCodec::Hevc => ClientVideoCodec::Hevc,
                        _ => {
                            STATUS.lock().message = "Host must stream AV1 or HEVC.".into();
                            continue;
                        }
                    };
                    let next_format = (frame.meta.width, frame.meta.height, codec);
                    if format != Some(next_format) {
                        if !frame.meta.is_keyframe {
                            loss.decoder_refused();
                            continue;
                        }
                        config.codec = codec;
                        decoder = None;
                        match create_decoder(
                            &config,
                            next_format.0,
                            next_format.1,
                            Some(window.0 as *mut _),
                        ) {
                            Ok(new) => {
                                decoder = Some(new);
                                format = Some(next_format);
                                let mut status = STATUS.lock();
                                status.width = next_format.0;
                                status.height = next_format.1;
                            }
                            Err(error) => {
                                tracing::warn!(?error, "Cannot create Home desktop decoder");
                                STATUS.lock().message =
                                    "Unable to decode the desktop stream. Tap Reconnect.".into();
                                loss.decoder_refused();
                                continue;
                            }
                        }
                    }
                    if let Some(decoder) = &mut decoder {
                        if let Err(error) = decoder.queue_input_buffer(
                            &frame.bitstream,
                            frame.meta.pts_us,
                            frame.meta.is_keyframe,
                        ) {
                            tracing::warn!(?error, "Home decoder refused frame");
                            loss.decoder_refused();
                        }
                    }
                }
            }
            Err(error) => {
                tracing::warn!(?error, "Home frame reassembly failed");
                loss.decoder_refused();
            }
        }
        if loss.take_request(now) {
            flags.raise(0);
            if let Some(control) = &control {
                control.notify();
            }
        }
        if let Some(decoder) = &mut decoder {
            let mut newest = None;
            while let Ok(Some(frame)) = decoder.dequeue_output_buffer(0) {
                if let Some(old) = newest.replace(frame) {
                    let _ = decoder.release_output_buffer(old.buffer_index, false);
                }
            }
            if let Some(frame) = newest {
                if decoder
                    .release_output_buffer(frame.buffer_index, true)
                    .is_ok()
                {
                    last_video = now;
                    stalled = false;
                    if let Some(audio) = &mut audio {
                        audio.update_video_presentation(frame.pts_us, now);
                    }
                    let mut status = STATUS.lock();
                    status.frames += 1;
                    status.input_ready = control.is_some() || tunnel.is_some();
                    status.message = "Drag to scroll · Hold to drag the mouse".into();
                }
            }
        }
        if !stalled && now.duration_since(last_video) > Duration::from_secs(3) {
            stalled = true;
            input.release();
            loss.decoder_refused();
            let mut status = STATUS.lock();
            status.input_ready = false;
            status.message = "Waiting for the desktop stream…".into();
        }
        if let Some(audio) = &mut audio {
            let _ = audio.step(now);
        }
        if let Some(feedback) = receiver.poll_feedback(now) {
            input.push(InputEvent::NetworkFeedback(feedback));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    input.release();
    // Give the sender a final poll to deliver button/key releases before closing its socket.
    std::thread::sleep(Duration::from_millis(20));
    running.store(false, Ordering::Release);
    if let Some(receiver_thread) = receiver_thread {
        let _ = receiver_thread.join();
    }
    if let Some(tunnel) = tunnel {
        tunnel.stop();
    }
    if let Some(sender) = sender {
        sender.stop();
    }
    if let Some(audio) = &mut audio {
        let _ = audio.stop();
    }
    drop(decoder);
    drop(window);
    *input.control.lock() = None;
    STATUS.lock().input_ready = false;
}

#[no_mangle]
pub extern "system" fn Java_com_linux_quest_client_HomePanelActivity_nativeStart(
    mut env: JNIEnv,
    _: JClass,
    activity: JObject,
    surface: JObject,
) -> jboolean {
    stop();
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();
    let result = (|| -> jni::errors::Result<()> {
        let mut context = APP_CONTEXT.lock();
        if context.is_none() {
            let application = env
                .call_method(
                    &activity,
                    "getApplicationContext",
                    "()Landroid/content/Context;",
                    &[],
                )?
                .l()?;
            let global = env.new_global_ref(application)?;
            let vm = env.get_java_vm()?;
            unsafe {
                ndk_context::initialize_android_context(
                    vm.get_java_vm_pointer().cast(),
                    global.as_obj().as_raw().cast(),
                );
            }
            *context = Some(global);
        }
        Ok(())
    })();
    if let Err(error) = result {
        tracing::error!(?error, "Cannot initialize Home panel JNI");
        STATUS.lock().message = "Cannot initialize the desktop panel. Tap Reconnect.".into();
        let _ = env.exception_clear();
        return 0;
    }
    let window = unsafe {
        ndk_sys::ANativeWindow_fromSurface(
            env.get_native_interface().cast(),
            surface.as_raw().cast(),
        )
    };
    if window.is_null() {
        STATUS.lock().message = "Desktop surface is not ready. Tap Reconnect.".into();
        return 0;
    }
    let window = SurfaceWindow(window as usize);
    let source = CombinedInputSource::new();
    let input = Arc::new(PanelInput {
        events: source.pointer_events(),
        ..Default::default()
    });
    let running = Arc::new(AtomicBool::new(true));
    *STATUS.lock() = PanelStatus {
        message: "Connecting…".into(),
        width: 1920,
        height: 1080,
        frames: 0,
        input_ready: false,
    };
    let (thread_running, thread_input) = (running.clone(), input.clone());
    match std::thread::Builder::new()
        .name("dq-home-stream".into())
        .spawn(move || run(window, thread_running, thread_input, source))
    {
        Ok(thread) => {
            *WORKER.lock() = Some(Worker {
                running,
                input,
                thread,
            });
            1
        }
        Err(error) => {
            STATUS.lock().message = error.to_string();
            0
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_com_linux_quest_client_HomePanelActivity_nativeStop(
    _: JNIEnv,
    _: JClass,
) {
    stop();
}

#[no_mangle]
pub extern "system" fn Java_com_linux_quest_client_HomePanelActivity_nativeStatus(
    env: JNIEnv,
    _: JClass,
) -> jstring {
    let json = serde_json::to_string(&*STATUS.lock()).unwrap_or_default();
    env.new_string(json)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

fn send(event: InputEvent) {
    let ready = STATUS.lock().input_ready;
    if ready {
        if let Some(worker) = &*WORKER.lock() {
            worker.input.push(event);
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_com_linux_quest_client_HomePanelActivity_nativePointer(
    _: JNIEnv,
    _: JClass,
    x: jfloat,
    y: jfloat,
) {
    if x.is_finite() && y.is_finite() {
        send(InputEvent::MouseMoveAbsolute {
            x: (x.clamp(0.0, 1.0) * 65535.0).round() as u32,
            y: (y.clamp(0.0, 1.0) * 65535.0).round() as u32,
            display_id: 0,
        });
    }
}

#[no_mangle]
pub extern "system" fn Java_com_linux_quest_client_HomePanelActivity_nativeButton(
    _: JNIEnv,
    _: JClass,
    button: jint,
    down: jboolean,
) {
    let button = match button {
        0 => MouseButton::Left,
        1 => MouseButton::Right,
        2 => MouseButton::Middle,
        _ => return,
    };
    send(InputEvent::MouseButton {
        button,
        state: if down != 0 {
            ElementState::Pressed
        } else {
            ElementState::Released
        },
    });
}

#[no_mangle]
pub extern "system" fn Java_com_linux_quest_client_HomePanelActivity_nativeScroll(
    _: JNIEnv,
    _: JClass,
    x: jfloat,
    y: jfloat,
) {
    if x.is_finite() && y.is_finite() {
        send(InputEvent::MouseScrollHiRes {
            delta_x: x.clamp(-20.0, 20.0),
            delta_y: y.clamp(-20.0, 20.0),
        });
    }
}

#[no_mangle]
pub extern "system" fn Java_com_linux_quest_client_HomePanelActivity_nativeKey(
    _: JNIEnv,
    _: JClass,
    key: jint,
    meta: jint,
    down: jboolean,
) -> jboolean {
    if !STATUS.lock().input_ready {
        return 0;
    }
    if let Some(keycode) = crate::input::android_keycode_to_evdev(key) {
        send(InputEvent::KeyboardKey {
            keycode,
            modifiers: crate::input::android_meta_to_modifiers(meta),
            state: if down != 0 {
                ElementState::Pressed
            } else {
                ElementState::Released
            },
        });
        return 1;
    }
    0
}

#[no_mangle]
pub extern "system" fn Java_com_linux_quest_client_HomePanelActivity_nativeReleaseInput(
    _: JNIEnv,
    _: JClass,
) {
    if let Some(worker) = &*WORKER.lock() {
        worker.input.release();
    }
}

#[no_mangle]
pub extern "system" fn Java_com_linux_quest_client_HomePanelActivity_nativeText(
    mut env: JNIEnv,
    _: JClass,
    text: JString,
) {
    if let Ok(value) = env.get_string(&text) {
        let text: String = value.into();
        if !text.is_empty() {
            send(InputEvent::TextCommit { text });
        }
    }
}

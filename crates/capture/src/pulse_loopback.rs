//! Linux Loopback backend: PulseAudio monitor source (ADR-0001 route).
//!
//! Records from the monitor source of a sink (PipeWire is reached through its
//! PulseAudio compatibility layer). The record stream *requests* f32le /
//! 48 kHz / stereo and the Pulse server converts server-side, so the consumer
//! thread only does stereo→mono mixdown through the shared `FeedCtx`.
//!
//! libpulse-binding callbacks cannot borrow the objects they belong to, so
//! this backend uses the same poll-thread shape as the Windows one: a
//! consumer thread peeks under the threaded-mainloop lock every 10 ms.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use libpulse_binding::callbacks::ListResult;
use libpulse_binding::context::{Context, FlagSet, State as ContextState};
use libpulse_binding::mainloop::threaded::Mainloop;
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::{PeekResult, State as StreamState, Stream};

use crate::{CaptureError, CaptureStream, DeviceInfo, FeedCtx, SourceKind, StreamHolder};

/// Shown when no PulseAudio/PipeWire service answers.
pub(crate) const NOTE: &str = "Loopback 不可用:未检测到 PulseAudio/PipeWire 服务";

const APP_NAME: &str = "music-spec";
const SETUP_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// f32le stereo frame size in bytes.
const FRAME_BYTES: usize = 8;
/// Requested spec: the Pulse server converts monitor audio to this for us.
const REQUEST_SPEC: Spec = Spec {
    format: Format::F32le,
    rate: crate::TARGET_SAMPLE_RATE,
    channels: 2,
};

pub(crate) fn list_loopback_devices() -> Vec<DeviceInfo> {
    with_pulse_connection(|ctx, ml| {
        let devices: Arc<Mutex<Vec<DeviceInfo>>> = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(false));
        let _op = ctx.introspect().get_source_info_list({
            let devices = devices.clone();
            let done = done.clone();
            move |res| match res {
                ListResult::Item(info) => {
                    if info.monitor_of_sink.is_some() {
                        if let (Some(name), Some(desc)) =
                            (info.name.clone(), info.description.clone())
                        {
                            devices.lock().unwrap().push(DeviceInfo {
                                kind: SourceKind::Loopback,
                                id: name.to_string(),
                                name: desc.to_string(),
                            });
                        }
                    }
                }
                _ => done.store(true, Ordering::Relaxed),
            }
        });
        wait_unlocked(ml, &done);
        devices.lock().unwrap().clone()
    })
    .unwrap_or_default()
}

/// Default Loopback Source: the monitor of the default sink, else the first
/// monitor found.
pub(crate) fn default_device() -> Option<DeviceInfo> {
    with_pulse_connection(|ctx, ml| {
        let devices = collect_monitors(ctx, ml);
        let sink_name = Arc::new(Mutex::new(None::<String>));
        let done = Arc::new(AtomicBool::new(false));
        let _op = ctx.introspect().get_server_info({
            let sink_name = sink_name.clone();
            let done = done.clone();
            move |info| {
                *sink_name.lock().unwrap() = info.default_sink_name.as_ref().map(|s| s.to_string());
                done.store(true, Ordering::Relaxed);
            }
        });
        wait_unlocked(ml, &done);
        let default_monitor = sink_name
            .lock()
            .unwrap()
            .as_ref()
            .map(|sink| format!("{sink}.monitor"));
        devices
            .iter()
            .find(|d| default_monitor.as_ref().is_some_and(|m| *m == d.id))
            .cloned()
            .or_else(|| devices.first().cloned())
    })
    .ok()
    .flatten()
}

fn collect_monitors(ctx: &mut Context, ml: &mut Mainloop) -> Vec<DeviceInfo> {
    let devices: Arc<Mutex<Vec<DeviceInfo>>> = Arc::new(Mutex::new(Vec::new()));
    let done = Arc::new(AtomicBool::new(false));
    let _op = ctx.introspect().get_source_info_list({
        let devices = devices.clone();
        let done = done.clone();
        move |res| match res {
            ListResult::Item(info) => {
                if info.monitor_of_sink.is_some() {
                    if let (Some(name), Some(desc)) = (info.name.clone(), info.description.clone())
                    {
                        devices.lock().unwrap().push(DeviceInfo {
                            kind: SourceKind::Loopback,
                            id: name.to_string(),
                            name: desc.to_string(),
                        });
                    }
                }
            }
            _ => done.store(true, Ordering::Relaxed),
        }
    });
    wait_unlocked(ml, &done);
    devices.lock().unwrap().clone()
}

pub(crate) fn open_loopback(source: &DeviceInfo) -> Result<CaptureStream, CaptureError> {
    let (prod, cons) = crate::split_ring();
    let dead = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));

    let mut mainloop = Mainloop::new()
        .ok_or_else(|| CaptureError::OpenFailed("无法创建 PulseAudio 主循环".into()))?;
    mainloop
        .start()
        .map_err(|e| CaptureError::OpenFailed(format!("主循环启动失败:{}", e.0)))?;
    mainloop.lock();

    // Setup with the PA lock held; early exits below remember to stop the loop.
    let setup = (|| -> Result<(Context, Stream), String> {
        let mut context = Context::new(&mainloop, APP_NAME).ok_or("无法创建上下文")?;
        context
            .connect(None, FlagSet::NOFAIL, None)
            .map_err(|e| format!("连接 PulseAudio 失败:{}", e.0))?;
        if !wait_for_context_ready(&mut mainloop, &context) {
            return Err("连接 PulseAudio 超时".into());
        }

        let mut stream = Stream::new(&mut context, "music-spec-loopback", &REQUEST_SPEC, None)
            .ok_or("无法创建记录流")?;
        stream
            .connect_record(Some(&source.id), None, Default::default())
            .map_err(|e| format!("连接记录流失败:{}", e.0))?;
        if !wait_for_stream_ready(&mut mainloop, &stream) {
            return Err("记录流就绪超时".into());
        }
        Ok((context, stream))
    })();

    let (context, stream) = match setup {
        Ok(parts) => parts,
        Err(msg) => {
            mainloop.unlock();
            mainloop.stop();
            return Err(CaptureError::OpenFailed(msg));
        }
    };

    // Move everything into shared handles; release the PA lock via the Arc.
    let ml_arc = Arc::new(Mutex::new(mainloop));
    let stream_arc = Arc::new(Mutex::new(stream));
    let ctx_arc = Arc::new(Mutex::new(context));
    ml_arc.lock().unwrap().unlock();

    let (tx, rx) = mpsc::channel::<Result<(), String>>();
    let stop_thread = stop.clone();
    let dead_thread = dead.clone();
    // libpulse-binding objects hold Rc and are !Send; access is only legal
    // under the PA lock, which the consumer thread takes. The thread joins
    // before anything is dropped or the mainloop is stopped.
    let consumer = SendHandle((ml_arc.clone(), stream_arc.clone(), ctx_arc.clone()));

    let thread = thread::Builder::new()
        .name("music-spec-pulse-loopback".into())
        .spawn(move || {
            let (ml_consumer, stream_consumer, ctx_consumer) = consumer.into_inner();
            let mut feed = FeedCtx::new(prod, crate::TARGET_SAMPLE_RATE, 2);
            let result = pulse_consumer_loop(
                &ml_consumer,
                &stream_consumer,
                &ctx_consumer,
                &mut feed,
                &stop_thread,
                &dead_thread,
            );
            let _ = tx.send(result);
        })
        .map_err(|e| CaptureError::OpenFailed(format!("无法启动采集线程:{e}")))?;

    match rx.recv_timeout(SETUP_TIMEOUT) {
        Ok(Ok(())) => Ok(CaptureStream::new(
            StreamHolder::Pulse(PulseSession {
                thread: Some(thread),
                stop,
                mainloop: ml_arc,
                stream: stream_arc,
                context: ctx_arc,
            }),
            cons,
            dead,
        )),
        Ok(Err(msg)) => {
            stop.store(true, Ordering::Relaxed);
            let _ = thread.join();
            Err(CaptureError::OpenFailed(msg))
        }
        Err(_) => {
            stop.store(true, Ordering::Relaxed);
            let _ = thread.join();
            Err(CaptureError::OpenFailed("PulseAudio 初始化超时".into()))
        }
    }
}

/// The consumer thread's body: peeks monitor frames under the PA lock.
/// On return the stream and context are disconnected.
fn pulse_consumer_loop(
    ml_arc: &Arc<Mutex<Mainloop>>,
    stream_arc: &Arc<Mutex<Stream>>,
    ctx_arc: &Arc<Mutex<Context>>,
    feed: &mut FeedCtx,
    stop: &AtomicBool,
    dead: &AtomicBool,
) -> Result<(), String> {
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(POLL_INTERVAL);
        let healthy = {
            let mut ml = ml_arc.lock().unwrap();
            let mut stream = stream_arc.lock().unwrap();
            ml.lock();
            let healthy = drain_stream(&mut stream, feed);
            ml.unlock();
            drop(stream);
            drop(ml);
            healthy
        };
        if !healthy {
            dead.store(true, Ordering::Relaxed);
            break;
        }
    }

    // Teardown from this (non-mainloop) thread: legal under the PA lock.
    let mut ml = ml_arc.lock().unwrap();
    ml.lock();
    let mut stream = stream_arc.lock().unwrap();
    let _ = stream.disconnect();
    drop(stream);
    let mut context = ctx_arc.lock().unwrap();
    let _ = context.disconnect();
    drop(context);
    ml.unlock();
    Ok(())
}

/// Drains everything currently readable; `false` on stream errors.
fn drain_stream(stream: &mut Stream, feed: &mut FeedCtx) -> bool {
    let available = match stream.readable_size() {
        Some(n) => n,
        None => return false,
    };
    let mut remaining = available;
    while remaining >= FRAME_BYTES {
        match stream.peek() {
            Ok(PeekResult::Data(bytes)) => {
                // Whole interleaved f32le frames: [L f32][R f32].
                for chunk in bytes.chunks_exact(FRAME_BYTES) {
                    let l = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                    let r = f32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
                    feed.push_sample(l);
                    feed.push_sample(r);
                }
                if stream.discard().is_err() {
                    return false;
                }
            }
            Ok(PeekResult::Hole(_)) => {
                if stream.discard().is_err() {
                    return false;
                }
            }
            Ok(PeekResult::Empty) => break,
            Err(_) => return false,
        }
        remaining = match stream.readable_size() {
            Some(n) => n,
            None => return false,
        };
    }
    true
}

// ---------------------------------------------------------------------------
// Shared connection scaffolding
// ---------------------------------------------------------------------------

/// Runs `op` against a fresh connected context; PA lock is held on entry to
/// the op (use [`wait_unlocked`] to release it while polling flags).
fn with_pulse_connection<T>(
    op: impl FnOnce(&mut Context, &mut Mainloop) -> T,
) -> Result<T, String> {
    let mut mainloop = Mainloop::new().ok_or("无法创建 PulseAudio 主循环")?;
    mainloop
        .start()
        .map_err(|e| format!("主循环启动失败:{}", e.0))?;
    mainloop.lock();
    let mut context = Context::new(&mainloop, APP_NAME).ok_or("无法创建上下文")?;
    if let Err(e) = context.connect(None, FlagSet::NOFAIL, None) {
        mainloop.unlock();
        mainloop.stop();
        return Err(format!("连接 PulseAudio 失败:{}", e.0));
    }
    if !wait_for_context_ready(&mut mainloop, &context) {
        mainloop.unlock();
        mainloop.stop();
        return Err("连接 PulseAudio 超时".into());
    }
    let out = op(&mut context, &mut mainloop);
    let _ = context.disconnect();
    mainloop.unlock();
    mainloop.stop();
    Ok(out)
}

/// Waits for the context to reach Ready, releasing the PA lock while sleeping.
fn wait_for_context_ready(mainloop: &mut Mainloop, context: &Context) -> bool {
    let deadline = Instant::now() + SETUP_TIMEOUT;
    loop {
        match context.get_state() {
            ContextState::Ready => return true,
            ContextState::Failed | ContextState::Terminated => return false,
            _ => {}
        }
        if Instant::now() > deadline {
            return false;
        }
        mainloop.unlock();
        thread::sleep(POLL_INTERVAL);
        mainloop.lock();
    }
}

/// Waits for the record stream to reach Ready, same lock dance.
fn wait_for_stream_ready(mainloop: &mut Mainloop, stream: &Stream) -> bool {
    let deadline = Instant::now() + SETUP_TIMEOUT;
    loop {
        match stream.get_state() {
            StreamState::Ready => return true,
            StreamState::Failed | StreamState::Terminated => return false,
            _ => {}
        }
        if Instant::now() > deadline {
            return false;
        }
        mainloop.unlock();
        thread::sleep(POLL_INTERVAL);
        mainloop.lock();
    }
}

/// Releases the PA lock while polling `done`, then reacquires it.
fn wait_unlocked(mainloop: &mut Mainloop, done: &Arc<AtomicBool>) {
    mainloop.unlock();
    let deadline = Instant::now() + SETUP_TIMEOUT;
    while !done.load(Ordering::Relaxed) && Instant::now() < deadline {
        thread::sleep(POLL_INTERVAL);
    }
    mainloop.lock();
}

/// Asserts `Send` for the PA handles moved into the consumer thread: they
/// are only used under the PA lock on that thread, which joins before any
/// handle is dropped.
struct SendHandle<T>(T);
unsafe impl<T> Send for SendHandle<T> {}
impl<T> SendHandle<T> {
    /// Methods take `self`, so the closure captures the whole (Send) wrapper
    /// instead of destructuring into the non-Send fields inside.
    fn into_inner(self) -> T {
        self.0
    }
}

/// Keeps the PulseAudio machinery alive; dropping it stops the capture.
pub(crate) struct PulseSession {
    thread: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    mainloop: Arc<Mutex<Mainloop>>,
    stream: Arc<Mutex<Stream>>,
    context: Arc<Mutex<Context>>,
}

impl Drop for PulseSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // Must be called off the event-loop thread: we are on the app thread.
        if let Ok(mut ml) = self.mainloop.lock() {
            ml.stop();
        }
    }
}

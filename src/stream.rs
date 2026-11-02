//! Pull the Tapo RTSP stream with GStreamer and hand JPEG frames to the window.
//!
//! The C200 publishes H.264 on `/stream1` (1080p) and `/stream2` (about 360p).
//! TCP is forced because Wi-Fi drops the UDP variant. Frames are capped at
//! 15 fps so the preview stays light. A dead pipeline reconnects on its own.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use serde::Serialize;
use tauri::ipc::{Channel, InvokeResponseBody};

use crate::config::{redact, rtsp_url, CameraConfig};
use crate::motion::{MotionRegion, MotionWatch};

pub struct Hub {
    latest: Mutex<Option<Vec<u8>>>,
    subs: Mutex<Vec<Channel<InvokeResponseBody>>>,
    running: AtomicBool,
    meter: Mutex<Meter>,
    motion: MotionWatch,
}

struct Meter {
    frames: u64,
    bytes: u64,
    window_frames: u64,
    window_bytes: u64,
    window_start: Instant,
    fps: f64,
    kbps: f64,
    width: u32,
    height: u32,
    last_frame: Option<Instant>,
    error: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub running: bool,
    pub receiving: bool,
    pub frames: u64,
    pub fps: f64,
    pub kbps: f64,
    pub width: u32,
    pub height: u32,
    pub error: Option<String>,
    pub motion_enabled: bool,
    pub motion_borders: bool,
    pub motion_active: bool,
    pub motion_regions: Vec<MotionRegion>,
}

impl Hub {
    pub fn new() -> Self {
        Self {
            latest: Mutex::new(None),
            subs: Mutex::new(Vec::new()),
            running: AtomicBool::new(false),
            meter: Mutex::new(Meter {
                frames: 0,
                bytes: 0,
                window_frames: 0,
                window_bytes: 0,
                window_start: Instant::now(),
                fps: 0.0,
                kbps: 0.0,
                width: 0,
                height: 0,
                last_frame: None,
                error: None,
            }),
            motion: MotionWatch::new(),
        }
    }

    pub fn configure_motion(&self, notify: bool, borders: bool, sensitivity: u8, name: &str) {
        self.motion.configure(notify, borders, sensitivity, name);
    }

    pub fn suppress_motion(&self, duration: Duration) {
        self.motion.suppress_for(duration);
    }

    pub fn subscribe(&self, channel: Channel<InvokeResponseBody>) {
        if let Some(frame) = self.latest.lock().expect("frame lock").clone() {
            let _ = channel.send(InvokeResponseBody::Raw(frame));
        }
        self.subs.lock().expect("sub lock").push(channel);
    }

    pub fn latest(&self) -> Option<Vec<u8>> {
        self.latest.lock().expect("frame lock").clone()
    }

    pub fn stats(&self) -> Stats {
        let meter = self.meter.lock().expect("meter lock");
        let receiving = meter
            .last_frame
            .map(|t| t.elapsed() < Duration::from_secs(2))
            .unwrap_or(false);
        Stats {
            running: self.running.load(Ordering::Relaxed),
            receiving,
            frames: meter.frames,
            fps: if receiving { meter.fps } else { 0.0 },
            kbps: if receiving { meter.kbps } else { 0.0 },
            width: meter.width,
            height: meter.height,
            error: meter.error.clone(),
            motion_enabled: self.motion.notifies(),
            motion_borders: self.motion.borders(),
            motion_active: self.motion.active(),
            motion_regions: self.motion.regions(),
        }
    }

    fn publish(&self, jpeg: Vec<u8>) {
        if let Some((w, h)) = jpeg_size(&jpeg) {
            let mut meter = self.meter.lock().expect("meter lock");
            meter.width = w;
            meter.height = h;
        }
        {
            let mut meter = self.meter.lock().expect("meter lock");
            meter.frames += 1;
            meter.bytes += jpeg.len() as u64;
            meter.last_frame = Some(Instant::now());
            meter.error = None;
            let dt = meter.window_start.elapsed().as_secs_f64();
            if dt >= 1.0 {
                meter.fps = (meter.frames - meter.window_frames) as f64 / dt;
                meter.kbps = ((meter.bytes - meter.window_bytes) as f64 * 8.0 / 1000.0) / dt;
                meter.window_frames = meter.frames;
                meter.window_bytes = meter.bytes;
                meter.window_start = Instant::now();
            }
        }
        *self.latest.lock().expect("frame lock") = Some(jpeg.clone());
        self.motion.observe(&jpeg);
        self.subs
            .lock()
            .expect("sub lock")
            .retain(|channel| channel.send(InvokeResponseBody::Raw(jpeg.clone())).is_ok());
    }

    fn set_error(&self, message: String) {
        self.meter.lock().expect("meter lock").error = Some(message);
    }

    fn clear_error(&self) {
        self.meter.lock().expect("meter lock").error = None;
    }

    fn frame_count(&self) -> u64 {
        self.meter.lock().expect("meter lock").frames
    }
}

struct FrameSlot {
    frame: Mutex<Option<Vec<u8>>>,
    cv: Condvar,
}

pub struct StreamEngine {
    stop_flag: Arc<AtomicBool>,
    pipeline: Arc<Mutex<Option<gst::Element>>>,
    slot: Arc<FrameSlot>,
    thread: Option<JoinHandle<()>>,
    publisher: Option<JoinHandle<()>>,
}

impl StreamEngine {
    pub fn new() -> Self {
        Self {
            stop_flag: Arc::new(AtomicBool::new(false)),
            pipeline: Arc::new(Mutex::new(None)),
            slot: Arc::new(FrameSlot {
                frame: Mutex::new(None),
                cv: Condvar::new(),
            }),
            thread: None,
            publisher: None,
        }
    }

    pub fn start(&mut self, cfg: CameraConfig, hub: Arc<Hub>) {
        self.stop();
        let stop = Arc::new(AtomicBool::new(false));
        self.stop_flag = stop.clone();
        let pipeline = self.pipeline.clone();
        let slot = self.slot.clone();
        hub.running.store(true, Ordering::Relaxed);
        hub.clear_error();

        let publish_stop = stop.clone();
        let publish_slot = slot.clone();
        let publish_hub = hub.clone();
        self.publisher = Some(
            thread::Builder::new()
                .name("camera-frames".into())
                .spawn(move || publish_loop(publish_stop, publish_slot, publish_hub))
                .expect("spawn frame thread"),
        );

        self.thread = Some(
            thread::Builder::new()
                .name("camera-rtsp".into())
                .spawn(move || stream_loop(cfg, stop, pipeline, slot, hub))
                .expect("spawn rtsp thread"),
        );
    }

    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(pipeline) = self.pipeline.lock().expect("pipeline lock").take() {
            let _ = pipeline.set_state(gst::State::Null);
        }
        self.slot.cv.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.publisher.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for StreamEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

fn publish_loop(stop: Arc<AtomicBool>, slot: Arc<FrameSlot>, hub: Arc<Hub>) {
    loop {
        let frame = {
            let mut guard = slot.frame.lock().expect("slot lock");
            while guard.is_none() && !stop.load(Ordering::SeqCst) {
                guard = slot.cv.wait(guard).expect("slot wait");
            }
            guard.take()
        };
        match frame {
            Some(frame) => hub.publish(frame),
            None if stop.load(Ordering::SeqCst) => break,
            None => {}
        }
    }
}

fn stream_loop(
    cfg: CameraConfig,
    stop: Arc<AtomicBool>,
    pipeline_slot: Arc<Mutex<Option<gst::Element>>>,
    slot: Arc<FrameSlot>,
    hub: Arc<Hub>,
) {
    if let Err(err) = gst::init() {
        hub.set_error(format!("GStreamer failed to start: {err}"));
        hub.running.store(false, Ordering::Relaxed);
        return;
    }

    let mut backoff_ms = 800u64;
    while !stop.load(Ordering::SeqCst) {
        let before = hub.frame_count();
        let mut failure = match run_once(&cfg, &stop, &pipeline_slot, &slot, Codec::H264) {
            RunEnd::Stopped => break,
            RunEnd::Failed(err) => err,
        };
        if hub.frame_count() == before && codec_mismatch(&failure) && !stop.load(Ordering::SeqCst) {
            match run_once(&cfg, &stop, &pipeline_slot, &slot, Codec::H265) {
                RunEnd::Stopped => break,
                RunEnd::Failed(next) => {
                    if hub.frame_count() > before {
                        failure = next;
                    }
                }
            }
        }
        if stop.load(Ordering::SeqCst) {
            break;
        }
        log::warn!("{}", redact(&failure, &cfg));
        hub.set_error(humanize(&failure));
        sleep_interruptible(&stop, backoff_ms);
        backoff_ms = (backoff_ms * 2).min(8_000);
    }
    hub.clear_error();
    hub.running.store(false, Ordering::Relaxed);
    stop.store(true, Ordering::SeqCst);
    slot.cv.notify_all();
}

#[derive(Clone, Copy)]
enum Codec {
    H264,
    H265,
}

enum RunEnd {
    Stopped,
    Failed(String),
}

fn run_once(
    cfg: &CameraConfig,
    stop: &AtomicBool,
    pipeline_slot: &Mutex<Option<gst::Element>>,
    slot: &Arc<FrameSlot>,
    codec: Codec,
) -> RunEnd {
    let url = rtsp_url(cfg);
    let chain = match codec {
        Codec::H264 => {
            "application/x-rtp,media=video,encoding-name=H264 ! rtph264depay wait-for-keyframe=true ! h264parse ! avdec_h264"
        }
        Codec::H265 => {
            "application/x-rtp,media=video,encoding-name=H265 ! rtph265depay ! h265parse ! avdec_h265"
        }
    };
    // timeout is microseconds. TCP survives the C200's Wi-Fi better than UDP.
    let description = format!(
        "rtspsrc location=\"{url}\" protocols=tcp latency=150 drop-on-latency=true timeout=5000000 name=src \
         src. ! {chain} ! videoconvert ! videorate drop-only=true ! video/x-raw,framerate=15/1 ! \
         jpegenc quality=70 ! appsink name=sink max-buffers=1 drop=true sync=false"
    );
    log::info!(
        "opening rtsp://{}:{}/{} ({})",
        cfg.host,
        cfg.rtsp_port,
        crate::config::stream_path(&cfg.stream),
        match codec {
            Codec::H264 => "h264",
            Codec::H265 => "h265",
        }
    );

    let element = match gst::parse::launch(&description) {
        Ok(element) => element,
        Err(err) => return RunEnd::Failed(err.to_string()),
    };
    let pipeline = match element.clone().downcast::<gst::Pipeline>() {
        Ok(pipeline) => pipeline,
        Err(_) => return RunEnd::Failed("GStreamer did not build a pipeline.".into()),
    };
    let appsink = match pipeline.by_name("sink") {
        Some(sink) => sink,
        None => return RunEnd::Failed("The video pipeline has no output.".into()),
    };
    let appsink = match appsink.downcast::<gstreamer_app::AppSink>() {
        Ok(sink) => sink,
        Err(_) => return RunEnd::Failed("The video pipeline output is the wrong type.".into()),
    };

    let slot_for_cb = Arc::clone(slot);
    appsink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                {
                    let mut guard = slot_for_cb.frame.lock().expect("slot lock");
                    *guard = Some(map.as_slice().to_vec());
                }
                slot_for_cb.cv.notify_one();
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    if stop.load(Ordering::SeqCst) {
        return RunEnd::Stopped;
    }
    *pipeline_slot.lock().expect("pipeline lock") = Some(element.clone());
    if let Err(err) = element.set_state(gst::State::Playing) {
        let _ = element.set_state(gst::State::Null);
        *pipeline_slot.lock().expect("pipeline lock") = None;
        return RunEnd::Failed(err.to_string());
    }

    let bus = pipeline.bus().expect("pipeline bus");
    let mut end = RunEnd::Stopped;
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let Some(message) = bus.timed_pop(gst::ClockTime::from_mseconds(200)) else {
            continue;
        };
        match message.view() {
            gst::MessageView::Error(err) => {
                let debug = err.debug().unwrap_or_default();
                end = RunEnd::Failed(format!("{} {debug}", err.error()));
                break;
            }
            gst::MessageView::Eos(_) => {
                end = RunEnd::Failed("The camera closed the stream.".into());
                break;
            }
            _ => {}
        }
    }

    let _ = element.set_state(gst::State::Null);
    *pipeline_slot.lock().expect("pipeline lock") = None;
    if stop.load(Ordering::SeqCst) {
        RunEnd::Stopped
    } else {
        end
    }
}

fn codec_mismatch(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("not-negotiated")
        || lower.contains("could not link")
        || lower.contains("no element")
        || (lower.contains("internal data stream error") && !lower.contains("unauthorized"))
}

fn humanize(message: &str) -> String {
    let lower = message.to_lowercase();
    if lower.contains("unauthorized") || lower.contains("401") || lower.contains("not authorized") {
        "The camera refused the username or password. Use the Camera Account from the Tapo app, not your TP-Link login.".into()
    } else if lower.contains("not found") || lower.contains("404") {
        "The camera did not recognize the stream path. Tapo uses /stream1 for HD and /stream2 for the lighter stream.".into()
    } else if lower.contains("failed to connect")
        || lower.contains("could not connect")
        || lower.contains("could not open resource")
        || lower.contains("connection refused")
        || lower.contains("timed out")
        || lower.contains("no route")
        || lower.contains("network is unreachable")
    {
        "Could not reach the camera. Check the IP address and that this computer is on the same network.".into()
    } else {
        let trimmed = message.trim();
        if trimmed.chars().count() > 220 {
            format!("{}…", trimmed.chars().take(220).collect::<String>())
        } else {
            trimmed.to_string()
        }
    }
}

fn sleep_interruptible(stop: &AtomicBool, total_ms: u64) {
    let mut left = total_ms;
    while left > 0 && !stop.load(Ordering::SeqCst) {
        let step = left.min(200);
        thread::sleep(Duration::from_millis(step));
        left -= step;
    }
}

fn jpeg_size(buf: &[u8]) -> Option<(u32, u32)> {
    if buf.len() < 4 || buf[0] != 0xFF || buf[1] != 0xD8 {
        return None;
    }
    let mut i = 2;
    while i + 9 < buf.len() {
        if buf[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = buf[i + 1];
        if marker == 0xC0 || marker == 0xC1 || marker == 0xC2 {
            let height = u16::from_be_bytes([buf[i + 5], buf[i + 6]]) as u32;
            let width = u16::from_be_bytes([buf[i + 7], buf[i + 8]]) as u32;
            return Some((width, height));
        }
        if marker == 0xD8 || marker == 0xD9 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            i += 2;
            continue;
        }
        if i + 3 >= buf.len() {
            break;
        }
        let len = u16::from_be_bytes([buf[i + 2], buf[i + 3]]) as usize;
        if len < 2 {
            break;
        }
        i += 2 + len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_jpeg_dimensions() {
        let mut buf = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08];
        buf.extend_from_slice(&360u16.to_be_bytes());
        buf.extend_from_slice(&640u16.to_be_bytes());
        buf.extend_from_slice(&[0u8; 8]);
        assert_eq!(jpeg_size(&buf), Some((640, 360)));
    }

    #[test]
    fn explains_a_refused_connection() {
        let text = humanize(
            "Could not open resource for reading and writing. Failed to connect. (Generic error)",
        );
        assert!(text.contains("Could not reach the camera"));
    }
}

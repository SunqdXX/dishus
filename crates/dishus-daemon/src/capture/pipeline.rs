//! GStreamer capture → H.264 pipeline (runs inside the capture worker) with encoder
//! auto-detection (NVENC → VA-API → x264 → openh264) and live bitrate / fps / keyframe control.

use std::{
    os::fd::{IntoRawFd, OwnedFd},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoder {
    Nvenc,
    Va,
    X264,
    OpenH264,
}

impl Encoder {
    pub fn name(self) -> &'static str {
        match self {
            Encoder::Nvenc => "nvenc",
            Encoder::Va => "vaapi",
            Encoder::X264 => "x264",
            Encoder::OpenH264 => "openh264",
        }
    }

    fn factories(self) -> &'static [&'static str] {
        match self {
            Encoder::Nvenc => &["nvh264enc"],
            Encoder::Va => &["vah264enc", "vapostproc"],
            Encoder::X264 => &["x264enc"],
            Encoder::OpenH264 => &["openh264enc"],
        }
    }

    fn present(self) -> bool {
        self.factories().iter().all(|f| gst::ElementFactory::find(f).is_some())
    }
}

pub fn init() -> Result<()> {
    gst::init().context("initialising GStreamer")?;
    Ok(())
}

/// Encoders to try, in order, given the config preference.
pub fn candidates(pref: &str) -> Vec<Encoder> {
    [Encoder::Nvenc, Encoder::Va, Encoder::X264, Encoder::OpenH264]
        .into_iter()
        .filter(|e| pref == "auto" || e.name() == pref)
        .filter(|e| e.present())
        .collect()
}

/// Where pixels come from.
pub enum Source {
    PipeWire { fd: OwnedFd, node: u32 },
    X11,
    Test,
}

#[derive(Debug, Clone, Copy)]
pub struct Params {
    pub fps: u8,
    pub bitrate_kbps: u32,
    pub max_height: u16,
    pub src_height: Option<u32>,
    pub show_cursor: bool,
}

pub struct Frame {
    pub pts_us: u64,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

fn source_desc(src: &Source, p: &Params) -> Result<String> {
    Ok(match src {
        Source::PipeWire { fd, node } => {
            // pipewiresrc takes ownership of the fd it is given, so hand it a dup.
            let dup = fd.try_clone().context("dup pipewire fd")?;
            // Force system-memory buffers: NVENC/VA/x264 all accept them and it avoids
            // driver-specific DMA-BUF import paths (a TODO for zero-copy, see docs/PLAN.md).
            format!(
                "pipewiresrc fd={} path={node} do-timestamp=true keepalive-time=500 always-copy=false ! video/x-raw",
                dup.into_raw_fd()
            )
        }
        Source::X11 => format!(
            "ximagesrc use-damage=false show-pointer={} do-timestamp=true ! video/x-raw,framerate=60/1",
            p.show_cursor
        ),
        Source::Test => "videotestsrc is-live=true pattern=ball ! video/x-raw,width=1920,height=1080,framerate=60/1".into(),
    })
}

fn target_height(p: &Params) -> Option<u32> {
    match (p.max_height, p.src_height) {
        (0, _) => None,
        (h, Some(sh)) if sh <= u32::from(h) => None,
        (h, _) => Some(u32::from(h) & !1),
    }
}

fn encoder_desc(enc: Encoder, p: &Params) -> String {
    let kbps = p.bitrate_kbps;
    let scale = target_height(p).map(|h| format!(",height={h}")).unwrap_or_default();
    match enc {
        // Without scaling NVENC takes BGRx/RGBx straight from system memory and converts on the
        // GPU; that avoids loading NVRTC (cudaconvertscale), which costs ~100 MB of RSS.
        Encoder::Nvenc if scale.is_empty() => format!(
            "videoconvert ! nvh264enc name=enc preset=p1 tune=ultra-low-latency rc-mode=cbr bitrate={kbps} \
             gop-size=-1 bframes=0 zerolatency=true rc-lookahead=0 repeat-sequence-header=true"
        ),
        Encoder::Nvenc => format!(
            "cudaupload ! cudascale ! video/x-raw(memory:CUDAMemory){scale} ! \
             nvh264enc name=enc preset=p1 tune=ultra-low-latency rc-mode=cbr bitrate={kbps} \
             gop-size=-1 bframes=0 zerolatency=true rc-lookahead=0 repeat-sequence-header=true"
        ),
        Encoder::Va => format!(
            "vapostproc ! video/x-raw(memory:VAMemory),format=NV12{scale} ! \
             vah264enc name=enc rate-control=cbr bitrate={kbps} key-int-max=1024 b-frames=0 target-usage=7"
        ),
        Encoder::X264 => format!(
            "videoconvert ! videoscale ! video/x-raw,format=I420{scale} ! \
             x264enc name=enc tune=zerolatency speed-preset=ultrafast bitrate={kbps} key-int-max=1024 \
             bframes=0 byte-stream=true sliced-threads=true"
        ),
        Encoder::OpenH264 => format!(
            "videoconvert ! videoscale ! video/x-raw,format=I420{scale} ! \
             openh264enc name=enc bitrate={} rate-control=bitrate complexity=low gop-size=1024",
            kbps * 1000
        ),
    }
}

pub struct Control {
    enc: gst::Element,
    rate: gst::Element,
    sink: gst::Element,
    kind: Encoder,
}

impl Control {
    pub fn force_keyframe(&self) {
        let ev = gstreamer_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build();
        self.sink.send_event(ev);
    }
    pub fn set_bitrate(&self, kbps: u32) {
        let kbps = kbps.clamp(100, 200_000);
        match self.kind {
            Encoder::OpenH264 => self.enc.set_property("bitrate", kbps * 1000),
            _ => self.enc.set_property("bitrate", kbps),
        }
    }
    pub fn set_fps(&self, fps: u8) {
        self.rate.set_property("max-rate", i32::from(fps.clamp(1, 120)));
    }
}

pub struct Running {
    pub control: Control,
    pub width: u32,
    pub height: u32,
    pub encoder: Encoder,
    pipeline: gst::Pipeline,
    stop: Arc<AtomicBool>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

type OnFrame = Arc<dyn Fn(Frame) + Send + Sync>;
type OnFail = Arc<dyn Fn(String) + Send + Sync>;

fn start_one(src: &Source, enc: Encoder, p: &Params, on_frame: OnFrame, on_fail: OnFail) -> Result<Running> {
    let desc = format!(
        "{} ! queue max-size-buffers=2 max-size-time=0 max-size-bytes=0 leaky=downstream ! \
         videorate name=rate drop-only=true max-rate={} ! {} ! \
         h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au ! \
         appsink name=sink sync=false max-buffers=2 drop=false",
        source_desc(src, p)?,
        p.fps.clamp(1, 120),
        encoder_desc(enc, p),
    );
    tracing::debug!("pipeline: {desc}");
    let pipeline = gst::parse::launch(&desc)
        .with_context(|| format!("building {} pipeline", enc.name()))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("not a pipeline"))?;
    let get = |n: &str| pipeline.by_name(n).ok_or_else(|| anyhow!("element {n} missing"));
    let sink = get("sink")?;
    let control = Control { enc: get("enc")?, rate: get("rate")?, sink: sink.clone(), kind: enc };
    let appsink = sink.downcast::<gst_app::AppSink>().map_err(|_| anyhow!("not an appsink"))?;

    let (first_tx, first_rx) = std::sync::mpsc::sync_channel::<(u32, u32)>(1);
    let first_tx = Mutex::new(Some(first_tx));
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                if let Some(tx0) = first_tx.lock().ok().and_then(|mut g| g.take())
                    && let Some(st) = sample.caps().and_then(|c| c.structure(0))
                {
                    let w = st.get::<i32>("width").unwrap_or(0).max(0) as u32;
                    let h = st.get::<i32>("height").unwrap_or(0).max(0) as u32;
                    let _ = tx0.try_send((w, h));
                }
                let buf = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buf.map_readable().map_err(|_| gst::FlowError::Error)?;
                // Blocking write to the daemon: back-pressure ends at the leaky queue above,
                // which drops raw frames, so the encoded reference chain stays intact.
                on_frame(Frame {
                    pts_us: buf.pts().map(|t| t.useconds()).unwrap_or(0),
                    keyframe: !buf.flags().contains(gst::BufferFlags::DELTA_UNIT),
                    data: map.as_slice().to_vec(),
                });
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    pipeline.set_state(gst::State::Playing).with_context(|| format!("starting {} pipeline", enc.name()))?;
    let stop = Arc::new(AtomicBool::new(false));
    let bus = pipeline.bus().context("no bus")?;
    let deadline = std::time::Instant::now() + Duration::from_secs(6);
    let (width, height) = loop {
        if let Ok(dims) = first_rx.try_recv() {
            break dims;
        }
        if let Some(msg) = bus.timed_pop_filtered(gst::ClockTime::from_mseconds(50), &[gst::MessageType::Error])
            && let gst::MessageView::Error(e) = msg.view()
        {
            let _ = pipeline.set_state(gst::State::Null);
            bail!("{} pipeline error: {} ({:?})", enc.name(), e.error(), e.debug());
        }
        if std::time::Instant::now() > deadline {
            let _ = pipeline.set_state(gst::State::Null);
            bail!("{} pipeline produced no frames within 6 s", enc.name());
        }
    };
    let running = Running { control, width, height, encoder: enc, pipeline: pipeline.clone(), stop: stop.clone() };

    std::thread::Builder::new().name("gst-bus".into()).spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            let Some(msg) = bus.timed_pop_filtered(
                gst::ClockTime::from_mseconds(250),
                &[gst::MessageType::Error, gst::MessageType::Eos],
            ) else {
                continue;
            };
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let why = match msg.view() {
                gst::MessageView::Error(e) => format!("pipeline error: {} ({:?})", e.error(), e.debug()),
                _ => "pipeline ended (EOS)".into(),
            };
            let _ = pipeline.set_state(gst::State::Null);
            on_fail(why);
            break;
        }
    })?;
    Ok(running)
}

/// Try encoders in order until one runs.
pub fn start(src: &Source, encoders: &[Encoder], p: &Params, on_frame: OnFrame, on_fail: OnFail) -> Result<Running> {
    if encoders.is_empty() {
        bail!("no H.264 encoder found (gst-plugins-bad provides NVENC/VA, gst-plugins-ugly provides x264)");
    }
    let mut errors = Vec::new();
    for &e in encoders {
        match start_one(src, e, p, on_frame.clone(), on_fail.clone()) {
            Ok(r) => return Ok(r),
            Err(err) => {
                tracing::warn!("encoder {} failed: {err:#}", e.name());
                errors.push(format!("{}: {err:#}", e.name()));
            }
        }
    }
    bail!("all encoders failed: {}", errors.join("; "))
}

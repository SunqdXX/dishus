//! GStreamer capture → H.264 pipeline (runs inside the capture worker) with encoder
//! auto-detection (NVENC → VA-API → x264 → openh264) and live bitrate / fps / keyframe control.

use std::{
    os::fd::{IntoRawFd, OwnedFd},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
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
    /// VA-API "low power" entry point: the only H.264 encoder on recent Intel iGPUs (Ice Lake
    /// and newer drop the shader-based one on some drivers).
    VaLowPower,
    X264,
    OpenH264,
}

impl Encoder {
    pub fn name(self) -> &'static str {
        match self {
            Encoder::Nvenc => "nvenc",
            Encoder::Va => "vaapi",
            Encoder::VaLowPower => "vaapi-lp",
            Encoder::X264 => "x264",
            Encoder::OpenH264 => "openh264",
        }
    }

    fn factories(self) -> &'static [&'static str] {
        match self {
            Encoder::Nvenc => &["nvh264enc"],
            Encoder::Va => &["vah264enc", "vapostproc"],
            Encoder::VaLowPower => &["vah264lpenc", "vapostproc"],
            Encoder::X264 => &["x264enc"],
            Encoder::OpenH264 => &["openh264enc"],
        }
    }

    fn present(self) -> bool {
        self.missing().is_empty()
    }

    /// GStreamer elements this encoder needs that aren't installed (or that the GPU driver
    /// doesn't offer: `vah264enc` only registers when the VA driver can encode H.264).
    pub fn missing(self) -> Vec<&'static str> {
        self.factories()
            .iter()
            .copied()
            .filter(|f| gst::ElementFactory::find(f).is_none())
            .collect()
    }

    /// Every encoder, in the order `auto` tries them.
    pub const ALL: [Encoder; 5] = [
        Encoder::Nvenc,
        Encoder::Va,
        Encoder::VaLowPower,
        Encoder::X264,
        Encoder::OpenH264,
    ];

    /// Runs on the CPU (works on any PC, costs CPU time).
    pub fn is_software(self) -> bool {
        matches!(self, Encoder::X264 | Encoder::OpenH264)
    }
}

pub fn init() -> Result<()> {
    gst::init().context("initialising GStreamer")?;
    Ok(())
}

/// Encoders to try, in order, given the config preference.
pub fn candidates(pref: &str) -> Vec<Encoder> {
    Encoder::ALL
        .into_iter()
        // "vaapi" means either VA-API entry point.
        .filter(|e| {
            pref == "auto" || e.name() == pref || (pref == "vaapi" && *e == Encoder::VaLowPower)
        })
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
    /// Source size if known before the pipeline runs (portal, test source; not X11).
    pub src_size: Option<(u32, u32)>,
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

/// Caps suffix for downscaling to `max_height`, or "" for native size.
///
/// Always pins square pixels: with only `height` fixed, scalers keep the width and change the
/// pixel aspect ratio instead (1920x1080 -> 1920x720 PAR 2/3), which the phone shows squashed.
/// When the source size is known the width is given explicitly and rounded to even, since
/// 4:2:0 H.264 needs even dimensions and some Android decoders reject odd ones.
fn scale_caps(p: &Params) -> String {
    let h = match (p.max_height, p.src_size) {
        (0, _) => return String::new(),
        (h, Some((_, sh))) if sh <= u32::from(h) => return String::new(),
        (h, _) => u32::from(h) & !1,
    };
    match p.src_size {
        Some((sw, sh)) if sh > 0 => {
            let w = ((u64::from(sw) * u64::from(h) + u64::from(sh) / 2) / u64::from(sh)) as u32;
            let w = (w + 1) & !1;
            format!(",width={w},height={h},pixel-aspect-ratio=1/1")
        }
        _ => format!(",height={h},pixel-aspect-ratio=1/1"),
    }
}

fn encoder_desc(enc: Encoder, p: &Params) -> String {
    let kbps = p.bitrate_kbps;
    let scale = scale_caps(p);
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
        Encoder::Va | Encoder::VaLowPower => format!(
            "vapostproc ! video/x-raw(memory:VAMemory),format=NV12{scale} ! \
             {} name=enc rate-control=cbr bitrate={kbps} key-int-max=1024 b-frames=0 target-usage=7",
            if enc == Encoder::Va {
                "vah264enc"
            } else {
                "vah264lpenc"
            }
        ),
        // Software paths: scale first (fewer pixels to convert), and use every core for both;
        // the GStreamer default of one thread can't convert 1440p at 30 fps on slower CPUs.
        Encoder::X264 => format!(
            "videoscale n-threads=0 ! videoconvert n-threads=0 ! video/x-raw,format=I420{scale} ! \
             x264enc name=enc tune=zerolatency speed-preset=ultrafast bitrate={kbps} key-int-max=1024 \
             bframes=0 byte-stream=true sliced-threads=true"
        ),
        Encoder::OpenH264 => format!(
            "videoscale n-threads=0 ! videoconvert n-threads=0 ! video/x-raw,format=I420{scale} ! \
             openh264enc name=enc bitrate={} rate-control=bitrate complexity=low gop-size=1024",
            kbps * 1000
        ),
    }
}

/// Frame-rate cap, applied to raw frames before the encoder.
///
/// `videorate max-rate` can't do this: PipeWire screen capture announces a *variable*
/// framerate (`framerate=0/1`), and videorate then passes every frame through. On a 180 Hz
/// monitor that meant ~90-180 frames/s reached the sender against a 30 fps target, which
/// filled the in-flight window, forced drops and a storm of keyframe requests.
///
/// Admission is by presentation time against a running "next due" clock, with a quarter
/// interval of slack so source jitter (a 60 Hz frame landing at 33.0 ms for a 33.3 ms slot)
/// is not dropped. The clock advances by exactly one interval per admitted frame, so the
/// average rate converges on the target instead of drifting above it.
pub struct Throttle {
    fps: AtomicU32,
    next_due_us: Mutex<Option<u64>>,
}

impl Throttle {
    fn new(fps: u8) -> Self {
        Self {
            fps: AtomicU32::new(u32::from(fps.clamp(1, 120))),
            next_due_us: Mutex::new(None),
        }
    }
    fn set_fps(&self, fps: u8) {
        self.fps
            .store(u32::from(fps.clamp(1, 120)), Ordering::Relaxed);
    }
    /// Whether the frame presented at `pts_us` should be encoded.
    fn admit(&self, pts_us: u64) -> bool {
        let interval = 1_000_000 / u64::from(self.fps.load(Ordering::Relaxed).max(1));
        let slack = interval / 4;
        let Ok(mut due) = self.next_due_us.lock() else {
            return true;
        };
        match *due {
            // Clock went backwards (before the last admitted frame): start over from here.
            Some(d) if pts_us + 2 * interval < d => {
                *due = Some(pts_us + interval);
                true
            }
            Some(d) if pts_us + slack < d => false,
            Some(d) => {
                // Behind by more than one slot (stall, fps raised): resync on this frame
                // rather than letting a burst through to "catch up".
                let base = if pts_us > d + interval { pts_us } else { d };
                *due = Some(base + interval);
                true
            }
            None => {
                *due = Some(pts_us + interval);
                true
            }
        }
    }
}

pub struct Control {
    enc: gst::Element,
    throttle: Arc<Throttle>,
    sink: gst::Element,
    kind: Encoder,
}

impl Control {
    pub fn force_keyframe(&self) {
        let ev = gstreamer_video::UpstreamForceKeyUnitEvent::builder()
            .all_headers(true)
            .build();
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
        self.throttle.set_fps(fps);
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

fn start_one(
    src: &Source,
    enc: Encoder,
    p: &Params,
    on_frame: OnFrame,
    on_fail: OnFail,
) -> Result<Running> {
    let desc = format!(
        "{} ! queue max-size-buffers=2 max-size-time=0 max-size-bytes=0 leaky=downstream ! \
         identity name=rate silent=true ! {} ! \
         h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au ! \
         appsink name=sink sync=false max-buffers=2 drop=false",
        source_desc(src, p)?,
        encoder_desc(enc, p),
    );
    tracing::debug!("pipeline: {desc}");
    let pipeline = gst::parse::launch(&desc)
        .with_context(|| format!("building {} pipeline", enc.name()))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("not a pipeline"))?;
    let get = |n: &str| {
        pipeline
            .by_name(n)
            .ok_or_else(|| anyhow!("element {n} missing"))
    };
    let sink = get("sink")?;
    let throttle = Arc::new(Throttle::new(p.fps));
    {
        let t = throttle.clone();
        get("rate")?
            .static_pad("src")
            .context("rate has no src pad")?
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                let pts = info.buffer().and_then(|b| b.pts());
                match pts {
                    Some(pts) if !t.admit(pts.useconds()) => gst::PadProbeReturn::Drop,
                    _ => gst::PadProbeReturn::Ok,
                }
            });
    }
    let control = Control {
        enc: get("enc")?,
        throttle,
        sink: sink.clone(),
        kind: enc,
    };
    let appsink = sink
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow!("not an appsink"))?;

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

    pipeline
        .set_state(gst::State::Playing)
        .with_context(|| format!("starting {} pipeline", enc.name()))?;
    let stop = Arc::new(AtomicBool::new(false));
    let bus = pipeline.bus().context("no bus")?;
    let deadline = std::time::Instant::now() + Duration::from_secs(6);
    let (width, height) = loop {
        if let Ok(dims) = first_rx.try_recv() {
            break dims;
        }
        if let Some(msg) = bus.timed_pop_filtered(
            gst::ClockTime::from_mseconds(50),
            &[gst::MessageType::Error],
        ) && let gst::MessageView::Error(e) = msg.view()
        {
            let _ = pipeline.set_state(gst::State::Null);
            bail!(
                "{} pipeline error: {} ({:?})",
                enc.name(),
                e.error(),
                e.debug()
            );
        }
        if std::time::Instant::now() > deadline {
            let _ = pipeline.set_state(gst::State::Null);
            bail!("{} pipeline produced no frames within 6 s", enc.name());
        }
    };
    let running = Running {
        control,
        width,
        height,
        encoder: enc,
        pipeline: pipeline.clone(),
        stop: stop.clone(),
    };

    std::thread::Builder::new()
        .name("gst-bus".into())
        .spawn(move || {
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
                    gst::MessageView::Error(e) => {
                        format!("pipeline error: {} ({:?})", e.error(), e.debug())
                    }
                    _ => "pipeline ended (EOS)".into(),
                };
                let _ = pipeline.set_state(gst::State::Null);
                on_fail(why);
                break;
            }
        })?;
    Ok(running)
}

/// Really encodes a moving test pattern with `enc` for about a second and returns the
/// frames per second it delivered. Used by `dishus-daemon --doctor`: a plugin being installed
/// proves nothing (a VA driver without H.264 encode, NVENC on nouveau), only frames do.
pub fn probe(enc: Encoder, max_height: u16, fps: u8) -> Result<f64> {
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = count.clone();
    let failed = Arc::new(Mutex::new(None::<String>));
    let f = failed.clone();
    let p = Params {
        fps,
        bitrate_kbps: 4000,
        max_height,
        src_size: Some((1920, 1080)),
        show_cursor: false,
    };
    let run = start_one(
        &Source::Test,
        enc,
        &p,
        Arc::new(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        }),
        Arc::new(move |why| {
            if let Ok(mut g) = f.lock() {
                g.get_or_insert(why);
            }
        }),
    )?;
    let start = std::time::Instant::now();
    let before = count.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(1200));
    let n = count.load(Ordering::SeqCst) - before;
    let secs = start.elapsed().as_secs_f64();
    drop(run);
    if let Some(why) = failed.lock().ok().and_then(|mut g| g.take()) {
        bail!("{why}");
    }
    if n == 0 {
        bail!("started, then produced no frames");
    }
    Ok(n as f64 / secs)
}

/// Try encoders in order until one runs.
pub fn start(
    src: &Source,
    encoders: &[Encoder],
    p: &Params,
    on_frame: OnFrame,
    on_fail: OnFail,
) -> Result<Running> {
    if encoders.is_empty() {
        bail!(
            "no H.264 encoder found (gst-plugins-bad provides NVENC/VA, gst-plugins-ugly provides x264)"
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    fn p(max_height: u16, src_size: Option<(u32, u32)>) -> Params {
        Params {
            fps: 30,
            bitrate_kbps: 1000,
            max_height,
            src_size,
            show_cursor: true,
        }
    }

    /// Feeds `n` frames at `hz` (with optional +/- jitter alternating) and counts admissions.
    fn admitted(t: &Throttle, hz: f64, secs: f64, jitter_us: i64) -> usize {
        let n = (hz * secs) as usize;
        (0..n)
            .filter(|&i| {
                let j = if i % 2 == 0 { jitter_us } else { -jitter_us };
                let pts = (1_000_000.0 * i as f64 / hz) as i64 + j + 1_000_000;
                t.admit(pts as u64)
            })
            .count()
    }

    #[test]
    fn throttle_caps_fast_sources_to_target() {
        // The real case: a 180 Hz monitor, variable framerate, 30 fps wanted.
        let n = admitted(&Throttle::new(30), 180.0, 10.0, 0);
        assert!(
            (299..=301).contains(&n),
            "180 Hz -> 30 fps admitted {n} in 10 s"
        );
        let n = admitted(&Throttle::new(60), 144.0, 10.0, 0);
        assert!(
            (595..=601).contains(&n),
            "144 Hz -> 60 fps admitted {n} in 10 s"
        );
    }

    #[test]
    fn throttle_passes_matching_and_slower_sources_despite_jitter() {
        // 60 Hz with 60 fps wanted, frames +/- 2 ms off their slots: keep them all.
        assert_eq!(admitted(&Throttle::new(60), 60.0, 10.0, 2_000), 600);
        // A 30 Hz source never gets cut further by a 60 fps cap.
        assert_eq!(admitted(&Throttle::new(60), 30.0, 10.0, 1_000), 300);
        // 60 Hz to 30 fps: every second frame, exactly.
        assert_eq!(admitted(&Throttle::new(30), 60.0, 10.0, 1_000), 300);
    }

    #[test]
    fn throttle_follows_fps_changes_and_stalls() {
        let t = Throttle::new(30);
        assert!((299..=301).contains(&admitted(&t, 120.0, 10.0, 0)));
        t.set_fps(60);
        // Same timestamps restart from 0: treated as a clock jump, resynced, no burst.
        let n = admitted(&t, 120.0, 10.0, 0);
        assert!((599..=601).contains(&n), "after raise admitted {n}");
        // A 5 s stall must not let a catch-up burst through.
        assert!(t.admit(100_000_000));
        assert!(!t.admit(100_001_000));
        assert!(t.admit(100_016_700));
    }

    /// The probe is really in the pipeline: a 60 fps live source capped to 20, then raised to
    /// 60 at runtime. Skips (passes) where no H.264 encoder plugin is installed, e.g. CI.
    #[test]
    fn running_pipeline_honours_fps_cap_and_live_change() {
        init().unwrap();
        let encoders = candidates("auto");
        if encoders.is_empty() {
            eprintln!("no H.264 encoder available; skipping");
            return;
        }
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = count.clone();
        let mut params = p(720, Some((1920, 1080)));
        params.fps = 20;
        let run = start(
            &Source::Test,
            &encoders,
            &params,
            Arc::new(move |_| {
                c.fetch_add(1, Ordering::SeqCst);
            }),
            Arc::new(|_| {}),
        )
        .unwrap();
        let measure = || {
            std::thread::sleep(Duration::from_millis(500));
            let a = count.load(Ordering::SeqCst);
            std::thread::sleep(Duration::from_secs(3));
            count.load(Ordering::SeqCst) - a
        };
        let capped = measure();
        assert!(
            (54..=66).contains(&capped),
            "20 fps cap: {capped} frames in 3 s"
        );
        run.control.set_fps(60);
        let raised = measure();
        assert!(
            (165..=186).contains(&raised),
            "raised to 60: {raised} frames in 3 s"
        );
    }

    #[test]
    fn scaling_keeps_aspect_and_even_sizes() {
        assert_eq!(scale_caps(&p(0, Some((2560, 1440)))), "");
        assert_eq!(scale_caps(&p(1440, Some((2560, 1440)))), "");
        assert_eq!(
            scale_caps(&p(720, Some((1920, 1080)))),
            ",width=1280,height=720,pixel-aspect-ratio=1/1"
        );
        assert_eq!(
            scale_caps(&p(1080, Some((2560, 1440)))),
            ",width=1920,height=1080,pixel-aspect-ratio=1/1"
        );
        // 21:9 ultrawide: 1706.67 -> 1707 -> even 1708
        assert_eq!(
            scale_caps(&p(720, Some((2560, 1080)))),
            ",width=1708,height=720,pixel-aspect-ratio=1/1"
        );
        // unknown source size (X11): still square pixels
        assert_eq!(
            scale_caps(&p(720, None)),
            ",height=720,pixel-aspect-ratio=1/1"
        );
    }
}

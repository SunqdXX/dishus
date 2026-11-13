//! GStreamer capture → H.264 pipeline (runs inside the capture worker) with encoder
//! auto-detection (NVENC → VA-API → x264 → openh264) and live bitrate / fps / keyframe control.
//! Also the audio pipeline: the default output's monitor → Opus.

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

pub use super::throttle::Throttle;

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
                    pts_us: monotonic_us(s.upcast_ref(), buf.pts()),
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

/// A buffer's capture time on the system's monotonic clock (CLOCK_MONOTONIC, microseconds).
/// Video and audio run in separate pipelines with their own clocks and base times; this puts
/// both on one clock so the phone can compare them. Without a clock: now.
fn monotonic_us(elem: &gst::Element, pts: Option<gst::ClockTime>) -> u64 {
    let now = gst::glib::monotonic_time().max(0) as u64;
    match (elem.clock(), elem.base_time(), pts) {
        (Some(clock), Some(base), Some(pts)) => {
            let age = clock.time().saturating_sub(base + pts);
            now.saturating_sub(age.useconds())
        }
        _ => now,
    }
}

/// Opus bitrates (kbit/s) the daemon may set; see `crate::audio`.
pub const AUDIO_KBPS_RANGE: (u32, u32) = (16, 128);

pub struct AudioRunning {
    enc: gst::Element,
    pipeline: gst::Pipeline,
    stop: Arc<AtomicBool>,
}

impl AudioRunning {
    pub fn set_bitrate(&self, kbps: u32) {
        let kbps = kbps.clamp(AUDIO_KBPS_RANGE.0, AUDIO_KBPS_RANGE.1);
        self.enc.set_property("bitrate", (kbps * 1000) as i32);
    }
}

impl Drop for AudioRunning {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// One Opus packet (20 ms, 48 kHz stereo).
pub struct AudioPacket {
    pub pts_us: u64,
    pub data: Vec<u8>,
}

/// Capture what the PC plays (the default output's monitor; follows the default when it
/// changes) or, with `test`, a quiet 440 Hz tone, and encode it to Opus.
pub fn start_audio(
    test: bool,
    kbps: u32,
    on_packet: Arc<dyn Fn(AudioPacket) + Send + Sync>,
    on_fail: OnFail,
) -> Result<AudioRunning> {
    for f in ["opusenc", "audioconvert", "audioresample"] {
        if gst::ElementFactory::find(f).is_none() {
            bail!("GStreamer element {f} missing (gst-plugins-base)");
        }
    }
    let src = if test {
        "audiotestsrc is-live=true wave=sine freq=440 volume=0.1".to_string()
    } else {
        if gst::ElementFactory::find("pipewiresrc").is_none() {
            bail!("GStreamer element pipewiresrc missing (gst-plugin-pipewire)");
        }
        // stream.capture.sink: record the default sink's monitor, i.e. what the speakers play.
        // Read-only: the PC's own sound is unchanged.
        "pipewiresrc do-timestamp=true stream-properties=\"props,stream.capture.sink=true,\
         node.name=dishus-audio,node.description=DiShus\""
            .to_string()
    };
    let kbps = kbps.clamp(AUDIO_KBPS_RANGE.0, AUDIO_KBPS_RANGE.1);
    let desc = format!(
        "{src} ! queue max-size-time=100000000 max-size-buffers=0 max-size-bytes=0 \
         leaky=downstream ! audioconvert ! audioresample ! \
         audio/x-raw,rate=48000,channels=2 ! \
         opusenc name=enc frame-size=20 bitrate={} inband-fec=true packet-loss-percentage=10 ! \
         appsink name=sink sync=false max-buffers=8 drop=true",
        kbps * 1000
    );
    tracing::debug!("audio pipeline: {desc}");
    let pipeline = gst::parse::launch(&desc)
        .context("building the audio pipeline")?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("not a pipeline"))?;
    let enc = pipeline.by_name("enc").context("no encoder")?;
    let appsink = pipeline
        .by_name("sink")
        .context("no sink")?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow!("not an appsink"))?;
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buf = sample.buffer().ok_or(gst::FlowError::Error)?;
                if buf.flags().contains(gst::BufferFlags::HEADER) || buf.size() == 0 {
                    return Ok(gst::FlowSuccess::Ok);
                }
                let map = buf.map_readable().map_err(|_| gst::FlowError::Error)?;
                on_packet(AudioPacket {
                    pts_us: monotonic_us(s.upcast_ref(), buf.pts()),
                    data: map.as_slice().to_vec(),
                });
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    pipeline
        .set_state(gst::State::Playing)
        .context("starting the audio pipeline")?;
    let stop = Arc::new(AtomicBool::new(false));
    let bus = pipeline.bus().context("no bus")?;
    let (p, st) = (pipeline.clone(), stop.clone());
    std::thread::Builder::new()
        .name("gst-audio-bus".into())
        .spawn(move || {
            while !st.load(Ordering::SeqCst) {
                let Some(msg) = bus.timed_pop_filtered(
                    gst::ClockTime::from_mseconds(250),
                    &[gst::MessageType::Error, gst::MessageType::Eos],
                ) else {
                    continue;
                };
                if st.load(Ordering::SeqCst) {
                    break;
                }
                let why = match msg.view() {
                    gst::MessageView::Error(e) => {
                        format!("audio pipeline error: {} ({:?})", e.error(), e.debug())
                    }
                    _ => "audio pipeline ended (EOS)".into(),
                };
                let _ = p.set_state(gst::State::Null);
                on_fail(why);
                break;
            }
        })?;
    Ok(AudioRunning {
        enc,
        pipeline,
        stop,
    })
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

    /// The test tone comes out as 20 ms Opus packets stamped with the monotonic clock, and the
    /// bitrate can change while it runs.
    #[test]
    fn audio_pipeline_makes_20ms_packets_on_the_monotonic_clock() {
        init().unwrap();
        if gst::ElementFactory::find("opusenc").is_none() {
            eprintln!("no opusenc; skipping");
            return;
        }
        let got = Arc::new(Mutex::new(Vec::<(u64, usize)>::new()));
        let g = got.clone();
        let run = start_audio(
            true,
            64,
            Arc::new(move |p: AudioPacket| g.lock().unwrap().push((p.pts_us, p.data.len()))),
            Arc::new(|_| {}),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(1000));
        run.set_bitrate(24);
        std::thread::sleep(Duration::from_millis(1000));
        let now = gst::glib::monotonic_time() as u64;
        drop(run);
        let v = got.lock().unwrap().clone();
        assert!((90..=105).contains(&v.len()), "{} packets in 2 s", v.len());
        for w in v.windows(2).skip(2) {
            let d = w[1].0 as i64 - w[0].0 as i64;
            assert!((15_000..=25_000).contains(&d), "pts step {d} us");
        }
        let last = v.last().unwrap().0;
        assert!(
            now - last < 200_000,
            "pts {} ms behind now",
            (now - last) / 1000
        );
        let size = |r: &[(u64, usize)]| r.iter().map(|x| x.1).sum::<usize>() / r.len();
        let (hi, lo) = (size(&v[10..45]), size(&v[v.len() - 30..]));
        assert!(
            lo * 2 < hi,
            "24 kbps packets ({lo} B) not smaller than 64 kbps ({hi} B)"
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

//! The capture → encode loop on Windows: one thread per stream inside the capture worker.
//!
//! Desktop Duplication only delivers a frame when something changed (like PipeWire), so the
//! loop also re-encodes the last picture for keyframe requests and every 500 ms when idle (like
//! `pipewiresrc keepalive-time=500`). Losing the duplication (resolution change, full-screen
//! switch, UAC prompt, lock screen) does not end the stream: the duplication is recreated as
//! soon as Windows allows, and meanwhile the last picture keeps being sent.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{SyncSender, sync_channel},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::{
    Foundation::LUID,
    Graphics::{Direct3D11::*, Dxgi::Common::*},
    System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize},
};

use super::{
    convert::{Converter, output_size},
    dxgi::{self, Acquired, CursorPainter, DupError, Duplicator},
    mf::{self, EncoderDesc, H264Encoder, Input, Kind},
};
use crate::capture::throttle::Throttle;

/// Re-send the last picture after this long without changes.
const KEEPALIVE: Duration = Duration::from_millis(500);
/// An encoder that swallowed this many frames without output is replaced by the next one.
const DEAD_ENCODER_FRAMES: u32 = 60;

#[derive(Debug, Clone)]
pub struct Params {
    pub monitor: u8,
    pub fps: u8,
    pub bitrate_kbps: u32,
    pub max_height: u16,
    pub show_cursor: bool,
    /// Stream a moving test pattern instead of the desktop (no monitor needed).
    pub test_source: bool,
    /// config `video.encoder`
    pub encoder_pref: String,
    /// Only this encoder (by MFT name); used by the doctor.
    pub encoder_name: Option<String>,
    /// GPU for the test pattern (default: the one showing the primary monitor).
    pub adapter: Option<LUID>,
}

pub struct Frame {
    pub pts_us: u64,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

/// Live knobs, read by the loop on every iteration.
pub struct Control {
    throttle: Throttle,
    fps: AtomicU32,
    keyframe: AtomicBool,
    bitrate: AtomicU32,
}

impl Control {
    pub fn force_keyframe(&self) {
        self.keyframe.store(true, Ordering::SeqCst);
    }
    pub fn set_bitrate(&self, kbps: u32) {
        self.bitrate
            .store(kbps.clamp(100, 200_000), Ordering::SeqCst);
    }
    pub fn set_fps(&self, fps: u8) {
        self.throttle.set_fps(fps);
        self.fps
            .store(u32::from(fps.clamp(1, 120)), Ordering::SeqCst);
    }
}

pub struct Running {
    pub control: Arc<Control>,
    pub width: u32,
    pub height: u32,
    pub encoder: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub type OnFrame = Arc<dyn Fn(Frame) + Send + Sync>;
pub type OnFail = Arc<dyn Fn(String) + Send + Sync>;

type Started = SyncSender<Result<(u32, u32, String)>>;

/// Start a stream; returns once the first frame has been encoded (or it failed).
pub fn start(p: &Params, on_frame: OnFrame, on_fail: OnFail) -> Result<Running> {
    let control = Arc::new(Control {
        throttle: Throttle::new(p.fps),
        fps: AtomicU32::new(u32::from(p.fps.clamp(1, 120))),
        keyframe: AtomicBool::new(true),
        bitrate: AtomicU32::new(0),
    });
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = sync_channel(1);
    let thread = {
        let (p, control, stop) = (p.clone(), control.clone(), stop.clone());
        std::thread::Builder::new()
            .name("capture".into())
            .spawn(move || run(p, control, stop, on_frame, on_fail, tx))?
    };
    let mut running = Running {
        control,
        width: 0,
        height: 0,
        encoder: String::new(),
        stop,
        thread: Some(thread),
    };
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok((w, h, enc))) => {
            running.width = w;
            running.height = h;
            running.encoder = enc;
            Ok(running)
        }
        Ok(Err(e)) => Err(e),
        Err(_) => bail!("capture produced no frames within 10 s"),
    }
}

/// COM for the current thread (multithreaded apartment), for as long as this lives.
pub struct Com;
impl Com {
    pub fn init() -> Self {
        // SAFETY: per-thread COM init, undone in Drop on the same thread.
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Com
    }
}
impl Drop for Com {
    fn drop(&mut self) {
        // SAFETY: balances CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
}

fn run(
    p: Params,
    ctl: Arc<Control>,
    stop: Arc<AtomicBool>,
    on_frame: OnFrame,
    on_fail: OnFail,
    started: Started,
) {
    let _com = Com::init();
    let mut started = Some(started);
    if let Err(e) = stream(&p, &ctl, &stop, &on_frame, &mut started) {
        match started.take() {
            Some(tx) => {
                let _ = tx.send(Err(e));
            }
            None => on_fail(format!("{e:#}")),
        }
    }
}

enum Source {
    Desktop {
        output: dxgi::Output,
        dup: Option<Duplicator>,
        retry_at: Instant,
    },
    Test {
        n: u64,
        next: Instant,
        pixels: Vec<u8>,
    },
}

/// The textures a source size needs: the clean desktop copy, the picture with the pointer drawn
/// in, and the converter reading the latter.
struct Surfaces {
    size: (u32, u32),
    desk: ID3D11Texture2D,
    composed: ID3D11Texture2D,
    conv: Converter,
}

impl Surfaces {
    fn new(
        dev: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        size: (u32, u32),
        out: (u32, u32),
    ) -> Result<Self> {
        let mk = || {
            dxgi::texture(
                dev,
                size.0,
                size.1,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE,
                D3D11_USAGE_DEFAULT,
                D3D11_CPU_ACCESS_FLAG(0),
            )
        };
        let desk = mk()?;
        let composed = mk()?;
        let conv = Converter::new(dev, ctx, &composed, size, out)?;
        Ok(Self {
            size,
            desk,
            composed,
            conv,
        })
    }
}

/// Software encoders get NV12 bytes: read the converted texture back.
struct Readback {
    staging: ID3D11Texture2D,
    buf: Vec<u8>,
}

impl Readback {
    fn new(dev: &ID3D11Device, (w, h): (u32, u32)) -> Result<Self> {
        Ok(Self {
            staging: dxgi::texture(
                dev,
                w,
                h,
                DXGI_FORMAT_NV12,
                D3D11_BIND_FLAG(0),
                D3D11_USAGE_STAGING,
                D3D11_CPU_ACCESS_READ,
            )?,
            buf: vec![0; (w * h * 3 / 2) as usize],
        })
    }

    fn read(
        &mut self,
        ctx: &ID3D11DeviceContext,
        tex: &ID3D11Texture2D,
        (w, h): (u32, u32),
    ) -> Result<&[u8]> {
        // SAFETY: same-size NV12 textures; the mapped Y plane is `h` rows of `RowPitch` bytes,
        // followed by the interleaved UV plane of `h / 2` rows.
        unsafe {
            ctx.CopyResource(&self.staging, tex);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
            let base = m.pData.cast::<u8>();
            let pitch = m.RowPitch as usize;
            let (w, h) = (w as usize, h as usize);
            for row in 0..h + h / 2 {
                let src = std::slice::from_raw_parts(base.add(row * pitch), w);
                self.buf[row * w..row * w + w].copy_from_slice(src);
            }
            ctx.Unmap(&self.staging, 0);
        }
        Ok(&self.buf)
    }
}

/// A moving pattern with sharp edges and gradients, so encoders have real work to do.
fn draw_test_pattern(px: &mut [u8], (w, h): (u32, u32), n: u64) {
    let bar = (n * 12 % u64::from(w)) as u32;
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let on_bar = x.abs_diff(bar) < 40;
            let v = ((x + y + n as u32 * 3) & 0xFF) as u8;
            px[i] = if on_bar { 255 } else { v };
            px[i + 1] = if on_bar { 255 } else { (y & 0xFF) as u8 };
            px[i + 2] = if on_bar { 255 } else { (x & 0xFF) as u8 };
            px[i + 3] = 255;
        }
    }
}

fn open_encoder(
    candidates: &[EncoderDesc],
    from: &mut usize,
    devmgr: &windows::Win32::Media::MediaFoundation::IMFDXGIDeviceManager,
    out: (u32, u32),
    fps: u8,
    kbps: u32,
) -> Result<H264Encoder> {
    let mut errors = Vec::new();
    while *from < candidates.len() {
        let d = &candidates[*from];
        match H264Encoder::new(d, Some(devmgr), out.0, out.1, fps, kbps) {
            Ok(e) => return Ok(e),
            Err(e) => {
                tracing::warn!("encoder {} failed: {e:#}", d.describe());
                errors.push(format!("{}: {e:#}", d.label));
                *from += 1;
            }
        }
    }
    bail!("all encoders failed: {}", errors.join("; "))
}

fn stream(
    p: &Params,
    ctl: &Control,
    stop: &AtomicBool,
    on_frame: &OnFrame,
    started: &mut Option<Started>,
) -> Result<()> {
    mf::available()?;
    let (adapter, luid, vendor, mut source, src_size) = if p.test_source {
        let all = dxgi::adapters()?;
        let a = match p.adapter {
            Some(l) => all
                .into_iter()
                .find(|a| a.luid.LowPart == l.LowPart && a.luid.HighPart == l.HighPart),
            None => {
                let mut all = all;
                all.sort_by_key(|a| a.outputs == 0);
                all.into_iter().next()
            }
        }
        .context("no GPU found")?;
        let size = (1920, 1080);
        let src = Source::Test {
            n: 0,
            next: Instant::now(),
            pixels: vec![0; (size.0 * size.1 * 4) as usize],
        };
        (a.adapter, a.luid, a.vendor_id, src, size)
    } else {
        let mut outs = dxgi::outputs()?;
        if usize::from(p.monitor) >= outs.len() {
            bail!(
                "monitor {} does not exist ({} found)",
                p.monitor,
                outs.len()
            );
        }
        let o = outs.swap_remove(usize::from(p.monitor));
        let size = (o.width, o.height);
        (
            o.adapter.clone(),
            o.luid,
            o.vendor_id,
            Source::Desktop {
                output: o,
                dup: None,
                retry_at: Instant::now(),
            },
            size,
        )
    };
    let (dev, ctx) = dxgi::create_device(&adapter)?;
    let mut src_size = src_size;
    if let Source::Desktop { output, dup, .. } = &mut source {
        let d = Duplicator::new(&output.output, &dev)?;
        src_size = (d.width, d.height);
        *dup = Some(d);
    }
    let out_size = output_size(src_size, p.max_height);
    let mut surf = Surfaces::new(&dev, &ctx, src_size, out_size)?;
    let devmgr = mf::device_manager(&dev)?;
    let candidates: Vec<EncoderDesc> = mf::list(luid, vendor)?
        .into_iter()
        .filter(|e| e.matches(&p.encoder_pref))
        .filter(|e| p.encoder_name.as_ref().is_none_or(|n| *n == e.name))
        .collect();
    if candidates.is_empty() {
        bail!(
            "no H.264 encoder found for video.encoder = \"{}\"",
            p.encoder_pref
        );
    }
    let mut cand = 0;
    let mut enc = open_encoder(
        &candidates,
        &mut cand,
        &devmgr,
        out_size,
        p.fps,
        p.bitrate_kbps,
    )?;
    let mut readback: Option<Readback> = None;
    let mut painter = CursorPainter::new();
    let epoch = Instant::now();
    let mut dirty = true;
    let mut last_submit: Option<Instant> = None;
    let mut pending_key = true;

    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        if ctl.keyframe.swap(false, Ordering::SeqCst) {
            pending_key = true;
        }
        let kbps = ctl.bitrate.swap(0, Ordering::SeqCst);
        if kbps != 0 {
            enc.set_bitrate(kbps);
        }
        let interval =
            Duration::from_micros(1_000_000 / u64::from(ctl.fps.load(Ordering::SeqCst).max(1)));

        // 1. Capture.
        match &mut source {
            Source::Desktop {
                output,
                dup,
                retry_at,
            } => {
                if dup.is_none() && Instant::now() >= *retry_at {
                    match Duplicator::new(&output.output, &dev) {
                        Ok(d) => {
                            tracing::info!("desktop duplication back ({}x{})", d.width, d.height);
                            if (d.width, d.height) != surf.size {
                                surf = Surfaces::new(&dev, &ctx, (d.width, d.height), out_size)?;
                            }
                            *dup = Some(d);
                            dirty = true;
                            pending_key = true;
                        }
                        Err(e) => {
                            tracing::debug!("desktop duplication not available yet: {e:#}");
                            *retry_at = Instant::now() + Duration::from_millis(250);
                        }
                    }
                }
                match dup {
                    Some(d) => {
                        let wait = if dirty || pending_key {
                            1
                        } else {
                            interval.as_millis().clamp(1, 50) as u32
                        };
                        match d.acquire(wait) {
                            Ok(Acquired::Timeout) => {}
                            Ok(Acquired::Frame { desktop, pointer }) => {
                                if desktop {
                                    d.copy_to(&ctx, &surf.desk)?;
                                }
                                if desktop || (pointer && p.show_cursor) {
                                    // SAFETY: same-device, same-size textures.
                                    unsafe { ctx.CopyResource(&surf.composed, &surf.desk) };
                                    if p.show_cursor {
                                        painter.draw(
                                            &dev,
                                            &ctx,
                                            &surf.composed,
                                            surf.size,
                                            &d.pointer,
                                        )?;
                                    }
                                    dirty = true;
                                }
                            }
                            Err(DupError::Lost(e)) => {
                                tracing::info!(
                                    "desktop duplication lost ({}); recreating it",
                                    e.message()
                                );
                                *dup = None;
                                *retry_at = Instant::now();
                            }
                            Err(DupError::Fatal(e)) => {
                                return Err(anyhow!(e).context("Desktop Duplication"));
                            }
                        }
                    }
                    None => std::thread::sleep(Duration::from_millis(20)),
                }
            }
            Source::Test { n, next, pixels } => {
                let now = Instant::now();
                if now < *next {
                    std::thread::sleep((*next - now).min(Duration::from_millis(5)));
                } else {
                    *next += Duration::from_micros(16_667);
                    if *next < now {
                        *next = now;
                    }
                    *n += 1;
                    draw_test_pattern(pixels, surf.size, *n);
                    // SAFETY: the buffer holds a full BGRA image of the texture's size.
                    unsafe {
                        ctx.UpdateSubresource(
                            &surf.composed,
                            0,
                            None,
                            pixels.as_ptr().cast(),
                            surf.size.0 * 4,
                            0,
                        );
                    }
                    dirty = true;
                }
            }
        }

        // 2. Encode, if a new picture is due, a keyframe was asked for, or to keep alive.
        let now = Instant::now();
        let pts = now.duration_since(epoch).as_micros() as u64;
        let since = last_submit.map(|t| now.duration_since(t));
        let keepalive = since.is_none_or(|s| s >= KEEPALIVE);
        let key_due = pending_key && since.is_none_or(|s| s >= interval);
        let submit = (dirty && ctl.throttle.admit(pts)) || keepalive || key_due;
        let mut emit = |au: mf::Au| {
            if let Some(tx) = started.take() {
                let _ = tx.send(Ok((
                    out_size.0,
                    out_size.1,
                    current_label(&candidates, cand),
                )));
            }
            on_frame(Frame {
                pts_us: au.pts_us,
                keyframe: au.keyframe,
                data: au.data,
            });
        };
        if submit {
            let tex = surf.conv.convert()?.clone();
            let accepted = if enc.desc.kind == Kind::Hardware {
                enc.encode(
                    Input::Texture(&tex),
                    pts,
                    interval.as_micros() as u64,
                    pending_key,
                    &mut emit,
                )?
            } else {
                let rb = match &mut readback {
                    Some(r) => r,
                    None => readback.insert(Readback::new(&dev, out_size)?),
                };
                let bytes = rb.read(&ctx, &tex, out_size)?;
                enc.encode(
                    Input::Nv12(bytes),
                    pts,
                    interval.as_micros() as u64,
                    pending_key,
                    &mut emit,
                )?
            };
            if accepted {
                dirty = false;
                pending_key = false;
                last_submit = Some(now);
            }
        } else {
            enc.poll(&mut emit)?;
        }

        if enc.pending >= DEAD_ENCODER_FRAMES {
            tracing::warn!(
                "encoder {} took {} frames without producing any; trying the next one",
                enc.desc.describe(),
                enc.pending
            );
            cand += 1;
            drop(enc);
            enc = open_encoder(
                &candidates,
                &mut cand,
                &devmgr,
                out_size,
                ctl.fps.load(Ordering::SeqCst) as u8,
                p.bitrate_kbps,
            )?;
            pending_key = true;
        }
    }
}

fn current_label(c: &[EncoderDesc], i: usize) -> String {
    c.get(i).map(|d| d.label.clone()).unwrap_or_default()
}

/// Really encode a moving test pattern with one encoder for about a second; returns the frames
/// per second it delivered. Used by the doctor: an encoder being listed proves nothing.
pub fn probe(enc: &EncoderDesc, adapter: LUID, max_height: u16, fps: u8) -> Result<f64> {
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = count.clone();
    let failed = Arc::new(std::sync::Mutex::new(None::<String>));
    let f = failed.clone();
    let p = Params {
        monitor: 0,
        fps,
        bitrate_kbps: 4000,
        max_height,
        show_cursor: false,
        test_source: true,
        encoder_pref: "auto".into(),
        encoder_name: Some(enc.name.clone()),
        adapter: Some(adapter),
    };
    let run = start(
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
    let start = Instant::now();
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

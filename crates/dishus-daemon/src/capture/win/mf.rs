//! Media Foundation H.264 encoding.
//!
//! `mfplat.dll` is loaded at run time, never imported: Windows "N" editions ship without Media
//! Foundation, and an imported DLL that is missing stops the whole program from starting. This
//! way the daemon runs, and `dishus doctor` can say what to install (the Media Feature Pack).
//!
//! Encoders: the GPU's hardware MFT for the adapter that shows the monitor (NVIDIA, Intel,
//! AMD all ship one; asynchronous, fed with D3D11 textures), then the software "Microsoft H264
//! Video Encoder" (synchronous, fed from system memory). Low-latency mode, CBR, no B-frames,
//! long GOP (IDRs come on request), live bitrate changes, forced keyframes.

use std::{
    ffi::c_void,
    mem::ManuallyDrop,
    sync::OnceLock,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use windows::{
    Win32::{
        Foundation::{HMODULE, LUID},
        Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D},
        Media::MediaFoundation::*,
        System::{
            Com::CoTaskMemFree,
            LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW},
            Variant::VARIANT,
        },
    },
    core::{GUID, HRESULT, Interface, PWSTR, s, w},
};

use super::nal::ParamSets;

type FnStartup = unsafe extern "system" fn(u32, u32) -> HRESULT;
type FnCreateAttributes = unsafe extern "system" fn(*mut *mut c_void, u32) -> HRESULT;
type FnCreateObj = unsafe extern "system" fn(*mut *mut c_void) -> HRESULT;
type FnCreateMemoryBuffer = unsafe extern "system" fn(u32, *mut *mut c_void) -> HRESULT;
type FnCreateDxgiSurfaceBuffer =
    unsafe extern "system" fn(*const GUID, *mut c_void, u32, i32, *mut *mut c_void) -> HRESULT;
type FnCreateDxgiDeviceManager = unsafe extern "system" fn(*mut u32, *mut *mut c_void) -> HRESULT;
type FnTEnumEx = unsafe extern "system" fn(
    GUID,
    u32,
    *const MFT_REGISTER_TYPE_INFO,
    *const MFT_REGISTER_TYPE_INFO,
    *mut *mut *mut c_void,
    *mut u32,
) -> HRESULT;
type FnTEnum2 = unsafe extern "system" fn(
    GUID,
    u32,
    *const MFT_REGISTER_TYPE_INFO,
    *const MFT_REGISTER_TYPE_INFO,
    *mut c_void,
    *mut *mut *mut c_void,
    *mut u32,
) -> HRESULT;

/// The few `mfplat.dll` exports we need, resolved once.
struct Api {
    create_attributes: FnCreateAttributes,
    create_media_type: FnCreateObj,
    create_sample: FnCreateObj,
    create_memory_buffer: FnCreateMemoryBuffer,
    create_dxgi_surface_buffer: FnCreateDxgiSurfaceBuffer,
    create_dxgi_device_manager: FnCreateDxgiDeviceManager,
    tenum_ex: FnTEnumEx,
    /// Windows 10 1703+; filters hardware encoders by adapter.
    tenum2: Option<FnTEnum2>,
}

/// Shown when Media Foundation is missing (Windows N/KN editions).
pub const N_EDITION_HELP: &str = "Media Foundation is missing (Windows \"N\" edition?). Install \
the Media Feature Pack: Settings > Apps > Optional features > Add a feature > \"Media Feature \
Pack\", then restart the PC.";

static API: OnceLock<std::result::Result<Api, String>> = OnceLock::new();

fn load() -> std::result::Result<Api, String> {
    // SAFETY: loading a system DLL by name from System32 only; every symbol is checked for
    // presence and transmuted to the prototype documented in mfapi.h.
    unsafe {
        let lib: HMODULE = LoadLibraryExW(w!("mfplat.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32)
            .map_err(|e| format!("{N_EDITION_HELP} ({e})"))?;
        type Proc = unsafe extern "system" fn() -> isize;
        macro_rules! sym {
            ($name:literal as $t:ty) => {
                std::mem::transmute::<Proc, $t>(
                    GetProcAddress(lib, s!($name))
                        .ok_or_else(|| format!("mfplat.dll has no {}", $name))?,
                )
            };
        }
        let startup = sym!("MFStartup" as FnStartup);
        let api = Api {
            create_attributes: sym!("MFCreateAttributes" as FnCreateAttributes),
            create_media_type: sym!("MFCreateMediaType" as FnCreateObj),
            create_sample: sym!("MFCreateSample" as FnCreateObj),
            create_memory_buffer: sym!("MFCreateMemoryBuffer" as FnCreateMemoryBuffer),
            create_dxgi_surface_buffer: sym!(
                "MFCreateDXGISurfaceBuffer" as FnCreateDxgiSurfaceBuffer
            ),
            create_dxgi_device_manager: sym!(
                "MFCreateDXGIDeviceManager" as FnCreateDxgiDeviceManager
            ),
            tenum_ex: sym!("MFTEnumEx" as FnTEnumEx),
            tenum2: GetProcAddress(lib, s!("MFTEnum2"))
                .map(|f| std::mem::transmute::<Proc, FnTEnum2>(f)),
        };
        // Never MFShutdown: the worker process exits when the session ends.
        startup(MF_VERSION, MFSTARTUP_LITE)
            .ok()
            .map_err(|e| format!("MFStartup failed: {e}"))?;
        Ok(api)
    }
}

fn api() -> Result<&'static Api> {
    API.get_or_init(load).as_ref().map_err(|e| anyhow!("{e}"))
}

/// Whether Media Foundation can be used here; the error explains what to install.
pub fn available() -> Result<()> {
    api().map(|_| ())
}

/// Turn an out-pointer filled by an MF factory into an interface.
///
/// # Safety
/// `p` must be an owned interface pointer of type `T` (or null).
unsafe fn take<T: Interface>(p: *mut c_void) -> Result<T> {
    if p.is_null() {
        bail!("Media Foundation returned null");
    }
    // SAFETY: per contract; from_raw takes over the reference.
    Ok(unsafe { T::from_raw(p) })
}

fn create_media_type() -> Result<IMFMediaType> {
    let mut p = std::ptr::null_mut();
    // SAFETY: documented prototype; `p` receives an owned IMFMediaType.
    unsafe {
        (api()?.create_media_type)(&mut p).ok()?;
        take(p)
    }
}

pub fn create_sample() -> Result<IMFSample> {
    let mut p = std::ptr::null_mut();
    // SAFETY: as above, IMFSample.
    unsafe {
        (api()?.create_sample)(&mut p).ok()?;
        take(p)
    }
}

pub fn create_memory_buffer(len: u32) -> Result<IMFMediaBuffer> {
    let mut p = std::ptr::null_mut();
    // SAFETY: as above, IMFMediaBuffer.
    unsafe {
        (api()?.create_memory_buffer)(len, &mut p).ok()?;
        take(p)
    }
}

/// A sample wrapping a GPU texture (hardware encoders read it directly).
pub fn texture_sample(tex: &ID3D11Texture2D) -> Result<IMFSample> {
    let mut p = std::ptr::null_mut();
    // SAFETY: documented prototype; the texture pointer is borrowed for the call and the
    // buffer takes its own reference.
    let buf: IMFMediaBuffer = unsafe {
        (api()?.create_dxgi_surface_buffer)(&ID3D11Texture2D::IID, tex.as_raw(), 0, 0, &mut p)
            .ok()?;
        take(p)?
    };
    // SAFETY: plain COM calls on valid interfaces.
    unsafe {
        if let Ok(b2) = buf.cast::<IMF2DBuffer>() {
            buf.SetCurrentLength(b2.GetContiguousLength()?)?;
        }
        let sample = create_sample()?;
        sample.AddBuffer(&buf)?;
        Ok(sample)
    }
}

/// The D3D11 device shared with hardware encoders.
pub fn device_manager(device: &ID3D11Device) -> Result<IMFDXGIDeviceManager> {
    let mut token = 0u32;
    let mut p = std::ptr::null_mut();
    // SAFETY: documented prototype; then a plain COM call.
    unsafe {
        (api()?.create_dxgi_device_manager)(&mut token, &mut p).ok()?;
        let m: IMFDXGIDeviceManager = take(p)?;
        m.ResetDevice(device, token)?;
        Ok(m)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Hardware,
    Software,
}

/// An H.264 encoder found on this PC (plain data, so it can cross threads; the MFT itself is
/// created on the encoding thread).
#[derive(Debug, Clone)]
pub struct EncoderDesc {
    /// Short name, as in config.toml and on the phone: nvenc, qsv, amf, hardware, software.
    pub label: String,
    /// The MFT's own name, e.g. "Intel® Quick Sync Video H.264 Encoder MFT".
    pub name: String,
    pub kind: Kind,
    clsid: GUID,
    luid: Option<LUID>,
}

impl EncoderDesc {
    pub fn describe(&self) -> String {
        format!("{} ({})", self.label, self.name)
    }
    /// Whether `pref` (config `video.encoder`) allows this encoder.
    pub fn matches(&self, pref: &str) -> bool {
        match pref {
            "auto" => true,
            "hardware" => self.kind == Kind::Hardware,
            other => self.label == other,
        }
    }
}

/// GPU maker → encoder label.
pub fn vendor_label(vendor_id: u32) -> &'static str {
    match vendor_id {
        0x10DE => "nvenc",
        0x8086 => "qsv",
        0x1002 | 0x1022 => "amf",
        _ => "hardware",
    }
}

fn type_info(sub: GUID) -> MFT_REGISTER_TYPE_INFO {
    MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: sub,
    }
}

/// Enumerate H.264 encoder activation objects.
fn enumerate(hardware: bool, luid: Option<LUID>) -> Result<Vec<IMFActivate>> {
    let api = api()?;
    let input = type_info(MFVideoFormat_NV12);
    let output = type_info(MFVideoFormat_H264);
    let flags = MFT_ENUM_FLAG_SORTANDFILTER.0 as u32
        | if hardware {
            MFT_ENUM_FLAG_HARDWARE.0 as u32
        } else {
            MFT_ENUM_FLAG_SYNCMFT.0 as u32
        };
    let mut arr: *mut *mut c_void = std::ptr::null_mut();
    let mut n = 0u32;
    // SAFETY: documented prototypes; the returned array holds `n` owned IMFActivate pointers
    // and is freed with CoTaskMemFree after we took them over.
    unsafe {
        match (hardware, luid, api.tenum2) {
            (true, Some(luid), Some(tenum2)) => {
                let mut ap = std::ptr::null_mut();
                (api.create_attributes)(&mut ap, 1).ok()?;
                let attrs: IMFAttributes = take(ap)?;
                let bytes = std::slice::from_raw_parts(
                    std::ptr::from_ref(&luid).cast::<u8>(),
                    std::mem::size_of::<LUID>(),
                );
                attrs.SetBlob(&MFT_ENUM_ADAPTER_LUID, bytes)?;
                tenum2(
                    MFT_CATEGORY_VIDEO_ENCODER,
                    flags,
                    &input,
                    &output,
                    attrs.as_raw(),
                    &mut arr,
                    &mut n,
                )
                .ok()?;
            }
            _ => (api.tenum_ex)(
                MFT_CATEGORY_VIDEO_ENCODER,
                flags,
                &input,
                &output,
                &mut arr,
                &mut n,
            )
            .ok()?,
        }
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n as usize {
            out.push(IMFActivate::from_raw(*arr.add(i)));
        }
        if !arr.is_null() {
            CoTaskMemFree(Some(arr.cast()));
        }
        Ok(out)
    }
}

fn string_attr(a: &IMFActivate, key: &GUID) -> Option<String> {
    let mut p = PWSTR::null();
    let mut len = 0u32;
    // SAFETY: the string is CoTaskMemAlloc'd by MF and freed here.
    unsafe {
        a.GetAllocatedString(key, &mut p, &mut len).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0.cast()));
        s
    }
}

fn describe(a: &IMFActivate, kind: Kind, label: &str, luid: Option<LUID>) -> Option<EncoderDesc> {
    // SAFETY: plain attribute read.
    let clsid = unsafe { a.GetGUID(&MFT_TRANSFORM_CLSID_Attribute) }.ok()?;
    Some(EncoderDesc {
        label: label.into(),
        name: string_attr(a, &MFT_FRIENDLY_NAME_Attribute)
            .unwrap_or_else(|| "H.264 encoder".into()),
        kind,
        clsid,
        luid,
    })
}

/// Encoders usable with textures from the adapter `luid` (made by `vendor_id`), best first:
/// its hardware encoders, then software.
pub fn list(luid: LUID, vendor_id: u32) -> Result<Vec<EncoderDesc>> {
    let mut out = Vec::new();
    // Without MFTEnum2 we can't tell which adapter a hardware MFT belongs to; then only
    // software is safe (a texture from one GPU can't be fed to another GPU's encoder).
    if api()?.tenum2.is_some() {
        for a in enumerate(true, Some(luid)).unwrap_or_default() {
            // Some drivers register the same encoder twice (one per media engine); one is enough.
            if let Some(d) = describe(&a, Kind::Hardware, vendor_label(vendor_id), Some(luid))
                && !out.iter().any(|o: &EncoderDesc| o.name == d.name)
            {
                out.push(d);
            }
        }
    }
    for a in enumerate(false, None)? {
        if let Some(d) = describe(&a, Kind::Software, "software", None) {
            out.push(d);
        }
    }
    Ok(out)
}

fn activate(desc: &EncoderDesc) -> Result<IMFTransform> {
    let all = enumerate(desc.kind == Kind::Hardware, desc.luid)?;
    for a in all {
        // SAFETY: plain attribute read and activation.
        unsafe {
            if a.GetGUID(&MFT_TRANSFORM_CLSID_Attribute).ok() == Some(desc.clsid) {
                return a
                    .ActivateObject::<IMFTransform>()
                    .with_context(|| format!("activating {}", desc.name));
            }
        }
    }
    bail!("{} disappeared", desc.name)
}

/// Encoder input: a GPU texture for hardware MFTs, NV12 bytes (pitch = width) for software.
pub enum Input<'a> {
    Texture(&'a ID3D11Texture2D),
    Nv12(&'a [u8]),
}

/// One encoded access unit.
pub struct Au {
    pub data: Vec<u8>,
    pub pts_us: u64,
    pub keyframe: bool,
}

pub struct H264Encoder {
    pub desc: EncoderDesc,
    mft: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    codec: Option<ICodecAPI>,
    need_input: u32,
    provides_samples: bool,
    out_size: u32,
    width: u32,
    height: u32,
    params: ParamSets,
    /// Inputs since the last output: an encoder that never produces anything is replaced.
    pub pending: u32,
    /// Input time per pts, for the encode-latency figure in the debug log.
    inputs: std::collections::VecDeque<(u64, Instant)>,
    lat: (u32, Duration, Duration),
}

fn set_codec(codec: &ICodecAPI, key: &GUID, v: VARIANT, what: &str) {
    // SAFETY: plain COM call with valid pointers.
    if let Err(e) = unsafe { codec.SetValue(key, &v) } {
        tracing::debug!("encoder: {what} not supported ({e})");
    }
}

impl H264Encoder {
    pub fn new(
        desc: &EncoderDesc,
        devmgr: Option<&IMFDXGIDeviceManager>,
        width: u32,
        height: u32,
        fps: u8,
        kbps: u32,
    ) -> Result<Self> {
        let mft = activate(desc)?;
        let hw = desc.kind == Kind::Hardware;
        // SAFETY: COM calls on the freshly created MFT, in the order MF documents for
        // encoders: unlock (async) → D3D manager → codec settings → output type → input type.
        unsafe {
            let attrs = mft.GetAttributes().ok();
            let mut events = None;
            if let Some(a) = &attrs {
                if a.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0 {
                    a.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
                    events = Some(mft.cast::<IMFMediaEventGenerator>()?);
                }
                let _ = a.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            if hw {
                let m = devmgr.context("hardware encoder needs a D3D11 device")?;
                mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, m.as_raw() as usize)
                    .context("encoder refused the D3D11 device")?;
            }
            let codec = mft.cast::<ICodecAPI>().ok();
            if let Some(c) = &codec {
                set_codec(
                    c,
                    &CODECAPI_AVLowLatencyMode,
                    VARIANT::from(true),
                    "low latency",
                );
                set_codec(
                    c,
                    &CODECAPI_AVEncCommonRateControlMode,
                    VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
                    "CBR",
                );
                set_codec(
                    c,
                    &CODECAPI_AVEncCommonMeanBitRate,
                    VARIANT::from(kbps * 1000),
                    "bitrate",
                );
                set_codec(
                    c,
                    &CODECAPI_AVEncMPVDefaultBPictureCount,
                    VARIANT::from(0u32),
                    "B-frames",
                );
                // Keyframes come when the session asks; a long GOP like Linux's 1024.
                set_codec(
                    c,
                    &CODECAPI_AVEncMPVGOPSize,
                    VARIANT::from(1024u32),
                    "GOP size",
                );
                if !hw {
                    // The software encoder's default is tuned for quality, too slow for live.
                    set_codec(
                        c,
                        &CODECAPI_AVEncCommonQualityVsSpeed,
                        VARIANT::from(0u32),
                        "speed",
                    );
                }
            }

            let out = create_media_type()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, kbps * 1000)?;
            out.SetUINT64(
                &MF_MT_FRAME_SIZE,
                (u64::from(width) << 32) | u64::from(height),
            )?;
            out.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(fps.max(1)) << 32) | 1)?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
            mft.SetOutputType(0, &out, 0)
                .context("encoder refused the H.264 output type")?;

            let inp = create_media_type()?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            inp.SetUINT64(
                &MF_MT_FRAME_SIZE,
                (u64::from(width) << 32) | u64::from(height),
            )?;
            inp.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(fps.max(1)) << 32) | 1)?;
            inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            mft.SetInputType(0, &inp, 0)
                .context("encoder refused the NV12 input type")?;

            let info = mft.GetOutputStreamInfo(0)?;
            let provides_samples = info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                    as u32
                != 0;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            let mut enc = Self {
                desc: desc.clone(),
                mft,
                events,
                codec,
                need_input: 0,
                provides_samples,
                out_size: info.cbSize.max(width * height * 3 / 2),
                width,
                height,
                params: ParamSets::default(),
                pending: 0,
                inputs: Default::default(),
                lat: Default::default(),
            };
            enc.read_sequence_header();
            Ok(enc)
        }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// SPS/PPS from the output media type, if the encoder publishes them there.
    fn read_sequence_header(&mut self) {
        // SAFETY: plain COM calls; the blob buffer is sized by the first call.
        unsafe {
            let Ok(t) = self.mft.GetOutputCurrentType(0) else {
                return;
            };
            let Ok(n) = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) else {
                return;
            };
            let mut buf = vec![0u8; n as usize];
            if t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, None)
                .is_ok()
            {
                self.params.observe(&buf);
            }
        }
    }

    pub fn set_bitrate(&self, kbps: u32) {
        if let Some(c) = &self.codec {
            set_codec(
                c,
                &CODECAPI_AVEncCommonMeanBitRate,
                VARIANT::from(kbps * 1000),
                "bitrate",
            );
        }
    }

    fn make_sample(&self, input: Input<'_>, pts_us: u64, duration_us: u64) -> Result<IMFSample> {
        let sample = match input {
            Input::Texture(t) => texture_sample(t)?,
            Input::Nv12(bytes) => {
                let buf = create_memory_buffer(bytes.len() as u32)?;
                // SAFETY: the buffer was allocated with at least `bytes.len()` bytes.
                unsafe {
                    let mut p = std::ptr::null_mut();
                    buf.Lock(&mut p, None, None)?;
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
                    buf.Unlock()?;
                    buf.SetCurrentLength(bytes.len() as u32)?;
                }
                let s = create_sample()?;
                // SAFETY: plain COM call.
                unsafe { s.AddBuffer(&buf)? };
                s
            }
        };
        // SAFETY: plain COM calls; MF times are in 100 ns units.
        unsafe {
            sample.SetSampleTime((pts_us * 10) as i64)?;
            sample.SetSampleDuration((duration_us * 10) as i64)?;
        }
        Ok(sample)
    }

    /// Async MFTs: collect NeedInput / HaveOutput events without blocking.
    fn pump_events(&mut self, out: &mut dyn FnMut(Au)) -> Result<()> {
        let Some(ev) = self.events.clone() else {
            return Ok(());
        };
        loop {
            // SAFETY: plain COM call.
            let e = match unsafe { ev.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(e) => e,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(e) => return Err(e).context("encoder event"),
            };
            // SAFETY: plain COM call.
            let t = unsafe { e.GetType()? };
            if t == METransformNeedInput.0 as u32 {
                self.need_input += 1;
            } else if t == METransformHaveOutput.0 as u32 {
                self.process_output(out)?;
            }
        }
    }

    /// Fetch one output sample (async: after HaveOutput) or all of them (sync).
    fn process_output(&mut self, out: &mut dyn FnMut(Au)) -> Result<bool> {
        let sample = if self.provides_samples {
            None
        } else {
            let s = create_sample()?;
            let b = create_memory_buffer(self.out_size)?;
            // SAFETY: plain COM call.
            unsafe { s.AddBuffer(&b)? };
            Some(s)
        };
        let mut odb = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        // SAFETY: one valid output buffer descriptor; we take back ownership of the sample and
        // events afterwards so they are released exactly once.
        let r = unsafe { self.mft.ProcessOutput(0, &mut odb, &mut status) };
        let sample = unsafe { ManuallyDrop::take(&mut odb[0].pSample) };
        drop(unsafe { ManuallyDrop::take(&mut odb[0].pEvents) });
        match r {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // The encoder wants to restate its output type (some hardware MFTs do this
                // once at start): take the one it offers.
                // SAFETY: plain COM calls.
                unsafe {
                    let t = self.mft.GetOutputAvailableType(0, 0)?;
                    self.mft.SetOutputType(0, &t, 0)?;
                }
                self.read_sequence_header();
                return Ok(true);
            }
            Err(e) => return Err(e).context("encoder output"),
        }
        let Some(sample) = sample else {
            return Ok(true);
        };
        // SAFETY: plain COM calls; the locked range is copied before Unlock.
        let (data, pts) = unsafe {
            let buf = sample.ConvertToContiguousBuffer()?;
            let mut p = std::ptr::null_mut();
            let mut len = 0u32;
            buf.Lock(&mut p, None, Some(&mut len))?;
            let data = std::slice::from_raw_parts(p, len as usize).to_vec();
            buf.Unlock()?;
            (data, sample.GetSampleTime().unwrap_or(0).max(0) as u64 / 10)
        };
        self.pending = 0;
        self.note_latency(pts);
        if !data.is_empty() {
            let (data, keyframe) = self.params.fix(data);
            out(Au {
                data,
                pts_us: pts,
                keyframe,
            });
        }
        Ok(true)
    }

    fn note_latency(&mut self, pts_us: u64) {
        while let Some(&(p, t)) = self.inputs.front() {
            if p > pts_us {
                break;
            }
            self.inputs.pop_front();
            if p == pts_us {
                let d = t.elapsed();
                self.lat.0 += 1;
                self.lat.1 += d;
                self.lat.2 = self.lat.2.max(d);
            }
        }
        if self.lat.0 >= 300 {
            tracing::debug!(
                "encoder {}: latency avg {:.1} ms, max {:.1} ms over {} frames",
                self.desc.label,
                self.lat.1.as_secs_f64() * 1000.0 / f64::from(self.lat.0),
                self.lat.2.as_secs_f64() * 1000.0,
                self.lat.0
            );
            self.lat = Default::default();
        }
    }

    /// Encode one frame. Returns false if the encoder had no room for it (the frame is dropped,
    /// like the Linux pipeline's leaky queue drops raw frames).
    pub fn encode(
        &mut self,
        input: Input<'_>,
        pts_us: u64,
        duration_us: u64,
        keyframe: bool,
        out: &mut dyn FnMut(Au),
    ) -> Result<bool> {
        if self.events.is_some() {
            self.pump_events(out)?;
            if self.need_input == 0 {
                // A hardware encoder that is still busy: give it a moment.
                let until = Instant::now() + Duration::from_millis(15);
                while self.need_input == 0 && Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(1));
                    self.pump_events(out)?;
                }
                if self.need_input == 0 {
                    return Ok(false);
                }
            }
        }
        let sample = self.make_sample(input, pts_us, duration_us)?;
        if keyframe && let Some(c) = &self.codec {
            set_codec(
                c,
                &CODECAPI_AVEncVideoForceKeyFrame,
                VARIANT::from(1u32),
                "force keyframe",
            );
        }
        // SAFETY: plain COM call.
        unsafe { self.mft.ProcessInput(0, &sample, 0) }.context("encoder input")?;
        self.pending += 1;
        if self.inputs.len() > 64 {
            self.inputs.pop_front();
        }
        self.inputs.push_back((pts_us, Instant::now()));
        if self.events.is_some() {
            self.need_input -= 1;
            // Collect the output right away: hardware encoders take a few milliseconds.
            let until = Instant::now() + Duration::from_millis(40);
            let before = self.pending;
            while self.pending == before && Instant::now() < until {
                self.pump_events(out)?;
                if self.pending == before {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        } else {
            while self.process_output(out)? {}
        }
        Ok(true)
    }

    /// Collect outputs that arrived since the last call (async encoders).
    pub fn poll(&mut self, out: &mut dyn FnMut(Au)) -> Result<()> {
        self.pump_events(out)
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        // SAFETY: plain COM calls on a valid MFT.
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
            if let Ok(s) = self.mft.cast::<IMFShutdown>() {
                let _ = s.Shutdown();
            }
        }
    }
}

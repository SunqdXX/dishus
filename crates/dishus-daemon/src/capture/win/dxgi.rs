//! Monitors, D3D11 devices and DXGI Desktop Duplication, plus drawing the mouse pointer into the
//! captured image (Desktop Duplication delivers the pointer separately from the desktop).

use anyhow::{Context, Result, bail};
use windows::{
    Win32::{
        Foundation::{HMODULE, LUID, RECT},
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0},
            Direct3D11::*,
            Dxgi::{Common::*, *},
        },
    },
    core::Interface,
};

/// One monitor: an output attached to the desktop, and the adapter (GPU) that drives it.
pub struct Output {
    pub adapter: IDXGIAdapter1,
    pub output: IDXGIOutput1,
    /// e.g. "DISPLAY1"
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
    pub luid: LUID,
    pub vendor_id: u32,
    pub adapter_name: String,
}

pub struct Adapter {
    pub adapter: IDXGIAdapter1,
    pub name: String,
    pub vendor_id: u32,
    pub luid: LUID,
    pub outputs: usize,
}

fn wstr(w: &[u16]) -> String {
    let n = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..n])
}

fn factory() -> Result<IDXGIFactory1> {
    // SAFETY: plain factory creation.
    unsafe { CreateDXGIFactory1() }.context("creating DXGI factory")
}

/// Hardware adapters (the "Microsoft Basic Render Driver" is skipped).
pub fn adapters() -> Result<Vec<Adapter>> {
    let f = factory()?;
    let mut out = Vec::new();
    // SAFETY: enumeration stops at DXGI_ERROR_NOT_FOUND; descriptors are plain data.
    unsafe {
        for i in 0.. {
            let Ok(a) = f.EnumAdapters1(i) else { break };
            let d = a.GetDesc1()?;
            if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                continue;
            }
            let mut n = 0;
            while a.EnumOutputs(n).is_ok() {
                n += 1;
            }
            out.push(Adapter {
                name: wstr(&d.Description),
                vendor_id: d.VendorId,
                luid: d.AdapterLuid,
                outputs: n as usize,
                adapter: a,
            });
        }
    }
    Ok(out)
}

/// Monitors attached to the desktop, primary first (so monitor 0 is the main screen).
pub fn outputs() -> Result<Vec<Output>> {
    let mut out = Vec::new();
    for a in adapters()? {
        // SAFETY: as above.
        unsafe {
            for j in 0.. {
                let Ok(o) = a.adapter.EnumOutputs(j) else {
                    break;
                };
                let d = o.GetDesc()?;
                if !d.AttachedToDesktop.as_bool() {
                    continue;
                }
                let r: RECT = d.DesktopCoordinates;
                out.push(Output {
                    output: o.cast()?,
                    adapter: a.adapter.clone(),
                    name: wstr(&d.DeviceName).trim_start_matches(r"\\.\").to_string(),
                    width: (r.right - r.left).max(0) as u32,
                    height: (r.bottom - r.top).max(0) as u32,
                    primary: r.left == 0 && r.top == 0,
                    luid: a.luid,
                    vendor_id: a.vendor_id,
                    adapter_name: a.name.clone(),
                });
            }
        }
    }
    out.sort_by_key(|o| !o.primary);
    Ok(out)
}

/// A D3D11 device on `adapter` that can run the video processor and be shared with an encoder.
pub fn create_device(adapter: &IDXGIAdapter1) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut dev = None;
    let mut ctx = None;
    // SAFETY: standard device creation; out-params are Options filled on success.
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut dev),
            None,
            Some(&mut ctx),
        )
        .context("creating D3D11 device")?;
        let dev: ID3D11Device = dev.context("no device")?;
        // The encoder uses the device from its own threads.
        if let Ok(mt) = dev.cast::<ID3D11Multithread>() {
            let _ = mt.SetMultithreadProtected(true);
        }
        Ok((dev, ctx.context("no context")?))
    }
}

pub fn texture(
    dev: &ID3D11Device,
    w: u32,
    h: u32,
    format: DXGI_FORMAT,
    bind: D3D11_BIND_FLAG,
    usage: D3D11_USAGE,
    cpu: D3D11_CPU_ACCESS_FLAG,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: usage,
        BindFlags: bind.0 as u32,
        CPUAccessFlags: cpu.0 as u32,
        MiscFlags: 0,
    };
    let mut t = None;
    // SAFETY: valid descriptor; out-param filled on success.
    unsafe { dev.CreateTexture2D(&desc, None, Some(&mut t)) }
        .with_context(|| format!("creating {w}x{h} texture"))?;
    t.context("no texture")
}

/// Why acquiring a frame failed.
pub enum DupError {
    /// Mode change, full-screen switch, secure desktop (UAC, lock screen), …: recreate the
    /// duplication (possibly after waiting) and carry on.
    Lost(windows::core::Error),
    Fatal(windows::core::Error),
}

pub enum Acquired {
    Timeout,
    /// A new desktop image (copied into `desk` by the caller via [`Duplicator::frame`]) and/or
    /// a pointer change.
    Frame {
        desktop: bool,
        pointer: bool,
    },
}

pub struct Pointer {
    pub visible: bool,
    pub x: i32,
    pub y: i32,
    pub shape: Vec<u8>,
    pub info: DXGI_OUTDUPL_POINTER_SHAPE_INFO,
}

pub struct Duplicator {
    dup: IDXGIOutputDuplication,
    pub width: u32,
    pub height: u32,
    frame: Option<IDXGIResource>,
    pub pointer: Pointer,
}

impl Duplicator {
    pub fn new(output: &IDXGIOutput1, dev: &ID3D11Device) -> Result<Self> {
        // SAFETY: plain COM calls on valid interfaces.
        let dup = unsafe {
            match output.cast::<IDXGIOutput5>() {
                Ok(o5) => o5
                    .DuplicateOutput1(dev, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM])
                    .or_else(|_| output.DuplicateOutput(dev)),
                Err(_) => output.DuplicateOutput(dev),
            }
        }
        .context("Desktop Duplication")?;
        // SAFETY: plain data out of the duplication.
        let d = unsafe { dup.GetDesc() };
        if d.Rotation != DXGI_MODE_ROTATION_IDENTITY && d.Rotation != DXGI_MODE_ROTATION_UNSPECIFIED
        {
            tracing::warn!("rotated monitor: the picture will be sideways (not handled yet)");
        }
        Ok(Self {
            dup,
            width: d.ModeDesc.Width,
            height: d.ModeDesc.Height,
            frame: None,
            pointer: Pointer {
                visible: false,
                x: 0,
                y: 0,
                shape: Vec::new(),
                info: DXGI_OUTDUPL_POINTER_SHAPE_INFO::default(),
            },
        })
    }

    /// Wait up to `timeout_ms` for a desktop or pointer update. On `Frame { desktop: true }`
    /// the image is available through [`Self::copy_to`] until the next call.
    pub fn acquire(&mut self, timeout_ms: u32) -> std::result::Result<Acquired, DupError> {
        self.release();
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res = None;
        // SAFETY: plain COM call; `res` receives an owned resource we release later.
        match unsafe { self.dup.AcquireNextFrame(timeout_ms, &mut info, &mut res) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(Acquired::Timeout),
            Err(e)
                if [
                    DXGI_ERROR_ACCESS_LOST,
                    DXGI_ERROR_INVALID_CALL,
                    windows::Win32::Foundation::E_ACCESSDENIED,
                    DXGI_ERROR_SESSION_DISCONNECTED,
                    DXGI_ERROR_DEVICE_REMOVED,
                    DXGI_ERROR_DEVICE_RESET,
                ]
                .contains(&e.code()) =>
            {
                return Err(DupError::Lost(e));
            }
            Err(e) => return Err(DupError::Fatal(e)),
        }
        self.frame = res;
        let pointer = info.LastMouseUpdateTime != 0;
        if pointer {
            self.pointer.visible = info.PointerPosition.Visible.as_bool();
            self.pointer.x = info.PointerPosition.Position.x;
            self.pointer.y = info.PointerPosition.Position.y;
        }
        if info.PointerShapeBufferSize > 0 {
            let mut buf = vec![0u8; info.PointerShapeBufferSize as usize];
            let mut need = 0u32;
            let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            // SAFETY: buffer of the size DXGI asked for.
            let ok = unsafe {
                self.dup.GetFramePointerShape(
                    buf.len() as u32,
                    buf.as_mut_ptr().cast(),
                    &mut need,
                    &mut shape,
                )
            };
            if ok.is_ok() {
                self.pointer.shape = buf;
                self.pointer.info = shape;
            }
        }
        Ok(Acquired::Frame {
            desktop: info.LastPresentTime != 0 && self.frame.is_some(),
            pointer: pointer || info.PointerShapeBufferSize > 0,
        })
    }

    /// Copy the acquired desktop image into `dst` (same size, BGRA).
    pub fn copy_to(&self, ctx: &ID3D11DeviceContext, dst: &ID3D11Texture2D) -> Result<()> {
        let Some(r) = &self.frame else {
            bail!("no frame acquired")
        };
        let tex: ID3D11Texture2D = r.cast()?;
        // SAFETY: both textures belong to the same device and have the same size and format.
        unsafe { ctx.CopyResource(dst, &tex) };
        Ok(())
    }

    fn release(&mut self) {
        if self.frame.take().is_some() {
            // SAFETY: a frame is held.
            let _ = unsafe { self.dup.ReleaseFrame() };
        }
    }
}

impl Drop for Duplicator {
    fn drop(&mut self) {
        self.release();
    }
}

/// Draws the pointer into a BGRA texture through a small CPU-side staging copy of the area
/// under it (a 32..64 px square: cheaper and simpler than a shader).
pub struct CursorPainter {
    staging: Option<(ID3D11Texture2D, u32, u32)>,
}

impl CursorPainter {
    pub fn new() -> Self {
        Self { staging: None }
    }

    pub fn draw(
        &mut self,
        dev: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        target: &ID3D11Texture2D,
        (tw, th): (u32, u32),
        p: &Pointer,
    ) -> Result<()> {
        let info = &p.info;
        if !p.visible || p.shape.is_empty() || info.Width == 0 {
            return Ok(());
        }
        let mono = info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32;
        let (cw, ch) = (info.Width, if mono { info.Height / 2 } else { info.Height });
        let x0 = p.x.max(0);
        let y0 = p.y.max(0);
        let x1 = (p.x + cw as i32).min(tw as i32);
        let y1 = (p.y + ch as i32).min(th as i32);
        if x1 <= x0 || y1 <= y0 {
            return Ok(());
        }
        let (w, h) = ((x1 - x0) as u32, (y1 - y0) as u32);
        if !matches!(&self.staging, Some((_, sw, sh)) if *sw >= w && *sh >= h) {
            let t = texture(
                dev,
                cw.max(w),
                ch.max(h),
                DXGI_FORMAT_B8G8R8A8_UNORM,
                D3D11_BIND_FLAG(0),
                D3D11_USAGE_STAGING,
                D3D11_CPU_ACCESS_READ | D3D11_CPU_ACCESS_WRITE,
            )?;
            self.staging = Some((t, cw.max(w), ch.max(h)));
        }
        let (st, _, _) = self.staging.as_ref().expect("staging");
        let area = D3D11_BOX {
            left: x0 as u32,
            top: y0 as u32,
            front: 0,
            right: x1 as u32,
            bottom: y1 as u32,
            back: 1,
        };
        // SAFETY: same-device textures, boxes inside both; the mapped rows are `RowPitch` apart
        // and we only touch the w x h area we copied.
        unsafe {
            ctx.CopySubresourceRegion(st, 0, 0, 0, 0, target, 0, Some(&area));
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(st, 0, D3D11_MAP_READ_WRITE, 0, Some(&mut m))?;
            let base = m.pData.cast::<u8>();
            let pitch = m.RowPitch as usize;
            // offset of the visible part inside the cursor image
            let (ox, oy) = ((x0 - p.x) as u32, (y0 - p.y) as u32);
            for row in 0..h {
                let dst =
                    std::slice::from_raw_parts_mut(base.add(row as usize * pitch), w as usize * 4);
                blend_row(dst, p, ox, oy + row, w, mono);
            }
            ctx.Unmap(st, 0);
            let back = D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: w,
                bottom: h,
                back: 1,
            };
            ctx.CopySubresourceRegion(target, 0, x0 as u32, y0 as u32, 0, st, 0, Some(&back));
        }
        Ok(())
    }
}

impl Default for CursorPainter {
    fn default() -> Self {
        Self::new()
    }
}

/// Blend one row of the pointer (starting at pointer column `ox`, pointer row `cy`) onto `dst`
/// (BGRA pixels).
fn blend_row(dst: &mut [u8], p: &Pointer, ox: u32, cy: u32, w: u32, mono: bool) {
    let info = &p.info;
    let pitch = info.Pitch as usize;
    let s = &p.shape;
    for i in 0..w {
        let cx = ox + i;
        let d = &mut dst[i as usize * 4..i as usize * 4 + 4];
        if mono {
            // AND mask in the top half, XOR mask in the bottom half, 1 bit per pixel.
            let half = (info.Height / 2) as usize;
            let byte = cx as usize / 8;
            let bit = 0x80u8 >> (cx % 8);
            let (Some(a), Some(x)) = (
                s.get(cy as usize * pitch + byte),
                s.get((cy as usize + half) * pitch + byte),
            ) else {
                continue;
            };
            match (a & bit != 0, x & bit != 0) {
                (false, false) => d[..3].copy_from_slice(&[0, 0, 0]),
                (false, true) => d[..3].copy_from_slice(&[255, 255, 255]),
                (true, false) => {}
                (true, true) => d[..3].iter_mut().for_each(|c| *c ^= 0xFF),
            }
            continue;
        }
        let o = cy as usize * pitch + cx as usize * 4;
        let Some(src) = s.get(o..o + 4) else { continue };
        if info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32 {
            // Alpha byte is a mask: 0 = replace with the colour, 0xFF = XOR with it.
            if src[3] == 0 {
                d[..3].copy_from_slice(&src[..3]);
            } else {
                for k in 0..3 {
                    d[k] ^= src[k];
                }
            }
        } else {
            let a = u32::from(src[3]);
            for k in 0..3 {
                d[k] = ((u32::from(src[k]) * a + u32::from(d[k]) * (255 - a)) / 255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pointer(
        t: DXGI_OUTDUPL_POINTER_SHAPE_TYPE,
        w: u32,
        h: u32,
        pitch: u32,
        shape: Vec<u8>,
    ) -> Pointer {
        Pointer {
            visible: true,
            x: 0,
            y: 0,
            shape,
            info: DXGI_OUTDUPL_POINTER_SHAPE_INFO {
                Type: t.0 as u32,
                Width: w,
                Height: h,
                Pitch: pitch,
                HotSpot: Default::default(),
            },
        }
    }

    #[test]
    fn colour_pointer_alpha_blends() {
        // 2 px: opaque red, half-transparent white
        let p = pointer(
            DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR,
            2,
            1,
            8,
            vec![0, 0, 255, 255, 255, 255, 255, 128],
        );
        let mut row = vec![10, 10, 10, 255, 0, 0, 0, 255];
        blend_row(&mut row, &p, 0, 0, 2, false);
        assert_eq!(&row[..3], &[0, 0, 255]);
        assert_eq!(&row[4..7], &[128, 128, 128]);
    }

    #[test]
    fn monochrome_pointer_and_xor() {
        // 4 px wide, 1 row: AND = 0b0011, XOR = 0b0101 → black, white, keep, invert
        let p = pointer(
            DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
            4,
            2,
            1,
            vec![0b0011_0000, 0b0101_0000],
        );
        let mut row = vec![100u8; 16];
        blend_row(&mut row, &p, 0, 0, 4, true);
        assert_eq!(&row[0..3], &[0, 0, 0]);
        assert_eq!(&row[4..7], &[255, 255, 255]);
        assert_eq!(&row[8..11], &[100, 100, 100]);
        assert_eq!(&row[12..15], &[155, 155, 155]);
    }

    #[test]
    fn masked_colour_pointer_replaces_or_xors() {
        let p = pointer(
            DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
            2,
            1,
            8,
            vec![1, 2, 3, 0, 0xFF, 0, 0, 0xFF],
        );
        let mut row = vec![0x0F, 0x0F, 0x0F, 255, 0x0F, 0x0F, 0x0F, 255];
        blend_row(&mut row, &p, 0, 0, 2, false);
        assert_eq!(&row[..3], &[1, 2, 3]);
        assert_eq!(&row[4..7], &[0xF0, 0x0F, 0x0F]);
    }
}

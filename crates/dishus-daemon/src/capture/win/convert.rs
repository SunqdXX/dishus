//! BGRA → NV12 conversion and downscaling on the GPU (D3D11 video processor), into a small
//! ring of NV12 textures the encoder reads.
//!
//! The output size is fixed for the lifetime of a stream (the phone was told it in
//! `StreamInfo`). If the desktop changes size meanwhile (resolution change, full-screen game in
//! another mode), the new picture is fitted inside, keeping its aspect ratio, with black bars.

use anyhow::{Context, Result};
use windows::Win32::{
    Foundation::RECT,
    Graphics::{Direct3D11::*, Dxgi::Common::*},
};
use windows::core::Interface;

use super::dxgi::texture;

/// NV12 textures in flight: hardware encoders read their input after `ProcessInput` returns.
const RING: usize = 4;

pub struct Converter {
    /// Kept alive with the views it created.
    _vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    vp: ID3D11VideoProcessor,
    input: ID3D11VideoProcessorInputView,
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
}

/// Where a `src`-sized picture goes inside `out`, aspect kept, centred, even coordinates.
pub fn fit((sw, sh): (u32, u32), (ow, oh): (u32, u32)) -> RECT {
    if sw == 0 || sh == 0 {
        return RECT {
            left: 0,
            top: 0,
            right: ow as i32,
            bottom: oh as i32,
        };
    }
    // Same aspect (within rounding): fill exactly.
    let (w, h) = if u64::from(sw) * u64::from(oh) > u64::from(sh) * u64::from(ow) {
        (
            ow,
            ((u64::from(sh) * u64::from(ow) / u64::from(sw)) as u32) & !1,
        )
    } else {
        (
            ((u64::from(sw) * u64::from(oh) / u64::from(sh)) as u32) & !1,
            oh,
        )
    };
    let (w, h) = if ow.abs_diff(w) <= 2 && oh.abs_diff(h) <= 2 {
        (ow, oh)
    } else {
        (w, h)
    };
    let left = ((ow - w) / 2) & !1;
    let top = ((oh - h) / 2) & !1;
    RECT {
        left: left as i32,
        top: top as i32,
        right: (left + w) as i32,
        bottom: (top + h) as i32,
    }
}

/// Encoded size for a `src` picture capped at `max_height` (0 = native): same rule as the Linux
/// pipeline (square pixels, even sizes, width from the aspect ratio).
pub fn output_size((sw, sh): (u32, u32), max_height: u16) -> (u32, u32) {
    let h = u32::from(max_height) & !1;
    if h == 0 || sh <= h || sh == 0 {
        return (sw & !1, sh & !1);
    }
    let w = ((u64::from(sw) * u64::from(h) + u64::from(sh) / 2) / u64::from(sh)) as u32;
    ((w + 1) & !1, h)
}

impl Converter {
    pub fn new(
        dev: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        src: &ID3D11Texture2D,
        src_size: (u32, u32),
        out_size: (u32, u32),
    ) -> Result<Self> {
        // SAFETY: COM calls on valid interfaces; descriptors are plain data.
        unsafe {
            let vdev: ID3D11VideoDevice = dev.cast().context("no D3D11 video device")?;
            let vctx: ID3D11VideoContext = ctx.cast().context("no D3D11 video context")?;
            let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL {
                    Numerator: 60,
                    Denominator: 1,
                },
                InputWidth: src_size.0,
                InputHeight: src_size.1,
                OutputFrameRate: DXGI_RATIONAL {
                    Numerator: 60,
                    Denominator: 1,
                },
                OutputWidth: out_size.0,
                OutputHeight: out_size.1,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let en = vdev
                .CreateVideoProcessorEnumerator(&content)
                .context("video processor")?;
            let vp = vdev.CreateVideoProcessor(&en, 0)?;

            let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let mut input = None;
            vdev.CreateVideoProcessorInputView(src, &en, &in_desc, Some(&mut input))?;
            let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut outputs = Vec::with_capacity(RING);
            for _ in 0..RING {
                let t = texture(
                    dev,
                    out_size.0,
                    out_size.1,
                    DXGI_FORMAT_NV12,
                    D3D11_BIND_RENDER_TARGET,
                    D3D11_USAGE_DEFAULT,
                    D3D11_CPU_ACCESS_FLAG(0),
                )?;
                let mut v = None;
                vdev.CreateVideoProcessorOutputView(&t, &en, &out_desc, Some(&mut v))?;
                outputs.push((t, v.context("no output view")?));
            }

            // Full-range RGB in; BT.709 limited-range YCbCr out (what decoders assume for HD).
            // Bitfield: Usage | RGB_Range << 1 | YCbCr_Matrix << 2 | xvYCC << 3 | Nominal_Range << 4
            vctx.VideoProcessorSetStreamColorSpace(
                &vp,
                0,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 },
            );
            vctx.VideoProcessorSetOutputColorSpace(
                &vp,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                    _bitfield: (1 << 2) | (1 << 4),
                },
            );
            vctx.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            vctx.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);
            let src_rect = RECT {
                left: 0,
                top: 0,
                right: src_size.0 as i32,
                bottom: src_size.1 as i32,
            };
            let dst_rect = fit(src_size, out_size);
            let out_rect = RECT {
                left: 0,
                top: 0,
                right: out_size.0 as i32,
                bottom: out_size.1 as i32,
            };
            vctx.VideoProcessorSetStreamSourceRect(&vp, 0, true, Some(&src_rect));
            vctx.VideoProcessorSetStreamDestRect(&vp, 0, true, Some(&dst_rect));
            vctx.VideoProcessorSetOutputTargetRect(&vp, true, Some(&out_rect));
            vctx.VideoProcessorSetOutputBackgroundColor(
                &vp,
                false,
                &D3D11_VIDEO_COLOR {
                    Anonymous: D3D11_VIDEO_COLOR_0 {
                        RGBA: D3D11_VIDEO_COLOR_RGBA {
                            R: 0.0,
                            G: 0.0,
                            B: 0.0,
                            A: 1.0,
                        },
                    },
                },
            );
            Ok(Self {
                _vdev: vdev,
                vctx,
                vp,
                input: input.context("no input view")?,
                outputs,
                next: 0,
            })
        }
    }

    /// Convert the source texture into the next NV12 texture of the ring and return it.
    pub fn convert(&mut self) -> Result<&ID3D11Texture2D> {
        let (tex, view) = &self.outputs[self.next];
        self.next = (self.next + 1) % self.outputs.len();
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(self.input.clone())),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        let mut streams = [stream];
        // SAFETY: one stream descriptor whose input view stays alive for the call; the cloned
        // reference in it is released right after.
        let r = unsafe { self.vctx.VideoProcessorBlt(&self.vp, view, 0, &streams) };
        unsafe { std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        r.context("video processor (BGRA to NV12)")?;
        Ok(tex)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_size_matches_linux_rule() {
        assert_eq!(output_size((2560, 1440), 0), (2560, 1440));
        assert_eq!(output_size((2560, 1440), 1440), (2560, 1440));
        assert_eq!(output_size((1920, 1080), 720), (1280, 720));
        assert_eq!(output_size((2560, 1600), 720), (1152, 720));
        assert_eq!(output_size((2560, 1080), 720), (1708, 720));
    }

    #[test]
    fn fit_keeps_aspect_with_bars() {
        let full = |w: u32, h: u32| RECT {
            left: 0,
            top: 0,
            right: w as i32,
            bottom: h as i32,
        };
        assert_eq!(fit((2560, 1600), (1152, 720)), full(1152, 720));
        // 16:10 stream, desktop became 16:9: bars top and bottom
        let r = fit((1920, 1080), (1152, 720));
        assert_eq!((r.left, r.right), (0, 1152));
        assert_eq!((r.top, r.bottom), (36, 684));
        // 4:3 desktop: bars left and right
        let r = fit((1024, 768), (1152, 720));
        assert_eq!((r.right - r.left, r.bottom - r.top), (960, 720));
        assert_eq!(r.left, 96);
    }
}

//! The capture worker on Windows: DXGI Desktop Duplication → D3D11 video processor (BGRA to
//! NV12, scaling) → Media Foundation H.264 encoder. Everything used ships with Windows.
//! Sound: WASAPI loopback → libopus ([`audio`]).
//!
//! Win32/COM calls need `unsafe`; it is allowed in this module only (see docs/SECURITY.md).
#![allow(unsafe_code)]

pub mod audio;
mod convert;
pub mod dxgi;
pub mod gpupref;
pub mod mf;
mod nal;
pub mod pipeline;
pub mod worker;

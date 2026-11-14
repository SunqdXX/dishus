//! DiShus client core, shared by the Android app (through UniFFI, feature `ffi`) and the
//! desktop test client.

pub mod client;
pub mod vault;

#[cfg(feature = "ffi")]
pub mod ffi;

#[cfg(target_os = "android")]
mod android;

#[cfg(feature = "ffi")]
uniffi::setup_scaffolding!();

pub use client::{Client, NetConfig, Refusal, Session, VideoPacket};
pub use dishus_proto as proto;
pub use vault::{LoginScreen, PairedPc, VaultData};

//! DiShus PC daemon library. The binary in `main.rs` wires these together; integration tests
//! drive the same code with fake capture/input backends.

pub mod abr;
pub mod backend;
pub mod capture;
pub mod config;
pub mod devices;
#[cfg(feature = "capture")]
pub mod doctor;
pub mod gate;
pub mod identity;
pub mod ipc;
pub mod net;
pub mod notify;
pub mod pairing;
pub mod paths;
pub mod session;
pub mod state;

#[cfg(feature = "uinput")]
pub mod input;

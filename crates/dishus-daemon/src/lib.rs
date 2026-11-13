//! DiShus PC daemon library. The binary in `main.rs` wires these together; integration tests
//! drive the same code with fake capture/input backends.

pub mod abr;
pub mod audio;
pub mod backend;
pub mod capture;
pub mod config;
pub mod devices;
#[cfg(all(feature = "capture", not(windows)))]
pub mod doctor;
#[cfg(all(feature = "capture", windows))]
#[path = "doctor_win.rs"]
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

#[cfg(all(feature = "uinput", not(windows)))]
pub mod input;
#[cfg(all(feature = "sendinput", windows))]
#[path = "input_win.rs"]
pub mod input;
#[cfg(windows)]
pub mod winsys;

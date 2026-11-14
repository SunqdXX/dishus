//! Phone → PC file send (session/3 only).
//!
//! One bidirectional QUIC stream per file, opened by the phone while the session runs:
//!
//! ```text
//! phone: FileOffer                      PC: FileAnswer (Accept / Refuse)
//! phone: exactly `size` raw bytes, FIN  PC: FileResult (once the file is in place)
//! ```
//!
//! Frames use the control-stream framing ([`crate::write_frame`]) with [`MAX_FILE_FRAME`]. The
//! name in the offer is only a wish: the PC cleans it up, never overwrites anything, and says
//! what it really saved the file as. The phone cancels by resetting its send side with
//! [`CANCELLED`]; the PC then deletes what it had written.

use serde::{Deserialize, Serialize};

use crate::frame::Validate;

/// Cap on one frame of the file-send exchange.
pub const MAX_FILE_FRAME: usize = 2048;
/// Longest file name the phone may offer (bytes). The PC shortens names further.
pub const MAX_OFFER_NAME: usize = 1024;
/// Longest name the PC reports back (bytes).
pub const MAX_SAVED_NAME: usize = 255;
/// QUIC stream error code: the phone cancelled the transfer.
pub const CANCELLED: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileOffer {
    pub name: String,
    pub size: u64,
}

impl Validate for FileOffer {
    fn validate(&self) -> Result<(), &'static str> {
        if self.name.is_empty() || self.name.len() > MAX_OFFER_NAME {
            Err("bad file name length")
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileRefusal {
    /// `files.enabled = false` on the PC (the default).
    Disabled,
    TooBig {
        max_bytes: u64,
    },
    /// Not enough free disk space on the PC.
    NoSpace,
    /// Another file is still being received.
    Busy,
    /// The PC could not create the file.
    Failed,
}

impl std::fmt::Display for FileRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileRefusal::Disabled => f.write_str("file sending is off on this PC"),
            FileRefusal::TooBig { max_bytes } => {
                write!(
                    f,
                    "too big: this PC takes files up to {} MB",
                    max_bytes >> 20
                )
            }
            FileRefusal::NoSpace => f.write_str("not enough free space on the PC"),
            FileRefusal::Busy => f.write_str("the PC is still receiving another file"),
            FileRefusal::Failed => f.write_str("the PC could not save the file"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileAnswer {
    Accept,
    Refuse(FileRefusal),
}

impl Validate for FileAnswer {
    fn validate(&self) -> Result<(), &'static str> {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileResult {
    /// Saved in the PC's DiShus folder under this name.
    Saved {
        name: String,
    },
    Failed,
}

impl Validate for FileResult {
    fn validate(&self) -> Result<(), &'static str> {
        match self {
            FileResult::Saved { name }
                if name.is_empty()
                    || name.len() > MAX_SAVED_NAME
                    || name.chars().any(char::is_control) =>
            {
                Err("bad saved name")
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds() {
        let o = |n: &str| FileOffer {
            name: n.into(),
            size: 1,
        };
        assert!(o("a.txt").validate().is_ok());
        // Odd names are the PC's job to clean up, not a protocol error.
        assert!(o("../../x").validate().is_ok());
        assert!(o("").validate().is_err());
        assert!(o(&"x".repeat(MAX_OFFER_NAME + 1)).validate().is_err());
        let saved = |n: &str| FileResult::Saved { name: n.into() };
        assert!(saved("a (1).txt").validate().is_ok());
        assert!(saved("a\nb").validate().is_err());
        assert!(saved("").validate().is_err());
        assert!(FileResult::Failed.validate().is_ok());
    }

    #[test]
    fn refusal_text() {
        assert_eq!(
            FileRefusal::Disabled.to_string(),
            "file sending is off on this PC"
        );
        assert_eq!(
            FileRefusal::TooBig {
                max_bytes: 2048 << 20
            }
            .to_string(),
            "too big: this PC takes files up to 2048 MB"
        );
    }
}

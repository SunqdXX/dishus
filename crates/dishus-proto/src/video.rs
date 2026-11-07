//! Header of a video frame stream. Fixed 24 bytes, big-endian:
//!
//! ```text
//! magic "DSV1" | seq u64 | pts_us u64 | flags u8 | monitor u8 | reserved u16
//! ```
//!
//! followed by one H.264 Annex-B access unit (SPS/PPS precede every IDR).

pub const VIDEO_MAGIC: [u8; 4] = *b"DSV1";
pub const VIDEO_HEADER_LEN: usize = 24;

pub const FLAG_KEYFRAME: u8 = 0b0000_0001;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoHeader {
    /// Monotonic per session; gaps mean the sender dropped frames (and the next frame is an IDR).
    pub seq: u64,
    /// Capture timestamp, microseconds on the PC's monotonic clock.
    pub pts_us: u64,
    pub flags: u8,
    pub monitor: u8,
}

impl VideoHeader {
    pub fn is_keyframe(&self) -> bool {
        self.flags & FLAG_KEYFRAME != 0
    }

    pub fn encode(&self) -> [u8; VIDEO_HEADER_LEN] {
        let mut b = [0u8; VIDEO_HEADER_LEN];
        b[0..4].copy_from_slice(&VIDEO_MAGIC);
        b[4..12].copy_from_slice(&self.seq.to_be_bytes());
        b[12..20].copy_from_slice(&self.pts_us.to_be_bytes());
        b[20] = self.flags;
        b[21] = self.monitor;
        b
    }

    /// Parse a header from the start of `buf`; returns the header and the payload.
    pub fn decode(buf: &[u8]) -> Result<(Self, &[u8]), &'static str> {
        if buf.len() < VIDEO_HEADER_LEN {
            return Err("short video frame");
        }
        if buf[0..4] != VIDEO_MAGIC {
            return Err("bad video magic");
        }
        if buf[20] & !FLAG_KEYFRAME != 0 {
            return Err("unknown video flags");
        }
        if buf[22] != 0 || buf[23] != 0 {
            return Err("reserved bytes set");
        }
        let h = VideoHeader {
            seq: u64::from_be_bytes(buf[4..12].try_into().expect("8 bytes")),
            pts_us: u64::from_be_bytes(buf[12..20].try_into().expect("8 bytes")),
            flags: buf[20],
            monitor: buf[21],
        };
        let payload = &buf[VIDEO_HEADER_LEN..];
        if payload.is_empty() {
            return Err("empty video payload");
        }
        Ok((h, payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let h = VideoHeader { seq: 7, pts_us: 123_456, flags: FLAG_KEYFRAME, monitor: 1 };
        let mut buf = h.encode().to_vec();
        buf.extend_from_slice(&[0, 0, 0, 1, 0x65]);
        let (h2, p) = VideoHeader::decode(&buf).unwrap();
        assert_eq!(h, h2);
        assert!(h2.is_keyframe());
        assert_eq!(p, &[0, 0, 0, 1, 0x65]);
    }

    #[test]
    fn rejects_bad() {
        assert!(VideoHeader::decode(&[0; 10]).is_err());
        let h = VideoHeader { seq: 1, pts_us: 1, flags: 0, monitor: 0 };
        let mut buf = h.encode().to_vec();
        assert!(VideoHeader::decode(&buf).is_err(), "empty payload");
        buf.push(1);
        buf[0] = b'X';
        assert!(VideoHeader::decode(&buf).is_err(), "magic");
        buf[0] = b'D';
        buf[20] = 0x80;
        assert!(VideoHeader::decode(&buf).is_err(), "flags");
    }
}

//! PC audio, one QUIC datagram per Opus packet (session/2 only). Fixed 16-byte header,
//! big-endian:
//!
//! ```text
//! magic "DSA1" | seq u32 | pts_us u64
//! ```
//!
//! followed by one Opus packet: 48 kHz stereo, 20 ms. Datagrams are never retransmitted, so a
//! lost packet never delays the next one; the phone conceals gaps. `pts_us` is on the same PC
//! clock as [`crate::VideoHeader::pts_us`], so the phone can tell how far sound and picture are
//! apart.

pub const AUDIO_MAGIC: [u8; 4] = *b"DSA1";
pub const AUDIO_HEADER_LEN: usize = 16;
/// Largest Opus packet (RFC 6716).
pub const MAX_OPUS_PACKET: usize = 1275;
pub const AUDIO_SAMPLE_RATE: u32 = 48_000;
pub const AUDIO_CHANNELS: u8 = 2;
pub const AUDIO_FRAME_MS: u32 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioHeader {
    /// +1 per packet the PC encoded; gaps are packets lost or dropped under congestion.
    pub seq: u32,
    /// Capture timestamp, microseconds on the PC's monotonic clock.
    pub pts_us: u64,
}

impl AudioHeader {
    pub fn encode(&self) -> [u8; AUDIO_HEADER_LEN] {
        let mut b = [0u8; AUDIO_HEADER_LEN];
        b[0..4].copy_from_slice(&AUDIO_MAGIC);
        b[4..8].copy_from_slice(&self.seq.to_be_bytes());
        b[8..16].copy_from_slice(&self.pts_us.to_be_bytes());
        b
    }

    /// Parse a datagram; returns the header and the Opus packet.
    pub fn decode(buf: &[u8]) -> Result<(Self, &[u8]), &'static str> {
        if buf.len() <= AUDIO_HEADER_LEN {
            return Err("short audio packet");
        }
        if buf[0..4] != AUDIO_MAGIC {
            return Err("bad audio magic");
        }
        let payload = &buf[AUDIO_HEADER_LEN..];
        if payload.len() > MAX_OPUS_PACKET {
            return Err("audio packet too large");
        }
        let seq = u32::from_be_bytes(buf[4..8].try_into().expect("4 bytes"));
        let pts_us = u64::from_be_bytes(buf[8..16].try_into().expect("8 bytes"));
        Ok((Self { seq, pts_us }, payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_rejects() {
        let h = AudioHeader {
            seq: 7,
            pts_us: 123_456_789,
        };
        let mut buf = h.encode().to_vec();
        buf.extend_from_slice(&[0xFC, 1, 2]);
        let (d, p) = AudioHeader::decode(&buf).unwrap();
        assert_eq!((d, p), (h, &[0xFC, 1, 2][..]));

        assert!(AudioHeader::decode(&h.encode()).is_err(), "no payload");
        let mut bad = buf.clone();
        bad[0] = b'X';
        assert!(AudioHeader::decode(&bad).is_err(), "magic");
        let mut big = h.encode().to_vec();
        big.resize(AUDIO_HEADER_LEN + MAX_OPUS_PACKET, 0);
        assert!(AudioHeader::decode(&big).is_ok());
        big.push(0);
        assert!(AudioHeader::decode(&big).is_err(), "over the Opus maximum");
    }
}

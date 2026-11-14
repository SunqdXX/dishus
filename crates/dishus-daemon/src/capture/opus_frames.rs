//! Raw PCM to 20 ms Opus packets, for captures that hand over plain samples (Windows WASAPI;
//! the Linux capture encodes inside GStreamer instead). 48 kHz, stereo, 16-bit interleaved.
//!
//! libopus comes from the `opus` crate, built from source (Windows has no Opus encoder).

use anyhow::{Context, Result};
use dishus_proto::audio::{AUDIO_FRAME_MS, AUDIO_SAMPLE_RATE, MAX_OPUS_PACKET};

/// Samples per channel in one packet (960 at 48 kHz).
pub const FRAME: usize = (AUDIO_SAMPLE_RATE as usize) * (AUDIO_FRAME_MS as usize) / 1000;
const CHANNELS: usize = 2;
const FRAME_US: u64 = AUDIO_FRAME_MS as u64 * 1000;
/// Same limits as the Linux encoder.
pub const KBPS_RANGE: (u32, u32) = (16, 128);
/// A new chunk this far from where the buffered audio ends is a gap (the capture skipped, or
/// nothing played for a while): the partial packet is dropped and timing starts again.
const JUMP_US: u64 = 40_000;

pub struct OpusFrames {
    enc: opus::Encoder,
    buf: Vec<i16>,
    /// Timestamp of `buf[0]`, microseconds.
    buf_pts_us: u64,
    out: Vec<u8>,
}

impl OpusFrames {
    pub fn new(kbps: u32) -> Result<Self> {
        let mut enc = opus::Encoder::new(
            AUDIO_SAMPLE_RATE,
            opus::Channels::Stereo,
            opus::Application::Audio,
        )
        .context("creating the Opus encoder")?;
        // Same choices as the Linux pipeline: forward error correction for a lossy link.
        enc.set_inband_fec(true).context("Opus FEC")?;
        enc.set_packet_loss_perc(10).context("Opus loss estimate")?;
        let mut me = Self {
            enc,
            buf: Vec::with_capacity(FRAME * CHANNELS * 4),
            buf_pts_us: 0,
            out: vec![0; MAX_OPUS_PACKET],
        };
        me.set_bitrate(kbps);
        Ok(me)
    }

    pub fn set_bitrate(&mut self, kbps: u32) {
        let k = kbps.clamp(KBPS_RANGE.0, KBPS_RANGE.1);
        if let Err(e) = self.enc.set_bitrate(opus::Bitrate::Bits((k * 1000) as i32)) {
            tracing::warn!("Opus bitrate {k} kbps: {e}");
        }
    }

    /// `pcm`: interleaved stereo samples, the first one captured at `pts_us`. Calls `emit` with
    /// (pts, packet) for every complete 20 ms frame.
    pub fn push(
        &mut self,
        pcm: &[i16],
        pts_us: u64,
        mut emit: impl FnMut(u64, Vec<u8>),
    ) -> Result<()> {
        if self.buf.is_empty() {
            self.buf_pts_us = pts_us;
        } else {
            let buffered_us =
                (self.buf.len() / CHANNELS) as u64 * 1_000_000 / AUDIO_SAMPLE_RATE as u64;
            let expected = self.buf_pts_us + buffered_us;
            if pts_us.abs_diff(expected) > JUMP_US {
                self.buf.clear();
                self.buf_pts_us = pts_us;
            }
        }
        self.buf.extend_from_slice(pcm);
        let n = FRAME * CHANNELS;
        let mut done = 0;
        while self.buf.len() - done >= n {
            let len = self
                .enc
                .encode(&self.buf[done..done + n], &mut self.out)
                .context("Opus encode")?;
            emit(self.buf_pts_us, self.out[..len].to_vec());
            self.buf_pts_us += FRAME_US;
            done += n;
        }
        self.buf.drain(..done);
        Ok(())
    }
}

/// A quiet 440 Hz tone for `DISHUS_TEST_SOURCE` (no sound device needed): fills `out` with the
/// next samples. `phase` carries on between calls.
pub fn test_tone(out: &mut [i16], phase: &mut u64) {
    for f in out.as_chunks_mut::<CHANNELS>().0 {
        let t = *phase as f64 / AUDIO_SAMPLE_RATE as f64;
        let v = ((t * 440.0 * std::f64::consts::TAU).sin() * 0.1 * i16::MAX as f64) as i16;
        f.fill(v);
        *phase += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize, phase: &mut u64) -> Vec<i16> {
        let mut v = vec![0i16; frames * CHANNELS];
        test_tone(&mut v, phase);
        v
    }

    #[test]
    fn odd_chunks_become_20ms_packets_with_steady_timestamps() {
        let mut f = OpusFrames::new(64).unwrap();
        let mut phase = 0;
        let mut got: Vec<(u64, Vec<u8>)> = Vec::new();
        // 0.5 s in WASAPI-like chunks of 441 frames (9.1875 ms).
        let mut pts = 5_000_000u64;
        let mut sent = 0;
        while sent < 24_000 {
            let n = 441.min(24_000 - sent);
            f.push(&tone(n, &mut phase), pts, |p, d| got.push((p, d)))
                .unwrap();
            pts += n as u64 * 1_000_000 / 48_000;
            sent += n;
        }
        assert_eq!(got.len(), 25, "0.5 s = 25 packets");
        for (i, (p, d)) in got.iter().enumerate() {
            assert_eq!(*p, 5_000_000 + i as u64 * 20_000);
            assert!(!d.is_empty() && d.len() <= MAX_OPUS_PACKET);
        }
        // It decodes back to sound, not silence.
        let mut dec = opus::Decoder::new(48_000, opus::Channels::Stereo).unwrap();
        let mut pcm = vec![0i16; FRAME * CHANNELS];
        let mut peak = 0i16;
        for (_, d) in &got[5..] {
            let n = dec.decode(d, &mut pcm, false).unwrap();
            assert_eq!(n, FRAME);
            peak = peak.max(pcm.iter().map(|s| s.saturating_abs()).max().unwrap());
        }
        assert!(peak > 2000, "peak {peak}");
    }

    #[test]
    fn a_gap_restarts_the_timing() {
        let mut f = OpusFrames::new(64).unwrap();
        let mut phase = 0;
        let mut got = Vec::new();
        f.push(&tone(1500, &mut phase), 1_000_000, |p, _| got.push(p))
            .unwrap();
        assert_eq!(got, [1_000_000]); // 540 frames left over
        // Next chunk arrives 1 s later (nothing played meanwhile): the leftover is dropped.
        f.push(&tone(960, &mut phase), 2_031_250, |p, _| got.push(p))
            .unwrap();
        assert_eq!(got, [1_000_000, 2_031_250]);
        // Contiguous chunks keep counting from there.
        f.push(&tone(960, &mut phase), 2_051_250, |p, _| got.push(p))
            .unwrap();
        assert_eq!(got, [1_000_000, 2_031_250, 2_051_250]);
    }

    #[test]
    fn bitrate_changes_packet_size() {
        let mut phase = 0;
        let pcm = tone(48_000, &mut phase);
        let size = |kbps| {
            let mut f = OpusFrames::new(kbps).unwrap();
            let mut total = 0;
            f.push(&pcm, 0, |_, d| total += d.len()).unwrap();
            total
        };
        let (low, high) = (size(24), size(64));
        assert!(low * 2 < high, "24 kbps {low} B/s vs 64 kbps {high} B/s");
    }
}

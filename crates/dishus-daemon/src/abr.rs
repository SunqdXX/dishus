//! Adaptive bitrate / fps controller. Pure logic, driven by a 500 ms tick.
//!
//! Signals of congestion: the sender had to drop frames (too many unacknowledged frames in
//! flight) or a frame stream stalled past its deadline. Back off multiplicatively, probe up
//! slowly. In adaptive-fps mode, start at 30 fps and go to 60 only after a sustained clean
//! period at a decent bitrate; fall back to 30 at the first sign of trouble.

use dishus_proto::Quality;

pub const TICK_MS: u64 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    pub min: u32,
    pub start: u32,
    pub max: u32,
}

pub fn bounds(q: Quality, ceiling: u32) -> Bounds {
    let (min, start, max) = match q {
        Quality::Low => (300, 1_500, 3_000),
        Quality::Medium => (500, 4_000, 8_000),
        Quality::High => (1_000, 8_000, u32::MAX),
        Quality::Auto => (500, 6_000, u32::MAX),
    };
    let max = max.min(ceiling).max(min);
    Bounds { min, start: start.min(max), max }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TickStats {
    pub dropped: u32,
    pub stalled: u32,
    pub peak_inflight: usize,
}

#[derive(Debug)]
pub struct Abr {
    b: Bounds,
    pub bitrate: u32,
    /// None = fixed fps chosen by the phone
    adaptive_fps: bool,
    pub fps: u8,
    good_ticks: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Change {
    pub bitrate: Option<u32>,
    pub fps: Option<u8>,
}

impl Abr {
    /// `fps == 0` means adaptive.
    pub fn new(b: Bounds, fps: u8) -> Self {
        let adaptive_fps = fps == 0;
        Self { b, bitrate: b.start, adaptive_fps, fps: if adaptive_fps { 30 } else { fps }, good_ticks: 0 }
    }

    pub fn tick(&mut self, s: TickStats) -> Change {
        let (old_br, old_fps) = (self.bitrate, self.fps);
        let congested = s.dropped > 0 || s.stalled > 0 || s.peak_inflight >= 3;
        if congested {
            self.good_ticks = 0;
            self.bitrate = ((self.bitrate as u64 * 7 / 10) as u32).max(self.b.min);
            if self.adaptive_fps {
                self.fps = 30;
            }
        } else {
            self.good_ticks = self.good_ticks.saturating_add(1);
            if self.good_ticks % 4 == 0 {
                self.bitrate = ((self.bitrate as u64 * 115 / 100) as u32).min(self.b.max);
            }
            if self.adaptive_fps
                && self.fps < 60
                && self.good_ticks >= 10
                && self.bitrate as u64 * 2 >= self.b.max.min(20_000) as u64
            {
                self.fps = 60;
            }
        }
        Change {
            bitrate: (self.bitrate != old_br).then_some(self.bitrate),
            fps: (self.fps != old_fps).then_some(self.fps),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backs_off_and_recovers() {
        let mut a = Abr::new(bounds(Quality::Auto, 20_000), 0);
        assert_eq!((a.bitrate, a.fps), (6_000, 30));
        let c = a.tick(TickStats { dropped: 2, ..Default::default() });
        assert_eq!(c.bitrate, Some(4_200));
        for _ in 0..20 {
            a.tick(TickStats { dropped: 5, ..Default::default() });
        }
        assert_eq!(a.bitrate, 500, "floors at min");
        for _ in 0..200 {
            a.tick(TickStats::default());
        }
        assert_eq!(a.bitrate, 20_000, "caps at max");
        assert_eq!(a.fps, 60, "adaptive fps climbs to 60 when clean");
        let c = a.tick(TickStats { stalled: 1, ..Default::default() });
        assert_eq!(c.fps, Some(30), "drops to 30 on trouble");
    }

    #[test]
    fn fixed_fps_never_changes() {
        let mut a = Abr::new(bounds(Quality::High, 20_000), 45);
        for _ in 0..100 {
            assert_eq!(a.tick(TickStats::default()).fps, None);
        }
        assert_eq!(a.tick(TickStats { dropped: 1, ..Default::default() }).fps, None);
        assert_eq!(a.fps, 45);
    }

    #[test]
    fn bounds_respect_ceiling() {
        let b = bounds(Quality::High, 2_000);
        assert_eq!(b, Bounds { min: 1_000, start: 2_000, max: 2_000 });
        let b = bounds(Quality::Low, 100);
        assert!(b.max >= b.min);
    }
}

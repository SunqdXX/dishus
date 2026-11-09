//! Frame-rate cap shared by the capture backends (a GStreamer pad probe on Linux, the Desktop
//! Duplication loop on Windows).

use std::sync::{
    Mutex,
    atomic::{AtomicU32, Ordering},
};

/// Frame-rate cap, applied to raw frames before the encoder.
///
/// `videorate max-rate` can't do this: PipeWire screen capture announces a *variable*
/// framerate (`framerate=0/1`), and videorate then passes every frame through. On a 180 Hz
/// monitor that meant ~90-180 frames/s reached the sender against a 30 fps target, which
/// filled the in-flight window, forced drops and a storm of keyframe requests.
///
/// Admission is by presentation time against a running "next due" clock, with a quarter
/// interval of slack so source jitter (a 60 Hz frame landing at 33.0 ms for a 33.3 ms slot)
/// is not dropped. The clock advances by exactly one interval per admitted frame, so the
/// average rate converges on the target instead of drifting above it.
pub struct Throttle {
    fps: AtomicU32,
    next_due_us: Mutex<Option<u64>>,
}

impl Throttle {
    pub fn new(fps: u8) -> Self {
        Self {
            fps: AtomicU32::new(u32::from(fps.clamp(1, 120))),
            next_due_us: Mutex::new(None),
        }
    }
    pub fn set_fps(&self, fps: u8) {
        self.fps
            .store(u32::from(fps.clamp(1, 120)), Ordering::Relaxed);
    }
    /// Whether the frame presented at `pts_us` should be encoded.
    pub fn admit(&self, pts_us: u64) -> bool {
        let interval = 1_000_000 / u64::from(self.fps.load(Ordering::Relaxed).max(1));
        let slack = interval / 4;
        let Ok(mut due) = self.next_due_us.lock() else {
            return true;
        };
        match *due {
            // Clock went backwards (before the last admitted frame): start over from here.
            Some(d) if pts_us + 2 * interval < d => {
                *due = Some(pts_us + interval);
                true
            }
            Some(d) if pts_us + slack < d => false,
            Some(d) => {
                // Behind by more than one slot (stall, fps raised): resync on this frame
                // rather than letting a burst through to "catch up".
                let base = if pts_us > d + interval { pts_us } else { d };
                *due = Some(base + interval);
                true
            }
            None => {
                *due = Some(pts_us + interval);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds `n` frames at `hz` (with optional +/- jitter alternating) and counts admissions.
    fn admitted(t: &Throttle, hz: f64, secs: f64, jitter_us: i64) -> usize {
        let n = (hz * secs) as usize;
        (0..n)
            .filter(|&i| {
                let j = if i % 2 == 0 { jitter_us } else { -jitter_us };
                let pts = (1_000_000.0 * i as f64 / hz) as i64 + j + 1_000_000;
                t.admit(pts as u64)
            })
            .count()
    }

    #[test]
    fn throttle_caps_fast_sources_to_target() {
        // The real case: a 180 Hz monitor, variable framerate, 30 fps wanted.
        let n = admitted(&Throttle::new(30), 180.0, 10.0, 0);
        assert!(
            (299..=301).contains(&n),
            "180 Hz -> 30 fps admitted {n} in 10 s"
        );
        let n = admitted(&Throttle::new(60), 144.0, 10.0, 0);
        assert!(
            (595..=601).contains(&n),
            "144 Hz -> 60 fps admitted {n} in 10 s"
        );
    }

    #[test]
    fn throttle_passes_matching_and_slower_sources_despite_jitter() {
        // 60 Hz with 60 fps wanted, frames +/- 2 ms off their slots: keep them all.
        assert_eq!(admitted(&Throttle::new(60), 60.0, 10.0, 2_000), 600);
        // A 30 Hz source never gets cut further by a 60 fps cap.
        assert_eq!(admitted(&Throttle::new(60), 30.0, 10.0, 1_000), 300);
        // 60 Hz to 30 fps: every second frame, exactly.
        assert_eq!(admitted(&Throttle::new(30), 60.0, 10.0, 1_000), 300);
    }

    #[test]
    fn throttle_follows_fps_changes_and_stalls() {
        let t = Throttle::new(30);
        assert!((299..=301).contains(&admitted(&t, 120.0, 10.0, 0)));
        t.set_fps(60);
        // Same timestamps restart from 0: treated as a clock jump, resynced, no burst.
        let n = admitted(&t, 120.0, 10.0, 0);
        assert!((599..=601).contains(&n), "after raise admitted {n}");
        // A 5 s stall must not let a catch-up burst through.
        assert!(t.admit(100_000_000));
        assert!(!t.admit(100_001_000));
        assert!(t.admit(100_016_700));
    }
}

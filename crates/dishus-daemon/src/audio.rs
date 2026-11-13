//! When PC audio may use the link. Pure logic, driven by the video controller's state.
//!
//! Audio is lower priority than input and video. QUIC sends datagrams ahead of stream data in
//! every packet, so the transport won't hold audio back; this does:
//! * the video controller sees congestion: Opus drops to [`LOW_KBPS`], back up after
//!   [`UP_AFTER_TICKS`] clean ticks;
//! * congestion with video already at its minimum bitrate: audio packets are dropped;
//! * the datagram queue holds more than [`MAX_QUEUED_BYTES`] (about 100 ms): dropped, so a
//!   backlog never turns into delay.

use std::sync::atomic::{AtomicU8, Ordering};

pub const HIGH_KBPS: u32 = 64;
pub const LOW_KBPS: u32 = 24;
/// Clean 500 ms ticks before going back to [`HIGH_KBPS`] (10 s).
pub const UP_AFTER_TICKS: u32 = 20;
/// About 100 ms of audio at [`HIGH_KBPS`], headers included.
pub const MAX_QUEUED_BYTES: usize = 1200;
/// noq's default datagram send buffer (`TransportConfig::datagram_send_buffer_size`); the
/// endpoint doesn't change it. Queued bytes = this minus the free space.
pub const DATAGRAM_SEND_BUFFER: usize = 1024 * 1024;

/// The video controller's view of the link at its last tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Pressure {
    Clean = 0,
    /// Congested; video backed off.
    Congested = 1,
    /// Congested with video at its minimum bitrate: nothing left to give.
    Starved = 2,
}

impl Pressure {
    pub fn store(self, a: &AtomicU8) {
        a.store(self as u8, Ordering::Relaxed);
    }
    pub fn load(a: &AtomicU8) -> Self {
        match a.load(Ordering::Relaxed) {
            0 => Pressure::Clean,
            1 => Pressure::Congested,
            _ => Pressure::Starved,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dropped {
    Congestion,
    Queue,
}

/// May this packet be sent now? `queued`: bytes waiting in the datagram queue.
pub fn admit(p: Pressure, queued: usize) -> Result<(), Dropped> {
    if p == Pressure::Starved {
        Err(Dropped::Congestion)
    } else if queued > MAX_QUEUED_BYTES {
        Err(Dropped::Queue)
    } else {
        Ok(())
    }
}

#[derive(Debug)]
pub struct AudioPolicy {
    pub kbps: u32,
    clean: u32,
}

impl Default for AudioPolicy {
    fn default() -> Self {
        Self {
            kbps: HIGH_KBPS,
            clean: 0,
        }
    }
}

impl AudioPolicy {
    /// Once per video tick; returns a new Opus bitrate to set.
    pub fn tick(&mut self, p: Pressure) -> Option<u32> {
        let old = self.kbps;
        if p == Pressure::Clean {
            self.clean = self.clean.saturating_add(1);
            if self.clean >= UP_AFTER_TICKS {
                self.kbps = HIGH_KBPS;
            }
        } else {
            self.clean = 0;
            self.kbps = LOW_KBPS;
        }
        (self.kbps != old).then_some(self.kbps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_down_at_once_and_up_slowly() {
        let mut a = AudioPolicy::default();
        assert_eq!(a.tick(Pressure::Clean), None);
        assert_eq!(a.tick(Pressure::Congested), Some(LOW_KBPS));
        assert_eq!(a.tick(Pressure::Starved), None, "already low");
        for _ in 0..UP_AFTER_TICKS - 1 {
            assert_eq!(a.tick(Pressure::Clean), None);
        }
        assert_eq!(a.tick(Pressure::Clean), Some(HIGH_KBPS));
        // one hiccup restarts the wait
        a.tick(Pressure::Congested);
        for _ in 0..UP_AFTER_TICKS / 2 {
            a.tick(Pressure::Clean);
        }
        a.tick(Pressure::Congested);
        for _ in 0..UP_AFTER_TICKS - 1 {
            a.tick(Pressure::Clean);
        }
        assert_eq!(a.kbps, LOW_KBPS);
    }

    #[test]
    fn drops_when_video_has_nothing_left_or_the_queue_backs_up() {
        assert_eq!(admit(Pressure::Clean, 0), Ok(()));
        assert_eq!(admit(Pressure::Congested, MAX_QUEUED_BYTES), Ok(()));
        assert_eq!(admit(Pressure::Starved, 0), Err(Dropped::Congestion));
        assert_eq!(
            admit(Pressure::Clean, MAX_QUEUED_BYTES + 1),
            Err(Dropped::Queue)
        );
        let a = AtomicU8::new(0);
        Pressure::Starved.store(&a);
        assert_eq!(Pressure::load(&a), Pressure::Starved);
    }
}

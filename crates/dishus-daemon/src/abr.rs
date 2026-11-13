//! Adaptive bitrate / fps controller. Pure logic, driven by a 500 ms tick.
//!
//! Signals of congestion: the sender had to drop frames (too many unacknowledged frames in
//! flight) or a frame stream stalled past its deadline. Back off multiplicatively, probe up
//! slowly. In adaptive-fps mode, start at 30 fps and go to 60 only after a sustained clean
//! period at a decent bitrate; fall back to 30 at the first sign of trouble.

use std::collections::VecDeque;

use dishus_proto::Quality;

pub const TICK_MS: u64 = 500;
/// Clean ticks before adaptive fps goes back from 30 to 60: starts at 5 s and doubles every
/// time 60 fps fails, up to 30 s, so a weak link holds 30 instead of flipping every few seconds.
const UP_AFTER_MIN: u32 = 10;
const UP_AFTER_MAX: u32 = 60;
/// Clean ticks at 60 fps (30 s) after which a hiccup counts as isolated: back to the short wait.
const STABLE_AT_60: u32 = 60;
/// RTT samples (5 s) whose minimum is the link's normal RTT. A higher RTT that lasts this long
/// (moved to a relay, to mobile data) becomes the new normal.
const RTT_WINDOW: usize = 10;
/// The RTT has risen (queues are building) above twice the normal RTT plus this.
const RTT_RISE_MS: u32 = 40;
/// Packet loss counts as congestion from this many packets in a tick and over 2 % of those sent.
const LOSS_MIN_PACKETS: u64 = 3;

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
    Bounds {
        min,
        start: start.min(max),
        max,
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TickStats {
    pub dropped: u32,
    pub stalled: u32,
    pub peak_inflight: usize,
    /// Frames handed to the network (for the stats log; not a congestion signal).
    pub sent: u32,
    /// Bytes of those frames (stats log only).
    pub bytes: u64,
    pub keys_sent: u32,
    pub key_requests: u32,
    /// The sender's current in-flight limit (0 = unknown: use the LAN default of 4).
    pub inflight_limit: usize,
    /// Selected path's RTT (0 = unknown), QUIC packets sent and lost (MTU probes excluded).
    pub rtt_ms: u32,
    pub sent_packets: u64,
    pub lost_packets: u64,
}

#[derive(Debug)]
pub struct Abr {
    b: Bounds,
    pub bitrate: u32,
    /// None = fixed fps chosen by the phone
    adaptive_fps: bool,
    pub fps: u8,
    good_ticks: u32,
    /// Clean ticks needed before 30 -> 60 (see [`UP_AFTER_MIN`]).
    up_after: u32,
    rtts: VecDeque<u32>,
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
        Self {
            b,
            bitrate: b.start,
            adaptive_fps,
            fps: if adaptive_fps { 30 } else { fps },
            good_ticks: 0,
            up_after: UP_AFTER_MIN,
            rtts: VecDeque::with_capacity(RTT_WINDOW),
        }
    }

    pub fn tick(&mut self, s: TickStats) -> Change {
        let (old_br, old_fps) = (self.bitrate, self.fps);
        // Queueing builds up before frames get dropped: near the in-flight limit is congestion.
        let near_limit = s.inflight_limit.max(4) - 1;
        let congested = s.dropped > 0
            || s.stalled > 0
            || s.peak_inflight >= near_limit
            || self.link_congested(&s);
        if congested {
            if self.adaptive_fps && self.fps == 60 {
                self.fps = 30;
                // A long clean stretch at 60 first: an isolated hiccup, keep the short wait.
                self.up_after = if self.good_ticks >= self.up_after + STABLE_AT_60 {
                    UP_AFTER_MIN
                } else {
                    (self.up_after * 2).min(UP_AFTER_MAX)
                };
            }
            self.good_ticks = 0;
            self.bitrate = ((self.bitrate as u64 * 7 / 10) as u32).max(self.b.min);
        } else {
            self.good_ticks = self.good_ticks.saturating_add(1);
            if self.good_ticks.is_multiple_of(4) {
                self.bitrate = ((self.bitrate as u64 * 115 / 100) as u32).min(self.b.max);
            }
            if self.adaptive_fps
                && self.fps < 60
                && self.good_ticks >= self.up_after
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

    /// QUIC sees trouble before frames back up: lost packets, or an RTT well above normal.
    fn link_congested(&mut self, s: &TickStats) -> bool {
        let lossy = s.lost_packets >= LOSS_MIN_PACKETS && s.lost_packets * 50 > s.sent_packets;
        let normal = self.rtts.iter().min().copied();
        let risen = s.rtt_ms > 0 && normal.is_some_and(|n| s.rtt_ms > n * 2 + RTT_RISE_MS);
        if s.rtt_ms > 0 {
            if self.rtts.len() == RTT_WINDOW {
                self.rtts.pop_front();
            }
            self.rtts.push_back(s.rtt_ms);
        }
        lossy || risen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backs_off_and_recovers() {
        let mut a = Abr::new(bounds(Quality::Auto, 20_000), 0);
        assert_eq!((a.bitrate, a.fps), (6_000, 30));
        let c = a.tick(TickStats {
            dropped: 2,
            ..Default::default()
        });
        assert_eq!(c.bitrate, Some(4_200));
        for _ in 0..20 {
            a.tick(TickStats {
                dropped: 5,
                ..Default::default()
            });
        }
        assert_eq!(a.bitrate, 500, "floors at min");
        for _ in 0..200 {
            a.tick(TickStats::default());
        }
        assert_eq!(a.bitrate, 20_000, "caps at max");
        assert_eq!(a.fps, 60, "adaptive fps climbs to 60 when clean");
        let c = a.tick(TickStats {
            stalled: 1,
            ..Default::default()
        });
        assert_eq!(c.fps, Some(30), "drops to 30 on trouble");
    }

    /// Mobile data: ~6 s clean, then a congested tick, over and over. 60 fps must not come
    /// back every cycle (it flipped 30 <-> 60 every few seconds on a real phone).
    #[test]
    fn adaptive_fps_does_not_flap() {
        let mut a = Abr::new(bounds(Quality::Medium, 20_000), 0);
        let mut changes = 0;
        for _ in 0..10 {
            for _ in 0..12 {
                changes += usize::from(a.tick(TickStats::default()).fps.is_some());
            }
            changes += usize::from(
                a.tick(TickStats {
                    dropped: 1,
                    ..Default::default()
                })
                .fps
                .is_some(),
            );
        }
        assert!(changes <= 2, "{changes} fps changes in 10 cycles");
        assert_eq!(a.fps, 30);
        // A link that stays clean long enough still gets 60 back...
        for _ in 0..UP_AFTER_MAX {
            a.tick(TickStats::default());
        }
        assert_eq!(a.fps, 60);
        // ...and after a long clean stretch at 60, one hiccup only costs the short wait again.
        for _ in 0..STABLE_AT_60 {
            a.tick(TickStats::default());
        }
        a.tick(TickStats {
            dropped: 1,
            ..Default::default()
        });
        for _ in 0..UP_AFTER_MIN {
            a.tick(TickStats::default());
        }
        assert_eq!(a.fps, 60);
    }

    fn net(rtt_ms: u32, sent_packets: u64, lost_packets: u64) -> TickStats {
        TickStats {
            rtt_ms,
            sent_packets,
            lost_packets,
            ..Default::default()
        }
    }

    #[test]
    fn backs_off_on_packet_loss() {
        let mut a = Abr::new(bounds(Quality::Medium, 20_000), 0);
        assert_eq!(
            a.tick(net(40, 1000, 2)).bitrate,
            None,
            "2 lost packets: noise"
        );
        assert_eq!(
            a.tick(net(40, 1000, 15)).bitrate,
            None,
            "1.5 %: below threshold"
        );
        assert_eq!(
            a.tick(net(40, 1000, 30)).bitrate,
            Some(2_800),
            "3 % lost: back off"
        );
    }

    #[test]
    fn backs_off_on_rtt_rise_and_adapts_to_a_new_normal() {
        let mut a = Abr::new(bounds(Quality::Medium, 20_000), 0);
        for _ in 0..RTT_WINDOW {
            a.tick(net(40, 100, 0));
        }
        let br = a.bitrate;
        a.tick(net(100, 100, 0));
        assert!(a.bitrate >= br, "100 ms is under 2 x 40 + 40: no back-off");
        let br = a.bitrate;
        a.tick(net(150, 100, 0));
        assert!(a.bitrate < br, "150 ms: back off");
        // Moved to a slower path for good: after one window 150 ms is the normal RTT.
        for _ in 0..RTT_WINDOW {
            a.tick(net(150, 100, 0));
        }
        let br = a.bitrate;
        for _ in 0..4 {
            a.tick(net(150, 100, 0));
        }
        assert!(a.bitrate >= br, "no more back-offs at a steady 150 ms");
    }

    #[test]
    fn fixed_fps_never_changes() {
        let mut a = Abr::new(bounds(Quality::High, 20_000), 45);
        for _ in 0..100 {
            assert_eq!(a.tick(TickStats::default()).fps, None);
        }
        assert_eq!(
            a.tick(TickStats {
                dropped: 1,
                ..Default::default()
            })
            .fps,
            None
        );
        assert_eq!(a.fps, 45);
    }

    #[test]
    fn bounds_respect_ceiling() {
        let b = bounds(Quality::High, 2_000);
        assert_eq!(
            b,
            Bounds {
                min: 1_000,
                start: 2_000,
                max: 2_000
            }
        );
        let b = bounds(Quality::Low, 100);
        assert!(b.max >= b.min);
    }
}

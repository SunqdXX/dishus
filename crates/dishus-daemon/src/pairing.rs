//! The pairing window: closed unless the user explicitly opens it, one 256-bit token, short TTL,
//! single use, constant-time comparison, closes itself after a few wrong guesses.

use std::time::{Duration, Instant};

use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use dishus_proto::TOKEN_LEN;

/// Wrong tokens tolerated before the window slams shut.
pub const MAX_PAIR_FAILURES: u8 = 3;

struct Window {
    token: Zeroizing<[u8; TOKEN_LEN]>,
    deadline: Instant,
    failures: u8,
}

#[derive(Default)]
pub struct Pairing {
    window: Option<Window>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PairCheck {
    Accepted,
    Rejected,
    /// Window not open (or expired, or already used).
    Closed,
}

impl Pairing {
    /// Open (or re-open with a fresh token) the window. Returns the token to show to the user.
    pub fn open(&mut self, ttl: Duration, now: Instant) -> anyhow::Result<Zeroizing<[u8; TOKEN_LEN]>> {
        let mut token = Zeroizing::new([0u8; TOKEN_LEN]);
        getrandom::fill(token.as_mut()).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
        self.window = Some(Window { token: token.clone(), deadline: now + ttl, failures: 0 });
        Ok(token)
    }

    pub fn close(&mut self) {
        self.window = None; // Zeroizing wipes the token
    }

    pub fn is_open(&self, now: Instant) -> bool {
        self.window.as_ref().is_some_and(|w| now < w.deadline)
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.window.as_ref().map(|w| w.deadline)
    }

    /// Check a presented token. On success the window is consumed (single use).
    pub fn check(&mut self, presented: &[u8; TOKEN_LEN], now: Instant) -> PairCheck {
        let Some(w) = self.window.as_mut() else { return PairCheck::Closed };
        if now >= w.deadline {
            self.close();
            return PairCheck::Closed;
        }
        if bool::from(w.token.ct_eq(presented)) {
            self.close();
            PairCheck::Accepted
        } else {
            w.failures += 1;
            if w.failures >= MAX_PAIR_FAILURES {
                tracing::warn!("too many wrong pairing tokens; closing the pairing window");
                self.close();
            }
            PairCheck::Rejected
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_secs(60);

    #[test]
    fn closed_by_default() {
        let mut p = Pairing::default();
        let now = Instant::now();
        assert!(!p.is_open(now));
        assert_eq!(p.check(&[0; 32], now), PairCheck::Closed);
    }

    #[test]
    fn single_use() {
        let mut p = Pairing::default();
        let now = Instant::now();
        let t = p.open(TTL, now).unwrap();
        assert_ne!(*t, [0u8; 32]);
        assert_eq!(p.check(&t, now), PairCheck::Accepted);
        assert_eq!(p.check(&t, now), PairCheck::Closed, "token reuse");
        assert!(!p.is_open(now));
    }

    #[test]
    fn expires() {
        let mut p = Pairing::default();
        let now = Instant::now();
        let t = p.open(TTL, now).unwrap();
        assert!(p.is_open(now + Duration::from_secs(59)));
        assert_eq!(p.check(&t, now + TTL), PairCheck::Closed);
    }

    #[test]
    fn wrong_token_then_lockout() {
        let mut p = Pairing::default();
        let now = Instant::now();
        let t = p.open(TTL, now).unwrap();
        let mut wrong = *t;
        wrong[0] ^= 1;
        for _ in 0..MAX_PAIR_FAILURES - 1 {
            assert_eq!(p.check(&wrong, now), PairCheck::Rejected);
        }
        assert!(p.is_open(now));
        assert_eq!(p.check(&wrong, now), PairCheck::Rejected);
        assert!(!p.is_open(now), "closed after too many failures");
        assert_eq!(p.check(&t, now), PairCheck::Closed, "even the right token is refused now");
    }

    #[test]
    fn reopen_rotates_token() {
        let mut p = Pairing::default();
        let now = Instant::now();
        let a = p.open(TTL, now).unwrap();
        let b = p.open(TTL, now).unwrap();
        assert_ne!(*a, *b);
        assert_eq!(p.check(&a, now), PairCheck::Rejected);
        assert_eq!(p.check(&b, now), PairCheck::Accepted);
    }
}

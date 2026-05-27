// Real, fully unit-tested token bucket. The block device wiring that consumes
// it will land alongside virtio-blk in the next phase; until then the type is
// only exercised by its own tests.
#![allow(dead_code)]

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Token bucket I/O throttler.
///
/// `capacity` is the maximum number of tokens that can be stored (the burst
/// allowance). `refill_per_sec` is the steady-state token generation rate. A
/// caller spends `n` tokens by calling `try_consume(n)`; if the bucket is
/// short, it returns the wait duration the caller should sleep for before
/// retrying. The scaling monitor can call [`TokenBucket::set_rate`] at any time
/// to widen or narrow the throttle as part of a burst response.
pub struct TokenBucket {
    inner: Mutex<Inner>,
}

struct Inner {
    capacity: u64,
    refill_per_sec: u64,
    tokens: u64,
    last_refill: Instant,
}

impl TokenBucket {
    pub fn new(capacity: u64, refill_per_sec: u64) -> Self {
        Self {
            inner: Mutex::new(Inner {
                capacity,
                refill_per_sec,
                tokens: capacity,
                last_refill: Instant::now(),
            }),
        }
    }

    pub fn capacity(&self) -> u64 {
        self.inner.lock().unwrap().capacity
    }

    pub fn rate(&self) -> u64 {
        self.inner.lock().unwrap().refill_per_sec
    }

    pub fn set_rate(&self, refill_per_sec: u64) {
        let mut g = self.inner.lock().unwrap();
        g.refill_per_sec = refill_per_sec;
    }

    pub fn set_capacity(&self, capacity: u64) {
        let mut g = self.inner.lock().unwrap();
        g.capacity = capacity;
        if g.tokens > capacity {
            g.tokens = capacity;
        }
    }

    /// Try to consume `n` tokens. On success returns `Ok(())`. On failure
    /// returns the minimum [`Duration`] the caller should wait before retrying.
    pub fn try_consume(&self, n: u64) -> Result<(), Duration> {
        self.try_consume_at(n, Instant::now())
    }

    fn try_consume_at(&self, n: u64, now: Instant) -> Result<(), Duration> {
        let mut g = self.inner.lock().unwrap();
        g.refill(now);
        if g.tokens >= n {
            g.tokens -= n;
            return Ok(());
        }
        if g.refill_per_sec == 0 {
            // No future refills will land — caller will starve. Return a long
            // delay so the caller backs off rather than spinning.
            return Err(Duration::from_secs(3600));
        }
        let needed = n - g.tokens;
        let nanos = (needed as u128 * 1_000_000_000u128) / (g.refill_per_sec as u128);
        Err(Duration::from_nanos(nanos.min(u64::MAX as u128) as u64))
    }
}

impl Inner {
    fn refill(&mut self, now: Instant) {
        if now <= self.last_refill {
            return;
        }
        let elapsed = now - self.last_refill;
        let nanos = elapsed.as_nanos();
        let earned = (nanos * self.refill_per_sec as u128) / 1_000_000_000u128;
        if earned > 0 {
            self.tokens = self.tokens.saturating_add(earned as u64).min(self.capacity);
            self.last_refill = now;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_consumption_within_capacity_succeeds() {
        let tb = TokenBucket::new(100, 10);
        assert!(tb.try_consume(50).is_ok());
        assert!(tb.try_consume(50).is_ok());
    }

    #[test]
    fn empty_bucket_reports_wait_proportional_to_rate() {
        let tb = TokenBucket::new(10, 100); // 100 tok/s = 10ms / token
        assert!(tb.try_consume(10).is_ok()); // drain
        let wait = tb.try_consume(1).unwrap_err();
        // Expect ~10ms with some slack for refill that already happened.
        assert!(wait <= Duration::from_millis(11), "wait = {wait:?}");
    }

    #[test]
    fn refill_grants_more_tokens_over_time() {
        let tb = TokenBucket::new(100, 1_000_000);
        // Drain
        assert!(tb.try_consume(100).is_ok());
        // Advance virtual time
        let later = Instant::now() + Duration::from_millis(10);
        // 10ms @ 1M tok/s = 10_000 tokens earned, clamped to capacity 100.
        assert!(tb.try_consume_at(50, later).is_ok());
    }

    #[test]
    fn zero_rate_returns_long_wait_when_drained() {
        let tb = TokenBucket::new(5, 0);
        assert!(tb.try_consume(5).is_ok());
        let wait = tb.try_consume(1).unwrap_err();
        assert!(wait >= Duration::from_secs(60), "wait = {wait:?}");
    }

    #[test]
    fn set_rate_updates_throttle_live() {
        // Request more than capacity so both calls must report a wait
        // (otherwise a fast refill could satisfy a 1-token request between
        // the two reads).
        let tb = TokenBucket::new(10, 1);
        let slow = tb.try_consume(100).unwrap_err();
        tb.set_rate(1_000_000);
        let fast = tb.try_consume(100).unwrap_err();
        assert!(fast < slow, "raising rate should shorten the wait (slow={slow:?}, fast={fast:?})");
    }

    #[test]
    fn set_capacity_clamps_existing_tokens() {
        let tb = TokenBucket::new(100, 10);
        tb.set_capacity(10);
        // After clamping, only 10 tokens are available.
        assert!(tb.try_consume(10).is_ok());
        assert!(tb.try_consume(1).is_err());
    }
}

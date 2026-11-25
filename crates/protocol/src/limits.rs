//! A small deterministic token bucket used by the dedicated server to cap
//! per-client message rates.
//!
//! The bucket starts full and refills at `rate_per_second`, capped at the burst
//! size. Time is supplied by the caller so tests do not need real sleeps and
//! the server can share one `Instant` per tick.

use std::time::{Duration, Instant};

/// A token bucket rate limiter.
#[derive(Debug, Clone)]
pub struct RateLimit {
    capacity: f64,
    tokens: f64,
    rate_per_second: f64,
    last: Instant,
}

impl RateLimit {
    /// Creates a limiter that allows `burst` immediate messages and then
    /// refills at `rate_per_second`.
    ///
    /// Non-positive rates and zero bursts are clamped to one so a
    /// misconfiguration cannot accidentally allow infinite traffic.
    pub fn new(rate_per_second: f64, burst: u32) -> Self {
        let capacity = f64::from(burst.max(1));
        let rate_per_second = if rate_per_second.is_finite() && rate_per_second > 0.0 {
            rate_per_second
        } else {
            1.0
        };
        Self {
            capacity,
            tokens: capacity,
            rate_per_second,
            last: Instant::now(),
        }
    }

    /// Attempts to spend one token at `now`.
    ///
    /// Returns `false` when the bucket is empty; the caller should silently
    /// drop or defer the message.
    pub fn allow(&mut self, now: Instant) -> bool {
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Attempts to spend one token at the current instant.
    pub fn allow_now(&mut self) -> bool {
        self.allow(Instant::now())
    }

    /// Current tokens, for diagnostics and tests.
    pub fn tokens(&self, now: Instant) -> f64 {
        let mut clone = self.clone();
        clone.refill(now);
        clone.tokens
    }

    /// Refills the bucket up to its capacity.
    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last);
        if elapsed > Duration::ZERO {
            self.tokens = (self.tokens + elapsed.as_secs_f64() * self.rate_per_second).min(self.capacity);
            self.last = now;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_is_allowed_then_refills_over_time() {
        let start = Instant::now();
        let mut limit = RateLimit::new(2.0, 3);
        assert_eq!(limit.tokens(start), 3.0);
        let mut now = start;
        for _ in 0..3 {
            assert!(limit.allow(now));
        }
        assert!(!limit.allow(now), "the burst must be exhausted");

        now += Duration::from_secs(2);
        assert!(limit.allow(now), "the bucket must refill after two seconds");
        assert!(limit.allow(now));
        assert!(limit.allow(now));
        assert!(!limit.allow(now));
        now += Duration::from_secs(5);
        assert!(limit.allow(now));
        assert!(limit.tokens(now) <= 3.0);
    }

    #[test]
    fn misconfiguration_is_clamped() {
        let mut limit = RateLimit::new(f64::NAN, 0);
        assert!(limit.allow_now());
        assert!(!limit.allow_now());
        let start = Instant::now();
        assert!(limit.tokens(start) <= 1.0);
    }
}

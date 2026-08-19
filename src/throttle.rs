//! Token-bucket rate limiter for per-file and global download speed caps.
//!
//! The bucket holds up to `rate` tokens (one second of burst allowance) and
//! refills at `rate` tokens/second. Large charges are sliced into chunks of
//! at most `rate` bytes so a single charge never exceeds the bucket capacity
//! (which would otherwise deadlock the limiter on a big network chunk).

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

struct Bucket {
    tokens: f64,
    last: Instant,
    rate: f64, // bytes per second
}

/// Clone-able handle to a shared token bucket.
#[derive(Clone)]
pub struct Throttle {
    inner: Arc<Mutex<Bucket>>,
}

impl Throttle {
    /// Create a limiter. `rate_bytes_per_sec` must be > 0 (callers use
    /// `Option<Throttle>` for "no limit").
    pub fn new(rate_bytes_per_sec: f64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Bucket {
                // Start with one second's worth: allows a small burst so the
                // first chunk of a file isn't needlessly delayed.
                tokens: rate_bytes_per_sec,
                last: Instant::now(),
                rate: rate_bytes_per_sec,
            })),
        }
    }

    /// Consume `bytes` worth of budget, sleeping as needed to stay under the
    /// configured rate. The lock is released while sleeping so other tasks
    /// sharing the bucket can make progress.
    pub async fn acquire(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let rate = {
            let b = self.inner.lock().await;
            b.rate
        };
        let mut remaining = bytes as f64;
        while remaining > 0.0 {
            // Charge at most one second's worth at a time so a single charge
            // always fits within the bucket's capacity.
            let slice = remaining.min(rate);
            self.acquire_slice(slice).await;
            remaining -= slice;
        }
    }

    async fn acquire_slice(&self, bytes: f64) {
        loop {
            let wait = {
                let mut b = self.inner.lock().await;
                let now = Instant::now();
                let dt = now.duration_since(b.last).as_secs_f64();
                b.tokens = (b.tokens + dt * b.rate).min(b.rate);
                b.last = now;
                if b.tokens >= bytes {
                    b.tokens -= bytes;
                    0.0
                } else {
                    (bytes - b.tokens) / b.rate
                }
            };
            if wait == 0.0 {
                return;
            }
            // Sleep in small slices so other tasks can drain the bucket too.
            tokio::time::sleep(Duration::from_secs_f64(wait.min(0.25))).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn respects_rate_approximately() {
        // 100 bytes/sec -> 1000 bytes should take ~9s (first 100 burst, then 9s).
        let t = Throttle::new(100.0);
        let start = Instant::now();
        for _ in 0..10 {
            t.acquire(100).await;
        }
        let elapsed = start.elapsed().as_secs_f64();
        assert!(elapsed > 8.0, "elapsed too short: {elapsed:.2}s");
        assert!(elapsed < 15.0, "elapsed too long: {elapsed:.2}s");
    }

    #[tokio::test]
    async fn burst_is_allowed() {
        // First second's worth of bytes should not sleep at all.
        let t = Throttle::new(1_000_000.0);
        let start = Instant::now();
        t.acquire(1_000_000).await;
        assert!(start.elapsed().as_millis() < 100);
    }

    #[tokio::test]
    async fn large_single_charge_does_not_deadlock() {
        // A charge larger than one second's budget must still complete at
        // roughly the configured rate (this previously hung forever).
        let t = Throttle::new(2000.0); // 2 KiB/s
        let start = Instant::now();
        t.acquire(6000).await; // > burst; sliced internally
        let elapsed = start.elapsed().as_secs_f64();
        assert!(
            (1.5..=4.0).contains(&elapsed),
            "expected ~2s, got {elapsed:.1}s"
        );
    }
}

//! Fibonacci backoff: 1, 1, 2, 3, 5, 8, 13, 21, ... seconds, each step capped.
//!
//! Gentler than doubling at the start (a transient blip is retried within a
//! couple of seconds) and still backs off quickly enough that a peer or
//! gateway that is down for minutes is not hammered. Use [`Fibonacci`] as a
//! delay schedule in a loop, or [`retry_until`] to retry one fallible call
//! until a deadline.

use std::future::Future;
use std::time::{Duration, Instant};

/// The per-step ceiling most callers want.
pub const DEFAULT_CAP: Duration = Duration::from_secs(60);

/// An endless Fibonacci delay schedule, starting at `unit` (1 s by default)
/// and never exceeding `cap` per step.
#[derive(Debug, Clone)]
pub struct Fibonacci {
    unit: Duration,
    cap: Duration,
    a: u64,
    b: u64,
}

impl Fibonacci {
    /// 1 s, 1 s, 2 s, 3 s, 5 s, ... capped at `cap`.
    pub fn new(cap: Duration) -> Self {
        Self::with_unit(Duration::from_secs(1), cap)
    }

    /// `unit`, `unit`, 2*`unit`, 3*`unit`, 5*`unit`, ... capped at `cap`.
    pub fn with_unit(unit: Duration, cap: Duration) -> Self {
        Self {
            unit,
            cap,
            a: 1,
            b: 1,
        }
    }

    /// Start over from the first step (call after progress or success).
    pub fn reset(&mut self) {
        self.a = 1;
        self.b = 1;
    }

    /// The next delay in the schedule.
    pub fn next_delay(&mut self) -> Duration {
        let d = self
            .unit
            .checked_mul(u32::try_from(self.a).unwrap_or(u32::MAX))
            .unwrap_or(self.cap)
            .min(self.cap);
        // Stop growing once the cap is reached so the counters never overflow.
        if d < self.cap {
            let next = self.a.saturating_add(self.b);
            self.a = self.b;
            self.b = next;
        }
        d
    }
}

impl Default for Fibonacci {
    fn default() -> Self {
        Self::new(DEFAULT_CAP)
    }
}

impl Iterator for Fibonacci {
    type Item = Duration;
    fn next(&mut self) -> Option<Duration> {
        Some(self.next_delay())
    }
}

/// Call `f` until it succeeds or the next retry would start after
/// `deadline`, sleeping the next step of `schedule` between tries
/// (`Fibonacci::default()` for 1, 1, 2, 3, 5, ... s capped at 60 s).
/// Each failure is passed to `on_err` with the delay before the next try
/// (`None` when giving up). Returns the last error when time runs out; `f`
/// is always called at least once.
pub async fn retry_until<T, E, F, Fut>(
    deadline: Instant,
    mut schedule: Fibonacci,
    mut on_err: impl FnMut(&E, Option<Duration>),
    mut f: F,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    loop {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                let delay = schedule.next_delay();
                if Instant::now() + delay >= deadline {
                    on_err(&e, None);
                    return Err(e);
                }
                on_err(&e, Some(delay));
                tokio::time::sleep(delay).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(v: &[u64]) -> Vec<Duration> {
        v.iter().map(|s| Duration::from_secs(*s)).collect()
    }

    #[test]
    fn follows_fibonacci_then_caps() {
        let got: Vec<Duration> = Fibonacci::new(Duration::from_secs(60)).take(13).collect();
        assert_eq!(got, secs(&[1, 1, 2, 3, 5, 8, 13, 21, 34, 55, 60, 60, 60]));
    }

    #[test]
    fn default_cap_is_sixty_seconds() {
        let mut f = Fibonacci::default();
        assert!(f.by_ref().take(1000).all(|d| d <= Duration::from_secs(60)));
        assert_eq!(f.next_delay(), Duration::from_secs(60));
    }

    #[test]
    fn small_cap_and_unit() {
        let got: Vec<Duration> = Fibonacci::new(Duration::from_secs(4)).take(6).collect();
        assert_eq!(got, secs(&[1, 1, 2, 3, 4, 4]));
        let ms = Duration::from_millis(100);
        let got: Vec<Duration> = Fibonacci::with_unit(ms, Duration::from_secs(1))
            .take(7)
            .collect();
        assert_eq!(
            got,
            [100, 100, 200, 300, 500, 800, 1000]
                .map(Duration::from_millis)
                .to_vec()
        );
    }

    #[test]
    fn reset_starts_over() {
        let mut f = Fibonacci::default();
        for _ in 0..6 {
            f.next_delay();
        }
        assert_eq!(f.next_delay(), Duration::from_secs(13));
        f.reset();
        assert_eq!(f.next_delay(), Duration::from_secs(1));
        assert_eq!(f.next_delay(), Duration::from_secs(1));
        assert_eq!(f.next_delay(), Duration::from_secs(2));
    }

    #[test]
    fn never_overflows() {
        let mut f = Fibonacci::new(Duration::MAX);
        for _ in 0..200 {
            f.next_delay();
        }
    }

    fn fast() -> Fibonacci {
        Fibonacci::with_unit(Duration::from_millis(10), Duration::from_millis(50))
    }

    #[tokio::test]
    async fn retry_until_succeeds_after_failures() {
        let mut calls = 0;
        let mut waits = Vec::new();
        let r: Result<u32, &str> = retry_until(
            Instant::now() + Duration::from_secs(30),
            fast(),
            |_, d| waits.push(d),
            || {
                calls += 1;
                let n = calls;
                async move { if n < 4 { Err("down") } else { Ok(n) } }
            },
        )
        .await;
        assert_eq!(r, Ok(4));
        assert_eq!(
            waits,
            [10, 10, 20]
                .map(|ms| Some(Duration::from_millis(ms)))
                .to_vec()
        );
    }

    #[tokio::test]
    async fn retry_until_gives_up_at_deadline() {
        let mut calls = 0;
        let mut last = Some(Duration::ZERO);
        let started = Instant::now();
        let r: Result<(), &str> = retry_until(
            started + Duration::from_millis(100),
            fast(),
            |_, d| last = d,
            || {
                calls += 1;
                async { Err("down") }
            },
        )
        .await;
        assert_eq!(r, Err("down"));
        assert_eq!(last, None, "the final failure reports giving up");
        assert!(calls >= 2, "retried before giving up ({calls} calls)");
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[tokio::test]
    async fn retry_until_tries_once_even_past_deadline() {
        let mut calls = 0;
        let r: Result<(), &str> = retry_until(
            Instant::now(),
            fast(),
            |_, _| {},
            || {
                calls += 1;
                async { Err("down") }
            },
        )
        .await;
        assert_eq!(r, Err("down"));
        assert_eq!(calls, 1);
    }
}

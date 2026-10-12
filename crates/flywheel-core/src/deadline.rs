//! Reusable wall-clock deadline primitive.
//!
//! A single seam that bounds a future or a stream of items by total
//! wall-clock time elapsed since the deadline was armed, surfacing a
//! distinguishable [`DeadlineExceeded`] when the ceiling is exceeded -- even
//! while the wrapped stream is still producing items. This is *not* an
//! idle/silence timer: a stream that keeps yielding forever is still cut off
//! once the ceiling passes (spec 00066 D-2). The inter-message silence
//! watchdog in the harness is orthogonal; this bound is additive, not a
//! replacement.
//!
//! [`Deadline`] is the value type: arm it once, then bound any number of
//! awaits against the *same* ceiling with [`Deadline::bound`]. The remaining
//! budget is recomputed before each await, so neither a fast, never-ending
//! stream nor a wedged `next()` can outlast it. [`run_with_deadline`] and
//! [`stream_with_deadline`] are the one-shot conveniences over it.
//!
//! The clock is `tokio::time::Instant`, so tests may drive it under
//! `#[tokio::test]`.

use std::fmt;
use std::future::Future;
use std::time::Duration;
use tokio::time::Instant;

/// A wall-clock deadline was exceeded.
///
/// Deliberately its own error type (not a cancellation) so callers can tell
/// a deadline timeout apart from any other failure and classify it as a
/// timeout outcome. `ceiling` is the configured bound; `elapsed` is the
/// wall-clock time observed when the deadline fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub struct DeadlineExceeded {
    pub ceiling: Duration,
    pub elapsed: Duration,
}

impl DeadlineExceeded {
    pub fn ceiling_seconds(&self) -> f64 {
        self.ceiling.as_secs_f64()
    }

    pub fn elapsed_seconds(&self) -> f64 {
        self.elapsed.as_secs_f64()
    }
}

impl fmt::Display for DeadlineExceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "deadline of {}s exceeded after {:.6}s",
            self.ceiling_seconds(),
            self.elapsed_seconds()
        )
    }
}

/// An armed wall-clock ceiling.
///
/// `ceiling == None` is the explicit unbounded opt-out resolved by
/// [`crate::deadline_config::DeadlineConfig`]: every bound await simply
/// completes whenever the inner future does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadline {
    started: Instant,
    ceiling: Option<Duration>,
}

impl Deadline {
    /// Arm a deadline `ceiling` from now.
    pub fn new(ceiling: Duration) -> Self {
        Deadline {
            started: Instant::now(),
            ceiling: Some(ceiling),
        }
    }

    /// Arm a deadline that never fires.
    pub fn unbounded() -> Self {
        Deadline {
            started: Instant::now(),
            ceiling: None,
        }
    }

    /// Arm from a resolved config value: `Some` is a ceiling, `None` is the
    /// unbounded opt-out.
    pub fn from_ceiling(ceiling: Option<Duration>) -> Self {
        Deadline {
            started: Instant::now(),
            ceiling,
        }
    }

    pub fn ceiling(&self) -> Option<Duration> {
        self.ceiling
    }

    pub fn is_bounded(&self) -> bool {
        self.ceiling.is_some()
    }

    /// Wall-clock time since the deadline was armed.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Budget left before the ceiling; `None` when unbounded, zero once
    /// exceeded.
    pub fn remaining(&self) -> Option<Duration> {
        self.ceiling.map(|c| c.saturating_sub(self.elapsed()))
    }

    /// True once the ceiling has passed. Always false when unbounded.
    pub fn is_exceeded(&self) -> bool {
        self.ceiling.is_some_and(|c| self.elapsed() >= c)
    }

    fn exceeded(&self, ceiling: Duration) -> DeadlineExceeded {
        DeadlineExceeded {
            ceiling,
            elapsed: self.elapsed(),
        }
    }

    /// Await `future` within whatever budget remains.
    ///
    /// If the ceiling has already passed the future is dropped unpolled and
    /// [`DeadlineExceeded`] is returned; if the future outlives the
    /// remaining budget it is dropped (cancelled) and the same error is
    /// returned. The future's own output is otherwise passed through
    /// unchanged, so a `Result` inside stays distinguishable from a
    /// timeout.
    pub async fn bound<F: Future>(&self, future: F) -> Result<F::Output, DeadlineExceeded> {
        let Some(ceiling) = self.ceiling else {
            return Ok(future.await);
        };
        let remaining = ceiling.saturating_sub(self.elapsed());
        if remaining.is_zero() {
            return Err(self.exceeded(ceiling));
        }
        match tokio::time::timeout(remaining, future).await {
            Ok(output) => Ok(output),
            Err(_) => Err(self.exceeded(ceiling)),
        }
    }
}

/// Await `future`, failing with [`DeadlineExceeded`] past the ceiling.
///
/// A thin wall-clock bound over a single future. If it does not complete
/// within `ceiling` it is dropped and the error is returned; its own output
/// (including any inner error) propagates unchanged.
pub async fn run_with_deadline<F: Future>(
    future: F,
    ceiling: Duration,
) -> Result<F::Output, DeadlineExceeded> {
    Deadline::new(ceiling).bound(future).await
}

/// Drain `next` into `sink` until it yields `None` or the ceiling passes.
///
/// `next` is polled for one item at a time (an `async` closure such as
/// `async || rx.recv().await`). The bound is total elapsed time since the
/// call started, not idle time between items: a source that keeps yielding
/// forever is still cut off with [`DeadlineExceeded`] once `ceiling`
/// passes, and a source whose next item never arrives is cut off the same
/// way. Items already delivered to `sink` are not rolled back.
pub async fn stream_with_deadline<T, S, K>(
    ceiling: Duration,
    mut next: S,
    mut sink: K,
) -> Result<(), DeadlineExceeded>
where
    S: AsyncFnMut() -> Option<T>,
    K: FnMut(T),
{
    let deadline = Deadline::new(ceiling);
    while let Some(item) = deadline.bound(next()).await? {
        sink(item);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;
    use tokio::time::sleep;

    const CEILING: Duration = Duration::from_millis(50);
    const EPSILON: Duration = Duration::from_secs(1);

    #[tokio::test]
    async fn future_sleeping_past_ceiling_is_cut_off() {
        let started = Instant::now();
        let err = run_with_deadline(sleep(Duration::from_secs(3600)), CEILING)
            .await
            .unwrap_err();
        assert!(started.elapsed() < CEILING + EPSILON);
        assert_eq!(err.ceiling, CEILING);
        assert!(err.elapsed >= CEILING);
        assert!(
            err.to_string()
                .starts_with("deadline of 0.05s exceeded after ")
        );
    }

    #[tokio::test]
    async fn future_completing_before_ceiling_returns_its_value() {
        let value = run_with_deadline(async { "done" }, CEILING).await.unwrap();
        assert_eq!(value, "done");
    }

    #[tokio::test]
    async fn inner_error_propagates_unchanged_not_as_deadline() {
        let inner: Result<(), &str> = run_with_deadline(async { Err("boom") }, CEILING)
            .await
            .expect("completed within the ceiling");
        assert_eq!(inner, Err("boom"));
    }

    #[tokio::test]
    async fn forever_yielding_stream_is_cut_off_by_wall_clock() {
        // The load-bearing case: an idle timer that resets on each item would
        // let this run forever. The bound fires because total elapsed time
        // crosses the ceiling, not because the stream went quiet.
        let started = Instant::now();
        let mut i: u64 = 0;
        let mut seen = 0usize;
        let err = stream_with_deadline(
            CEILING,
            async || {
                i += 1;
                sleep(Duration::from_millis(1)).await;
                Some(i)
            },
            |_item| seen += 1,
        )
        .await
        .unwrap_err();
        assert!(started.elapsed() < CEILING + EPSILON);
        assert!(seen > 0, "items really streamed before the deadline fired");
        assert_eq!(err.ceiling, CEILING);
    }

    #[tokio::test]
    async fn wedged_next_is_cut_off() {
        let (tx, mut rx) = mpsc::channel::<u8>(4);
        tx.send(0).await.unwrap();
        // The sender stays alive but never sends again: `recv` wedges.
        let started = Instant::now();
        let mut collected = Vec::new();
        let err = stream_with_deadline(CEILING, async || rx.recv().await, |v| collected.push(v))
            .await
            .unwrap_err();
        assert!(started.elapsed() < CEILING + EPSILON);
        assert_eq!(collected, vec![0]);
        assert_eq!(err.ceiling, CEILING);
        drop(tx);
    }

    #[tokio::test]
    async fn finite_stream_within_ceiling_yields_all_items() {
        let (tx, mut rx) = mpsc::channel::<u8>(4);
        for v in 0..3 {
            tx.send(v).await.unwrap();
        }
        drop(tx);
        let mut collected = Vec::new();
        stream_with_deadline(CEILING, async || rx.recv().await, |v| collected.push(v))
            .await
            .unwrap();
        assert_eq!(collected, vec![0, 1, 2]);
    }

    #[tokio::test]
    async fn bound_rejects_without_polling_once_exceeded() {
        let deadline = Deadline::new(CEILING);
        assert!(!deadline.is_exceeded());
        let remaining = deadline.remaining().unwrap();
        assert!(remaining <= CEILING && !remaining.is_zero());
        sleep(CEILING * 2).await;
        assert!(deadline.is_exceeded());
        assert_eq!(deadline.remaining(), Some(Duration::ZERO));
        let mut polled = false;
        let err = deadline
            .bound(async {
                polled = true;
            })
            .await
            .unwrap_err();
        assert!(!polled);
        assert_eq!(err.ceiling, CEILING);
        assert!(err.elapsed >= CEILING * 2);
    }

    #[tokio::test]
    async fn unbounded_deadline_never_fires() {
        let deadline = Deadline::unbounded();
        assert!(!deadline.is_bounded());
        assert_eq!(deadline.remaining(), None);
        sleep(CEILING * 2).await;
        assert!(!deadline.is_exceeded());
        let value = deadline
            .bound(async {
                sleep(CEILING * 2).await;
                7
            })
            .await
            .unwrap();
        assert_eq!(value, 7);
        assert_eq!(Deadline::from_ceiling(None).ceiling(), None);
        assert_eq!(
            Deadline::from_ceiling(Some(CEILING)).ceiling(),
            Some(CEILING)
        );
    }
}

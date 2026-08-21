//! Which backend a monitor captures with, and when each one is tried again. Pure state: this
//! table knows two labels and nothing about what is behind them.

use std::time::{Duration, Instant};

/// Consecutive failed ticks before the fallback takes over, so a mode change or a lock screen is
/// ridden out rather than switched on.
const FAILOVER_AFTER: u32 = 3;
/// Base unit of both waits after failover: the primary's first probe comes this long after the
/// switch, the fallback's first re-open wait is one doubling of it (its takeover attempt owes
/// no wait), and every further failure doubles either side's wait up to `MAX_PROBE_INTERVAL`.
const FIRST_PROBE_DELAY: Duration = Duration::from_secs(1);
/// Upper bound of both widening waits: the primary's probe interval and the fallback's
/// re-open wait.
const MAX_PROBE_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backend {
    Primary,
    Fallback,
}

/// What one attempt said. `Answered` is a live session with nothing new to hand over — proof of
/// primary health, but neutral for the fallback's re-open pacing: the fallback answers that way
/// through its first-frame grace too, and a session that then dies of that grace must find its
/// backoff grown, not reset. Only `Delivered` — an actual frame — resets it.
#[derive(Clone, Copy)]
pub(crate) enum Outcome {
    Delivered,
    Answered,
    Failed,
}

enum State {
    Primary {
        failures: u32,
    },
    Fallback {
        probe_at: Instant,
        probe_interval: Duration,
        retry_at: Instant,
        retry_interval: Duration,
    },
}

pub(crate) struct Failover {
    state: State,
}

impl Default for Failover {
    fn default() -> Self {
        Self {
            state: State::Primary { failures: 0 },
        }
    }
}

impl Failover {
    /// Which backend this tick should try, if any: `None` while the primary's next probe and the
    /// fallback's next re-open both lie in the future. On a locked screen both backends fail,
    /// and without the fallback's wait every tick would rebuild a WGC session just to watch it
    /// die of its first-frame grace — session churn and a warn every few seconds for the whole
    /// lock.
    pub(crate) fn target(&self, now: Instant) -> Option<Backend> {
        match self.state {
            State::Primary { .. } => Some(Backend::Primary),
            State::Fallback { probe_at, .. } if now >= probe_at => Some(Backend::Primary),
            State::Fallback { retry_at, .. } if now >= retry_at => Some(Backend::Fallback),
            State::Fallback { .. } => None,
        }
    }

    /// Feed one attempt in. Returns the backend that just took over, when it changes.
    pub(crate) fn record(
        &mut self,
        used: Backend,
        outcome: Outcome,
        now: Instant,
    ) -> Option<Backend> {
        let (next, switched) = match (&self.state, used, outcome) {
            (State::Primary { .. }, Backend::Fallback, _) => return None,
            (State::Primary { .. }, Backend::Primary, Outcome::Delivered | Outcome::Answered) => {
                (State::Primary { failures: 0 }, None)
            }
            (State::Primary { failures }, Backend::Primary, Outcome::Failed)
                if failures + 1 >= FAILOVER_AFTER =>
            {
                let state = State::Fallback {
                    probe_at: now + FIRST_PROBE_DELAY,
                    probe_interval: FIRST_PROBE_DELAY,
                    // The takeover attempt owes no wait: nothing has failed on it yet.
                    retry_at: now,
                    retry_interval: FIRST_PROBE_DELAY,
                };
                (state, Some(Backend::Fallback))
            }
            (State::Primary { failures }, Backend::Primary, Outcome::Failed) => (
                State::Primary {
                    failures: failures + 1,
                },
                None,
            ),
            (State::Fallback { .. }, Backend::Primary, Outcome::Delivered | Outcome::Answered) => {
                (State::Primary { failures: 0 }, Some(Backend::Primary))
            }
            (
                State::Fallback {
                    probe_interval,
                    retry_at,
                    retry_interval,
                    ..
                },
                Backend::Primary,
                Outcome::Failed,
            ) => {
                let probe_interval = (*probe_interval * 2).min(MAX_PROBE_INTERVAL);
                let state = State::Fallback {
                    probe_at: now + probe_interval,
                    probe_interval,
                    retry_at: *retry_at,
                    retry_interval: *retry_interval,
                };
                (state, None)
            }
            (
                State::Fallback {
                    probe_at,
                    probe_interval,
                    ..
                },
                Backend::Fallback,
                Outcome::Delivered,
            ) => {
                let state = State::Fallback {
                    probe_at: *probe_at,
                    probe_interval: *probe_interval,
                    retry_at: now,
                    retry_interval: FIRST_PROBE_DELAY,
                };
                (state, None)
            }
            (State::Fallback { .. }, Backend::Fallback, Outcome::Answered) => return None,
            (
                State::Fallback {
                    probe_at,
                    probe_interval,
                    retry_interval,
                    ..
                },
                Backend::Fallback,
                Outcome::Failed,
            ) => {
                let retry_interval = (*retry_interval * 2).min(MAX_PROBE_INTERVAL);
                let state = State::Fallback {
                    probe_at: *probe_at,
                    probe_interval: *probe_interval,
                    retry_at: now + retry_interval,
                    retry_interval,
                };
                (state, None)
            }
        };
        self.state = next;
        switched
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed_over(now: Instant) -> Failover {
        let mut state = Failover::default();
        for _ in 0..FAILOVER_AFTER {
            state.record(Backend::Primary, Outcome::Failed, now);
        }
        state
    }

    #[test]
    fn a_failed_fallback_is_not_retried_on_the_very_next_tick() {
        let start = Instant::now();
        let mut state = failed_over(start);
        assert_eq!(
            state.target(start),
            Some(Backend::Fallback),
            "the takeover attempt owes no wait"
        );
        state.record(Backend::Fallback, Outcome::Failed, start);
        assert_eq!(
            state.target(start + Duration::from_millis(1)),
            None,
            "a fallback that just failed must not be rebuilt on the next tick"
        );
        assert_eq!(
            state.target(start + FIRST_PROBE_DELAY),
            Some(Backend::Primary),
            "the primary probe schedule is the fallback's problem to respect, not to block"
        );
    }

    #[test]
    fn the_reopen_wait_doubles_to_its_bound_and_a_frame_resets_it() {
        let start = Instant::now();
        let mut state = failed_over(start);
        for step in 0..8 {
            state.record(
                Backend::Fallback,
                Outcome::Failed,
                start + Duration::from_secs(step),
            );
        }
        for step in 8..16 {
            state.record(
                Backend::Primary,
                Outcome::Failed,
                start + Duration::from_secs(step),
            );
        }
        // The last fallback failure at +7 s with its wait capped: the re-open comes due at
        // +67 s. The last probe failure at +15 s, capped too, holds the probe until +75 s.
        let reopen = start + Duration::from_secs(67);
        assert_eq!(
            state.target(reopen - Duration::from_secs(1)),
            None,
            "the grown wait must hold the re-open back"
        );
        assert_eq!(
            state.target(reopen),
            Some(Backend::Fallback),
            "the wait must never exceed MAX_PROBE_INTERVAL"
        );
        state.record(Backend::Fallback, Outcome::Delivered, reopen);
        state.record(Backend::Fallback, Outcome::Failed, reopen);
        assert_eq!(
            state.target(reopen + FIRST_PROBE_DELAY),
            None,
            "one failure after a delivered frame earns one doubled first delay"
        );
        assert_eq!(
            state.target(reopen + 2 * FIRST_PROBE_DELAY),
            Some(Backend::Fallback),
            "a delivered frame must reset the grown wait"
        );
    }

    #[test]
    fn a_graceful_nothing_neither_resets_nor_grows_the_reopen_wait() {
        let start = Instant::now();
        let mut state = failed_over(start);
        state.record(Backend::Fallback, Outcome::Failed, start);
        state.record(
            Backend::Fallback,
            Outcome::Answered,
            start + Duration::from_secs(2),
        );
        state.record(
            Backend::Fallback,
            Outcome::Failed,
            start + Duration::from_secs(2),
        );
        state.record(
            Backend::Primary,
            Outcome::Failed,
            start + Duration::from_secs(2),
        );
        state.record(
            Backend::Primary,
            Outcome::Failed,
            start + Duration::from_secs(4),
        );
        assert_eq!(
            state.target(start + Duration::from_secs(5)),
            None,
            "an idle answer must not reset the wait to the first delay"
        );
        assert_eq!(
            state.target(start + Duration::from_secs(6)),
            Some(Backend::Fallback),
            "an idle answer must not grow the wait either"
        );
    }

    #[test]
    fn a_probe_that_answers_recovers_the_primary_no_matter_the_fallback_wait() {
        let start = Instant::now();
        let mut state = failed_over(start);
        for step in 0..6 {
            state.record(
                Backend::Fallback,
                Outcome::Failed,
                start + Duration::from_secs(step),
            );
        }
        let unlocked = start + Duration::from_secs(6);
        assert_eq!(state.target(unlocked), Some(Backend::Primary));
        assert_eq!(
            state.record(Backend::Primary, Outcome::Answered, unlocked),
            Some(Backend::Primary),
            "recovery must not wait out the fallback's cooldown"
        );
        assert_eq!(state.target(unlocked), Some(Backend::Primary));
    }
}

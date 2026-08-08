//! Which backend a monitor captures with, and when the primary is tried again. Pure state: this
//! table knows two labels and nothing about what is behind them.

use std::time::{Duration, Instant};

/// Consecutive failed ticks before the fallback takes over, so a mode change or a lock screen is
/// ridden out rather than switched on.
const FAILOVER_AFTER: u32 = 3;
/// First primary probe after a failover, doubling per failed probe up to `MAX_PROBE_INTERVAL`.
const FIRST_PROBE_DELAY: Duration = Duration::from_secs(1);
const MAX_PROBE_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backend {
    Primary,
    Fallback,
}

/// What one attempt said. An idle screen counts as `Answered`: only a live session can report that
/// there was nothing to capture.
#[derive(Clone, Copy)]
pub(crate) enum Outcome {
    Answered,
    Failed,
}

enum State {
    Primary {
        failures: u32,
    },
    Fallback {
        probe_at: Instant,
        interval: Duration,
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
    /// Which backend this tick should try first.
    pub(crate) fn target(&self, now: Instant) -> Backend {
        match self.state {
            State::Primary { .. } => Backend::Primary,
            State::Fallback { probe_at, .. } if now >= probe_at => Backend::Primary,
            State::Fallback { .. } => Backend::Fallback,
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
            (_, Backend::Fallback, _) => return None,
            (State::Primary { .. }, Backend::Primary, Outcome::Answered) => {
                (State::Primary { failures: 0 }, None)
            }
            (State::Primary { failures }, Backend::Primary, Outcome::Failed)
                if failures + 1 >= FAILOVER_AFTER =>
            {
                let state = State::Fallback {
                    probe_at: now + FIRST_PROBE_DELAY,
                    interval: FIRST_PROBE_DELAY,
                };
                (state, Some(Backend::Fallback))
            }
            (State::Primary { failures }, Backend::Primary, Outcome::Failed) => (
                State::Primary {
                    failures: failures + 1,
                },
                None,
            ),
            (State::Fallback { .. }, Backend::Primary, Outcome::Answered) => {
                (State::Primary { failures: 0 }, Some(Backend::Primary))
            }
            (State::Fallback { interval, .. }, Backend::Primary, Outcome::Failed) => {
                let interval = (*interval * 2).min(MAX_PROBE_INTERVAL);
                let state = State::Fallback {
                    probe_at: now + interval,
                    interval,
                };
                (state, None)
            }
        };
        self.state = next;
        switched
    }
}

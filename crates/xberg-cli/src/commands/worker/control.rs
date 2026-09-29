//! Request lifetime shared by the dispatcher and its inference lane.
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use xberg::cancellation::CancellationToken;

#[derive(Debug, Default)]
struct State {
    deadline: Option<Instant>,
    reason: Option<&'static str>,
    finished: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn completed_requests_cannot_be_cancelled_and_tokens_are_independent() {
        let first = RequestControl::default();
        let second = RequestControl::default();
        assert!(first.stop("cancelled"));
        assert!(first.token.is_cancelled());
        assert!(!second.token.is_cancelled());
        assert_eq!(first.finish(), Some("cancelled"));
        assert_eq!(second.finish(), None);
        assert!(!second.stop("cancelled"));
    }

    #[test]
    fn expired_deadline_wins_over_late_cancellation() {
        let control = RequestControl::with_timeout(Some(Duration::ZERO)).expect("valid duration");
        assert!(control.stop("cancelled"));
        assert_eq!(control.finish(), Some("timeout"));
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RequestControl {
    pub(crate) token: CancellationToken,
    state: Arc<Mutex<State>>,
}

impl RequestControl {
    pub(super) fn with_timeout(timeout: Option<Duration>) -> Result<Self, &'static str> {
        let deadline = timeout
            .map(|d| Instant::now().checked_add(d).ok_or("timeout_ms is too large"))
            .transpose()?;
        Ok(Self {
            token: CancellationToken::new(),
            state: Arc::new(Mutex::new(State {
                deadline,
                ..State::default()
            })),
        })
    }

    pub(super) fn stop(&self, reason: &'static str) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.finished {
            return false;
        }
        if state.reason.is_none() {
            state.reason = Some(if state.deadline.is_some_and(|d| Instant::now() >= d) {
                "timeout"
            } else {
                reason
            });
        }
        self.token.cancel();
        true
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.finished || state.reason.is_some() {
            None
        } else {
            state.deadline
        }
    }

    pub(super) fn expire(&self) {
        if self.deadline().is_some_and(|d| Instant::now() >= d) {
            self.stop("timeout");
        }
    }

    /// Called only after the handler returned: cancellation acknowledgement and
    /// the terminal response cannot race into contradictory success/cancel states.
    pub(super) fn finish(&self) -> Option<&'static str> {
        self.expire();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.finished = true;
        state.reason
    }
}

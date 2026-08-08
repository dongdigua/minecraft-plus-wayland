use std::time::{Duration, Instant};

pub const INPUT_FLASH_DURATION: Duration = Duration::from_millis(150);
pub const SUCCESS_DURATION: Duration = Duration::from_millis(500);
pub const IDLE_CLEAR_DURATION: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AttemptId(u64);

impl AttemptId {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthDecision {
    Authenticated,
    Denied,
    SystemFailure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockVisual {
    Hidden,
    InputBlue,
    AuthenticatingYellow,
    FailedRed,
    AuthenticatedGreen { attempt: AttemptId },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    LockPending,
    Idle,
    Authenticating {
        attempt: AttemptId,
    },
    AuthFailed {
        retry_after: Instant,
    },
    Authenticated {
        attempt: AttemptId,
        started_at: Instant,
    },
    UnlockRequested,
    Finished,
    Fatal,
}

/// Pure lock/authentication state. Wayland, PAM, channels and GPU objects stay outside this type.
pub struct LockState {
    phase: Phase,
    next_attempt: u64,
    failures: u8,
    input_flash_until: Option<Instant>,
    last_input_at: Option<Instant>,
    compositor_confirmed: bool,
    unlock_called: bool,
}

impl LockState {
    pub fn new() -> Self {
        Self {
            phase: Phase::LockPending,
            next_attempt: 1,
            failures: 0,
            input_flash_until: None,
            last_input_at: None,
            compositor_confirmed: false,
            unlock_called: false,
        }
    }

    /// This is the only transition that enables password editing.
    pub fn compositor_locked(&mut self) -> bool {
        if self.phase != Phase::LockPending {
            return false;
        }
        self.compositor_confirmed = true;
        self.phase = Phase::Idle;
        true
    }

    pub fn can_edit(&self) -> bool {
        self.phase == Phase::Idle && self.compositor_confirmed
    }

    pub fn note_edit(&mut self, now: Instant) {
        if self.can_edit() {
            self.input_flash_until = now.checked_add(INPUT_FLASH_DURATION);
            self.last_input_at = Some(now);
        }
    }

    pub fn note_cancel(&mut self, now: Instant) {
        self.note_edit(now);
    }

    pub fn begin_authentication(&mut self) -> Option<AttemptId> {
        if !self.can_edit() {
            return None;
        }
        let attempt = AttemptId::new(self.next_attempt);
        self.next_attempt = self.next_attempt.checked_add(1)?;
        self.phase = Phase::Authenticating { attempt };
        self.input_flash_until = None;
        self.last_input_at = None;
        Some(attempt)
    }

    /// Returns true only when the current attempt consumed this result.
    pub fn authentication_result(
        &mut self,
        attempt: AttemptId,
        decision: AuthDecision,
        now: Instant,
    ) -> bool {
        if self.phase != (Phase::Authenticating { attempt }) {
            return false;
        }
        match decision {
            AuthDecision::Authenticated => {
                self.phase = Phase::Authenticated {
                    attempt,
                    started_at: now,
                };
            }
            AuthDecision::Denied => {
                self.failures = self.failures.saturating_add(1);
                let shift = u32::from(self.failures.saturating_sub(1).min(3));
                let delay = Duration::from_secs(1_u64 << shift);
                let Some(retry_after) = now.checked_add(delay) else {
                    self.enter_fatal();
                    return true;
                };
                self.phase = Phase::AuthFailed { retry_after };
            }
            AuthDecision::SystemFailure => self.enter_fatal(),
        }
        true
    }

    /// Advances monotonic deadlines. Returns true when an idle password must be cleared.
    pub fn tick(&mut self, now: Instant) -> bool {
        if self
            .input_flash_until
            .is_some_and(|deadline| now >= deadline)
        {
            self.input_flash_until = None;
        }
        match self.phase {
            Phase::Idle => {
                if self
                    .last_input_at
                    .is_some_and(|last| now.saturating_duration_since(last) >= IDLE_CLEAR_DURATION)
                {
                    self.last_input_at = None;
                    self.input_flash_until = None;
                    return true;
                }
            }
            Phase::AuthFailed { retry_after } if now >= retry_after => {
                self.phase = Phase::Idle;
                self.input_flash_until = None;
                self.last_input_at = None;
            }
            _ => {}
        }
        false
    }

    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        let phase_deadline = match self.phase {
            Phase::Idle => self
                .last_input_at
                .and_then(|last| last.checked_add(IDLE_CLEAR_DURATION)),
            Phase::AuthFailed { retry_after } => Some(retry_after),
            Phase::Authenticated { started_at, .. } => started_at.checked_add(SUCCESS_DURATION),
            _ => None,
        };
        let input_deadline = self.input_flash_until.filter(|deadline| *deadline > now);
        let phase_deadline = phase_deadline.filter(|deadline| *deadline > now);
        match (input_deadline, phase_deadline) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
            (None, None) => None,
        }
    }

    pub fn visual(&self, now: Instant) -> LockVisual {
        match self.phase {
            Phase::Idle
                if self
                    .input_flash_until
                    .is_some_and(|deadline| now < deadline) =>
            {
                LockVisual::InputBlue
            }
            Phase::Authenticating { .. } => LockVisual::AuthenticatingYellow,
            Phase::AuthFailed { .. } | Phase::Fatal => LockVisual::FailedRed,
            Phase::Authenticated { attempt, .. } => LockVisual::AuthenticatedGreen { attempt },
            _ => LockVisual::Hidden,
        }
    }

    pub fn prepare_unlock(&mut self, now: Instant, all_outputs_presented: bool) -> bool {
        let Phase::Authenticated { started_at, .. } = self.phase else {
            return false;
        };
        if !all_outputs_presented || now.saturating_duration_since(started_at) < SUCCESS_DURATION {
            return false;
        }
        self.phase = Phase::UnlockRequested;
        true
    }

    /// The protocol adapter must call this immediately before its sole SessionLock::unlock call.
    pub fn consume_unlock_gate(&mut self) -> bool {
        if !self.compositor_confirmed || self.unlock_called || self.phase != Phase::UnlockRequested
        {
            return false;
        }
        self.unlock_called = true;
        true
    }

    pub fn awaiting_unlock_sync(&self) -> bool {
        self.phase == Phase::UnlockRequested && self.unlock_called
    }

    pub fn unlock_sync_completed(&mut self) -> bool {
        if self.phase != Phase::UnlockRequested || !self.unlock_called {
            return false;
        }
        self.phase = Phase::Finished;
        true
    }

    /// Returns whether the compositor had previously confirmed the lock.
    pub fn compositor_finished(&mut self) -> bool {
        let was_locked = self.compositor_confirmed;
        self.input_flash_until = None;
        self.last_input_at = None;
        self.phase = if was_locked {
            Phase::Fatal
        } else {
            Phase::Finished
        };
        was_locked
    }

    pub fn enter_fatal(&mut self) {
        self.input_flash_until = None;
        self.last_input_at = None;
        self.phase = Phase::Fatal;
    }

    pub fn is_fatal(&self) -> bool {
        self.phase == Phase::Fatal
    }
}

impl Default for LockState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_lock_cannot_edit_authenticate_or_unlock() {
        let now = Instant::now();
        let mut state = LockState::new();
        assert!(!state.can_edit());
        assert_eq!(state.begin_authentication(), None);
        assert!(!state.prepare_unlock(now + SUCCESS_DURATION, true));
        assert!(!state.consume_unlock_gate());
    }

    #[test]
    fn only_matching_success_can_reach_the_once_only_unlock_gate() {
        let now = Instant::now();
        let mut state = LockState::new();
        assert!(state.compositor_locked());
        let attempt = state.begin_authentication().unwrap();
        assert!(!state.authentication_result(AttemptId::new(99), AuthDecision::Authenticated, now));
        assert!(state.authentication_result(attempt, AuthDecision::Authenticated, now));
        assert_eq!(state.next_deadline(now), now.checked_add(SUCCESS_DURATION));
        state.tick(now + SUCCESS_DURATION);
        assert_eq!(state.next_deadline(now + SUCCESS_DURATION), None);
        assert!(!state.prepare_unlock(now + SUCCESS_DURATION, false));
        assert!(state.prepare_unlock(now + SUCCESS_DURATION, true));
        assert!(state.consume_unlock_gate());
        assert!(state.awaiting_unlock_sync());
        assert!(!state.consume_unlock_gate());
        assert!(state.unlock_sync_completed());
        assert!(!state.awaiting_unlock_sync());
        assert!(!state.unlock_sync_completed());
    }

    #[test]
    fn denials_back_off_and_never_unlock() {
        let start = Instant::now();
        let mut now = start;
        let mut state = LockState::new();
        state.compositor_locked();
        for expected in [1, 2, 4, 8, 8] {
            let attempt = state.begin_authentication().unwrap();
            state.authentication_result(attempt, AuthDecision::Denied, now);
            assert_eq!(state.visual(now), LockVisual::FailedRed);
            assert!(!state.prepare_unlock(now + Duration::from_secs(expected), true));
            assert!(!state.can_edit());
            now += Duration::from_secs(expected);
            state.tick(now);
            assert!(state.can_edit());
        }
        assert!(!state.consume_unlock_gate());
    }

    #[test]
    fn systemic_failure_and_worker_disconnect_fail_closed() {
        let now = Instant::now();
        let mut state = LockState::new();
        state.compositor_locked();
        let attempt = state.begin_authentication().unwrap();
        state.authentication_result(attempt, AuthDecision::SystemFailure, now);
        assert!(state.is_fatal());
        assert_eq!(state.visual(now), LockVisual::FailedRed);
        assert!(!state.consume_unlock_gate());
    }

    #[test]
    fn blue_flash_and_idle_clear_use_monotonic_deadlines() {
        let now = Instant::now();
        let mut state = LockState::new();
        state.compositor_locked();
        state.note_edit(now);
        assert_eq!(
            state.next_deadline(now),
            now.checked_add(INPUT_FLASH_DURATION)
        );
        assert_eq!(state.visual(now), LockVisual::InputBlue);
        assert_eq!(state.visual(now + INPUT_FLASH_DURATION), LockVisual::Hidden);
        assert!(!state.tick(now + INPUT_FLASH_DURATION));
        assert_eq!(
            state.next_deadline(now + INPUT_FLASH_DURATION),
            now.checked_add(IDLE_CLEAR_DURATION)
        );
        assert!(!state.tick(now + IDLE_CLEAR_DURATION - Duration::from_millis(1)));
        assert!(state.tick(now + IDLE_CLEAR_DURATION));
        assert_eq!(state.next_deadline(now + IDLE_CLEAR_DURATION), None);
    }

    #[test]
    fn finished_after_locked_never_constructs_success() {
        let mut pending = LockState::new();
        assert!(!pending.compositor_finished());
        assert!(!pending.consume_unlock_gate());

        let mut locked = LockState::new();
        locked.compositor_locked();
        assert!(locked.compositor_finished());
        assert!(locked.is_fatal());
        assert!(!locked.consume_unlock_gate());
    }
}

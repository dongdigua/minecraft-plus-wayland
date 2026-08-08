use std::time::{Duration, Instant};

use rand::Rng;

pub const SUCCESS_DURATION: Duration = Duration::from_millis(500);
pub const IDLE_CLEAR_DURATION: Duration = Duration::from_secs(10);
pub const ESC_FLASH_STEP_DURATION: Duration = Duration::from_millis(100);
const ESC_FLASH_STEPS: u8 = 4;

pub const REDSTONE_BIT: u8 = 0b0001;
pub const COPPER_BIT: u8 = 0b0010;
pub const SOUL_BIT: u8 = 0b0100;
pub const TORCH_BIT: u8 = 0b1000;
pub const ALL_TORCHES_MASK: u8 = REDSTONE_BIT | COPPER_BIT | SOUL_BIT | TORCH_BIT;

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
    Torch { mask: u8, state_id: u64 },
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EscFlash {
    step: u8,
    next_step_at: Instant,
}

/// Pure lock/authentication state. Wayland, PAM, channels and GPU objects stay outside this type.
pub struct LockState {
    phase: Phase,
    next_attempt: u64,
    failures: u8,
    torch_mask: u8,
    torch_visible: bool,
    torch_state_id: u64,
    esc_flash: Option<EscFlash>,
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
            torch_mask: ALL_TORCHES_MASK,
            torch_visible: false,
            torch_state_id: 0,
            esc_flash: None,
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
        self.phase == Phase::Idle && self.compositor_confirmed && self.esc_flash.is_none()
    }

    /// Records an edit that actually changed the password and chooses one valid torch flip.
    pub fn note_edit(&mut self, now: Instant) {
        if !self.can_edit() {
            return;
        }
        let valid_count = valid_flip_count(self.torch_mask);
        let choice = rand::thread_rng().gen_range(0, valid_count);
        self.note_edit_with_choice(now, choice);
    }

    fn note_edit_with_choice(&mut self, now: Instant, choice: usize) {
        if !self.can_edit() {
            return;
        }
        self.torch_mask = mask_after_valid_flip(self.torch_mask, choice)
            .expect("random choice is inside the valid torch flip set");
        self.torch_visible = true;
        self.advance_torch_state();
        self.last_input_at = Some(now);
    }

    /// Clears input feedback into the fixed on/off/on/off sequence. Input stays frozen until it ends.
    pub fn note_cancel(&mut self, now: Instant) {
        if !self.can_edit() {
            return;
        }
        self.torch_mask = ALL_TORCHES_MASK;
        self.torch_visible = true;
        self.last_input_at = None;
        let Some(next_step_at) = now.checked_add(ESC_FLASH_STEP_DURATION) else {
            self.enter_fatal();
            return;
        };
        self.esc_flash = Some(EscFlash {
            step: 0,
            next_step_at,
        });
        self.advance_torch_state();
    }

    pub fn begin_authentication(&mut self) -> Option<AttemptId> {
        if !self.can_edit() {
            return None;
        }
        let attempt = AttemptId::new(self.next_attempt);
        self.next_attempt = self.next_attempt.checked_add(1)?;
        self.phase = Phase::Authenticating { attempt };
        self.hide_torches();
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
        while let Some(flash) = self.esc_flash
            && now >= flash.next_step_at
        {
            let next_step = flash.step + 1;
            self.advance_torch_state();
            if next_step == ESC_FLASH_STEPS {
                self.torch_mask = ALL_TORCHES_MASK;
                self.esc_flash = None;
                // Timeout begins at the exact end of the flash, even if dispatch was delayed.
                self.last_input_at = Some(flash.next_step_at);
                break;
            }
            self.torch_mask = if next_step.is_multiple_of(2) {
                ALL_TORCHES_MASK
            } else {
                0
            };
            let Some(next_step_at) = flash.next_step_at.checked_add(ESC_FLASH_STEP_DURATION) else {
                self.enter_fatal();
                return false;
            };
            self.esc_flash = Some(EscFlash {
                step: next_step,
                next_step_at,
            });
        }

        match self.phase {
            Phase::Idle if self.esc_flash.is_none() => {
                if self
                    .last_input_at
                    .is_some_and(|last| now.saturating_duration_since(last) >= IDLE_CLEAR_DURATION)
                {
                    self.hide_torches();
                    return true;
                }
            }
            Phase::AuthFailed { retry_after } if now >= retry_after => {
                self.phase = Phase::Idle;
                self.hide_torches();
            }
            _ => {}
        }
        false
    }

    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        let phase_deadline = match self.phase {
            Phase::Idle => self.esc_flash.map(|flash| flash.next_step_at).or_else(|| {
                self.last_input_at
                    .and_then(|last| last.checked_add(IDLE_CLEAR_DURATION))
            }),
            Phase::AuthFailed { retry_after } => Some(retry_after),
            Phase::Authenticated { started_at, .. } => started_at.checked_add(SUCCESS_DURATION),
            _ => None,
        };
        phase_deadline.filter(|deadline| *deadline > now)
    }

    pub fn visual(&self, _now: Instant) -> LockVisual {
        match self.phase {
            Phase::Idle if self.torch_visible => LockVisual::Torch {
                mask: self.torch_mask,
                state_id: self.torch_state_id,
            },
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
        self.hide_torches();
        self.phase = if was_locked {
            Phase::Fatal
        } else {
            Phase::Finished
        };
        was_locked
    }

    pub fn enter_fatal(&mut self) {
        self.hide_torches();
        self.phase = Phase::Fatal;
    }

    pub fn is_fatal(&self) -> bool {
        self.phase == Phase::Fatal
    }

    fn advance_torch_state(&mut self) {
        self.torch_state_id = self.torch_state_id.wrapping_add(1);
    }

    fn hide_torches(&mut self) {
        self.torch_visible = false;
        self.esc_flash = None;
        self.last_input_at = None;
    }
}

impl Default for LockState {
    fn default() -> Self {
        Self::new()
    }
}

fn valid_flip_count(mask: u8) -> usize {
    (0..4)
        .filter(|bit| {
            let candidate = mask ^ (1 << bit);
            candidate != 0 && candidate != ALL_TORCHES_MASK
        })
        .count()
}

fn mask_after_valid_flip(mask: u8, choice: usize) -> Option<u8> {
    (0..4)
        .map(|bit| mask ^ (1 << bit))
        .filter(|candidate| *candidate != 0 && *candidate != ALL_TORCHES_MASK)
        .nth(choice)
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
    fn every_valid_mask_flip_excludes_both_endpoints() {
        assert_eq!(REDSTONE_BIT, 1 << 0);
        assert_eq!(COPPER_BIT, 1 << 1);
        assert_eq!(SOUL_BIT, 1 << 2);
        assert_eq!(TORCH_BIT, 1 << 3);

        for mask in 0..=ALL_TORCHES_MASK {
            let count = valid_flip_count(mask);
            for choice in 0..count {
                let result = mask_after_valid_flip(mask, choice).unwrap();
                assert_ne!(result, 0);
                assert_ne!(result, ALL_TORCHES_MASK);
                assert_eq!((mask ^ result).count_ones(), 1);
            }
        }
    }

    #[test]
    fn torch_flips_only_when_an_actual_edit_is_recorded() {
        let now = Instant::now();
        let mut state = LockState::new();
        state.compositor_locked();
        assert_eq!(state.visual(now), LockVisual::Hidden);

        // Rejected/no-op editing never calls note_edit, so neither state nor mask changes.
        assert_eq!(
            state.visual(now + Duration::from_millis(1)),
            LockVisual::Hidden
        );
        state.note_edit_with_choice(now, 0);
        let LockVisual::Torch { mask, state_id } = state.visual(now) else {
            panic!("successful edit did not show torches");
        };
        assert_ne!(mask, 0);
        assert_ne!(mask, ALL_TORCHES_MASK);
        assert_eq!(state.visual(now), LockVisual::Torch { mask, state_id });
    }

    #[test]
    fn esc_freezes_input_flashes_twice_and_restarts_timeout_at_end() {
        let start = Instant::now();
        let mut state = LockState::new();
        state.compositor_locked();
        state.note_edit_with_choice(start, 0);
        state.note_cancel(start);
        assert!(!state.can_edit());

        for (step, mask) in [(0, ALL_TORCHES_MASK), (1, 0), (2, ALL_TORCHES_MASK), (3, 0)] {
            let at = start + ESC_FLASH_STEP_DURATION * step;
            state.tick(at);
            assert!(
                matches!(state.visual(at), LockVisual::Torch { mask: actual, .. } if actual == mask)
            );
            assert!(!state.can_edit());
        }

        let end = start + ESC_FLASH_STEP_DURATION * 4;
        assert!(!state.tick(end));
        assert!(state.can_edit());
        assert!(matches!(
            state.visual(end),
            LockVisual::Torch {
                mask: ALL_TORCHES_MASK,
                ..
            }
        ));
        assert_eq!(
            state.next_deadline(end),
            end.checked_add(IDLE_CLEAR_DURATION)
        );
        assert!(!state.tick(end + IDLE_CLEAR_DURATION - Duration::from_millis(1)));
        assert!(state.tick(end + IDLE_CLEAR_DURATION));
        assert_eq!(state.visual(end + IDLE_CLEAR_DURATION), LockVisual::Hidden);
    }

    #[test]
    fn ordinary_input_times_out_to_the_module() {
        let now = Instant::now();
        let mut state = LockState::new();
        state.compositor_locked();
        state.note_edit_with_choice(now, 0);
        assert!(!state.tick(now + IDLE_CLEAR_DURATION - Duration::from_millis(1)));
        assert!(state.tick(now + IDLE_CLEAR_DURATION));
        assert_eq!(state.visual(now + IDLE_CLEAR_DURATION), LockVisual::Hidden);
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

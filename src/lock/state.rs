use std::time::{Duration, Instant};

use rand::Rng;

pub const CREEPER_APPROACH_DURATION: Duration = Duration::from_millis(500);
pub const DISSOLVE_DURATION: Duration = Duration::from_secs(1);
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
    Torch {
        mask: u8,
        state_id: u64,
    },
    Creeper {
        approach_started_at: Instant,
        red: bool,
    },
    DissolvingCreeper {
        attempt: AttemptId,
        approach_started_at: Instant,
        started_at: Instant,
    },
    FatalBlack,
}

impl LockVisual {
    pub fn wants_continuous_frames(self, now: Instant) -> bool {
        match self {
            Self::Creeper {
                approach_started_at,
                ..
            } => now.saturating_duration_since(approach_started_at) < CREEPER_APPROACH_DURATION,
            // Keep retrying the terminal all-black frame until every output has actually
            // presented it and the state leaves this visual through the unlock gate.
            Self::DissolvingCreeper { .. } => true,
            _ => false,
        }
    }

    pub fn completed_dissolve_attempt(self, now: Instant) -> Option<AttemptId> {
        let Self::DissolvingCreeper {
            attempt,
            started_at,
            ..
        } = self
        else {
            return None;
        };
        (now.saturating_duration_since(started_at) >= DISSOLVE_DURATION).then_some(attempt)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    LockPending,
    Idle,
    Authenticating {
        attempt: AttemptId,
        approach_started_at: Instant,
    },
    AuthFailed {
        retry_after: Instant,
        approach_started_at: Instant,
    },
    Authenticated {
        attempt: AttemptId,
        approach_started_at: Instant,
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

    pub fn begin_authentication(&mut self, now: Instant) -> Option<AttemptId> {
        if !self.can_edit() {
            return None;
        }
        let attempt = AttemptId::new(self.next_attempt);
        self.next_attempt = self.next_attempt.checked_add(1)?;
        self.phase = Phase::Authenticating {
            attempt,
            approach_started_at: now,
        };
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
        let Phase::Authenticating {
            attempt: current_attempt,
            approach_started_at,
        } = self.phase
        else {
            return false;
        };
        if current_attempt != attempt {
            return false;
        }
        match decision {
            AuthDecision::Authenticated => {
                self.phase = Phase::Authenticated {
                    attempt,
                    approach_started_at,
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
                self.phase = Phase::AuthFailed {
                    retry_after,
                    approach_started_at,
                };
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
            Phase::AuthFailed { retry_after, .. } if now >= retry_after => {
                self.phase = Phase::Idle;
                self.torch_mask = ALL_TORCHES_MASK;
                self.torch_visible = true;
                self.advance_torch_state();
                // A failed password is never restored. The initial all-on torch scene starts a
                // fresh empty-input idle period at the exact end of the authentication backoff.
                self.last_input_at = Some(retry_after);
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
            Phase::AuthFailed { retry_after, .. } => Some(retry_after),
            Phase::Authenticated { started_at, .. } => started_at.checked_add(DISSOLVE_DURATION),
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
            Phase::Authenticating {
                approach_started_at,
                ..
            } => LockVisual::Creeper {
                approach_started_at,
                red: false,
            },
            Phase::AuthFailed {
                approach_started_at,
                ..
            } => LockVisual::Creeper {
                approach_started_at,
                red: true,
            },
            Phase::Authenticated {
                attempt,
                approach_started_at,
                started_at,
            } => LockVisual::DissolvingCreeper {
                attempt,
                approach_started_at,
                started_at,
            },
            Phase::Fatal => LockVisual::FatalBlack,
            _ => LockVisual::Hidden,
        }
    }

    pub fn prepare_unlock(&mut self, now: Instant, all_outputs_presented: bool) -> bool {
        let Phase::Authenticated { started_at, .. } = self.phase else {
            return false;
        };
        if !all_outputs_presented || now.saturating_duration_since(started_at) < DISSOLVE_DURATION {
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

    pub fn can_use_lock_surfaces(&self) -> bool {
        !self.unlock_called && !matches!(self.phase, Phase::Finished | Phase::Fatal)
    }

    pub fn accepts_new_outputs(&self) -> bool {
        self.can_use_lock_surfaces() && self.phase != Phase::UnlockRequested
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
        assert_eq!(state.begin_authentication(now), None);
        assert!(!state.prepare_unlock(now + DISSOLVE_DURATION, true));
        assert!(!state.consume_unlock_gate());
    }

    #[test]
    fn only_matching_success_and_presented_dissolve_can_reach_unlock_gate() {
        let approach_started_at = Instant::now();
        let authenticated_at = approach_started_at + Duration::from_millis(200);
        let mut state = LockState::new();
        assert!(state.compositor_locked());
        let attempt = state.begin_authentication(approach_started_at).unwrap();
        assert!(!state.authentication_result(
            AttemptId::new(99),
            AuthDecision::Authenticated,
            authenticated_at
        ));
        assert!(state.authentication_result(
            attempt,
            AuthDecision::Authenticated,
            authenticated_at
        ));
        assert_eq!(
            state.visual(authenticated_at),
            LockVisual::DissolvingCreeper {
                attempt,
                approach_started_at,
                started_at: authenticated_at,
            }
        );
        assert_eq!(
            state.next_deadline(authenticated_at),
            authenticated_at.checked_add(DISSOLVE_DURATION)
        );
        assert!(!state.prepare_unlock(
            authenticated_at + DISSOLVE_DURATION - Duration::from_millis(1),
            true
        ));
        assert!(!state.prepare_unlock(authenticated_at + DISSOLVE_DURATION, false));
        assert!(state.can_use_lock_surfaces());
        assert!(state.accepts_new_outputs());
        assert!(state.prepare_unlock(authenticated_at + DISSOLVE_DURATION, true));
        assert!(state.can_use_lock_surfaces());
        assert!(!state.accepts_new_outputs());
        assert!(state.consume_unlock_gate());
        assert!(state.awaiting_unlock_sync());
        assert!(!state.can_use_lock_surfaces());
        assert!(!state.accepts_new_outputs());
        assert!(!state.consume_unlock_gate());
        assert!(state.unlock_sync_completed());
        assert!(!state.awaiting_unlock_sync());
        assert!(!state.can_use_lock_surfaces());
        assert!(!state.accepts_new_outputs());
        assert!(!state.unlock_sync_completed());
    }

    #[test]
    fn success_preserves_the_independent_approach_timeline() {
        let approach_started_at = Instant::now();
        for authenticated_after in [
            Duration::ZERO,
            Duration::from_millis(200),
            Duration::from_secs(2),
        ] {
            let authenticated_at = approach_started_at + authenticated_after;
            let mut state = LockState::new();
            state.compositor_locked();
            let attempt = state.begin_authentication(approach_started_at).unwrap();
            state.authentication_result(attempt, AuthDecision::Authenticated, authenticated_at);
            assert_eq!(
                state.visual(authenticated_at),
                LockVisual::DissolvingCreeper {
                    attempt,
                    approach_started_at,
                    started_at: authenticated_at,
                }
            );
        }
    }

    #[test]
    fn visual_frame_chain_and_completion_use_monotonic_deadlines() {
        let started_at = Instant::now();
        let approaching = LockVisual::Creeper {
            approach_started_at: started_at,
            red: false,
        };
        assert!(approaching.wants_continuous_frames(
            started_at + CREEPER_APPROACH_DURATION - Duration::from_millis(1)
        ));
        assert!(!approaching.wants_continuous_frames(started_at + CREEPER_APPROACH_DURATION));

        let attempt = AttemptId::new(7);
        let dissolving = LockVisual::DissolvingCreeper {
            attempt,
            approach_started_at: started_at,
            started_at,
        };
        assert!(dissolving.wants_continuous_frames(started_at + DISSOLVE_DURATION));
        assert_eq!(
            dissolving.completed_dissolve_attempt(
                started_at + DISSOLVE_DURATION - Duration::from_millis(1)
            ),
            None
        );
        assert_eq!(
            dissolving.completed_dissolve_attempt(started_at + DISSOLVE_DURATION),
            Some(attempt)
        );
    }

    #[test]
    fn denials_back_off_return_to_empty_all_on_torches_and_never_unlock() {
        let start = Instant::now();
        let mut now = start;
        let mut state = LockState::new();
        state.compositor_locked();
        for expected in [1, 2, 4, 8, 8] {
            state.note_edit_with_choice(now, 0);
            let attempt = state.begin_authentication(now).unwrap();
            state.authentication_result(attempt, AuthDecision::Denied, now);
            assert_eq!(
                state.visual(now),
                LockVisual::Creeper {
                    approach_started_at: now,
                    red: true,
                }
            );
            assert!(!state.prepare_unlock(now + Duration::from_secs(expected), true));
            assert!(!state.can_edit());
            now += Duration::from_secs(expected);
            state.tick(now);
            assert!(state.can_edit());
            assert!(matches!(
                state.visual(now),
                LockVisual::Torch {
                    mask: ALL_TORCHES_MASK,
                    ..
                }
            ));
        }
        assert!(!state.consume_unlock_gate());
    }

    #[test]
    fn systemic_failure_and_worker_disconnect_fail_closed() {
        let now = Instant::now();
        let mut state = LockState::new();
        state.compositor_locked();
        let attempt = state.begin_authentication(now).unwrap();
        state.authentication_result(attempt, AuthDecision::SystemFailure, now);
        assert!(state.is_fatal());
        assert_eq!(state.visual(now), LockVisual::FatalBlack);
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

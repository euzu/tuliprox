//! The one place that decides which recording state changes are legal.
//!
//! Before this module the rules were restated in at least four places: the
//! queue's `pause_active` / `resume_active` / `retry_finished`, the service's
//! own guards, a stringly-typed `EDITABLE_STATES` list, and the frontend's
//! action table. They had already drifted — the queue would pause a task in
//! any state as long as its kind was resumable, while the frontend only
//! offered pause for three of them.
//!
//! Everything here is a pure function of `(kind, state, command)`. The allowed
//! action set is *projected from the same predicates* rather than restated, so
//! a rule and the button that triggers it cannot disagree.

use shared::model::{RecordingKind, RecordingTaskState};

/// A state-changing command a principal can issue against a recording.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingCommand {
    Pause,
    Resume,
    Cancel,
    Retry,
}

impl RecordingCommand {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Cancel => "cancel",
            Self::Retry => "retry",
        }
    }
}

/// Why a command was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransitionError {
    /// The media kind never supports this command. A Live capture cannot be
    /// paused or resumed without losing part of an unrepeatable broadcast, and
    /// cannot be retried because its programme window is gone.
    NotSupportedForKind { kind: RecordingKind, command: RecordingCommand },
    /// The command is supported, but not from this state.
    NotAllowedInState { state: RecordingTaskState, command: RecordingCommand },
}

impl std::fmt::Display for TransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSupportedForKind { kind, command } => {
                write!(f, "a {kind} recording cannot be {}d", command.as_str())
            }
            Self::NotAllowedInState { state, command } => {
                write!(f, "cannot {} a recording in state {}", command.as_str(), state.label())
            }
        }
    }
}

impl std::error::Error for TransitionError {}

/// The states a running transfer can be interrupted from.
const PAUSABLE: &[RecordingTaskState] =
    &[RecordingTaskState::Running, RecordingTaskState::WaitingForCapacity, RecordingTaskState::RetryWaiting];

/// The states a recording can still be stopped from, i.e. everything before it
/// reaches a terminal state.
const CANCELLABLE: &[RecordingTaskState] = &[
    RecordingTaskState::Queued,
    RecordingTaskState::Scheduled,
    RecordingTaskState::Running,
    RecordingTaskState::Paused,
    RecordingTaskState::WaitingForCapacity,
    RecordingTaskState::RetryWaiting,
];

/// The states an edit may still change the plan from. Once a recording is
/// running its window is being written to disk, so the plan is fixed.
const EDITABLE: &[RecordingTaskState] = &[
    RecordingTaskState::Scheduled,
    RecordingTaskState::Queued,
    RecordingTaskState::WaitingForCapacity,
    RecordingTaskState::RetryWaiting,
];

/// The terminal states a failed or cancelled transfer can be restarted from.
const RETRYABLE: &[RecordingTaskState] = &[RecordingTaskState::Failed, RecordingTaskState::Cancelled];

/// Applies `command` to a recording, returning the state it moves to.
pub fn transition(
    kind: RecordingKind,
    current: RecordingTaskState,
    command: RecordingCommand,
) -> Result<RecordingTaskState, TransitionError> {
    let unsupported = TransitionError::NotSupportedForKind { kind, command };
    let not_allowed = TransitionError::NotAllowedInState { state: current, command };
    match command {
        RecordingCommand::Pause => {
            if !kind.is_resumable() {
                return Err(unsupported);
            }
            PAUSABLE.contains(&current).then_some(RecordingTaskState::Paused).ok_or(not_allowed)
        }
        RecordingCommand::Resume => {
            if !kind.is_resumable() {
                return Err(unsupported);
            }
            (current == RecordingTaskState::Paused).then_some(RecordingTaskState::Running).ok_or(not_allowed)
        }
        RecordingCommand::Cancel => {
            CANCELLABLE.contains(&current).then_some(RecordingTaskState::Cancelled).ok_or(not_allowed)
        }
        RecordingCommand::Retry => {
            if !kind.is_resumable() {
                return Err(unsupported);
            }
            RETRYABLE.contains(&current).then_some(RecordingTaskState::Queued).ok_or(not_allowed)
        }
    }
}

/// `true` when the recording plan can still be changed.
pub fn can_edit(state: RecordingTaskState) -> bool { EDITABLE.contains(&state) }

/// `true` when a terminal recording can be removed from a library.
pub fn can_remove(state: RecordingTaskState) -> bool { state.is_terminal() }

/// `true` when a recording of this kind can legitimately be found in `state`.
///
/// A Live capture has no pause and no retry backoff, so those two states are
/// unreachable for it. A record that claims otherwise is corrupt.
pub fn state_is_reachable(kind: RecordingKind, state: RecordingTaskState) -> bool {
    kind.is_resumable() || !matches!(state, RecordingTaskState::Paused | RecordingTaskState::RetryWaiting)
}

/// Which controls a recording currently offers.
///
/// Every field is derived from the same predicate the command itself uses, so
/// a control cannot be offered for a command that would then be refused.
// One flag per control is the point: the set maps 1:1 onto the buttons a
// client renders, and collapsing it would hide which command was refused.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RecordingAllowedActions {
    pub pause: bool,
    pub resume: bool,
    pub cancel: bool,
    pub retry: bool,
    pub edit: bool,
    pub remove: bool,
}

pub fn allowed_actions(kind: RecordingKind, state: RecordingTaskState) -> RecordingAllowedActions {
    let allows = |command| transition(kind, state, command).is_ok();
    RecordingAllowedActions {
        pause: allows(RecordingCommand::Pause),
        resume: allows(RecordingCommand::Resume),
        cancel: allows(RecordingCommand::Cancel),
        retry: allows(RecordingCommand::Retry),
        edit: can_edit(state),
        remove: can_remove(state),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        allowed_actions, can_edit, can_remove, state_is_reachable, transition, RecordingAllowedActions,
        RecordingCommand, TransitionError,
    };
    use shared::model::{RecordingKind, RecordingTaskState};

    const ALL_KINDS: [RecordingKind; 3] = [RecordingKind::Live, RecordingKind::Vod, RecordingKind::Series];
    const ALL_STATES: [RecordingTaskState; 9] = [
        RecordingTaskState::Queued,
        RecordingTaskState::Scheduled,
        RecordingTaskState::WaitingForCapacity,
        RecordingTaskState::RetryWaiting,
        RecordingTaskState::Running,
        RecordingTaskState::Paused,
        RecordingTaskState::Completed,
        RecordingTaskState::Failed,
        RecordingTaskState::Cancelled,
    ];
    const ALL_COMMANDS: [RecordingCommand; 4] =
        [RecordingCommand::Pause, RecordingCommand::Resume, RecordingCommand::Cancel, RecordingCommand::Retry];

    /// Every edge the graph allows, as `(kind is resumable, from, command, to)`.
    /// Anything not listed here must be refused.
    const ALLOWED_EDGES: &[(bool, RecordingTaskState, RecordingCommand, RecordingTaskState)] = &[
        (true, RecordingTaskState::Running, RecordingCommand::Pause, RecordingTaskState::Paused),
        (true, RecordingTaskState::WaitingForCapacity, RecordingCommand::Pause, RecordingTaskState::Paused),
        (true, RecordingTaskState::RetryWaiting, RecordingCommand::Pause, RecordingTaskState::Paused),
        (true, RecordingTaskState::Paused, RecordingCommand::Resume, RecordingTaskState::Running),
        (true, RecordingTaskState::Failed, RecordingCommand::Retry, RecordingTaskState::Queued),
        (true, RecordingTaskState::Cancelled, RecordingCommand::Retry, RecordingTaskState::Queued),
        (false, RecordingTaskState::Queued, RecordingCommand::Cancel, RecordingTaskState::Cancelled),
        (false, RecordingTaskState::Scheduled, RecordingCommand::Cancel, RecordingTaskState::Cancelled),
        (false, RecordingTaskState::Running, RecordingCommand::Cancel, RecordingTaskState::Cancelled),
        (false, RecordingTaskState::Paused, RecordingCommand::Cancel, RecordingTaskState::Cancelled),
        (false, RecordingTaskState::WaitingForCapacity, RecordingCommand::Cancel, RecordingTaskState::Cancelled),
        (false, RecordingTaskState::RetryWaiting, RecordingCommand::Cancel, RecordingTaskState::Cancelled),
    ];

    /// Looks the edge up in the table above; `None` means it must be refused.
    fn expected(
        kind: RecordingKind,
        from: RecordingTaskState,
        command: RecordingCommand,
    ) -> Option<RecordingTaskState> {
        ALLOWED_EDGES.iter().find_map(|(needs_resumable, edge_from, edge_command, to)| {
            let kind_ok = !needs_resumable || kind.is_resumable();
            (kind_ok && *edge_from == from && *edge_command == command).then_some(*to)
        })
    }

    #[test]
    fn the_transition_table_is_exhaustive() {
        for kind in ALL_KINDS {
            for state in ALL_STATES {
                for command in ALL_COMMANDS {
                    let actual = transition(kind, state, command).ok();
                    assert_eq!(
                        actual,
                        expected(kind, state, command),
                        "{kind} in {} given {}",
                        state.label(),
                        command.as_str()
                    );
                }
            }
        }
    }

    #[test]
    fn live_denies_pause_resume_and_retry_from_every_state() {
        for state in ALL_STATES {
            for command in [RecordingCommand::Pause, RecordingCommand::Resume, RecordingCommand::Retry] {
                let error = transition(RecordingKind::Live, state, command).expect_err("live must refuse");
                // The refusal must name the kind, not the state: a Live
                // recording is never one state away from being pausable.
                assert_eq!(
                    error,
                    TransitionError::NotSupportedForKind { kind: RecordingKind::Live, command },
                    "live in {}",
                    state.label()
                );
            }
        }
    }

    #[test]
    fn live_can_never_be_paused_or_retry_waiting() {
        for state in [RecordingTaskState::Paused, RecordingTaskState::RetryWaiting] {
            assert!(!state_is_reachable(RecordingKind::Live, state), "{} must be unreachable for live", state.label());
        }
        for kind in [RecordingKind::Vod, RecordingKind::Series] {
            for state in ALL_STATES {
                assert!(state_is_reachable(kind, state));
            }
        }
    }

    #[test]
    fn live_can_still_be_cancelled_while_it_runs() {
        assert_eq!(
            transition(RecordingKind::Live, RecordingTaskState::Running, RecordingCommand::Cancel),
            Ok(RecordingTaskState::Cancelled)
        );
    }

    #[test]
    fn terminal_states_accept_no_command_except_retry() {
        for kind in ALL_KINDS {
            for state in [RecordingTaskState::Completed, RecordingTaskState::Failed, RecordingTaskState::Cancelled] {
                for command in [RecordingCommand::Pause, RecordingCommand::Resume, RecordingCommand::Cancel] {
                    assert!(transition(kind, state, command).is_err(), "{kind} in {} {command:?}", state.label());
                }
            }
            // Completed is final even for a resumable kind: there is nothing
            // left to fetch.
            assert!(transition(kind, RecordingTaskState::Completed, RecordingCommand::Retry).is_err());
        }
    }

    #[test]
    fn only_upcoming_states_are_editable() {
        for state in ALL_STATES {
            let editable = matches!(
                state,
                RecordingTaskState::Scheduled
                    | RecordingTaskState::Queued
                    | RecordingTaskState::WaitingForCapacity
                    | RecordingTaskState::RetryWaiting
            );
            assert_eq!(can_edit(state), editable, "{}", state.label());
        }
    }

    #[test]
    fn only_terminal_states_can_be_removed() {
        for state in ALL_STATES {
            assert_eq!(can_remove(state), state.is_terminal(), "{}", state.label());
        }
    }

    #[test]
    fn allowed_actions_never_offer_a_command_that_would_be_refused() {
        for kind in ALL_KINDS {
            for state in ALL_STATES {
                let actions = allowed_actions(kind, state);
                let offered = [
                    (actions.pause, RecordingCommand::Pause),
                    (actions.resume, RecordingCommand::Resume),
                    (actions.cancel, RecordingCommand::Cancel),
                    (actions.retry, RecordingCommand::Retry),
                ];
                for (offered, command) in offered {
                    assert_eq!(
                        offered,
                        transition(kind, state, command).is_ok(),
                        "{kind} in {} offers {} but the command disagrees",
                        state.label(),
                        command.as_str()
                    );
                }
            }
        }
    }

    #[test]
    fn a_live_capture_only_offers_cancel_and_remove() {
        let running = allowed_actions(RecordingKind::Live, RecordingTaskState::Running);
        assert_eq!(running, RecordingAllowedActions { cancel: true, ..RecordingAllowedActions::default() });

        let failed = allowed_actions(RecordingKind::Live, RecordingTaskState::Failed);
        assert_eq!(failed, RecordingAllowedActions { remove: true, ..RecordingAllowedActions::default() });
    }

    #[test]
    fn a_scheduled_recording_can_be_edited_and_cancelled_but_not_removed() {
        for kind in ALL_KINDS {
            let actions = allowed_actions(kind, RecordingTaskState::Scheduled);
            assert!(actions.edit);
            assert!(actions.cancel);
            assert!(!actions.remove, "an unfinished recording must be cancelled before it can be removed");
        }
    }
}

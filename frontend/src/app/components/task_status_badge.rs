//! Localized status pill for transfer and recording tasks.
//!
//! Task state used to reach the user two different ways: the downloads
//! view translated it, while the recording library rendered
//! `format!("{:?}", status)` — raw Rust debug output, untranslated and
//! inconsistent with the rest of the UI. This component is the single
//! renderer for both, so a state is named and coloured the same way
//! everywhere it appears.

use crate::i18n::YewI18n;
use shared::model::{TaskKindDto, TransferStatusDto};
use yew::{classes, component, html, Html, Properties};

/// The i18n key for a task state.
///
/// Only the running state depends on `kind`: a download in flight is
/// "Downloading", a recording in flight is "Recording". Every other
/// state reads the same for both.
pub fn task_status_i18n_key(status: &TransferStatusDto, kind: &TaskKindDto) -> &'static str {
    match status {
        TransferStatusDto::Scheduled => "LABEL.TASK_STATUS_SCHEDULED",
        TransferStatusDto::Queued => "LABEL.TASK_STATUS_QUEUED",
        TransferStatusDto::WaitingForCapacity => "LABEL.TASK_STATUS_WAITING_FOR_CAPACITY",
        TransferStatusDto::RetryWaiting => "LABEL.TASK_STATUS_RETRY_WAITING",
        TransferStatusDto::Running => match kind {
            TaskKindDto::Recording => "LABEL.TASK_STATUS_RUNNING",
            TaskKindDto::Download => "LABEL.DOWNLOAD_STATE_DOWNLOADING",
        },
        TransferStatusDto::Paused => "LABEL.TASK_STATUS_PAUSED",
        TransferStatusDto::Completed => "LABEL.TASK_STATUS_COMPLETED",
        TransferStatusDto::Failed => "LABEL.TASK_STATUS_FAILED",
        TransferStatusDto::Cancelled => "LABEL.TASK_STATUS_CANCELLED",
    }
}

/// The CSS modifier for a task state. Grouped by what the state means
/// to the user — pending, active, done, or gone — rather than one
/// colour per variant, so the palette stays readable.
pub fn task_status_modifier(status: &TransferStatusDto) -> &'static str {
    match status {
        TransferStatusDto::Scheduled | TransferStatusDto::Queued => "tp__task-status--pending",
        TransferStatusDto::WaitingForCapacity | TransferStatusDto::RetryWaiting => "tp__task-status--waiting",
        TransferStatusDto::Running => "tp__task-status--active",
        TransferStatusDto::Paused => "tp__task-status--paused",
        TransferStatusDto::Completed => "tp__task-status--done",
        TransferStatusDto::Failed => "tp__task-status--failed",
        TransferStatusDto::Cancelled => "tp__task-status--cancelled",
    }
}

/// Translated state name. Use this wherever a plain string is needed
/// (sorting, tooltips, aria labels) instead of the component.
pub fn task_status_label(translate: &YewI18n, status: &TransferStatusDto, kind: &TaskKindDto) -> String {
    translate.t(task_status_i18n_key(status, kind))
}

#[derive(Properties, Clone, PartialEq)]
pub struct TaskStatusBadgeProps {
    pub status: TransferStatusDto,
    #[prop_or(TaskKindDto::Download)]
    pub kind: TaskKindDto,
    /// Optional detail shown after the state name — a failure reason,
    /// for instance. Kept out of the pill's own styling so the state
    /// stays scannable.
    #[prop_or(None)]
    pub detail: Option<String>,
}

#[component]
pub fn TaskStatusBadge(props: &TaskStatusBadgeProps) -> Html {
    let translate = crate::i18n::use_translation();
    let label = task_status_label(&translate, &props.status, &props.kind);
    let title = props
        .detail
        .as_ref()
        .map_or_else(|| label.clone(), |detail| format!("{label}: {detail}"));
    html! {
        <span
            class={classes!("tp__task-status", task_status_modifier(&props.status))}
            title={title}
            aria-label={label.clone()}
        >
            { label }
        </span>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: &[TransferStatusDto] = &[
        TransferStatusDto::Scheduled,
        TransferStatusDto::Queued,
        TransferStatusDto::WaitingForCapacity,
        TransferStatusDto::RetryWaiting,
        TransferStatusDto::Running,
        TransferStatusDto::Paused,
        TransferStatusDto::Completed,
        TransferStatusDto::Failed,
        TransferStatusDto::Cancelled,
    ];

    #[test]
    fn every_state_has_a_distinct_i18n_key() {
        for kind in [TaskKindDto::Download, TaskKindDto::Recording] {
            let mut keys: Vec<&str> = ALL.iter().map(|s| task_status_i18n_key(s, &kind)).collect();
            let total = keys.len();
            keys.sort_unstable();
            keys.dedup();
            assert_eq!(keys.len(), total, "two states share an i18n key for {kind:?}");
        }
    }

    #[test]
    fn only_the_running_state_is_named_per_kind() {
        for status in ALL {
            let download = task_status_i18n_key(status, &TaskKindDto::Download);
            let recording = task_status_i18n_key(status, &TaskKindDto::Recording);
            if matches!(status, TransferStatusDto::Running) {
                assert_ne!(download, recording, "a running download is not a running recording");
            } else {
                assert_eq!(download, recording, "{status:?} must read the same for both kinds");
            }
        }
    }

    #[test]
    fn every_state_has_a_modifier_class() {
        for status in ALL {
            let modifier = task_status_modifier(status);
            assert!(modifier.starts_with("tp__task-status--"), "{modifier}");
        }
    }

    #[test]
    fn terminal_states_do_not_share_a_colour_with_active_ones() {
        assert_ne!(
            task_status_modifier(&TransferStatusDto::Failed),
            task_status_modifier(&TransferStatusDto::Completed)
        );
        assert_ne!(
            task_status_modifier(&TransferStatusDto::Running),
            task_status_modifier(&TransferStatusDto::Completed)
        );
    }
}

//! DVR library + quota view.
//!
//! The view keeps recordings separate from other media, displays private and
//! shared quota, and gates delete/edit controls by `recording.write` plus the
//! per-task ownership policy.

use super::recording_edit_view::{EditingTaskId, RecordingEditView};
use crate::{
    app::components::{text_button::TextButton, Table, TableDefinition},
    hooks::use_service_context,
    i18n::use_translation,
    model::{DialogResult, EventMessage},
    services::{DialogService, RecordingQuota, RecordingService, RecordingTaskResponse},
};
use shared::model::{
    permission::Permission,
    recording::{RecordingOwner, RecordingVisibility},
    web_socket::ProtocolMessage,
    SortOrder, UserId,
};
use std::rc::Rc;
use yew::prelude::*;

/// Permission gate: should the DVR navigation entry show?
/// True when the principal has `recording.read`.
#[allow(dead_code)]
pub fn can_show_dvr_nav(has_recording_read: bool) -> bool { has_recording_read }

/// Permission gate: can this principal edit/delete the given
/// task? Real users may edit their own private tasks with
/// `recording.write`. Administrators may edit/delete any
/// visible task (private, shared, or `LegacyAdmin`).
///
/// `is_admin_role` is true when the principal's roles include
/// the built-in administrator role. `is_owner` is true when
/// the principal is the immutable `UserId` owner of a private
/// task.
pub fn can_mutate_task(has_recording_write: bool, is_admin_role: bool, is_owner: bool) -> bool {
    if !has_recording_write {
        return false;
    }
    is_admin_role || is_owner
}

/// A task is visible in the recording library unless it is marked
/// `Deleting` (read via the DTO's `deleting_previous_state` flag)
/// or has no recording metadata — i.e. it is a generic download
/// rather than a recording.
pub fn is_visible_recording_task(
    owner: Option<&RecordingOwner>,
    visibility: Option<&RecordingVisibility>,
    deleting_previous_state: bool,
) -> bool {
    if deleting_previous_state {
        return false;
    }
    owner.is_some() && visibility.is_some()
}

/// Format a byte count for the human-readable quota display.
/// The frontend already has a human-readable helper
/// (`humanize_bytes`); this is a small wrapper that returns
/// `unlimited` for `None` so the view does not need to special-
/// case the absence of a configured limit.
pub fn quota_line(used: u64, limit: Option<u64>) -> String {
    match limit {
        Some(limit) => format!("{used} / {limit} bytes"),
        None => format!("{used} bytes (unlimited)"),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LibraryColumn {
    Channel,
    Title,
    Schedule,
    Status,
    Visibility,
    Actions,
}

const HEADERS: &[&str] = &[
    "LABEL.RECORDING_COLUMN_CHANNEL",
    "LABEL.RECORDING_COLUMN_TITLE",
    "LABEL.RECORDING_COLUMN_SCHEDULE",
    "LABEL.RECORDING_COLUMN_STATUS",
    "LABEL.RECORDING_COLUMN_VISIBILITY",
    "LABEL.RECORDING_COLUMN_ACTIONS",
];

fn task_channel(task: &RecordingTaskResponse) -> String {
    task.recording.as_ref().and_then(|r| r.channel_name.clone()).unwrap_or_else(|| "—".to_string())
}

fn task_title(task: &RecordingTaskResponse) -> String {
    task.recording.as_ref().and_then(|r| r.program_title.clone()).unwrap_or_else(|| task.title.clone())
}

fn task_schedule(task: &RecordingTaskResponse) -> String {
    let start = task.recording.as_ref().and_then(|r| r.program_start);
    let end = task.recording.as_ref().and_then(|r| r.program_end);
    match (start, end) {
        (Some(s), Some(e)) => format!("{} – {}", format_ts(s), format_ts(e)),
        _ => "—".to_string(),
    }
}

fn format_ts(ts: i64) -> String {
    use chrono::{TimeZone, Utc};
    Utc.timestamp_opt(ts, 0)
        .single()
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ts.to_string())
}

fn task_visibility(task: &RecordingTaskResponse) -> &'static str {
    match task.recording.as_ref().map(|r| &r.visibility) {
        Some(RecordingVisibility::Shared) => "Shared",
        Some(RecordingVisibility::Private) => "Private",
        None => "—",
    }
}

fn task_status(task: &RecordingTaskResponse) -> String { format!("{:?}", task.status) }

#[allow(dead_code)]
fn compare_tasks(
    a: &Rc<RecordingTaskResponse>,
    b: &Rc<RecordingTaskResponse>,
    col: LibraryColumn,
) -> std::cmp::Ordering {
    match col {
        LibraryColumn::Channel => task_channel(a).cmp(&task_channel(b)),
        LibraryColumn::Title => task_title(a).cmp(&task_title(b)),
        LibraryColumn::Schedule => task_schedule(a).cmp(&task_schedule(b)),
        LibraryColumn::Status => task_status(a).cmp(&task_status(b)),
        LibraryColumn::Visibility => task_visibility(a).cmp(task_visibility(b)),
        LibraryColumn::Actions => std::cmp::Ordering::Equal,
    }
}

fn is_sortable_col(col: LibraryColumn) -> bool { !matches!(col, LibraryColumn::Actions) }

#[function_component(RecordingLibraryView)]
pub fn recording_library_view() -> Html {
    let services = use_service_context();
    let dialog = use_context::<DialogService>().expect("Dialog service not found");
    let translate = use_translation();

    let has_recordings_read = services.auth.has_permission(Permission::RecordingRead);
    let has_recordings_write = services.auth.has_permission(Permission::RecordingWrite);
    let is_admin = services.auth.is_admin();

    let tasks = use_state(|| Rc::new(Vec::<RecordingTaskResponse>::new()));
    let quota = use_state(|| None::<RecordingQuota>);
    let editing_task_id = use_state(|| Rc::new(None::<String>));

    // Subscribe to WS-driven updates. Backend broadcasts RecordingChanged
    // after every mutation; each session's WS handler then re-runs the
    // per-session filtered snapshot and pushes it back. Live, no polling.
    {
        let tasks = tasks.clone();
        let svc = services.clone();
        use_effect_with((), move |_| {
            let sid = svc.event.subscribe(move |msg| {
                if let EventMessage::RecordingSnapshot { tasks: incoming, .. } = msg {
                    tasks.set(Rc::new(incoming.iter().map(|t| RecordingTaskResponse::from(t.clone())).collect()));
                }
            });
            // On WS connect, ask the backend for the current snapshot.
            let _ = svc.websocket.send_message(ProtocolMessage::RecordingSnapshotRequest);
            move || svc.event.unsubscribe(sid)
        });
    }

    // Mount: fetch initial snapshot + quota.
    {
        let tasks = tasks.clone();
        let quota = quota.clone();
        use_effect_with((), move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(s) = RecordingService::new().list_tasks().await {
                    tasks.set(Rc::new(s.tasks));
                }
                if let Ok(q) = RecordingService::new().get_quota().await {
                    quota.set(Some(q));
                }
            });
            || {}
        });
    }

    let filtered: Vec<Rc<RecordingTaskResponse>> = (*tasks)
        .iter()
        .filter(|t| {
            let rec = t.recording.as_ref();
            let owner = rec.and_then(|r| r.owner_id.clone().map(RecordingOwner::User));
            is_visible_recording_task(owner.as_ref(), rec.map(|r| &r.visibility), false)
        })
        .cloned()
        .map(Rc::new)
        .collect();

    let translate_for_render = translate.clone();
    let headers: Vec<String> = HEADERS.iter().map(|h| translate.t(h)).collect();
    let translate_for_render_actions = translate.clone();
    let translate_for_quota = translate.clone();
    let translate_for_header = translate.clone();

    let table_items = Rc::new(filtered);

    let render_header = Callback::from(move |col: usize| {
        let headers = headers.clone();
        let col_text = headers.get(col).cloned().unwrap_or_default();
        html! { <>{ col_text }</> }
    });
    let _ = translate_for_render;
    let _ = translate_for_header;

    let render_data = {
        let svc = services.clone();
        let translate = translate_for_render_actions;
        let editing_for_actions = editing_task_id.clone();
        Callback::from(move |(col, _idx, task): (usize, usize, Rc<RecordingTaskResponse>)| {
            let col = match col {
                0 => LibraryColumn::Channel,
                1 => LibraryColumn::Title,
                2 => LibraryColumn::Schedule,
                3 => LibraryColumn::Status,
                4 => LibraryColumn::Visibility,
                _ => LibraryColumn::Actions,
            };
            match col {
                LibraryColumn::Channel => html! { <>{ task_channel(&task) }</> },
                LibraryColumn::Title => html! { <>{ task_title(&task) }</> },
                LibraryColumn::Schedule => html! { <>{ task_schedule(&task) }</> },
                LibraryColumn::Status => html! { <>{ task_status(&task) }</> },
                LibraryColumn::Visibility => html! { <>{ task_visibility(&task) }</> },
                LibraryColumn::Actions => {
                    let is_owner = {
                        let current_user = UserId::from(services.auth.get_username().as_str());
                        task.recording
                            .as_ref()
                            .and_then(|r| r.owner_id.as_ref())
                            .map(|o| o == &current_user)
                            .unwrap_or(false)
                    };
                    let can_mutate = can_mutate_task(has_recordings_write, is_admin, is_owner);
                    if !can_mutate {
                        return html! { <></> };
                    }
                    let on_edit_noop = {
                        let id_clone = task.id.clone();
                        let editing_task_id = editing_for_actions.clone();
                        Callback::from(move |_: String| {
                            editing_task_id.set(Rc::new(Some(id_clone.clone())));
                        })
                    };
                    let id_for_cancel = task.id.clone();
                    let svc_for_cancel = svc.clone();
                    let on_cancel_click = Callback::from(move |_: String| {
                        let id = id_for_cancel.clone();
                        let svc = svc_for_cancel.clone();
                        wasm_bindgen_futures::spawn_local(async move {
                            match RecordingService::new().cancel_task(&id).await {
                                Ok(()) => svc.toastr.success("Task cancelled"),
                                Err(e) => svc.toastr.error(format!("Cancel failed: {}", e)),
                            }
                        });
                    });
                    let id_for_delete = task.id.clone();
                    let svc_for_delete = svc.clone();
                    let dialog_for_delete = dialog.clone();
                    let on_delete_click = Callback::from(move |_: String| {
                        let id = id_for_delete.clone();
                        let svc = svc_for_delete.clone();
                        let dialog = dialog_for_delete.clone();
                        wasm_bindgen_futures::spawn_local(async move {
                            if dialog.confirm("Delete this recording?").await != DialogResult::Ok {
                                return;
                            }
                            match RecordingService::new().delete_task(&id).await {
                                Ok(()) => svc.toastr.success("Task deleted"),
                                Err(e) => svc.toastr.error(format!("Delete failed: {}", e)),
                            }
                        });
                    });
                    html! {
                        <div class="tp__recording-row-actions">
                            <TextButton name="task_edit" icon="" title={translate.t("LABEL.RECORDING_ACTION_EDIT")} onclick={on_edit_noop} />
                            <TextButton name="task_cancel" icon="" title={translate.t("LABEL.RECORDING_ACTION_CANCEL")} onclick={on_cancel_click} />
                            <TextButton name="task_delete" icon="" class="tp__button--danger" title={translate.t("LABEL.RECORDING_ACTION_DELETE")} onclick={on_delete_click} />
                        </div>
                    }
                }
            }
        })
    };

    let is_sortable = Callback::from(|col: usize| {
        is_sortable_col(match col {
            0 => LibraryColumn::Channel,
            1 => LibraryColumn::Title,
            2 => LibraryColumn::Schedule,
            3 => LibraryColumn::Status,
            4 => LibraryColumn::Visibility,
            _ => LibraryColumn::Actions,
        })
    });

    let on_sort = Callback::from(|_: Option<(usize, SortOrder)>| {});

    let table_def = Rc::new(TableDefinition::<RecordingTaskResponse> {
        items: Some(table_items.clone()),
        num_cols: HEADERS.len(),
        is_sortable,
        render_header_cell: render_header,
        render_data_cell: render_data,
        on_sort,
    });

    let quota_view = (*quota).as_ref().map(|q| {
        let translate = translate_for_quota.clone();
        let private = quota_line(q.private_used_bytes, q.private_limit_bytes);
        let shared = quota_line(q.shared_used_bytes, q.shared_limit_bytes);
        html! {
            <div class="tp__recording-quota">
                <span>{ format!("{}: {}", translate.t("LABEL.RECORDING_QUOTA_PRIVATE"), private) }</span>
                <span>{ format!("{}: {}", translate.t("LABEL.RECORDING_QUOTA_SHARED"), shared) }</span>
            </div>
        }
    });

    let _ = has_recordings_read; // permission gate is via home.rs; kept for symmetry

    let edit_view: Html = if editing_task_id.is_some() {
        let on_done = {
            let editing_task_id = editing_task_id.clone();
            Callback::from(move |_: ()| {
                editing_task_id.set(Rc::new(None));
            })
        };
        html! {
            <ContextProvider<EditingTaskId> context={EditingTaskId((*editing_task_id).clone())}>
                <RecordingEditView />
                // on_done is consumed inside the wrapper; the cancel button
                // emits via TaskEditForm's on_done prop. We attach it here so
                // that an explicit cancel clears the selection.
                <div class="tp__recording-edit-cancel">
                    <TextButton
                        name="task_edit_cancel"
                        icon=""
                        class="tp__button--secondary"
                        title="Close"
                        onclick={on_done.reform(|_: String| ())}
                    />
                </div>
            </ContextProvider<EditingTaskId>>
        }
    } else {
        html! { <></> }
    };

    html! {
        <div class="tp__recording-library-view tp__list-view">
            <div class="tp__recording-library-view__body tp__list-view__body">
                <div class="tp__recording-list tp__list-list">
                    <div class="tp__recording-list__header tp__list-list__header">
                        <h1>{ translate.t("LABEL.RECORDING_LIBRARY") }</h1>
                        { quota_view.unwrap_or_else(|| html! { <></> }) }
                    </div>
                    <div class="tp__recording-list__body tp__list-list__body">
                        <Table::<RecordingTaskResponse> definition={table_def} />
                        { edit_view }
                    </div>
                </div>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::UserId;

    #[test]
    fn dvr_nav_only_with_recording_read() {
        assert!(!can_show_dvr_nav(false));
        assert!(can_show_dvr_nav(true));
    }

    #[test]
    fn mutate_requires_write_and_owner_or_admin() {
        assert!(!can_mutate_task(false, true, true));
        assert!(can_mutate_task(true, true, false));
        assert!(can_mutate_task(true, false, true));
        assert!(!can_mutate_task(true, false, false));
    }

    #[test]
    fn visible_recording_requires_owner_and_visibility() {
        let owner = RecordingOwner::User(UserId::from("web:alice"));
        let visibility = RecordingVisibility::Private;
        assert!(is_visible_recording_task(Some(&owner), Some(&visibility), false));
        assert!(!is_visible_recording_task(Some(&owner), Some(&visibility), true));
        assert!(!is_visible_recording_task(None, None, false));
    }

    #[test]
    fn quota_line_handles_unlimited() {
        assert_eq!(quota_line(100, Some(1000)), "100 / 1000 bytes");
        assert_eq!(quota_line(100, None), "100 bytes (unlimited)");
    }

    #[test]
    fn compare_tasks_sorts_by_channel() {
        let mk = |channel: &str| {
            let r = RecordingTaskResponse {
                id: "1".to_string(),
                title: "t".to_string(),
                kind: shared::model::TaskKindDto::Recording,
                priority: shared::model::TaskPriorityDto::Normal,
                status: shared::model::TransferStatusDto::Scheduled,
                retry_attempts: 0,
                downloaded_bytes: 0,
                total_bytes: None,
                next_retry_at: None,
                scheduled_start_at: None,
                duration_secs: None,
                error: None,
                recording: Some(shared::model::recording::RecordingTaskDto {
                    owner_id: None,
                    visibility: RecordingVisibility::Private,
                    channel_id: None,
                    channel_name: Some(channel.to_string()),
                    program_title: None,
                    program_start: None,
                    program_end: None,
                    scheduled_start: None,
                    scheduled_end: None,
                    pre_roll_secs: 0,
                    post_roll_secs: 0,
                    completed_at: None,
                    filename: None,
                    epg: None,
                    rule_id: None,
                    occurrence_key: None,
                }),
            };
            Rc::new(r)
        };
        let a = mk("Alpha");
        let b = mk("Beta");
        assert_eq!(compare_tasks(&a, &b, LibraryColumn::Channel), std::cmp::Ordering::Less);
    }
}

//! DVR library + quota view.
//!
//! The view keeps recordings separate from other media, displays private and
//! shared quota, and gates delete/edit controls by `recording.write` plus the
//! per-task ownership policy.

use super::recording_edit_view::{EditingTaskId, RecordingEditView};
use crate::{
    app::components::{task_status_badge::TaskStatusBadge, text_button::TextButton, Table, TableDefinition},
    hooks::use_service_context,
    i18n::{use_translation, YewI18n},
    model::{DialogResult, EventMessage},
    services::{DialogService, RecordingError, RecordingQuota, RecordingService, RecordingTaskResponse},
    utils::format_bytes,
};
use shared::model::{
    permission::Permission,
    recording::{RecordingOwner, RecordingVisibility},
    web_socket::ProtocolMessage,
    DownloadsDelta, SortOrder, TaskKindDto, TransferStatusDto, UserId,
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
/// Returns a human-readable size, and `unlimited` for `None`, so the
/// view does not need to special-case an absent configured limit.
pub fn quota_line(used: u64, limit: Option<u64>) -> String {
    match limit {
        Some(limit) => format!("{} / {}", format_bytes(used), format_bytes(limit)),
        None => format!("{} (unlimited)", format_bytes(used)),
    }
}

/// Bytes transferred so far, with the total and a percentage when the
/// total is known. The recording rows used to show no progress at all,
/// so an in-flight recording looked identical to a scheduled one.
pub fn task_progress(task: &RecordingTaskResponse) -> String {
    match task.total_bytes {
        Some(total) if total > 0 => {
            let percent = (task.downloaded_bytes.saturating_mul(100)) / total;
            format!("{} / {} ({percent}%)", format_bytes(task.downloaded_bytes), format_bytes(total))
        }
        // A live recording has no known total — the duration is the
        // bound, not a content length — so show what has landed so far.
        _ if task.downloaded_bytes > 0 => format_bytes(task.downloaded_bytes),
        _ => "—".to_string(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LibraryColumn {
    Channel,
    Title,
    Schedule,
    Status,
    Progress,
    Visibility,
    Actions,
}

const HEADERS: &[&str] = &[
    "LABEL.RECORDING_COLUMN_CHANNEL",
    "LABEL.RECORDING_COLUMN_TITLE",
    "LABEL.RECORDING_COLUMN_SCHEDULE",
    "LABEL.RECORDING_COLUMN_STATUS",
    "LABEL.RECORDING_COLUMN_PROGRESS",
    "LABEL.RECORDING_COLUMN_VISIBILITY",
    "LABEL.RECORDING_COLUMN_ACTIONS",
];

/// Column index → column. One place to change when a column moves;
/// previously the mapping was written out three times (render, sort
/// predicate, and the sortable check) and could drift.
fn column_at(index: usize) -> LibraryColumn {
    match index {
        0 => LibraryColumn::Channel,
        1 => LibraryColumn::Title,
        2 => LibraryColumn::Schedule,
        3 => LibraryColumn::Status,
        4 => LibraryColumn::Progress,
        5 => LibraryColumn::Visibility,
        _ => LibraryColumn::Actions,
    }
}

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
    Utc.timestamp_opt(ts, 0).single().map_or_else(|| ts.to_string(), |dt| dt.format("%Y-%m-%d %H:%M").to_string())
}

/// i18n key for a task's visibility, or `None` for a task with no
/// recording metadata.
fn task_visibility_key(task: &RecordingTaskResponse) -> Option<&'static str> {
    match task.recording.as_ref().map(|r| &r.visibility) {
        Some(RecordingVisibility::Shared) => Some("LABEL.RECORDING_VISIBILITY_SHARED"),
        Some(RecordingVisibility::Private) => Some("LABEL.RECORDING_VISIBILITY_PRIVATE"),
        None => None,
    }
}

fn task_visibility(translate: &YewI18n, task: &RecordingTaskResponse) -> String {
    task_visibility_key(task).map_or_else(|| "—".to_string(), |key| translate.t(key))
}

/// Sort key for the status column. Ordering follows the lifecycle
/// (`TransferStatusDto`'s own `Ord`) rather than the localized text, so
/// the order does not change with the UI language.
fn task_status_order(task: &RecordingTaskResponse) -> &TransferStatusDto { &task.status }

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
        LibraryColumn::Status => task_status_order(a).cmp(task_status_order(b)),
        LibraryColumn::Progress => a.downloaded_bytes.cmp(&b.downloaded_bytes),
        LibraryColumn::Visibility => task_visibility_key(a).cmp(&task_visibility_key(b)),
        LibraryColumn::Actions => std::cmp::Ordering::Equal,
    }
}

fn is_sortable_col(col: LibraryColumn) -> bool { !matches!(col, LibraryColumn::Actions) }

/// Translate a service error for display.
///
/// Every failure path in this view used to render `format!("Cancel
/// failed: {}", e)` — untranslated English with a raw wire code
/// appended. The code still reaches the browser console; the user sees
/// a sentence in their own language.
fn error_message(translate: &YewI18n, error: &RecordingError) -> String { translate.t(error.i18n_key()) }

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
    // Revision of the snapshot currently rendered, so an out-of-order
    // delivery cannot replace newer data with older data.
    let last_revision = use_state(|| None::<u64>);
    let translate_for_events = translate.clone();

    // Subscribe to WS-driven updates. Backend broadcasts RecordingChanged
    // after every mutation; each session's WS handler then re-runs the
    // per-session filtered snapshot and pushes it back. Live, no polling.
    {
        let tasks = tasks.clone();
        let last_revision = last_revision.clone();
        let svc = services.clone();
        use_effect_with((), move |()| {
            let translate = translate_for_events.clone();
            let sid = svc.event.subscribe(move |msg| {
                // The socket may report an unavailable backend (stale
                // token, DVR switched off, or no `video.download`
                // block). The library view used to surface this as a
                // toast and a permanent banner, which turned an idle
                // recording tab into an alarm. Log and move on — the
                // actionable error surfaces only when the user clicks a
                // record action.
                if let EventMessage::RecordingUnavailable { code } = &msg {
                    let error = RecordingError::from_code(code);
                    log::warn!("recording socket unavailable: {code} ({})", error.i18n_key());
                    log::debug!("{}", error_message(&translate, &error));
                    return;
                }
                // Byte-level progress for the running transfer arrives on
                // the downloads delta channel, not as a recording
                // snapshot: the worker patches the active task and
                // broadcasts `DownloadsDelta::ActivePatched`. Without
                // this branch the progress column only moved on
                // lifecycle changes (start, finish, cancel).
                if let EventMessage::DownloadsDeltaUpdate(delta) = &msg {
                    if let DownloadsDelta::ActivePatched(task) = &**delta {
                        if task.kind == TaskKindDto::Recording {
                            let updated = RecordingTaskResponse::from(task.clone());
                            let mut current = (**tasks).clone();
                            if let Some(slot) = current.iter_mut().find(|t| t.id == updated.id) {
                                *slot = updated;
                                tasks.set(Rc::new(current));
                            }
                        }
                    }
                    return;
                }
                if let EventMessage::RecordingSnapshot { revision, tasks: incoming } = msg {
                    // The revision guard exists for ordering, not for
                    // completeness: `RecordingSnapshot` is a *full* list,
                    // so a snapshot that skips revisions is still current
                    // and needs no re-request. What it must not do is
                    // overwrite newer data — two events racing through the
                    // socket would otherwise leave the older list on
                    // screen until the next mutation.
                    //
                    // A gap does matter the moment the backend starts
                    // sending incremental changes; it is logged so that
                    // change has a hook to build on.
                    if let Some(previous) = *last_revision {
                        if revision < previous {
                            return;
                        }
                        if revision > previous.saturating_add(1) {
                            log::debug!("recording snapshot skipped revisions {previous} -> {revision}");
                        }
                    }
                    last_revision.set(Some(revision));
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
        let last_revision = last_revision.clone();
        use_effect_with((), move |()| {
            wasm_bindgen_futures::spawn_local(async move {
                // One service instance: each `new()` builds its own HTTP
                // client, and two were being constructed for two calls.
                let service = RecordingService::new();
                if let Ok(snapshot) = service.list_tasks().await {
                    last_revision.set(Some(snapshot.revision));
                    tasks.set(Rc::new(snapshot.tasks));
                }
                if let Ok(q) = service.get_quota().await {
                    quota.set(Some(q));
                }
            });
            || {}
        });
    }

    // Recomputed only when the task list actually changes, not on every
    // render: the filter clones an owner id and allocates an `Rc` per row.
    let filtered = use_memo((*tasks).clone(), |tasks| {
        tasks
            .iter()
            .filter(|t| {
                let rec = t.recording.as_ref();
                let owner = rec.and_then(|r| r.owner_id.clone().map(RecordingOwner::User));
                is_visible_recording_task(owner.as_ref(), rec.map(|r| &r.visibility), false)
            })
            .cloned()
            .map(Rc::new)
            .collect::<Vec<Rc<RecordingTaskResponse>>>()
    });

    let headers: Vec<String> = HEADERS.iter().map(|h| translate.t(h)).collect();
    let translate_for_render_actions = translate.clone();
    let translate_for_quota = translate.clone();

    let table_items = Rc::new((*filtered).clone());
    let is_empty = table_items.is_empty();

    let render_header = Callback::from(move |col: usize| {
        let col_text = headers.get(col).cloned().unwrap_or_default();
        html! { <>{ col_text }</> }
    });

    let render_data = {
        let svc = services.clone();
        let translate = translate_for_render_actions;
        let editing_for_actions = editing_task_id.clone();
        Callback::from(move |(_row, col, task): (usize, usize, Rc<RecordingTaskResponse>)| {
            match column_at(col) {
                LibraryColumn::Channel => html! { <>{ task_channel(&task) }</> },
                LibraryColumn::Title => html! { <>{ task_title(&task) }</> },
                LibraryColumn::Schedule => html! { <>{ task_schedule(&task) }</> },
                LibraryColumn::Status => html! {
                    <TaskStatusBadge
                        status={task.status.clone()}
                        kind={task.kind.clone()}
                        detail={task.error.clone()}
                    />
                },
                LibraryColumn::Progress => html! { <span class="tp__table__nowrap">{ task_progress(&task) }</span> },
                LibraryColumn::Visibility => html! { <>{ task_visibility(&translate, &task) }</> },
                LibraryColumn::Actions => {
                    let is_owner = {
                        let current_user = UserId::from(services.auth.get_username().as_str());
                        task.recording.as_ref().and_then(|r| r.owner_id.as_ref()).is_some_and(|o| o == &current_user)
                    };
                    let can_mutate = can_mutate_task(has_recordings_write, is_admin, is_owner);
                    if !can_mutate {
                        return html! { <></> };
                    }
                    let on_edit_click = {
                        let id_clone = task.id.clone();
                        let editing_task_id = editing_for_actions.clone();
                        Callback::from(move |_: String| {
                            editing_task_id.set(Rc::new(Some(id_clone.clone())));
                        })
                    };
                    let id_for_cancel = task.id.clone();
                    let svc_for_cancel = svc.clone();
                    let translate_for_cancel = translate.clone();
                    let on_cancel_click = Callback::from(move |_: String| {
                        let id = id_for_cancel.clone();
                        let svc = svc_for_cancel.clone();
                        let translate = translate_for_cancel.clone();
                        wasm_bindgen_futures::spawn_local(async move {
                            match RecordingService::new().cancel_task(&id).await {
                                Ok(()) => svc.toastr.success(translate.t("MESSAGES.RECORDING.TASK_CANCELLED")),
                                Err(error) => {
                                    // The wire code stays in the console for
                                    // support; the user gets a sentence.
                                    log::warn!("recording cancel failed: {error}");
                                    svc.toastr.error(error_message(&translate, &error));
                                }
                            }
                        });
                    });
                    let id_for_delete = task.id.clone();
                    let svc_for_delete = svc.clone();
                    let dialog_for_delete = dialog.clone();
                    let translate_for_delete = translate.clone();
                    let on_delete_click = Callback::from(move |_: String| {
                        let id = id_for_delete.clone();
                        let svc = svc_for_delete.clone();
                        let dialog = dialog_for_delete.clone();
                        let translate = translate_for_delete.clone();
                        wasm_bindgen_futures::spawn_local(async move {
                            let prompt = translate.t("LABEL.RECORDING_FORM_DELETE_CONFIRM");
                            if dialog.confirm(&prompt).await != DialogResult::Ok {
                                return;
                            }
                            match RecordingService::new().delete_task(&id).await {
                                Ok(()) => svc.toastr.success(translate.t("MESSAGES.RECORDING.TASK_DELETED")),
                                Err(error) => {
                                    log::warn!("recording delete failed: {error}");
                                    svc.toastr.error(error_message(&translate, &error));
                                }
                            }
                        });
                    });
                    let edit_label = translate.t("LABEL.RECORDING_ACTION_EDIT");
                    let cancel_label = translate.t("LABEL.RECORDING_ACTION_CANCEL");
                    let delete_label = translate.t("LABEL.RECORDING_ACTION_DELETE");
                    // Row actions carry the recording title in their
                    // accessible name: nine identical "Delete" buttons in a
                    // column are indistinguishable to a screen reader.
                    let row_title = task_title(&task);
                    html! {
                        <div class="tp__recording-row-actions">
                            <TextButton name="task_edit" icon="Edit" title={edit_label.clone()}
                                aria_label={format!("{edit_label}: {row_title}")} onclick={on_edit_click} />
                            <TextButton name="task_cancel" icon="Cancel" title={cancel_label.clone()}
                                aria_label={format!("{cancel_label}: {row_title}")} onclick={on_cancel_click} />
                            <TextButton name="task_delete" icon="Delete" class="tp__button--danger" title={delete_label.clone()}
                                aria_label={format!("{delete_label}: {row_title}")} onclick={on_delete_click} />
                        </div>
                    }
                }
            }
        })
    };

    let is_sortable = Callback::from(|col: usize| is_sortable_col(column_at(col)));

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
            Callback::from(move |(): ()| {
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
                        title={translate.t("LABEL.RECORDING_EDIT_CLOSE")}
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
                        if is_empty {
                            // An empty table reads as "something failed".
                            // Say what the list is for and where to start.
                            <p class="tp__recording-list__empty">
                                { translate.t("MESSAGES.RECORDING.EMPTY_LIBRARY") }
                            </p>
                        } else {
                            <Table::<RecordingTaskResponse> definition={table_def} />
                        }
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
        assert_eq!(quota_line(100, Some(1000)), format!("{} / {}", format_bytes(100), format_bytes(1000)));
        assert_eq!(quota_line(100, None), format!("{} (unlimited)", format_bytes(100)));
    }

    #[test]
    fn task_progress_reports_percentage_only_with_a_known_total() {
        let mut task = task_with_channel("Alpha");
        assert_eq!(task_progress(&task), "—");

        // A live recording has no content length; show what has landed.
        Rc::get_mut(&mut task).expect("unique").downloaded_bytes = 2048;
        let progress = task_progress(&task);
        assert!(!progress.contains('%'), "{progress}");
        assert!(progress.contains(&format_bytes(2048)), "{progress}");

        Rc::get_mut(&mut task).expect("unique").total_bytes = Some(4096);
        assert!(task_progress(&task).contains("(50%)"), "{}", task_progress(&task));
    }

    #[test]
    fn task_progress_does_not_divide_by_zero() {
        let mut task = task_with_channel("Alpha");
        {
            let task = Rc::get_mut(&mut task).expect("unique");
            task.total_bytes = Some(0);
            task.downloaded_bytes = 10;
        }
        assert_eq!(task_progress(&task), format_bytes(10));
    }

    #[test]
    fn status_sorts_by_lifecycle_not_by_localized_text() {
        // "Completed" sorts before "Failed" alphabetically in English but
        // the lifecycle order is what must hold, in every language.
        let mut running = task_with_channel("a");
        Rc::get_mut(&mut running).expect("unique").status = TransferStatusDto::Running;
        let mut completed = task_with_channel("a");
        Rc::get_mut(&mut completed).expect("unique").status = TransferStatusDto::Completed;
        assert_eq!(compare_tasks(&running, &completed, LibraryColumn::Status), std::cmp::Ordering::Less);
    }

    #[test]
    fn every_header_maps_to_a_column() {
        // A header added without a matching `column_at` arm would silently
        // render as the actions column.
        assert_eq!(HEADERS.len(), 7);
        assert!(matches!(column_at(0), LibraryColumn::Channel));
        assert!(matches!(column_at(4), LibraryColumn::Progress));
        assert!(matches!(column_at(HEADERS.len() - 1), LibraryColumn::Actions));
        assert!(!is_sortable_col(column_at(HEADERS.len() - 1)));
    }

    fn task_with_channel(channel: &str) -> Rc<RecordingTaskResponse> {
        Rc::new(RecordingTaskResponse {
            id: "1".to_string(),
            title: "t".to_string(),
            kind: shared::model::TaskKindDto::Recording,
            priority: shared::model::TaskPriorityDto::Normal,
            status: TransferStatusDto::Scheduled,
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
        })
    }

    #[test]
    fn compare_tasks_sorts_by_channel() {
        let a = task_with_channel("Alpha");
        let b = task_with_channel("Beta");
        assert_eq!(compare_tasks(&a, &b, LibraryColumn::Channel), std::cmp::Ordering::Less);
    }
}

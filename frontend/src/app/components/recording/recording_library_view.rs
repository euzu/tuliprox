//! The single recording table.
//!
//! One list for Live, VOD and Series, split into Current and Completed by
//! lifecycle state, with a Type column. The session receives owner-filtered
//! full snapshots over the WebSocket; there is no delta protocol, so a
//! partial list can never be mistaken for the whole one.

use crate::{
    app::components::{IconButton, LoadingIndicator, NoContent, Table, TableDefinition, TaskStatusBadge, TextButton},
    hooks::use_service_context,
    i18n::use_translation,
    model::DialogResult,
    services::{DialogService, RecordingService},
    utils::format_bytes,
};
use shared::{
    model::{Permission, ProtocolMessage, RecordingKind, RecordingTaskDto, SortOrder, TransferStatusDto},
    utils::unix_ts_to_str,
};
use std::{cmp::Ordering, rc::Rc};
use yew::{platform::spawn_local, prelude::*};

const HEADERS: [&str; 9] = [
    "LABEL.ACTIONS",
    "LABEL.NAME",
    "LABEL.TYPE",
    "LABEL.STATUS",
    "LABEL.RECORDING_TRANSFERRED",
    "LABEL.RECORDING_FILE_SIZE",
    "LABEL.START",
    "LABEL.DURATION",
    "LABEL.ERROR",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RecordingTab {
    Current,
    Completed,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct RecordingActionAvailability {
    pause: bool,
    resume: bool,
    cancel: bool,
    remove: bool,
    retry: bool,
}

/// Never leave the user on an empty Completed tab while work is running.
fn normalize_tab(current: RecordingTab, tasks: &[RecordingTaskDto]) -> RecordingTab {
    let has_completed = tasks.iter().any(RecordingTaskDto::is_terminal);
    let has_current = tasks.iter().any(|task| !task.is_terminal());
    match current {
        RecordingTab::Completed if !has_completed && has_current => RecordingTab::Current,
        _ => current,
    }
}

fn collect_tasks_for_tab(tab: RecordingTab, tasks: &Rc<Vec<RecordingTaskDto>>) -> Vec<Rc<RecordingTaskDto>> {
    tasks
        .iter()
        .filter(|task| match tab {
            RecordingTab::Current => !task.is_terminal(),
            RecordingTab::Completed => task.is_terminal(),
        })
        .cloned()
        .map(Rc::new)
        .collect()
}

fn sort_tasks(items: &mut [Rc<RecordingTaskDto>], sort: Option<(usize, SortOrder)>) {
    if let Some((col, order)) = sort {
        items.sort_by(|a, b| match order {
            SortOrder::Asc => compare_tasks(a, b, col),
            SortOrder::Desc => compare_tasks(b, a, col),
            SortOrder::None => Ordering::Equal,
        });
    }
}

fn collect_sorted_tasks_for_tab(
    tab: RecordingTab,
    tasks: &Rc<Vec<RecordingTaskDto>>,
    sort: Option<(usize, SortOrder)>,
) -> Vec<Rc<RecordingTaskDto>> {
    let mut items = collect_tasks_for_tab(tab, tasks);
    sort_tasks(&mut items, sort);
    items
}

fn format_recording_kind(translate: &crate::i18n::YewI18n, kind: RecordingKind) -> String {
    match kind {
        RecordingKind::Live => translate.t("LABEL.LIVE"),
        RecordingKind::Vod => "VOD".to_string(),
        RecordingKind::Series => translate.t("LABEL.SERIES"),
    }
}

fn format_progress(task: &RecordingTaskDto) -> String {
    if let Some(total) = task.total_bytes {
        if total > 0 {
            let percent = ((task.transferred_bytes as f64 / total as f64) * 100.0).round() as u32;
            return format!("{} / {} ({}%)", format_bytes(task.transferred_bytes), format_bytes(total), percent);
        }
    }
    format_bytes(task.transferred_bytes)
}

fn render_progress(task: &RecordingTaskDto) -> Html {
    let text = format_progress(task);
    let bar = task.total_bytes.filter(|total| *total > 0).map(|total| {
        html! {
            <progress class="tp__recording-table__progress-bar"
                aria-label={text.clone()}
                max={total.to_string()}
                value={task.transferred_bytes.to_string()} />
        }
    });
    html! {
        <span class="tp__table__nowrap tp__recording-table__progress">
            { bar }
            <span>{ text }</span>
        </span>
    }
}

fn format_start(task: &RecordingTaskDto) -> String { task.scheduled_start.and_then(unix_ts_to_str).unwrap_or_default() }

fn format_duration(task: &RecordingTaskDto) -> String {
    task.scheduled_duration_secs()
        .map(|seconds| {
            let hours = seconds / 3600;
            let minutes = (seconds % 3600) / 60;
            if hours > 0 {
                format!("{hours}h {minutes}m")
            } else {
                format!("{minutes}m")
            }
        })
        .unwrap_or_default()
}

fn format_error_parts(task: &RecordingTaskDto, attempt_label: &str, next_retry_label: &str) -> String {
    let mut parts = Vec::new();
    if let Some(error) = task.error.as_ref().filter(|error| !error.is_empty()) {
        parts.push(error.clone());
    }
    if task.retry_attempts > 0 {
        parts.push(format!("{attempt_label} {}", task.retry_attempts));
    }
    if let Some(next_retry_at) = task.next_retry_at.and_then(unix_ts_to_str) {
        parts.push(format!("{next_retry_label} {next_retry_at}"));
    }
    parts.join(" | ")
}

fn format_error(translate: &crate::i18n::YewI18n, task: &RecordingTaskDto) -> String {
    format_error_parts(task, &translate.t("LABEL.ATTEMPT"), &translate.t("LABEL.NEXT_RETRY"))
}

fn compare_tasks(a: &RecordingTaskDto, b: &RecordingTaskDto, col: usize) -> Ordering {
    match col {
        1 => a.title.cmp(&b.title),
        2 => a.kind.cmp(&b.kind),
        3 => a.status.cmp(&b.status),
        4 => a.transferred_bytes.cmp(&b.transferred_bytes),
        5 => a.total_bytes.unwrap_or(a.transferred_bytes).cmp(&b.total_bytes.unwrap_or(b.transferred_bytes)),
        6 => a.scheduled_start.unwrap_or_default().cmp(&b.scheduled_start.unwrap_or_default()),
        7 => a.scheduled_duration_secs().unwrap_or_default().cmp(&b.scheduled_duration_secs().unwrap_or_default()),
        8 => a.error.as_deref().unwrap_or_default().cmp(b.error.as_deref().unwrap_or_default()),
        _ => Ordering::Equal,
    }
}

fn is_sortable(col: usize) -> bool { (1..=8).contains(&col) }

/// Which controls a task offers: what the server says it would accept,
/// narrowed to what this viewer is permitted to ask for.
///
/// The state rules are not restated here. They used to be, and they had
/// drifted from the backend's.
fn action_availability(can_manage: bool, can_delete: bool, task: &RecordingTaskDto) -> RecordingActionAvailability {
    let allowed = task.allowed_actions;
    RecordingActionAvailability {
        pause: can_manage && allowed.pause,
        resume: can_manage && allowed.resume,
        cancel: can_manage && allowed.cancel,
        remove: can_delete && allowed.remove,
        retry: can_manage && allowed.retry,
    }
}

/// The controls the table offers, in one place so the confirmation
/// prompt, the success message and the service call cannot drift apart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TaskControl {
    Pause,
    Resume,
    Cancel,
    Remove,
    Retry,
}

impl TaskControl {
    /// Destructive controls ask first.
    fn confirm_key(self) -> Option<&'static str> {
        match self {
            Self::Cancel => Some("MESSAGES.RECORDING.CONFIRM_CANCEL"),
            Self::Remove => Some("MESSAGES.RECORDING.CONFIRM_REMOVE"),
            Self::Pause | Self::Resume | Self::Retry => None,
        }
    }

    fn success_key(self) -> &'static str {
        match self {
            Self::Pause => "MESSAGES.RECORDING.TASK_PAUSED",
            Self::Resume => "MESSAGES.RECORDING.TASK_RESUMED",
            Self::Cancel => "MESSAGES.RECORDING.TASK_CANCELLED",
            Self::Remove => "MESSAGES.RECORDING.TASK_REMOVED",
            Self::Retry => "MESSAGES.RECORDING.TASK_RETRIED",
        }
    }
}

async fn run_control(control: TaskControl, id: &str) -> Result<(), crate::services::RecordingError> {
    let service = RecordingService::new();
    match control {
        TaskControl::Pause => service.pause_task(id).await,
        TaskControl::Resume => service.resume_task(id).await,
        TaskControl::Cancel => service.cancel_task(id).await,
        TaskControl::Remove => service.remove_task(id).await,
        TaskControl::Retry => service.retry_task(id).await,
    }
}

/// One pool's usage, as `"1.2 GB / 5.0 GB"` or `"1.2 GB"` when unlimited.
///
/// An absent limit is unlimited, which must not render as `0` or as a bare
/// used figure implying a cap that does not exist.
fn format_quota_pool(used_bytes: u64, limit_bytes: Option<u64>) -> String {
    let used = format_bytes(used_bytes);
    match limit_bytes {
        Some(limit) => format!("{used} / {}", format_bytes(limit)),
        None => used,
    }
}

/// Whether an arriving snapshot should replace what is rendered.
///
/// Revisions are global, so gaps are normal and carry no meaning. Going
/// backwards does: that is a reordered delivery, and applying it would
/// regress the list to a state the server has already moved past. `None`
/// means nothing has been rendered yet, or a reconnect has just discarded
/// what was.
fn should_apply_snapshot(seen: Option<u64>, incoming: u64) -> bool { seen.is_none_or(|last| incoming > last) }

#[function_component(RecordingLibraryView)]
pub fn recording_library_view() -> Html {
    let translate = use_translation();
    let services = use_service_context();
    let dialog = use_context::<DialogService>().expect("Dialog service not found");
    let can_manage = services.auth.has_permission(Permission::RecordingManage);
    let can_delete = services.auth.has_permission(Permission::RecordingDelete);
    let active_tab = use_state(|| RecordingTab::Current);
    let tasks_state = use_state(|| Rc::new(Vec::<RecordingTaskDto>::new()));
    let table_items = use_state(|| None::<Rc<Vec<Rc<RecordingTaskDto>>>>);
    let sort_state = use_state(|| None::<(usize, SortOrder)>);
    // Distinguishes "still waiting for the first snapshot" from "nothing recorded"
    let initial_loaded = use_state(|| false);
    // Set when the socket refuses to serve recordings (DVR disabled, stale
    // token). Without it the view waited forever for a snapshot that the
    // server deliberately does not send.
    let unavailable_code = use_state(|| None::<String>);
    // The highest revision rendered so far. A `use_mut_ref` rather than
    // `use_state` because the subscription closure is created once and
    // would otherwise keep reading the value it captured.
    let seen_revision = use_mut_ref(|| None::<u64>);
    let quota_state = use_state(shared::model::RecordingQuotaSummaryDto::default);
    // Stale data is more useful than no data, but it has to be labelled.
    let connected = use_state(|| true);

    let request_snapshot = {
        let services = services.clone();
        Callback::from(move |()| {
            let _ = services.websocket.send_message(ProtocolMessage::RecordingSnapshotRequest);
        })
    };

    {
        let tasks_state = tasks_state.clone();
        let services = services.clone();
        let request_snapshot_effect = request_snapshot.clone();
        let initial_loaded = initial_loaded.clone();
        let unavailable_code = unavailable_code.clone();
        let seen_revision = seen_revision.clone();
        let quota_state = quota_state.clone();
        let connected = connected.clone();
        use_effect_with((), move |()| {
            request_snapshot_effect.emit(());
            let sub_id = services.event.subscribe(move |msg| match msg {
                crate::model::EventMessage::RecordingSnapshot { revision, tasks, quota, .. } => {
                    let mut seen = seen_revision.borrow_mut();
                    if !should_apply_snapshot(*seen, revision) {
                        return;
                    }
                    *seen = Some(revision);
                    drop(seen);
                    initial_loaded.set(true);
                    unavailable_code.set(None);
                    quota_state.set(quota);
                    tasks_state.set(tasks);
                }
                crate::model::EventMessage::RecordingUnavailable { code } => {
                    // A refusal is an answer: stop loading and say why instead
                    // of waiting for a snapshot the server will not send.
                    unavailable_code.set(Some(code));
                    initial_loaded.set(true);
                }
                crate::model::EventMessage::WebSocketStatus(online) => {
                    connected.set(online);
                    if online {
                        // A reconnect must be able to replace stale local state
                        // even if the server's revision has not moved.
                        *seen_revision.borrow_mut() = None;
                        request_snapshot_effect.emit(());
                    }
                }
                _ => {}
            });
            move || services.event.unsubscribe(sub_id)
        });
    }

    {
        let active_tab = active_tab.clone();
        let tasks_state = tasks_state.clone();
        let active_tab_set = active_tab.clone();
        let table_items = table_items.clone();
        let sort_state = sort_state.clone();
        use_effect_with((*active_tab, (*tasks_state).clone(), *sort_state), move |(tab, tasks, sort)| {
            let normalized_tab = normalize_tab(*tab, tasks.as_slice());
            if normalized_tab != *tab {
                active_tab_set.set(normalized_tab);
            }
            let items = collect_sorted_tasks_for_tab(normalized_tab, tasks, *sort);
            table_items.set((!items.is_empty()).then(|| Rc::new(items)));
            || ()
        });
    }

    let control_handler = {
        let request_snapshot = request_snapshot.clone();
        let services = services.clone();
        let translate = translate.clone();
        let dialog = dialog.clone();
        move |control: TaskControl| {
            let request_snapshot = request_snapshot.clone();
            let services = services.clone();
            let translate = translate.clone();
            let dialog = dialog.clone();
            Callback::from(move |id: String| {
                let request_snapshot = request_snapshot.clone();
                let services = services.clone();
                let translate = translate.clone();
                let dialog = dialog.clone();
                spawn_local(async move {
                    if let Some(key) = control.confirm_key() {
                        if dialog.confirm(&translate.t(key)).await != DialogResult::Ok {
                            return;
                        }
                    }
                    match run_control(control, &id).await {
                        Ok(()) => services.toastr.success(translate.t(control.success_key())),
                        Err(err) => services.toastr.error(translate.t(err.i18n_key())),
                    }
                    request_snapshot.emit(());
                });
            })
        }
    };

    let handle_pause = control_handler(TaskControl::Pause);
    let handle_resume = control_handler(TaskControl::Resume);
    let handle_cancel = control_handler(TaskControl::Cancel);
    let handle_remove = control_handler(TaskControl::Remove);
    let handle_retry = control_handler(TaskControl::Retry);

    let render_header_cell = {
        let translate = translate.clone();
        Callback::<usize, Html>::from(move |col| {
            let header_text = HEADERS.get(col).copied().map_or_else(String::new, |key| translate.t(key));

            html! { { header_text } }
        })
    };

    let render_data_cell = {
        let translate = translate.clone();
        let handle_pause = handle_pause.clone();
        let handle_resume = handle_resume.clone();
        let handle_cancel = handle_cancel.clone();
        let handle_remove = handle_remove.clone();
        let handle_retry = handle_retry.clone();
        Callback::<(usize, usize, Rc<RecordingTaskDto>), Html>::from(
            move |(_row, col, dto): (usize, usize, Rc<RecordingTaskDto>)| match col {
                0 => {
                    let actions = action_availability(can_manage, can_delete, &dto);
                    let retry_label = if dto.status == TransferStatusDto::Cancelled { "Resume" } else { "Retry" };
                    let retry_icon = if dto.status == TransferStatusDto::Cancelled { "Play" } else { "Refresh" };
                    let pause_id = dto.id.clone();
                    let resume_id = dto.id.clone();
                    let cancel_id = dto.id.clone();
                    let retry_id = dto.id.clone();
                    let remove_id = dto.id.clone();
                    let pause_handle = handle_pause.clone();
                    let resume_handle = handle_resume.clone();
                    let cancel_handle = handle_cancel.clone();
                    let retry_handle = handle_retry.clone();
                    let remove_handle = handle_remove.clone();
                    html! {
                        <div class="tp__recording-table__actions">
                            if actions.pause {
                                <IconButton name="Pause" icon="Pause" onclick={Callback::from(move |_| pause_handle.emit(pause_id.clone()))} />
                            }
                            if actions.resume {
                                <IconButton name="Resume" icon="Play" onclick={Callback::from(move |_| resume_handle.emit(resume_id.clone()))} />
                            }
                            if actions.cancel {
                                <IconButton name="Cancel" icon="Stop" onclick={Callback::from(move |_| cancel_handle.emit(cancel_id.clone()))} />
                            }
                            if actions.retry {
                                <IconButton name={retry_label} icon={retry_icon} onclick={Callback::from(move |_| retry_handle.emit(retry_id.clone()))} />
                            }
                            if actions.remove {
                                <IconButton name="Remove" icon="Delete" onclick={Callback::from(move |_| remove_handle.emit(remove_id.clone()))} />
                            }
                        </div>
                    }
                }
                1 => html! { <span class="tp__table__nowrap">{dto.title.clone()}</span> },
                2 => html! { format_recording_kind(&translate, dto.kind) },
                3 => html! { <TaskStatusBadge status={dto.status} detail={dto.error.clone()} /> },
                4 => render_progress(&dto),
                5 => {
                    html! { <span class="tp__table__nowrap">{dto.total_bytes.map_or_else(String::new, format_bytes)}</span> }
                }
                6 => html! { <span class="tp__table__nowrap">{format_start(&dto)}</span> },
                7 => html! { format_duration(&dto) },
                8 => html! { format_error(&translate, &dto) },
                _ => html! {},
            },
        )
    };

    let on_sort = {
        let active_tab = active_tab.clone();
        let tasks_state = tasks_state.clone();
        let table_items = table_items.clone();
        let sort_state = sort_state.clone();
        Callback::<Option<(usize, SortOrder)>, ()>::from(move |args| {
            sort_state.set(args);
            let items = collect_sorted_tasks_for_tab(*active_tab, &tasks_state, args);
            table_items.set((!items.is_empty()).then(|| Rc::new(items)));
        })
    };

    let table_definition = Rc::new(TableDefinition::<RecordingTaskDto> {
        items: (*table_items).clone(),
        num_cols: HEADERS.len(),
        is_sortable: Callback::from(is_sortable),
        render_header_cell,
        render_data_cell,
        on_sort,
    });

    let render_filter_button = |tab: RecordingTab, icon: &str, label: String| {
        let active_tab = active_tab.clone();
        let class = if *active_tab == tab { "active" } else { "primary" };
        html! {
            <TextButton
                class={class}
                name={label.clone()}
                icon={icon.to_string()}
                title={label}
                onclick={Callback::from(move |_| active_tab.set(tab))}
            />
        }
    };

    let unavailable_hint = unavailable_code.as_deref().map_or_else(String::new, |code| match code {
        "recording_disabled" => translate.t("LABEL.RECORDING_DISABLED_HINT"),
        "recording_token_refresh_required" => translate.t("LABEL.RECORDING_TOKEN_REFRESH_HINT"),
        _ => String::new(),
    });

    html! {
        <div class="tp__recording-view tp__list-view">
            <div class="tp__recording-view__body tp__list-view__body">
                <div class="tp__recording-list tp__list-list">
                    <div class="tp__recording-list__header tp__list-list__header">
                        <h1>{translate.t("LABEL.RECORDING_LIBRARY")}</h1>
                        <div class="tp__recording-list__header-toolbar tp__radio-button-group ">
                            {render_filter_button(RecordingTab::Current, "Record", translate.t("LABEL.RECORDING_CURRENT"))}
                            {render_filter_button(RecordingTab::Completed, "TaskDone", translate.t("LABEL.RECORDING_COMPLETED"))}
                        </div>
                    </div>
                    <div class="tp__recording-list__status">
                        <span class="tp__recording-list__quota">
                            { format!("{}: {}", translate.t("LABEL.RECORDING_QUOTA_PRIVATE"),
                                format_quota_pool(quota_state.private_used_bytes, quota_state.private_limit_bytes)) }
                        </span>
                        <span class="tp__recording-list__quota">
                            { format!("{}: {}", translate.t("LABEL.RECORDING_QUOTA_SHARED"),
                                format_quota_pool(quota_state.shared_used_bytes, quota_state.shared_limit_bytes)) }
                        </span>
                        if !*connected {
                            <span class="tp__recording-list__offline">
                                { translate.t("LABEL.RECORDING_OFFLINE") }
                            </span>
                        }
                    </div>
                    <div class="tp__recording-list__body tp__list-list__body">
                        if *initial_loaded {
                            if unavailable_code.is_some() {
                                <NoContent text={translate.t("LABEL.RECORDING_UNAVAILABLE")} hint={unavailable_hint} />
                            } else if tasks_state.is_empty() {
                                <NoContent text={translate.t("LABEL.RECORDING_EMPTY")} />
                            } else {
                                <Table::<RecordingTaskDto> definition={table_definition} />
                            }
                        } else {
                            <LoadingIndicator loading={true} />
                        }
                    </div>
                </div>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::{
        action_availability, collect_sorted_tasks_for_tab, collect_tasks_for_tab, format_error_parts,
        format_quota_pool, is_sortable, normalize_tab, should_apply_snapshot, RecordingTab,
    };
    use shared::model::{
        RecordingAllowedActions, RecordingKind, RecordingTaskDto, RecordingVisibility, SortOrder, TaskPriorityDto,
        TransferStatusDto,
    };
    use std::rc::Rc;

    fn task(id: &str, kind: RecordingKind, status: TransferStatusDto) -> RecordingTaskDto {
        RecordingTaskDto {
            id: id.to_string(),
            title: format!("{id}.mp4"),
            kind,
            priority: TaskPriorityDto::Background,
            status,
            retry_attempts: 0,
            transferred_bytes: 0,
            total_bytes: None,
            next_retry_at: None,
            error: None,
            owner_id: None,
            visibility: RecordingVisibility::Private,
            channel_id: None,
            channel_name: None,
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
            allowed_actions: RecordingAllowedActions::default(),
        }
    }

    /// A task the server would accept every command for.
    fn task_allowing_everything(kind: RecordingKind, status: TransferStatusDto) -> RecordingTaskDto {
        RecordingTaskDto {
            allowed_actions: RecordingAllowedActions {
                pause: true,
                resume: true,
                cancel: true,
                retry: true,
                edit: true,
                remove: true,
            },
            ..task("t", kind, status)
        }
    }

    fn all_kinds_snapshot() -> Rc<Vec<RecordingTaskDto>> {
        Rc::new(vec![
            task("live", RecordingKind::Live, TransferStatusDto::Scheduled),
            task("vod", RecordingKind::Vod, TransferStatusDto::Running),
            task("series", RecordingKind::Series, TransferStatusDto::Completed),
        ])
    }

    #[test]
    fn a_newer_snapshot_replaces_what_is_rendered() {
        assert!(should_apply_snapshot(Some(7), 8));
    }

    #[test]
    fn an_unlimited_pool_shows_usage_without_inventing_a_ceiling() {
        // An absent limit is unlimited. Rendering it as `/ 0` would read as
        // "full", and dropping the pool entirely would hide real usage.
        let unlimited = format_quota_pool(1_500_000, None);
        assert!(!unlimited.contains('/'), "no ceiling to show: {unlimited}");
        assert!(unlimited.contains('1'), "but the usage is still reported: {unlimited}");
    }

    #[test]
    fn a_limited_pool_shows_both_numbers() {
        let limited = format_quota_pool(1_500_000, Some(5_000_000));
        assert!(limited.contains('/'), "{limited}");
    }

    #[test]
    fn a_revision_gap_is_normal_and_applied() {
        // The revision is global across every recording on the server, so a
        // session sees only the subset that touched it. Treating a gap as
        // loss would make the list stop updating.
        assert!(should_apply_snapshot(Some(7), 900));
    }

    #[test]
    fn an_older_or_repeated_snapshot_is_ignored() {
        // Reordered delivery. Applying it would regress the list to a state
        // the server has already moved past.
        assert!(!should_apply_snapshot(Some(7), 6));
        assert!(!should_apply_snapshot(Some(7), 7), "the same revision has nothing new to say");
    }

    #[test]
    fn the_first_snapshot_is_always_applied() {
        assert!(should_apply_snapshot(None, 0), "revision zero is still the first thing to render");
        assert!(should_apply_snapshot(None, 42));
    }

    #[test]
    fn a_reconnect_can_replace_state_the_server_never_renumbered() {
        // On reconnect the view clears what it has seen. Without that, a
        // client that missed changes while offline would refuse the fresh
        // snapshot because its revision had not advanced past the stale one.
        let after_reconnect = None;
        assert!(should_apply_snapshot(after_reconnect, 7), "the same revision is accepted again after a reconnect");
    }

    #[test]
    fn current_holds_every_non_terminal_state_and_completed_the_rest() {
        let tasks = Rc::new(vec![
            task("a", RecordingKind::Live, TransferStatusDto::Scheduled),
            task("b", RecordingKind::Vod, TransferStatusDto::Queued),
            task("c", RecordingKind::Vod, TransferStatusDto::WaitingForCapacity),
            task("d", RecordingKind::Vod, TransferStatusDto::RetryWaiting),
            task("e", RecordingKind::Series, TransferStatusDto::Running),
            task("f", RecordingKind::Series, TransferStatusDto::Paused),
            task("g", RecordingKind::Vod, TransferStatusDto::Completed),
            task("h", RecordingKind::Vod, TransferStatusDto::Failed),
            task("i", RecordingKind::Live, TransferStatusDto::Cancelled),
        ]);

        let current: Vec<String> =
            collect_tasks_for_tab(RecordingTab::Current, &tasks).iter().map(|t| t.id.clone()).collect();
        let completed: Vec<String> =
            collect_tasks_for_tab(RecordingTab::Completed, &tasks).iter().map(|t| t.id.clone()).collect();

        assert_eq!(current, vec!["a", "b", "c", "d", "e", "f"]);
        assert_eq!(completed, vec!["g", "h", "i"]);
    }

    #[test]
    fn every_media_kind_reaches_the_one_table() {
        let tasks = all_kinds_snapshot();
        let mut kinds: Vec<RecordingKind> = collect_tasks_for_tab(RecordingTab::Current, &tasks)
            .iter()
            .chain(collect_tasks_for_tab(RecordingTab::Completed, &tasks).iter())
            .map(|task| task.kind)
            .collect();
        kinds.sort_unstable();
        assert_eq!(kinds, vec![RecordingKind::Live, RecordingKind::Vod, RecordingKind::Series]);
    }

    #[test]
    fn completed_tab_falls_back_to_current_while_work_runs() {
        let tasks = vec![task("a", RecordingKind::Vod, TransferStatusDto::Running)];
        assert_eq!(normalize_tab(RecordingTab::Completed, &tasks), RecordingTab::Current);

        let mixed = vec![
            task("a", RecordingKind::Vod, TransferStatusDto::Running),
            task("b", RecordingKind::Vod, TransferStatusDto::Completed),
        ];
        assert_eq!(normalize_tab(RecordingTab::Completed, &mixed), RecordingTab::Completed);
    }

    #[test]
    fn a_command_the_server_would_refuse_is_never_offered() {
        // Which states permit which command is the backend's transition graph.
        // The table must not second-guess it, whatever the viewer may do.
        let refused = task("live", RecordingKind::Live, TransferStatusDto::Running);
        assert_eq!(action_availability(true, true, &refused), super::RecordingActionAvailability::default());
    }

    #[test]
    fn no_action_without_a_mutation_permission() {
        let allowed = task_allowing_everything(RecordingKind::Vod, TransferStatusDto::Running);
        assert_eq!(action_availability(false, false, &allowed), super::RecordingActionAvailability::default());
    }

    #[test]
    fn manage_and_delete_are_gated_separately() {
        // `recording.delete` alone must not hand out the running-state
        // controls, and `recording.manage` alone must not offer removal.
        let allowed = task_allowing_everything(RecordingKind::Vod, TransferStatusDto::Running);
        assert!(action_availability(true, false, &allowed).cancel);
        assert!(!action_availability(true, false, &allowed).remove);
        assert!(!action_availability(false, true, &allowed).cancel);
        assert!(action_availability(false, true, &allowed).remove);
    }

    #[test]
    fn sorting_by_title_is_stable_across_tabs() {
        let tasks = Rc::new(vec![
            task("b", RecordingKind::Vod, TransferStatusDto::Running),
            task("a", RecordingKind::Live, TransferStatusDto::Running),
        ]);
        let sorted = collect_sorted_tasks_for_tab(RecordingTab::Current, &tasks, Some((1, SortOrder::Asc)));
        assert_eq!(sorted.iter().map(|t| t.id.clone()).collect::<Vec<_>>(), vec!["a", "b"]);
    }

    #[test]
    fn only_data_columns_are_sortable() {
        assert!(!is_sortable(0), "the action column is not sortable");
        for col in 1..=8 {
            assert!(is_sortable(col));
        }
        assert!(!is_sortable(9));
    }

    #[test]
    fn error_cell_joins_error_attempts_and_next_retry() {
        let mut t = task("t", RecordingKind::Vod, TransferStatusDto::RetryWaiting);
        t.error = Some("boom".to_string());
        t.retry_attempts = 2;
        let text = format_error_parts(&t, "Attempt", "Next");
        assert!(text.starts_with("boom | Attempt 2"), "{text}");
    }
}

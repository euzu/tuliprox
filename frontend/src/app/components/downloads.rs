use crate::{
    app::components::{IconButton, Table, TableDefinition, TextButton},
    hooks::use_service_context,
    i18n::use_translation,
    model::EventMessage,
    utils::format_bytes,
};
use shared::{
    model::{
        DownloadsDelta, DownloadsResponse, FileDownloadDto, Permission, ProtocolMessage, SortOrder, TaskKindDto,
        TransferStatusDto,
    },
    utils::unix_ts_to_str,
};
use std::{cmp::Ordering, rc::Rc};
use yew::{platform::spawn_local, prelude::*};

const HEADERS: [&str; 9] = [
    "LABEL.ACTIONS",
    "LABEL.NAME",
    "LABEL.TYPE",
    "LABEL.STATUS",
    "LABEL.DOWNLOAD_DOWNLOADED",
    "LABEL.DOWNLOAD_FILE_SIZE",
    "LABEL.START",
    "LABEL.DURATION",
    "LABEL.ERROR",
];

#[derive(Clone, PartialEq, Eq, Debug)]
enum DownloadTab {
    Queue,
    Finished,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct DownloadActionAvailability {
    pause: bool,
    resume: bool,
    cancel: bool,
    remove: bool,
    retry: bool,
}

fn normalize_download_tab(
    current: &DownloadTab,
    queue: &[FileDownloadDto],
    finished: &[FileDownloadDto],
    active: &[Rc<FileDownloadDto>],
) -> DownloadTab {
    match current {
        DownloadTab::Finished if finished.is_empty() && (!active.is_empty() || !queue.is_empty()) => DownloadTab::Queue,
        _ => current.clone(),
    }
}

fn collect_downloads_for_tab(
    tab: &DownloadTab,
    queue: &Rc<Vec<FileDownloadDto>>,
    finished: &Rc<Vec<FileDownloadDto>>,
    active: &Rc<Vec<Rc<FileDownloadDto>>>,
) -> Vec<Rc<FileDownloadDto>> {
    match tab {
        DownloadTab::Queue => {
            let mut items: Vec<Rc<FileDownloadDto>> = active.iter().cloned().collect();
            items.extend(queue.iter().cloned().map(Rc::new));
            items
        }
        DownloadTab::Finished => finished.iter().cloned().map(Rc::new).collect(),
    }
}

fn sort_download_items(items: &mut [Rc<FileDownloadDto>], sort: Option<(usize, SortOrder)>) {
    if let Some((col, order)) = sort {
        items.sort_by(|a, b| match order {
            SortOrder::Asc => compare_downloads(a, b, col),
            SortOrder::Desc => compare_downloads(b, a, col),
            SortOrder::None => Ordering::Equal,
        });
    }
}

fn collect_sorted_downloads_for_tab(
    tab: &DownloadTab,
    queue: &Rc<Vec<FileDownloadDto>>,
    finished: &Rc<Vec<FileDownloadDto>>,
    active: &Rc<Vec<Rc<FileDownloadDto>>>,
    sort: Option<(usize, SortOrder)>,
) -> Vec<Rc<FileDownloadDto>> {
    let mut items = collect_downloads_for_tab(tab, queue, finished, active);
    sort_download_items(&mut items, sort);
    items
}

fn format_download_kind(translate: &crate::i18n::YewI18n, kind: &TaskKindDto) -> String {
    match kind {
        TaskKindDto::Recording => translate.t("LABEL.RECORD"),
        TaskKindDto::Download => translate.t("LABEL.DOWNLOAD"),
    }
}

fn format_download_state(translate: &crate::i18n::YewI18n, state: &TransferStatusDto) -> String {
    match state {
        TransferStatusDto::Queued => translate.t("LABEL.DOWNLOAD_STATE_QUEUED"),
        TransferStatusDto::Scheduled => translate.t("LABEL.DOWNLOAD_STATE_SCHEDULED"),
        TransferStatusDto::Running => translate.t("LABEL.DOWNLOAD_STATE_DOWNLOADING"),
        TransferStatusDto::Paused => translate.t("LABEL.DOWNLOAD_STATE_PAUSED"),
        TransferStatusDto::Completed => translate.t("LABEL.DOWNLOAD_STATE_COMPLETED"),
        TransferStatusDto::Failed => translate.t("LABEL.DOWNLOAD_STATE_FAILED"),
        TransferStatusDto::Cancelled => translate.t("LABEL.DOWNLOAD_CANCEL"),
        TransferStatusDto::WaitingForCapacity => translate.t("LABEL.DOWNLOAD_STATE_WAITING_FOR_CAPACITY"),
        TransferStatusDto::RetryWaiting => translate.t("LABEL.DOWNLOAD_STATE_RETRY_WAITING"),
    }
}

fn format_download_progress(download: &FileDownloadDto) -> String {
    if let Some(total) = download.total_bytes {
        if total > 0 {
            let percent = ((download.downloaded_bytes as f64 / total as f64) * 100.0).round() as u32;
            return format!("{} / {} ({}%)", format_bytes(download.downloaded_bytes), format_bytes(total), percent);
        }
    }
    format_bytes(download.downloaded_bytes)
}

fn format_download_start(download: &FileDownloadDto) -> String {
    download.scheduled_start_at.and_then(unix_ts_to_str).unwrap_or_default()
}

fn format_download_duration(download: &FileDownloadDto) -> String {
    download
        .duration_secs
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

fn format_download_error_parts(download: &FileDownloadDto, attempt_label: &str, next_retry_label: &str) -> String {
    let mut parts = Vec::new();
    if let Some(error) = download.error.as_ref().filter(|error| !error.is_empty()) {
        parts.push(error.clone());
    }
    if download.retry_attempts > 0 {
        parts.push(format!("{attempt_label} {}", download.retry_attempts));
    }
    if let Some(next_retry_at) = download.next_retry_at.and_then(unix_ts_to_str) {
        parts.push(format!("{next_retry_label} {next_retry_at}"));
    }
    parts.join(" | ")
}

fn format_download_error(translate: &crate::i18n::YewI18n, download: &FileDownloadDto) -> String {
    format_download_error_parts(download, &translate.t("LABEL.ATTEMPT"), &translate.t("LABEL.NEXT_RETRY"))
}

fn compare_downloads(a: &FileDownloadDto, b: &FileDownloadDto, col: usize) -> Ordering {
    match col {
        1 => a.title.cmp(&b.title),
        2 => a.kind.cmp(&b.kind),
        3 => a.status.cmp(&b.status),
        4 => a.downloaded_bytes.cmp(&b.downloaded_bytes),
        5 => a.total_bytes.unwrap_or(a.downloaded_bytes).cmp(&b.total_bytes.unwrap_or(b.downloaded_bytes)),
        6 => a.scheduled_start_at.unwrap_or_default().cmp(&b.scheduled_start_at.unwrap_or_default()),
        7 => a.duration_secs.unwrap_or_default().cmp(&b.duration_secs.unwrap_or_default()),
        8 => a.error.as_deref().unwrap_or_default().cmp(b.error.as_deref().unwrap_or_default()),
        _ => Ordering::Equal,
    }
}

fn is_sortable(col: usize) -> bool { (1..=8).contains(&col) }

fn download_action_availability(can_write: bool, dto: &FileDownloadDto) -> DownloadActionAvailability {
    if !can_write {
        return DownloadActionAvailability::default();
    }

    DownloadActionAvailability {
        pause: matches!(
            dto.status,
            TransferStatusDto::Running | TransferStatusDto::WaitingForCapacity | TransferStatusDto::RetryWaiting
        ),
        resume: dto.status == TransferStatusDto::Paused,
        cancel: matches!(
            dto.status,
            TransferStatusDto::Running
                | TransferStatusDto::Queued
                | TransferStatusDto::Scheduled
                | TransferStatusDto::WaitingForCapacity
                | TransferStatusDto::RetryWaiting
        ),
        remove: matches!(
            dto.status,
            TransferStatusDto::Failed | TransferStatusDto::Completed | TransferStatusDto::Cancelled
        ),
        retry: dto.kind == TaskKindDto::Download
            && matches!(dto.status, TransferStatusDto::Failed | TransferStatusDto::Cancelled),
    }
}

fn optimistic_active_delta(
    active: &Rc<Vec<Rc<FileDownloadDto>>>,
    uuid: &str,
    status: TransferStatusDto,
) -> Option<DownloadsDelta> {
    active.iter().find(|download| download.id == uuid).map(|download| {
        let mut updated = (**download).clone();
        updated.status = status;
        DownloadsDelta::ActivePatched(updated)
    })
}

fn handle_download_action_result(
    result: Result<shared::model::DownloadActionResponse, crate::error::Error>,
    success_message: &str,
    failure_message: &str,
    request_downloads: &Callback<()>,
    services: &Rc<crate::hooks::Services>,
    optimistic_delta: Option<DownloadsDelta>,
) {
    match result {
        Ok(response) if response.success => {
            if let Some(delta) = optimistic_delta {
                services.event.broadcast(EventMessage::DownloadsDeltaUpdate(Rc::new(delta)));
            }
            services.toastr.success(success_message);
        }
        Ok(_) | Err(_) => {
            services.toastr.error(failure_message);
        }
    }
    request_downloads.emit(());
}

fn apply_download_snapshot(
    response: &DownloadsResponse,
    queue_state: &UseStateHandle<Rc<Vec<FileDownloadDto>>>,
    finished_state: &UseStateHandle<Rc<Vec<FileDownloadDto>>>,
    active_download: &UseStateHandle<Rc<Vec<Rc<FileDownloadDto>>>>,
) {
    queue_state.set(Rc::new(response.queue.clone()));
    finished_state.set(Rc::new(response.finished.clone()));
    active_download.set(Rc::new(response.active.iter().cloned().map(Rc::new).collect()));
}

fn apply_download_delta(
    delta: &DownloadsDelta,
    queue_state: &UseStateHandle<Rc<Vec<FileDownloadDto>>>,
    finished_state: &UseStateHandle<Rc<Vec<FileDownloadDto>>>,
    active_download: &UseStateHandle<Rc<Vec<Rc<FileDownloadDto>>>>,
) {
    match delta {
        DownloadsDelta::SnapshotReset(response) => {
            apply_download_snapshot(response, queue_state, finished_state, active_download);
        }
        DownloadsDelta::ActivePatched(download) => {
            let mut active_items: Vec<Rc<FileDownloadDto>> = active_download.iter().cloned().collect();
            if let Some(existing) = active_items.iter_mut().find(|item| item.id == download.id) {
                *existing = Rc::new(download.clone());
            } else {
                active_items.push(Rc::new(download.clone()));
            }
            active_download.set(Rc::new(active_items));
        }
        DownloadsDelta::ActiveCleared => {
            active_download.set(Rc::new(Vec::new()));
        }
        DownloadsDelta::QueueReplaced { queue } => {
            queue_state.set(Rc::new(queue.clone()));
        }
        DownloadsDelta::FinishedReplaced { finished } => {
            finished_state.set(Rc::new(finished.clone()));
        }
    }
}

#[function_component(DownloadsView)]
pub fn downloads_view() -> Html {
    let translate = use_translation();
    let services = use_service_context();
    let has_download_write = services.auth.has_permission(Permission::DownloadWrite);
    let active_tab = use_state(|| DownloadTab::Queue);
    let queue_state = use_state(|| Rc::new(Vec::<FileDownloadDto>::new()));
    let finished_state = use_state(|| Rc::new(Vec::<FileDownloadDto>::new()));
    let active_download = use_state(|| Rc::new(Vec::<Rc<FileDownloadDto>>::new()));
    let table_items = use_state(|| None::<Rc<Vec<Rc<FileDownloadDto>>>>);
    let sort_state = use_state(|| None::<(usize, SortOrder)>);

    let request_downloads = {
        let services = services.clone();
        Callback::from(move |_| {
            let _ = services.websocket.send_message(ProtocolMessage::DownloadsRequest);
        })
    };

    {
        let queue_state = queue_state.clone();
        let finished_state = finished_state.clone();
        let active_download = active_download.clone();
        let services = services.clone();
        let request_downloads_effect = request_downloads.clone();
        use_effect_with((), move |_| {
            request_downloads_effect.emit(());
            let sub_id = services.event.subscribe(move |msg| match msg {
                crate::model::EventMessage::DownloadsUpdate(snapshot) => {
                    apply_download_snapshot(&snapshot, &queue_state, &finished_state, &active_download);
                }
                crate::model::EventMessage::DownloadsDeltaUpdate(delta) => {
                    apply_download_delta(&delta, &queue_state, &finished_state, &active_download);
                }
                crate::model::EventMessage::WebSocketStatus(true) => {
                    request_downloads_effect.emit(());
                }
                _ => {}
            });
            move || services.event.unsubscribe(sub_id)
        });
    }

    {
        let active_tab = active_tab.clone();
        let queue_state = queue_state.clone();
        let finished_state = finished_state.clone();
        let active_download = active_download.clone();
        let active_tab_set = active_tab.clone();
        let table_items = table_items.clone();
        let sort_state = sort_state.clone();
        use_effect_with(
            (
                (*active_tab).clone(),
                (*queue_state).clone(),
                (*finished_state).clone(),
                (*active_download).clone(),
                *sort_state,
            ),
            move |(tab, queue, finished, active, sort)| {
                let normalized_tab =
                    normalize_download_tab(tab, queue.as_slice(), finished.as_slice(), active.as_ref());
                if normalized_tab != *tab {
                    active_tab_set.set(normalized_tab.clone());
                }
                let items = collect_sorted_downloads_for_tab(&normalized_tab, queue, finished, active, *sort);
                table_items.set((!items.is_empty()).then(|| Rc::new(items)));
                || ()
            },
        );
    }

    let handle_pause = {
        let request_downloads = request_downloads.clone();
        let services = services.clone();
        let translate = translate.clone();
        let active_download = active_download.clone();
        Callback::from(move |uuid: String| {
            let request_downloads = request_downloads.clone();
            let services = services.clone();
            let translate = translate.clone();
            let active_download = active_download.clone();
            spawn_local(async move {
                let optimistic_delta = optimistic_active_delta(&active_download, &uuid, TransferStatusDto::Paused);
                let result = services.downloads.pause_download(uuid).await;
                handle_download_action_result(
                    result,
                    &translate.t("MESSAGES.DOWNLOAD.DOWNLOAD_PAUSED"),
                    &translate.t("MESSAGES.DOWNLOAD.FAIL"),
                    &request_downloads,
                    &services,
                    optimistic_delta,
                );
            });
        })
    };

    let handle_resume = {
        let request_downloads = request_downloads.clone();
        let services = services.clone();
        let translate = translate.clone();
        let active_download = active_download.clone();
        Callback::from(move |uuid: String| {
            let request_downloads = request_downloads.clone();
            let services = services.clone();
            let translate = translate.clone();
            let active_download = active_download.clone();
            spawn_local(async move {
                let optimistic_delta = optimistic_active_delta(&active_download, &uuid, TransferStatusDto::Running);
                let result = services.downloads.resume_download(uuid).await;
                handle_download_action_result(
                    result,
                    &translate.t("MESSAGES.DOWNLOAD.DOWNLOAD_RESUMED"),
                    &translate.t("MESSAGES.DOWNLOAD.FAIL"),
                    &request_downloads,
                    &services,
                    optimistic_delta,
                );
            });
        })
    };

    let handle_cancel = {
        let request_downloads = request_downloads.clone();
        let services = services.clone();
        let translate = translate.clone();
        let active_download = active_download.clone();
        Callback::from(move |uuid: String| {
            let request_downloads = request_downloads.clone();
            let services = services.clone();
            let translate = translate.clone();
            let active_download = active_download.clone();
            spawn_local(async move {
                let optimistic_delta = optimistic_active_delta(&active_download, &uuid, TransferStatusDto::Cancelled);
                let result = services.downloads.cancel_download(uuid).await;
                handle_download_action_result(
                    result,
                    &translate.t("MESSAGES.DOWNLOAD.DOWNLOAD_CANCELLED"),
                    &translate.t("MESSAGES.DOWNLOAD.FAIL"),
                    &request_downloads,
                    &services,
                    optimistic_delta,
                );
            });
        })
    };

    let handle_remove = {
        let request_downloads = request_downloads.clone();
        let services = services.clone();
        let translate = translate.clone();
        Callback::from(move |uuid: String| {
            let request_downloads = request_downloads.clone();
            let services = services.clone();
            let translate = translate.clone();
            spawn_local(async move {
                let result = services.downloads.remove_download(uuid).await;
                handle_download_action_result(
                    result,
                    &translate.t("MESSAGES.DOWNLOAD.DOWNLOAD_REMOVED"),
                    &translate.t("MESSAGES.DOWNLOAD.FAIL"),
                    &request_downloads,
                    &services,
                    None,
                );
            });
        })
    };

    let handle_retry = {
        let request_downloads = request_downloads.clone();
        let services = services.clone();
        let translate = translate.clone();
        Callback::from(move |uuid: String| {
            let request_downloads = request_downloads.clone();
            let services = services.clone();
            let translate = translate.clone();
            spawn_local(async move {
                let result = services.downloads.retry_download(uuid).await;
                handle_download_action_result(
                    result,
                    &translate.t("MESSAGES.DOWNLOAD.DOWNLOAD_RETRIED"),
                    &translate.t("MESSAGES.DOWNLOAD.FAIL"),
                    &request_downloads,
                    &services,
                    None,
                );
            });
        })
    };

    let render_header_cell = {
        let translate = translate.clone();
        Callback::<usize, Html>::from(move |col| {
            let header_text = HEADERS.get(col).copied().map(|key| translate.t(key)).unwrap_or_else(|| "".into());

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
        Callback::<(usize, usize, Rc<FileDownloadDto>), Html>::from(
            move |(_row, col, dto): (usize, usize, Rc<FileDownloadDto>)| match col {
                0 => {
                    let actions = download_action_availability(has_download_write, &dto);
                    let retry_label = if dto.status == TransferStatusDto::Cancelled { "Resume" } else { "Retry" };
                    let retry_icon = if dto.status == TransferStatusDto::Cancelled { "Play" } else { "Refresh" };
                    let pause_uuid = dto.id.clone();
                    let resume_uuid = dto.id.clone();
                    let cancel_uuid = dto.id.clone();
                    let retry_uuid = dto.id.clone();
                    let remove_uuid = dto.id.clone();
                    let pause_handle = handle_pause.clone();
                    let resume_handle = handle_resume.clone();
                    let cancel_handle = handle_cancel.clone();
                    let retry_handle = handle_retry.clone();
                    let remove_handle = handle_remove.clone();
                    html! {
                        <div class="tp__downloads-table__actions">
                            if actions.pause {
                                <IconButton name="Pause" icon="Pause" onclick={Callback::from(move |_| pause_handle.emit(pause_uuid.clone()))} />
                            }
                            if actions.resume {
                                <IconButton name="Resume" icon="Play" onclick={Callback::from(move |_| resume_handle.emit(resume_uuid.clone()))} />
                            }
                            if actions.cancel {
                                <IconButton name="Cancel" icon="Stop" onclick={Callback::from(move |_| cancel_handle.emit(cancel_uuid.clone()))} />
                            }
                            if actions.retry {
                                <IconButton name={retry_label} icon={retry_icon} onclick={Callback::from(move |_| retry_handle.emit(retry_uuid.clone()))} />
                            }
                            if actions.remove {
                                <IconButton name="Remove" icon="Delete" onclick={Callback::from(move |_| remove_handle.emit(remove_uuid.clone()))} />
                            }
                        </div>
                    }
                }
                1 => html! { <span class="tp__table__nowrap">{dto.title.clone()}</span> },
                2 => html! { format_download_kind(&translate, &dto.kind) },
                3 => html! { format_download_state(&translate, &dto.status) },
                4 => html! { <span class="tp__table__nowrap">{format_download_progress(&dto)}</span> },
                5 => {
                    html! { <span class="tp__table__nowrap">{dto.total_bytes.map_or_else(String::new, format_bytes)}</span> }
                }
                6 => html! { <span class="tp__table__nowrap">{format_download_start(&dto)}</span> },
                7 => html! { format_download_duration(&dto) },
                8 => html! { format_download_error(&translate, &dto) },
                _ => html! {},
            },
        )
    };

    let on_sort = {
        let active_tab = active_tab.clone();
        let queue_state = queue_state.clone();
        let finished_state = finished_state.clone();
        let active_download = active_download.clone();
        let table_items = table_items.clone();
        let sort_state = sort_state.clone();
        Callback::<Option<(usize, SortOrder)>, ()>::from(move |args| {
            sort_state.set(args);
            let items =
                collect_sorted_downloads_for_tab(&active_tab, &queue_state, &finished_state, &active_download, args);
            table_items.set((!items.is_empty()).then(|| Rc::new(items)));
        })
    };

    let table_definition = Rc::new(TableDefinition::<FileDownloadDto> {
        items: (*table_items).clone(),
        num_cols: HEADERS.len(),
        is_sortable: Callback::from(is_sortable),
        render_header_cell,
        render_data_cell,
        on_sort,
    });

    let render_filter_button = |tab: DownloadTab, icon: &str, label: String| {
        let active_tab = active_tab.clone();
        let button_tab = tab.clone();
        let class = if *active_tab == tab { "active" } else { "primary" };
        html! {
            <TextButton
                class={class}
                name={label.clone()}
                icon={icon.to_string()}
                title={label}
                onclick={Callback::from(move |_| active_tab.set(button_tab.clone()))}
            />
        }
    };

    html! {
        <div class="tp__downloads-view tp__list-view">
            <div class="tp__downloads-view__body tp__list-view__body">
                <div class="tp__downloads-list tp__list-list">
                    <div class="tp__downloads-list__header tp__list-list__header">
                        <h1>{translate.t("LABEL.DOWNLOADS")}</h1>
                        <div class="tp__downloads-list__header-toolbar tp__radio-button-group ">
                            {render_filter_button(DownloadTab::Queue, "Download", translate.t("LABEL.DOWNLOAD_QUEUE"))}
                            {render_filter_button(DownloadTab::Finished, "TaskDone", translate.t("LABEL.DOWNLOAD_FINISHED"))}
                        </div>
                    </div>
                    <div class="tp__downloads-list__body tp__list-list__body">
                        <Table::<FileDownloadDto> definition={table_definition} />
                    </div>
                </div>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::{
        collect_downloads_for_tab, collect_sorted_downloads_for_tab, download_action_availability,
        format_download_error_parts, is_sortable, normalize_download_tab, optimistic_active_delta, DownloadTab,
    };
    use shared::model::{DownloadsDelta, FileDownloadDto, SortOrder, TaskKindDto, TransferStatusDto};
    use std::rc::Rc;

    fn download(id: &str, status: TransferStatusDto) -> FileDownloadDto {
        FileDownloadDto {
            id: id.to_string(),
            title: format!("{id}.mp4"),
            kind: TaskKindDto::Download,
            priority: shared::model::TaskPriorityDto::Background,
            status,
            retry_attempts: 0,
            downloaded_bytes: 0,
            total_bytes: None,
            next_retry_at: None,
            scheduled_start_at: None,
            duration_secs: None,
            error: None,
        }
    }

    #[test]
    fn queue_tab_shows_active_download_first_then_queue() {
        let queue = Rc::new(vec![download("q1", TransferStatusDto::Queued), download("q2", TransferStatusDto::Queued)]);
        let finished = Rc::new(vec![]);
        let active = Rc::new(vec![Rc::new(download("active", TransferStatusDto::Running))]);

        let items = collect_downloads_for_tab(&DownloadTab::Queue, &queue, &finished, &active);

        assert_eq!(items.len(), 3);
        assert_eq!(items[0].id, "active");
        assert_eq!(items[1].id, "q1");
        assert_eq!(items[2].id, "q2");
    }

    #[test]
    fn queue_tab_works_without_active_download() {
        let queue = Rc::new(vec![download("q1", TransferStatusDto::Queued)]);
        let finished = Rc::new(vec![]);
        let active = Rc::new(Vec::new());

        let items = collect_downloads_for_tab(&DownloadTab::Queue, &queue, &finished, &active);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "q1");
    }

    #[test]
    fn finished_tab_stays_when_has_items() {
        let queue = vec![];
        let finished = vec![download("done", TransferStatusDto::Completed)];
        let active: Rc<Vec<Rc<FileDownloadDto>>> = Rc::new(Vec::new());

        assert_eq!(
            normalize_download_tab(&DownloadTab::Finished, &queue, &finished, active.as_ref()),
            DownloadTab::Finished
        );
    }

    #[test]
    fn finished_tab_falls_back_to_queue_when_empty() {
        let queue = vec![download("q1", TransferStatusDto::Queued)];
        let finished = vec![];
        let active: Rc<Vec<Rc<FileDownloadDto>>> = Rc::new(Vec::new());

        assert_eq!(
            normalize_download_tab(&DownloadTab::Finished, &queue, &finished, active.as_ref()),
            DownloadTab::Queue
        );
    }

    fn apply_active_delta(
        active: &Rc<Vec<Rc<FileDownloadDto>>>,
        delta: &DownloadsDelta,
    ) -> Rc<Vec<Rc<FileDownloadDto>>> {
        match delta {
            DownloadsDelta::SnapshotReset(response) => Rc::new(response.active.iter().cloned().map(Rc::new).collect()),
            DownloadsDelta::ActivePatched(download) => {
                let mut active_items: Vec<Rc<FileDownloadDto>> = active.iter().cloned().collect();
                if let Some(existing) = active_items.iter_mut().find(|item| item.id == download.id) {
                    *existing = Rc::new(download.clone());
                } else {
                    active_items.push(Rc::new(download.clone()));
                }
                Rc::new(active_items)
            }
            DownloadsDelta::ActiveCleared => Rc::new(Vec::new()),
            DownloadsDelta::QueueReplaced { .. } | DownloadsDelta::FinishedReplaced { .. } => active.clone(),
        }
    }

    #[test]
    fn active_delta_replaces_existing_active_download() {
        let active = Rc::new(vec![Rc::new(download("active", TransferStatusDto::Running))]);
        let mut updated = download("active", TransferStatusDto::Running);
        updated.downloaded_bytes = 2048;
        updated.total_bytes = Some(4096);

        let patched = apply_active_delta(&active, &DownloadsDelta::ActivePatched(updated.clone()));

        assert_eq!(patched.len(), 1);
        assert_eq!(patched[0].id, "active");
        assert_eq!(patched[0].downloaded_bytes, 2048);
        assert_eq!(patched[0].total_bytes, Some(4096));
    }

    #[test]
    fn active_delta_appends_new_active_when_uuid_differs() {
        let active = Rc::new(vec![Rc::new(download("old", TransferStatusDto::Running))]);
        let updated = download("new", TransferStatusDto::Running);

        let patched = apply_active_delta(&active, &DownloadsDelta::ActivePatched(updated));

        assert_eq!(patched.len(), 2);
        assert_eq!(patched[0].id, "old");
        assert_eq!(patched[1].id, "new");
    }

    #[test]
    fn optimistic_active_delta_updates_matching_item_status() {
        let active = Rc::new(vec![Rc::new(download("active", TransferStatusDto::Running))]);

        let delta = optimistic_active_delta(&active, "active", TransferStatusDto::Paused).expect("delta");

        match delta {
            DownloadsDelta::ActivePatched(download) => {
                assert_eq!(download.id, "active");
                assert_eq!(download.status, TransferStatusDto::Paused);
            }
            _ => panic!("expected active patch"),
        }
    }

    #[test]
    fn is_sortable_matches_compare_columns() {
        assert!(!is_sortable(0));
        assert!(is_sortable(1));
        assert!(is_sortable(8));
        assert!(!is_sortable(9));
    }

    #[test]
    fn format_download_error_uses_supplied_labels_for_retry_metadata() {
        let mut dto = download("task", TransferStatusDto::RetryWaiting);
        dto.error = Some("provider timeout".to_string());
        dto.retry_attempts = 3;
        dto.next_retry_at = Some(1_775_354_400);

        let formatted = format_download_error_parts(&dto, "Versuch", "Naechster Versuch");

        assert!(formatted.contains("provider timeout"));
        assert!(formatted.contains("Versuch 3"));
        assert!(formatted.contains("Naechster Versuch"));
    }

    #[test]
    fn collect_sorted_downloads_for_tab_preserves_selected_sort_on_refresh() {
        let queue = Rc::new(vec![download("b", TransferStatusDto::Queued), download("a", TransferStatusDto::Queued)]);
        let finished = Rc::new(vec![]);
        let active = Rc::new(Vec::new());

        let items = collect_sorted_downloads_for_tab(
            &DownloadTab::Queue,
            &queue,
            &finished,
            &active,
            Some((1, SortOrder::Asc)),
        );

        assert_eq!(items[0].id, "a");
        assert_eq!(items[1].id, "b");
    }

    #[test]
    fn download_actions_require_write_permission() {
        let dto = download("active", TransferStatusDto::Running);

        let actions = download_action_availability(false, &dto);

        assert!(!actions.pause);
        assert!(!actions.resume);
        assert!(!actions.cancel);
        assert!(!actions.remove);
        assert!(!actions.retry);
    }

    #[test]
    fn cancelled_recordings_do_not_offer_retry_semantics() {
        let dto = FileDownloadDto {
            id: "rec".to_string(),
            title: "rec.ts".to_string(),
            kind: TaskKindDto::Recording,
            priority: shared::model::TaskPriorityDto::Background,
            status: TransferStatusDto::Cancelled,
            retry_attempts: 0,
            downloaded_bytes: 0,
            total_bytes: None,
            next_retry_at: None,
            scheduled_start_at: None,
            duration_secs: Some(60),
            error: Some("Cancelled by user".to_string()),
        };

        let can_retry = dto.kind == TaskKindDto::Download
            && matches!(dto.status, TransferStatusDto::Failed | TransferStatusDto::Cancelled);

        assert!(!can_retry);
    }

    #[test]
    fn download_actions_hide_retry_for_cancelled_recordings() {
        let dto = FileDownloadDto {
            id: "rec".to_string(),
            title: "rec.ts".to_string(),
            kind: TaskKindDto::Recording,
            priority: shared::model::TaskPriorityDto::Background,
            status: TransferStatusDto::Cancelled,
            retry_attempts: 0,
            downloaded_bytes: 0,
            total_bytes: None,
            next_retry_at: None,
            scheduled_start_at: None,
            duration_secs: Some(60),
            error: Some("Cancelled by user".to_string()),
        };

        let actions = download_action_availability(true, &dto);

        assert!(!actions.retry);
        assert!(actions.remove);
    }
}

use crate::{
    app::components::{Breadcrumbs, IconButton, Table, TableDefinition, TextButton},
    hooks::use_service_context,
    i18n::use_translation,
    utils::format_bytes,
};
use shared::{
    model::{FileDownloadDto, SortOrder},
    utils::unix_ts_to_str,
};
use std::{cmp::Ordering, rc::Rc};
use yew::platform::spawn_local;
use yew::prelude::*;
use yew_hooks::use_interval;

const HEADERS: [&str; 9] = [
    "LABEL.NAME",
    "LABEL.TYPE",
    "LABEL.STATUS",
    "LABEL.DOWNLOAD_DOWNLOADED",
    "LABEL.DOWNLOAD_FILE_SIZE",
    "LABEL.START",
    "LABEL.DURATION",
    "LABEL.ERROR",
    "LABEL.ACTIONS",
];

#[derive(Clone, PartialEq, Eq, Debug)]
enum DownloadTab {
    Queue,
    Active,
    Finished,
}

fn collect_downloads_for_tab(
    tab: &DownloadTab,
    queue: &Rc<Vec<FileDownloadDto>>,
    finished: &Rc<Vec<FileDownloadDto>>,
    active: &Option<Rc<FileDownloadDto>>,
) -> Vec<Rc<FileDownloadDto>> {
    match tab {
        DownloadTab::Queue => queue.iter().cloned().map(Rc::new).collect(),
        DownloadTab::Active => active.iter().cloned().collect(),
        DownloadTab::Finished => finished.iter().cloned().map(Rc::new).collect(),
    }
}

fn format_download_kind(translate: &crate::i18n::YewI18n, kind: &str) -> String {
    match kind {
        "Recording" => translate.t("LABEL.RECORD"),
        "Download" | "" => translate.t("LABEL.DOWNLOAD"),
        _ => kind.to_string(),
    }
}

fn format_download_state(translate: &crate::i18n::YewI18n, state: &str) -> String {
    match state {
        "Queued" => translate.t("LABEL.DOWNLOAD_STATE_QUEUED"),
        "Scheduled" => translate.t("LABEL.DOWNLOAD_STATE_SCHEDULED"),
        "Downloading" => translate.t("LABEL.DOWNLOAD_STATE_DOWNLOADING"),
        "Paused" => translate.t("LABEL.DOWNLOAD_STATE_PAUSED"),
        "Completed" => translate.t("LABEL.DOWNLOAD_STATE_COMPLETED"),
        "Failed" => translate.t("LABEL.DOWNLOAD_STATE_FAILED"),
        "Cancelled" => translate.t("LABEL.DOWNLOAD_CANCEL"),
        _ => state.to_string(),
    }
}

fn format_download_progress(download: &FileDownloadDto) -> String {
    if let Some(total) = download.total_size {
        if total > 0 {
            let percent = ((download.filesize as f64 / total as f64) * 100.0).round() as u32;
            return format!("{} / {} ({}%)", format_bytes(download.filesize), format_bytes(total), percent);
        }
    }
    format_bytes(download.filesize)
}

fn format_download_start(download: &FileDownloadDto) -> String {
    download.start_at.and_then(unix_ts_to_str).unwrap_or_default()
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

fn compare_downloads(a: &FileDownloadDto, b: &FileDownloadDto, col: usize) -> Ordering {
    match col {
        0 => a.filename.cmp(&b.filename),
        1 => a.kind.cmp(&b.kind),
        2 => a.state.cmp(&b.state),
        3 => a.filesize.cmp(&b.filesize),
        4 => a.total_size.unwrap_or(a.filesize).cmp(&b.total_size.unwrap_or(b.filesize)),
        5 => a.start_at.unwrap_or_default().cmp(&b.start_at.unwrap_or_default()),
        6 => a.duration_secs.unwrap_or_default().cmp(&b.duration_secs.unwrap_or_default()),
        7 => a.error.as_deref().unwrap_or_default().cmp(b.error.as_deref().unwrap_or_default()),
        _ => Ordering::Equal,
    }
}

fn is_sortable(col: usize) -> bool { col < 8 }

#[function_component(DownloadsView)]
pub fn downloads_view() -> Html {
    let translate = use_translation();
    let services = use_service_context();
    let breadcrumbs = use_state(|| Rc::new(vec![translate.t("LABEL.DOWNLOADS"), translate.t("LABEL.LIST")]));
    let active_tab = use_state(|| DownloadTab::Queue);
    let queue_state = use_state(|| Rc::new(Vec::<FileDownloadDto>::new()));
    let finished_state = use_state(|| Rc::new(Vec::<FileDownloadDto>::new()));
    let active_download = use_state(|| None::<Rc<FileDownloadDto>>);
    let table_items = use_state(|| None::<Rc<Vec<Rc<FileDownloadDto>>>>);

    let fetch_downloads = {
        let queue_state = queue_state.clone();
        let finished_state = finished_state.clone();
        let active_download = active_download.clone();
        let services = services.clone();
        Callback::from(move |_| {
            let queue_state = queue_state.clone();
            let finished_state = finished_state.clone();
            let active_download = active_download.clone();
            let services = services.clone();
            spawn_local(async move {
                if let Ok(response) = services.downloads.get_downloads().await {
                    queue_state.set(Rc::new(response.queue.clone()));
                    finished_state.set(Rc::new(response.downloads.clone()));
                    active_download.set(response.active.map(Rc::new));
                }
            });
        })
    };

    {
        let fetch = fetch_downloads.clone();
        use_effect_with((), move |_| {
            fetch.emit(());
            || {}
        });
    }

    {
        let fetch = fetch_downloads.clone();
        let queue_state = queue_state.clone();
        let active_download = active_download.clone();
        use_interval(
            move || {
                if active_download.is_some() || !queue_state.is_empty() {
                    fetch.emit(());
                }
            },
            2000,
        );
    }

    {
        let active_tab = active_tab.clone();
        let queue_state = queue_state.clone();
        let finished_state = finished_state.clone();
        let active_download = active_download.clone();
        let table_items = table_items.clone();
        use_effect_with(
            (
                (*active_tab).clone(),
                (*queue_state).clone(),
                (*finished_state).clone(),
                (*active_download).clone(),
            ),
            move |(tab, queue, finished, active)| {
                let items = collect_downloads_for_tab(tab, queue, finished, active);
                table_items.set((!items.is_empty()).then(|| Rc::new(items)));
                || ()
            },
        );
    }

    let handle_pause = {
        let fetch = fetch_downloads.clone();
        let services = services.clone();
        let translate = translate.clone();
        Callback::from(move |uuid: String| {
            let fetch = fetch.clone();
            let services = services.clone();
            let translate = translate.clone();
            spawn_local(async move {
                if services.downloads.pause_download(uuid).await.is_ok() {
                    services.toastr.success(translate.t("MESSAGES.DOWNLOAD.DOWNLOAD_PAUSED"));
                    fetch.emit(());
                }
            });
        })
    };

    let handle_resume = {
        let fetch = fetch_downloads.clone();
        let services = services.clone();
        let translate = translate.clone();
        Callback::from(move |uuid: String| {
            let fetch = fetch.clone();
            let services = services.clone();
            let translate = translate.clone();
            spawn_local(async move {
                if services.downloads.resume_download(uuid).await.is_ok() {
                    services.toastr.success(translate.t("MESSAGES.DOWNLOAD.DOWNLOAD_RESUMED"));
                    fetch.emit(());
                }
            });
        })
    };

    let handle_cancel = {
        let fetch = fetch_downloads.clone();
        let services = services.clone();
        let translate = translate.clone();
        Callback::from(move |uuid: String| {
            let fetch = fetch.clone();
            let services = services.clone();
            let translate = translate.clone();
            spawn_local(async move {
                if services.downloads.cancel_download(uuid).await.is_ok() {
                    services.toastr.success(translate.t("MESSAGES.DOWNLOAD.DOWNLOAD_CANCELLED"));
                    fetch.emit(());
                }
            });
        })
    };

    let handle_remove = {
        let fetch = fetch_downloads.clone();
        let services = services.clone();
        let translate = translate.clone();
        Callback::from(move |uuid: String| {
            let fetch = fetch.clone();
            let services = services.clone();
            let translate = translate.clone();
            spawn_local(async move {
                if services.downloads.remove_download(uuid).await.is_ok() {
                    services.toastr.success(translate.t("MESSAGES.DOWNLOAD.DOWNLOAD_REMOVED"));
                    fetch.emit(());
                }
            });
        })
    };

    let handle_retry = {
        let fetch = fetch_downloads.clone();
        let services = services.clone();
        let translate = translate.clone();
        Callback::from(move |uuid: String| {
            let fetch = fetch.clone();
            let services = services.clone();
            let translate = translate.clone();
            spawn_local(async move {
                if services.downloads.retry_download(uuid).await.is_ok() {
                    services.toastr.success(translate.t("MESSAGES.DOWNLOAD.DOWNLOAD_RETRIED"));
                    fetch.emit(());
                }
            });
        })
    };

    let render_header_cell = {
        let translate = translate.clone();
        Callback::<usize, Html>::from(move |col| {
            html! { { HEADERS.get(col).map_or_else(String::new, |key| translate.t(*key)) } }
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
            0 => html! { <span class="tp__table__nowrap">{dto.filename.clone()}</span> },
            1 => html! { format_download_kind(&translate, &dto.kind) },
            2 => html! { format_download_state(&translate, &dto.state) },
            3 => html! { <span class="tp__table__nowrap">{format_download_progress(&dto)}</span> },
            4 => html! { <span class="tp__table__nowrap">{dto.total_size.map_or_else(String::new, format_bytes)}</span> },
            5 => html! { <span class="tp__table__nowrap">{format_download_start(&dto)}</span> },
            6 => html! { format_download_duration(&dto) },
            7 => html! { dto.error.clone().unwrap_or_default() },
            8 => {
                let can_pause = dto.state == "Downloading";
                let can_resume = dto.state == "Paused";
                let can_cancel = dto.state == "Downloading" || dto.state == "Queued" || dto.state == "Scheduled";
                let can_remove =
                    dto.finished || dto.state == "Failed" || dto.state == "Completed" || dto.state == "Cancelled";
                let can_retry = dto.state == "Failed";
                let pause_uuid = dto.uuid.clone();
                let resume_uuid = dto.uuid.clone();
                let cancel_uuid = dto.uuid.clone();
                let retry_uuid = dto.uuid.clone();
                let remove_uuid = dto.uuid.clone();
                let pause_handle = handle_pause.clone();
                let resume_handle = handle_resume.clone();
                let cancel_handle = handle_cancel.clone();
                let retry_handle = handle_retry.clone();
                let remove_handle = handle_remove.clone();
                html! {
                    <div class="tp__downloads-table__actions">
                        if can_pause {
                            <IconButton name="Pause" icon="Pause" onclick={Callback::from(move |_| pause_handle.emit(pause_uuid.clone()))} />
                        }
                        if can_resume {
                            <IconButton name="Resume" icon="Play" onclick={Callback::from(move |_| resume_handle.emit(resume_uuid.clone()))} />
                        }
                        if can_cancel {
                            <IconButton name="Cancel" icon="Stop" onclick={Callback::from(move |_| cancel_handle.emit(cancel_uuid.clone()))} />
                        }
                        if can_retry {
                            <IconButton name="Retry" icon="Refresh" onclick={Callback::from(move |_| retry_handle.emit(retry_uuid.clone()))} />
                        }
                        if can_remove {
                            <IconButton name="Remove" icon="Delete" onclick={Callback::from(move |_| remove_handle.emit(remove_uuid.clone()))} />
                        }
                    </div>
                }
            }
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
        Callback::<Option<(usize, SortOrder)>, ()>::from(move |args| {
            let mut items = collect_downloads_for_tab(&active_tab, &queue_state, &finished_state, &active_download);
            if let Some((col, order)) = args {
                items.sort_by(|a, b| match order {
                    SortOrder::Asc => compare_downloads(a, b, col),
                    SortOrder::Desc => compare_downloads(b, a, col),
                    SortOrder::None => Ordering::Equal,
                });
            }
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
        let class = if *active_tab == tab { "active" } else { "secondary" };
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
            <Breadcrumbs items={&*breadcrumbs}/>
            <div class="tp__downloads-view__body tp__list-view__body">
                <div class="tp__downloads-list tp__list-list">
                    <div class="tp__downloads-list__header tp__list-list__header">
                        <h1>{translate.t("LABEL.DOWNLOADS")}</h1>
                        <div class="tp__downloads-list__header-toolbar">
                            {render_filter_button(DownloadTab::Queue, "Download", translate.t("LABEL.DOWNLOAD_QUEUE"))}
                            {render_filter_button(DownloadTab::Active, "Play", translate.t("LABEL.DOWNLOAD_ACTIVE"))}
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
    use super::{collect_downloads_for_tab, DownloadTab};
    use shared::model::FileDownloadDto;
    use std::rc::Rc;

    fn download(uuid: &str, state: &str) -> FileDownloadDto {
        FileDownloadDto {
            uuid: uuid.to_string(),
            filename: format!("{uuid}.mp4"),
            kind: "Download".to_string(),
            filesize: 0,
            total_size: None,
            finished: false,
            paused: false,
            state: state.to_string(),
            start_at: None,
            duration_secs: None,
            error: None,
        }
    }

    #[test]
    fn collect_downloads_for_active_tab_only_returns_active_download() {
        let queue = Rc::new(vec![download("queued", "Queued")]);
        let finished = Rc::new(vec![download("done", "Completed")]);
        let active = Some(Rc::new(download("active", "Downloading")));

        let active_items = collect_downloads_for_tab(&DownloadTab::Active, &queue, &finished, &active);
        let queue_items = collect_downloads_for_tab(&DownloadTab::Queue, &queue, &finished, &active);
        let finished_items = collect_downloads_for_tab(&DownloadTab::Finished, &queue, &finished, &active);

        assert_eq!(active_items.len(), 1);
        assert_eq!(active_items[0].uuid, "active");
        assert_eq!(queue_items.len(), 1);
        assert_eq!(queue_items[0].uuid, "queued");
        assert_eq!(finished_items.len(), 1);
        assert_eq!(finished_items[0].uuid, "done");
    }
}

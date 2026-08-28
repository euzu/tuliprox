use crate::{
    app::components::{
        Country, DateInput, DropDownOption, PagedTable, RevealContent, Search, TabItem, TabSet, Table, TableDefinition,
        TextButton, PAGE_SIZES, TP_PAGE_SIZE_KEY,
    },
    hooks::use_service_context,
    i18n::use_translation,
    utils::{
        format_bytes, format_duration, format_local_day_boundary_utc, format_ts, get_local_storage_item,
        set_local_storage_item,
    },
};
use futures::join;
use shared::{
    defaults::default_page_size,
    model::{
        PagedResponseDto, QosSnapshotRecordDto, QosSnapshotWindowDto, SearchRequest, StreamHistoryEventType,
        StreamHistoryPageRequestDto, StreamHistoryProviderSummaryDto, StreamHistoryRecordDto, StreamHistorySearchField,
    },
};
use std::rc::Rc;
use wasm_bindgen_futures::spawn_local;
use yew::{prelude::*, use_mut_ref};

const NUM_COLS: usize = 15;
const SUMMARY_NUM_COLS: usize = 6;
const QOS_SUMMARY_NUM_COLS: usize = 8;
const QOS_DETAIL_NUM_COLS: usize = 8;

const STREAM_HISTORY_TAB_HISTORY: &str = "stream-history";
const STREAM_HISTORY_TAB_QOS: &str = "qos-snapshot";
const STREAM_HISTORY_TAB_SUMMARY: &str = "summary";

fn stream_history_tab_ids() -> [&'static str; 3] {
    [STREAM_HISTORY_TAB_HISTORY, STREAM_HISTORY_TAB_QOS, STREAM_HISTORY_TAB_SUMMARY]
}

#[derive(Clone, PartialEq)]
struct ProviderSummaryRow {
    provider_name: String,
    session_count: u64,
    disconnect_count: u64,
    total_bytes_sent: u64,
    avg_session_duration_secs: Option<u64>,
    avg_first_byte_latency_ms: Option<u64>,
}

#[derive(Clone, PartialEq)]
struct QosSummaryRow {
    stream_identity_key: String,
    input_name: String,
    provider_name: String,
    provider_id: u32,
    target_name: String,
    item_type: String,
    window_24h: QosSnapshotWindowDto,
}

#[derive(Clone, PartialEq)]
struct QosDetailRow {
    window_name: String,
    window: QosSnapshotWindowDto,
}

fn today_start_ts() -> i64 {
    #[cfg(target_arch = "wasm32")]
    let today = {
        let now = js_sys::Date::new_0();
        i32::try_from(now.get_full_year())
            .ok()
            .and_then(|year| chrono::NaiveDate::from_ymd_opt(year, now.get_month() + 1, now.get_date()))
    };
    #[cfg(not(target_arch = "wasm32"))]
    let today = Some(chrono::Utc::now().date_naive());

    today.and_then(|date| date.and_hms_opt(0, 0, 0)).map_or(0, |dt| dt.and_utc().timestamp())
}

fn ts_to_filter_str(ts: i64, end_of_day: bool) -> String { format_local_day_boundary_utc(ts, end_of_day) }

fn optional_record_text_str(value: Option<&str>) -> &str { value.unwrap_or("-") }
fn optional_record_text(value: Option<String>) -> String { value.unwrap_or_else(|| String::from("-")) }

fn search_request_parts(request: &SearchRequest) -> (Option<String>, Option<String>, Option<Vec<String>>) {
    match request {
        SearchRequest::Clear => (None, None, None),
        SearchRequest::Text(text, fields) => (
            Some(text.clone()),
            Some(String::from("text")),
            fields.as_ref().map(|selected| selected.iter().cloned().collect()),
        ),
        SearchRequest::Regexp(pattern, fields) => (
            Some(pattern.clone()),
            Some(String::from("regex")),
            fields.as_ref().map(|selected| selected.iter().cloned().collect()),
        ),
    }
}

fn is_latest_request(latest_request_id: u64, request_id: u64) -> bool { latest_request_id == request_id }

fn qos_snapshot_matches(snapshot: &QosSnapshotRecordDto, filter: &SearchRequest) -> bool {
    match filter {
        SearchRequest::Clear => true,
        SearchRequest::Text(text, _) => {
            let text_lower = text.to_lowercase();
            [
                snapshot.stream_identity_key.as_str(),
                snapshot.input_name.as_str(),
                snapshot.provider_name.as_str(),
                snapshot.item_type.as_str(),
            ]
            .iter()
            .any(|field| field.to_lowercase().contains(&text_lower))
                || snapshot.target_name.to_lowercase().contains(&text_lower)
                || snapshot.provider_id.to_string().contains(&text_lower)
                || snapshot.virtual_id.to_string().contains(&text_lower)
        }
        SearchRequest::Regexp(pattern, _) => {
            if let Ok(re) = shared::model::REGEX_CACHE.get_or_compile(pattern) {
                re.is_match(&snapshot.stream_identity_key)
                    || re.is_match(&snapshot.input_name)
                    || re.is_match(&snapshot.provider_name)
                    || re.is_match(snapshot.item_type.as_str())
                    || re.is_match(&snapshot.target_name)
                    || re.is_match(&snapshot.provider_id.to_string())
                    || re.is_match(&snapshot.virtual_id.to_string())
            } else {
                false
            }
        }
    }
}

fn top_qos_snapshots(
    snapshots: &[QosSnapshotRecordDto],
    filter: &SearchRequest,
    limit: usize,
) -> Vec<QosSnapshotRecordDto> {
    let mut filtered =
        snapshots.iter().filter(|snapshot| qos_snapshot_matches(snapshot, filter)).cloned().collect::<Vec<_>>();
    filtered.sort_by(|left, right| {
        right
            .window_24h
            .score
            .cmp(&left.window_24h.score)
            .then_with(|| right.window_24h.confidence.cmp(&left.window_24h.confidence))
            .then_with(|| left.stream_identity_key.cmp(&right.stream_identity_key))
    });
    filtered.truncate(limit);
    filtered
}

fn qos_score_label(score: u8) -> &'static str {
    match score {
        85..=u8::MAX => "strong",
        65..=84 => "good",
        40..=64 => "watch",
        _ => "poor",
    }
}

fn provider_summary_rows(summaries: &[StreamHistoryProviderSummaryDto]) -> Vec<ProviderSummaryRow> {
    summaries
        .iter()
        .map(|summary| ProviderSummaryRow {
            provider_name: summary.provider_name.clone(),
            session_count: summary.session_count,
            disconnect_count: summary.disconnect_count,
            total_bytes_sent: summary.total_bytes_sent,
            avg_session_duration_secs: summary.avg_session_duration_secs,
            avg_first_byte_latency_ms: summary.avg_first_byte_latency_ms,
        })
        .collect()
}

fn qos_summary_rows(snapshots: &[QosSnapshotRecordDto]) -> Vec<QosSummaryRow> {
    snapshots
        .iter()
        .map(|snapshot| QosSummaryRow {
            stream_identity_key: snapshot.stream_identity_key.clone(),
            input_name: snapshot.input_name.clone(),
            provider_name: snapshot.provider_name.clone(),
            provider_id: snapshot.provider_id,
            target_name: snapshot.target_name.clone(),
            item_type: snapshot.item_type.to_string(),
            window_24h: snapshot.window_24h.clone(),
        })
        .collect()
}

fn qos_detail_rows(snapshot: &QosSnapshotRecordDto) -> Vec<QosDetailRow> {
    [("24h", snapshot.window_24h.clone()), ("7d", snapshot.window_7d.clone()), ("30d", snapshot.window_30d.clone())]
        .into_iter()
        .map(|(window_name, window)| QosDetailRow { window_name: window_name.to_string(), window })
        .collect()
}

fn stream_history_table_items(
    response: Option<&PagedResponseDto<StreamHistoryRecordDto>>,
) -> Vec<Rc<StreamHistoryRecordDto>> {
    response.map_or_else(Vec::new, |r| r.items.iter().cloned().map(Rc::new).collect())
}

#[component]
pub fn StreamHistoryView() -> Html {
    let services = use_service_context();
    let translate = use_translation();
    let from_date = use_state(|| Some(today_start_ts()));
    let to_date = use_state(|| Some(today_start_ts()));
    let paged_response = use_state(|| None::<PagedResponseDto<StreamHistoryRecordDto>>);
    let page = use_state(|| 1u32);
    let page_size = use_state(|| {
        get_local_storage_item(TP_PAGE_SIZE_KEY)
            .and_then(|v| v.parse::<u16>().ok())
            .filter(|size| PAGE_SIZES.contains(size))
            .unwrap_or_else(default_page_size)
    });
    let summaries = use_state(Vec::<StreamHistoryProviderSummaryDto>::new);
    let qos_snapshots = use_state(Vec::<QosSnapshotRecordDto>::new);
    let selected_qos_snapshot = use_state(|| None::<QosSnapshotRecordDto>);
    let search_filter = use_state(|| SearchRequest::Clear);
    let loading = use_state(|| false);
    let request_id = use_mut_ref(|| 0u64);
    let search_options: Rc<Vec<DropDownOption>> = {
        let translate = translate.clone();
        use_memo(translate, move |translate| {
            // Ids come from the shared enum so backend parsing can never drift.
            let label_key = |field: StreamHistorySearchField| match field {
                StreamHistorySearchField::EventTsUtc => "LABEL.STREAM_HISTORY_TIME",
                StreamHistorySearchField::EventType => "LABEL.STREAM_HISTORY_EVENT",
                StreamHistorySearchField::Title => "LABEL.TITLE",
                StreamHistorySearchField::Group => "LABEL.GROUP",
                StreamHistorySearchField::ApiUsername => "LABEL.USERNAME",
                StreamHistorySearchField::ProviderName => "LABEL.PROVIDER",
                StreamHistorySearchField::ProviderId => "LABEL.PROVIDER_ID",
                StreamHistorySearchField::BytesSent => "LABEL.STREAM_HISTORY_BYTES",
                StreamHistorySearchField::FirstByteLatencyMs => "LABEL.STREAM_HISTORY_FIRST_BYTE",
                StreamHistorySearchField::UserAgent => "LABEL.USER_AGENT",
                StreamHistorySearchField::ItemType => "LABEL.TYPE",
                StreamHistorySearchField::Container => "LABEL.CONTAINER",
                StreamHistorySearchField::DisconnectReason => "LABEL.STREAM_HISTORY_REASON",
                StreamHistorySearchField::SourceAddr => "LABEL.STREAM_HISTORY_IP",
                StreamHistorySearchField::Country => "LABEL.COUNTRY",
                StreamHistorySearchField::Cluster => "LABEL.CLUSTER",
            };
            use strum::IntoEnumIterator;
            StreamHistorySearchField::iter()
                .map(|field| DropDownOption::new(field.as_ref(), html! { translate.t(label_key(field)) }, false))
                .collect::<Vec<_>>()
        })
    };

    let handle_from_change = {
        let from_date = from_date.clone();
        Callback::from(move |ts: Option<i64>| from_date.set(ts))
    };

    let handle_to_change = {
        let to_date = to_date.clone();
        Callback::from(move |ts: Option<i64>| to_date.set(ts))
    };

    let fetch_history_page: Rc<dyn Fn(u32, u16, SearchRequest)> = {
        let services = services.clone();
        let from_date = from_date.clone();
        let to_date = to_date.clone();
        let paged_response = paged_response.clone();
        let loading = loading.clone();
        let request_id = request_id.clone();
        Rc::new(move |requested_page: u32, requested_page_size: u16, request: SearchRequest| {
            let services = services.clone();
            let paged_response = paged_response.clone();
            let loading = loading.clone();
            let request_id = request_id.clone();
            let from_str = (*from_date).map(|ts| ts_to_filter_str(ts, false));
            let to_str = (*to_date).map(|ts| ts_to_filter_str(ts, true));
            let (search_text, search_mode, search_fields) = search_request_parts(&request);
            let next_request_id = {
                let mut current = request_id.borrow_mut();
                *current = current.saturating_add(1);
                *current
            };
            loading.set(true);
            spawn_local(async move {
                let result = services
                    .stream_history
                    .get_history_page(StreamHistoryPageRequestDto {
                        from: from_str,
                        to: to_str,
                        page: requested_page,
                        page_size: requested_page_size,
                        search: search_text,
                        search_mode,
                        search_fields,
                    })
                    .await;
                if is_latest_request(*request_id.borrow(), next_request_id) {
                    match result {
                        Ok(Some(response)) => paged_response.set(Some(response)),
                        Ok(None) | Err(_) => paged_response.set(None),
                    }
                    loading.set(false);
                }
            });
        })
    };

    let handle_load = {
        let services = services.clone();
        let from_date = from_date.clone();
        let to_date = to_date.clone();
        let paged_response = paged_response.clone();
        let summaries = summaries.clone();
        let qos_snapshots = qos_snapshots.clone();
        let selected_qos_snapshot = selected_qos_snapshot.clone();
        let loading = loading.clone();
        let request_id = request_id.clone();
        let page = page.clone();
        let page_size = page_size.clone();
        let search_filter = search_filter.clone();
        Callback::from(move |_: String| {
            let services = services.clone();
            let paged_response = paged_response.clone();
            let summaries = summaries.clone();
            let qos_snapshots = qos_snapshots.clone();
            let selected_qos_snapshot = selected_qos_snapshot.clone();
            let loading = loading.clone();
            let request_id = request_id.clone();
            let from_str = (*from_date).map(|ts| ts_to_filter_str(ts, false));
            let to_str = (*to_date).map(|ts| ts_to_filter_str(ts, true));
            let next_request_id = {
                let mut current = request_id.borrow_mut();
                *current = current.saturating_add(1);
                *current
            };
            let requested_page_size = *page_size;
            let active_search = (*search_filter).clone();
            let (search_text, search_mode, search_fields) = search_request_parts(&active_search);
            page.set(1);
            loading.set(true);
            spawn_local(async move {
                let (history_result, summary_result, qos_result) = join!(
                    services.stream_history.get_history_page(StreamHistoryPageRequestDto {
                        from: from_str.clone(),
                        to: to_str.clone(),
                        page: 1,
                        page_size: requested_page_size,
                        search: search_text,
                        search_mode,
                        search_fields,
                    }),
                    services.stream_history.get_summary(from_str.as_deref(), to_str.as_deref()),
                    services.stream_history.get_qos_snapshots()
                );
                if is_latest_request(*request_id.borrow(), next_request_id) {
                    match history_result {
                        Ok(Some(resp)) => paged_response.set(Some(resp)),
                        Ok(None) | Err(_) => paged_response.set(None),
                    }
                    match summary_result {
                        Ok(Some(items)) => summaries.set(items),
                        Ok(None) | Err(_) => summaries.set(Vec::new()),
                    }
                    if let Ok(Some(items)) = qos_result {
                        selected_qos_snapshot.set(None);
                        qos_snapshots.set(items);
                    } else {
                        selected_qos_snapshot.set(None);
                        qos_snapshots.set(Vec::new());
                    }
                    loading.set(false);
                }
            });
        })
    };

    // Load on mount with default date range (today)
    {
        let handle_load = handle_load.clone();
        use_effect_with((), move |()| {
            handle_load.emit(String::new());
            || ()
        });
    }

    let handle_page_change = {
        let page = page.clone();
        let page_size = page_size.clone();
        let search_filter = search_filter.clone();
        let fetch_history_page = fetch_history_page.clone();
        Callback::from(move |new_page: u32| {
            page.set(new_page);
            fetch_history_page(new_page, *page_size, (*search_filter).clone());
        })
    };

    let handle_page_size_change = {
        let page = page.clone();
        let page_size = page_size.clone();
        let search_filter = search_filter.clone();
        let fetch_history_page = fetch_history_page.clone();
        Callback::from(move |new_size: u16| {
            page.set(1);
            page_size.set(new_size);
            set_local_storage_item(TP_PAGE_SIZE_KEY, &new_size.to_string());
            fetch_history_page(1, new_size, (*search_filter).clone());
        })
    };

    let handle_search = {
        let search_filter = search_filter.clone();
        let page = page.clone();
        let page_size = page_size.clone();
        let fetch_history_page = fetch_history_page.clone();
        Callback::from(move |req: SearchRequest| {
            search_filter.set(req.clone());
            page.set(1);
            fetch_history_page(1, *page_size, req);
        })
    };

    let table_items: Rc<Vec<Rc<StreamHistoryRecordDto>>> =
        use_memo((*paged_response).clone(), |resp| stream_history_table_items(resp.as_ref()));

    let visible_qos_snapshots: Rc<Vec<QosSnapshotRecordDto>> =
        use_memo(((*qos_snapshots).clone(), (*search_filter).clone()), |(snapshots, filter)| {
            top_qos_snapshots(snapshots, filter, 8)
        });
    let summary_rows: Rc<Vec<Rc<ProviderSummaryRow>>> =
        use_memo((*summaries).clone(), |items| provider_summary_rows(items).into_iter().map(Rc::new).collect());
    let qos_summary_rows_list: Rc<Vec<Rc<QosSummaryRow>>> =
        use_memo((*visible_qos_snapshots).clone(), |items| qos_summary_rows(items).into_iter().map(Rc::new).collect());
    let qos_detail_rows_list: Rc<Vec<Rc<QosDetailRow>>> = use_memo((*selected_qos_snapshot).clone(), |snapshot| {
        snapshot.as_ref().map_or_else(Vec::new, |item| qos_detail_rows(item).into_iter().map(Rc::new).collect())
    });

    let handle_qos_select = {
        let services = services.clone();
        let selected_qos_snapshot = selected_qos_snapshot.clone();
        Callback::from(move |stream_identity_key: String| {
            let services = services.clone();
            let selected_qos_snapshot = selected_qos_snapshot.clone();
            spawn_local(async move {
                match services.stream_history.get_qos_snapshot_detail(&stream_identity_key).await {
                    Ok(Some(snapshot)) => selected_qos_snapshot.set(Some(snapshot)),
                    Ok(None) | Err(_) => selected_qos_snapshot.set(None),
                }
            });
        })
    };

    let translate_for_table = translate.clone();
    let table_def: Rc<TableDefinition<StreamHistoryRecordDto>> = use_memo(table_items.clone(), move |items| {
        let translate = translate_for_table.clone();
        TableDefinition {
            items: Some(items.clone()),
            num_cols: NUM_COLS,
            is_sortable: Callback::from(|_| false),
            on_sort: Callback::noop(),
            render_header_cell: Callback::from(move |col: usize| {
                let label = match col {
                    0 => translate.t("LABEL.STREAM_HISTORY_TIME"),
                    1 => translate.t("LABEL.STREAM_HISTORY_EVENT"),
                    2 => translate.t("LABEL.USERNAME"),
                    3 => translate.t("LABEL.GROUP"),
                    4 => translate.t("LABEL.TITLE"),
                    5 => translate.t("LABEL.PROVIDER"),
                    6 => translate.t("LABEL.DURATION"),
                    7 => translate.t("LABEL.STREAM_HISTORY_BYTES"),
                    8 => translate.t("LABEL.STREAM_HISTORY_FIRST_BYTE"),
                    9 => translate.t("LABEL.USER_AGENT"),
                    10 => translate.t("LABEL.TYPE"),
                    11 => translate.t("LABEL.CONTAINER"),
                    12 => translate.t("LABEL.STREAM_HISTORY_REASON"),
                    13 => translate.t("LABEL.STREAM_HISTORY_IP"),
                    14 => translate.t("LABEL.COUNTRY"),
                    _ => String::new(),
                };
                html! { <span>{label}</span> }
            }),
            render_data_cell: Callback::from(
                |(_, col, record): (usize, usize, Rc<StreamHistoryRecordDto>)| match col {
                    0 => html! { <span class="tp__stream-history__cell--time">{format_ts(record.event_ts_utc)}</span> },
                    1 => {
                        let is_connect = record.event_type == StreamHistoryEventType::Connect;
                        let badge_class = if is_connect {
                            "tp__stream-history__badge tp__stream-history__badge--connect"
                        } else {
                            "tp__stream-history__badge tp__stream-history__badge--disconnect"
                        };
                        html! { <span class={badge_class}>{record.event_type.to_string()}</span> }
                    }
                    2 => html! { <span>{record.api_username.as_deref().unwrap_or("-")}</span> },
                    3 => {
                        html! { <span class="tp__stream-history__cell--title">{record.group.as_deref().unwrap_or("-")}</span> }
                    }
                    4 => {
                        html! { <span class="tp__stream-history__cell--title">{record.title.as_deref().unwrap_or("-")}</span> }
                    }
                    5 => html! {
                        <span>
                            {
                                match (record.provider_name.as_deref(), record.provider_id) {
                                    (Some(name), Some(id)) => format!("{name} (#{id})"),
                                    (Some(name), None) => name.to_string(),
                                    (None, Some(id)) => format!("#{id}"),
                                    (None, None) => String::from("-"),
                                }
                            }
                        </span>
                    },
                    6 => html! {
                        <span class="tp__stream-history__cell--mono">
                            {record.session_duration.map(format_duration).unwrap_or_default()}
                        </span>
                    },
                    7 => html! {
                        <span class="tp__stream-history__cell--mono">
                            {record.bytes_sent.map(format_bytes).unwrap_or_default()}
                        </span>
                    },
                    8 => html! {
                        <span class="tp__stream-history__cell--mono">
                            {record.first_byte_latency_ms.map(|v| v.to_string()).unwrap_or_default()}
                        </span>
                    },
                    9 => html! {
                        <span class="tp__stream-history__cell--title">
                        <RevealContent preview={record.user_agent.as_deref().map(|ua| html! {ua})}>{record.user_agent.as_deref()}</RevealContent>
                        </span>
                    },
                    10 => html! {
                        <span>
                            {optional_record_text(record.item_type.as_ref().map(ToString::to_string))}
                        </span>
                    },
                    11 => html! {
                        <span>
                            {optional_record_text_str(record.container.as_deref())}
                        </span>
                    },
                    12 => html! {
                        <span>
                            {record.disconnect_reason.as_ref().map_or_else(|| String::from("-"), ToString::to_string).replace('_', " ")}
                        </span>
                    },
                    13 => html! {
                        <span class="tp__stream-history__cell--ip">
                            {record.source_addr.as_deref().unwrap_or("-")}
                        </span>
                    },
                    14 => html! {
                        <span class="tp__stream-history__cell--country">
                            <Country country_code={record.country.clone()} />
                        </span>
                    },
                    _ => html! {},
                },
            ),
        }
    });
    let translate_for_summary = translate.clone();
    let summary_table_def: Rc<TableDefinition<ProviderSummaryRow>> = use_memo(summary_rows.clone(), move |rows| {
        let translate = translate_for_summary.clone();
        TableDefinition {
            items: Some(rows.clone()),
            num_cols: SUMMARY_NUM_COLS,
            is_sortable: Callback::from(|_| false),
            on_sort: Callback::noop(),
            render_header_cell: Callback::from(move |col: usize| {
                let label = match col {
                    0 => translate.t("LABEL.PROVIDER"),
                    1 => translate.t("LABEL.STREAM_HISTORY_SESSIONS"),
                    2 => translate.t("LABEL.STREAM_HISTORY_BYTES"),
                    3 => translate.t("LABEL.DURATION"),
                    4 => translate.t("LABEL.STREAM_HISTORY_FIRST_BYTE"),
                    5 => translate.t("LABEL.STREAM_HISTORY_DISCONNECTS"),
                    _ => String::new(),
                };
                html! { <span>{label}</span> }
            }),
            render_data_cell: Callback::from(|(_, col, row): (usize, usize, Rc<ProviderSummaryRow>)| match col {
                0 => html! { <span>{row.provider_name.clone()}</span> },
                1 => html! { <span>{row.session_count}</span> },
                2 => html! { <span>{format_bytes(row.total_bytes_sent)}</span> },
                3 => html! { <span>{row.avg_session_duration_secs.map(format_duration).unwrap_or_default()}</span> },
                4 => {
                    html! { <span>{row.avg_first_byte_latency_ms.map(|v| format!("{v} ms")).unwrap_or_default()}</span> }
                }
                5 => html! { <span>{row.disconnect_count}</span> },
                _ => html! {},
            }),
        }
    });
    let translate_for_qos_summary = translate.clone();
    let handle_qos_select_for_table = handle_qos_select.clone();
    let qos_summary_table_def: Rc<TableDefinition<QosSummaryRow>> = use_memo(
        qos_summary_rows_list.clone(),
        move |rows| {
            let translate = translate_for_qos_summary.clone();
            let handle_qos_select = handle_qos_select_for_table.clone();
            TableDefinition {
                items: Some(rows.clone()),
                num_cols: QOS_SUMMARY_NUM_COLS,
                is_sortable: Callback::from(|_| false),
                on_sort: Callback::noop(),
                render_header_cell: Callback::from(move |col: usize| {
                    let label = match col {
                        0 => translate.t("LABEL.INPUT"),
                        1 => translate.t("LABEL.PROVIDER"),
                        2 => translate.t("LABEL.TARGET"),
                        3 => translate.t("LABEL.TYPE"),
                        4 => translate.t("LABEL.QUALITY"),
                        5 => translate.t("LABEL.STREAM_HISTORY_CONFIDENCE"),
                        6 => translate.t("LABEL.STREAM_HISTORY_FIRST_BYTE"),
                        7 => translate.t("LABEL.DURATION"),
                        _ => String::new(),
                    };
                    html! { <span>{label}</span> }
                }),
                render_data_cell: Callback::from(move |(_, col, row): (usize, usize, Rc<QosSummaryRow>)| match col {
                    0 => html! {
                        <button
                            type="button"
                            class="tp__text-button"
                            onclick={{
                                let handle_qos_select = handle_qos_select.clone();
                                let stream_identity_key = row.stream_identity_key.clone();
                                Callback::from(move |_| handle_qos_select.emit(stream_identity_key.clone()))
                            }}>
                            {row.input_name.clone()}
                        </button>
                    },
                    1 => html! { <span>{format!("{} (#{})", row.provider_name, row.provider_id)}</span> },
                    2 => html! { <span>{row.target_name.clone()}</span> },
                    3 => html! { <span>{row.item_type.clone()}</span> },
                    4 => {
                        html! { <span>{format!("{} ({})", row.window_24h.score, qos_score_label(row.window_24h.score))}</span> }
                    }
                    5 => html! { <span>{format!("{}%", row.window_24h.confidence)}</span> },
                    6 => {
                        html! { <span>{row.window_24h.avg_first_byte_latency_ms.map(|v| format!("{v} ms")).unwrap_or_default()}</span> }
                    }
                    7 => {
                        html! { <span>{row.window_24h.avg_session_duration_secs.map(format_duration).unwrap_or_default()}</span> }
                    }
                    _ => html! {},
                }),
            }
        },
    );
    let translate_for_qos_detail = translate.clone();
    let qos_detail_table_def: Rc<TableDefinition<QosDetailRow>> = use_memo(qos_detail_rows_list.clone(), move |rows| {
        let translate = translate_for_qos_detail.clone();
        TableDefinition {
            items: Some(rows.clone()),
            num_cols: QOS_DETAIL_NUM_COLS,
            is_sortable: Callback::from(|_| false),
            on_sort: Callback::noop(),
            render_header_cell: Callback::from(move |col: usize| {
                let label = match col {
                    0 => translate.t("LABEL.STREAM_HISTORY_WINDOW"),
                    1 => translate.t("LABEL.STREAM_HISTORY_SCORE"),
                    2 => translate.t("LABEL.STREAM_HISTORY_CONFIDENCE"),
                    3 => translate.t("LABEL.STREAM_HISTORY_SESSIONS"),
                    4 => translate.t("LABEL.STREAM_HISTORY_CONNECT_FAILED"),
                    5 => translate.t("LABEL.STREAM_HISTORY_RUNTIME_ABORTS"),
                    6 => translate.t("LABEL.STREAM_HISTORY_FIRST_BYTE"),
                    7 => translate.t("LABEL.DURATION"),
                    _ => String::new(),
                };
                html! { <span>{label}</span> }
            }),
            render_data_cell: Callback::from(|(_, col, row): (usize, usize, Rc<QosDetailRow>)| match col {
                0 => html! { <span>{row.window_name.clone()}</span> },
                1 => html! { <span>{format!("{} ({})", row.window.score, qos_score_label(row.window.score))}</span> },
                2 => html! { <span>{format!("{}%", row.window.confidence)}</span> },
                3 => html! { <span>{row.window.connect_count}</span> },
                4 => html! { <span>{row.window.connect_failed_count}</span> },
                5 => html! { <span>{row.window.runtime_abort_count}</span> },
                6 => {
                    html! { <span>{row.window.avg_first_byte_latency_ms.map(|v| format!("{v} ms")).unwrap_or_default()}</span> }
                }
                7 => {
                    html! { <span>{row.window.avg_session_duration_secs.map(format_duration).unwrap_or_default()}</span> }
                }
                _ => html! {},
            }),
        }
    });

    let history_content = if let Some(ref resp) = *paged_response {
        html! {
            <PagedTable::<StreamHistoryRecordDto>
                definition={table_def.clone()}
                page={resp.page}
                page_size={resp.page_size}
                total_items={resp.total_items}
                total_pages={resp.total_pages}
                has_prev={resp.has_prev}
                has_next={resp.has_next}
                on_page_change={handle_page_change.clone()}
                on_page_size_change={handle_page_size_change.clone()}
            />
        }
    } else {
        html! { <Table::<StreamHistoryRecordDto> definition={table_def.clone()} /> }
    };

    let qos_content = html! {
        <>
            <Table::<QosSummaryRow> definition={qos_summary_table_def.clone()} />
            if (*selected_qos_snapshot).is_some() {
                <div class="tp__stream-history__summary">
                    <h2>{translate.t("LABEL.STREAM_HISTORY_QOS_DETAIL")}</h2>
                    <Table::<QosDetailRow> definition={qos_detail_table_def.clone()} />
                </div>
            }
        </>
    };

    let tab_ids = stream_history_tab_ids();
    let tabs = Rc::new(vec![
        TabItem {
            id: tab_ids[0].to_string(),
            title: translate.t("LABEL.STREAM_HISTORY"),
            icon: "Playlist".to_string(),
            children: history_content,
            active_class: None,
            inactive_class: None,
        },
        TabItem {
            id: tab_ids[1].to_string(),
            title: translate.t("LABEL.STREAM_HISTORY_QOS"),
            icon: "Status".to_string(),
            children: qos_content,
            active_class: None,
            inactive_class: None,
        },
        TabItem {
            id: tab_ids[2].to_string(),
            title: translate.t("LABEL.STREAM_HISTORY_SUMMARY"),
            icon: "Stats".to_string(),
            children: html! { <Table::<ProviderSummaryRow> definition={summary_table_def.clone()} /> },
            active_class: None,
            inactive_class: None,
        },
    ]);

    html! {
        <div class="tp__stream-history">
            <div class="tp__stream-history__header">
                <h1>{translate.t("LABEL.STREAM_HISTORY")}</h1>
            </div>
            <div class="tp__stream-history__toolbar">
                <div class="tp__stream-history__date-range">
                    <DateInput
                        name="from"
                        label={Some(translate.t("LABEL.STREAM_HISTORY_FROM"))}
                        value={*from_date}
                        on_change={Some(handle_from_change)}
                    />
                    <DateInput
                        name="to"
                        label={Some(translate.t("LABEL.STREAM_HISTORY_TO"))}
                        value={*to_date}
                        on_change={Some(handle_to_change)}
                    />
                    <TextButton
                        name="load"
                        title={translate.t("LABEL.STREAM_HISTORY_LOAD")}
                        class="primary"
                        onclick={handle_load}
                    />
                </div>
                <Search onsearch={Some(handle_search)} min_length={1} options={Some(search_options)} />
                if *loading {
                    <div class="tp__stream-history__loading tp__stream-history__loading--inline">
                        <span>{translate.t("LABEL.STREAM_HISTORY_LOADING")}</span>
                    </div>
                }
            </div>
            <div class="tp__stream-history__body">
                <TabSet tabs={tabs} class="tp__stream-history__tabset" />
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::{
        is_latest_request, optional_record_text_str, provider_summary_rows, qos_detail_rows, qos_score_label,
        stream_history_tab_ids, stream_history_table_items, top_qos_snapshots,
    };
    use shared::model::{
        PagedResponseDto, PlaylistItemType, QosSnapshotRecordDto, QosSnapshotWindowDto, SearchRequest,
        StreamHistoryEventType, StreamHistoryProviderSummaryDto, StreamHistoryRecordDto,
    };

    fn make_record(session_id: u64) -> StreamHistoryRecordDto {
        StreamHistoryRecordDto {
            event_type: StreamHistoryEventType::Connect,
            event_ts_utc: 1_700_000_000,
            partition_day_utc: "2026-05-06".to_string(),
            session_id,
            source_addr: None,
            api_username: None,
            provider_name: None,
            input_name: None,
            virtual_id: Some(1),
            item_type: Some(PlaylistItemType::Live),
            title: Some(format!("stream-{session_id}")),
            group: None,
            country: None,
            user_agent: None,
            shared: None,
            provider_id: None,
            cluster: None,
            container: None,
            video_codec: None,
            audio_codec: None,
            resolution: None,
            failure_stage: None,
            connect_failure_reason: None,
            disconnect_reason: None,
            session_duration: None,
            bytes_sent: None,
            first_byte_latency_ms: None,
            previous_session_id: None,
            target_name: None,
        }
    }

    fn empty_window() -> QosSnapshotWindowDto {
        QosSnapshotWindowDto {
            connect_count: 0,
            connect_failed_count: 0,
            runtime_abort_count: 0,
            provider_closed_count: 0,
            avg_first_byte_latency_ms: None,
            avg_session_duration_secs: None,
            last_success_ts: None,
            last_failure_ts: None,
            score: 0,
            confidence: 0,
            ..Default::default()
        }
    }

    fn make_snapshot(
        stream_identity_key: &str,
        provider_name: &str,
        score: u8,
        confidence: u8,
    ) -> QosSnapshotRecordDto {
        QosSnapshotRecordDto {
            stream_identity_key: stream_identity_key.to_string(),
            input_name: "input-a".to_string(),
            target_name: "target-a".to_string(),
            provider_name: provider_name.to_string(),
            provider_id: 10,
            virtual_id: 99,
            item_type: PlaylistItemType::Live,
            updated_at: None,
            last_event_at: None,
            window_24h: QosSnapshotWindowDto { score, confidence, ..empty_window() },
            window_7d: empty_window(),
            window_30d: empty_window(),
            daily_buckets: Default::default(),
        }
    }

    #[test]
    fn top_qos_snapshots_orders_by_score_then_confidence() {
        let snapshots = vec![
            make_snapshot("stream-a", "provider-a", 81, 55),
            make_snapshot("stream-b", "provider-b", 81, 80),
            make_snapshot("stream-c", "provider-c", 60, 90),
        ];

        let ranked = top_qos_snapshots(&snapshots, &SearchRequest::Clear, 2);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].stream_identity_key, "stream-b");
        assert_eq!(ranked[1].stream_identity_key, "stream-a");
    }

    #[test]
    fn qos_detail_rows_expand_into_fixed_window_order() {
        let snapshot = QosSnapshotRecordDto {
            stream_identity_key: "stream-a".to_string(),
            input_name: "input-a".to_string(),
            target_name: "target-a".to_string(),
            provider_name: "provider-a".to_string(),
            provider_id: 10,
            virtual_id: 99,
            item_type: PlaylistItemType::Live,
            updated_at: None,
            last_event_at: None,
            window_24h: QosSnapshotWindowDto { score: 90, ..empty_window() },
            window_7d: QosSnapshotWindowDto { score: 70, ..empty_window() },
            window_30d: QosSnapshotWindowDto { score: 50, ..empty_window() },
            daily_buckets: Default::default(),
        };

        let rows = qos_detail_rows(&snapshot);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].window_name, "24h");
        assert_eq!(rows[0].window.score, 90);
        assert_eq!(rows[1].window_name, "7d");
        assert_eq!(rows[1].window.score, 70);
        assert_eq!(rows[2].window_name, "30d");
        assert_eq!(rows[2].window.score, 50);
    }

    #[test]
    fn provider_summary_rows_preserve_summary_values() {
        let summaries = vec![StreamHistoryProviderSummaryDto {
            provider_name: "provider-a".to_string(),
            session_count: 4,
            disconnect_count: 1,
            total_bytes_sent: 2_048,
            avg_session_duration_secs: Some(120),
            avg_first_byte_latency_ms: Some(240),
        }];

        let rows = provider_summary_rows(&summaries);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provider_name, "provider-a");
        assert_eq!(rows[0].session_count, 4);
        assert_eq!(rows[0].disconnect_count, 1);
        assert_eq!(rows[0].total_bytes_sent, 2_048);
    }

    #[test]
    fn qos_score_label_maps_ranges_to_expected_bands() {
        assert_eq!(qos_score_label(90), "strong");
        assert_eq!(qos_score_label(70), "good");
        assert_eq!(qos_score_label(50), "watch");
        assert_eq!(qos_score_label(10), "poor");
    }

    #[test]
    fn optional_record_text_preserves_value_or_dash_fallback() {
        assert_eq!(optional_record_text_str(Some("Lavf53.32.100")), "Lavf53.32.100");
        assert_eq!(optional_record_text_str(Some("live")), "live");
        assert_eq!(optional_record_text_str(None), "-");
    }

    #[test]
    fn request_token_accepts_only_current_response() {
        assert!(is_latest_request(7, 7));
        assert!(!is_latest_request(8, 7));
    }

    #[test]
    fn stream_history_table_items_come_from_current_paged_response_items() {
        let response = PagedResponseDto::new(vec![make_record(10), make_record(11)], 1, 50, 519);

        let rows = stream_history_table_items(Some(&response));

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].session_id, 10);
        assert_eq!(rows[1].session_id, 11);
    }

    #[test]
    fn stream_history_tabs_are_ordered_for_query_first_workflow() {
        assert_eq!(stream_history_tab_ids(), ["stream-history", "qos-snapshot", "summary"]);
    }
}

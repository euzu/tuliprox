use crate::{
    app::components::{DateInput, NoContent, Search, Table, TableDefinition, TextButton},
    hooks::use_service_context,
    i18n::use_translation,
    services::{StreamHistoryProviderSummary, StreamHistoryQosSnapshot, StreamHistoryRecord},
    utils::{format_bytes, format_duration, format_ts},
};
use futures::join;
use shared::model::SearchRequest;
use std::rc::Rc;
use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;

const NUM_COLS: usize = 11;
const SUMMARY_NUM_COLS: usize = 6;
const QOS_SUMMARY_NUM_COLS: usize = 8;
const QOS_DETAIL_NUM_COLS: usize = 8;

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
    target_id: u16,
    item_type: String,
    window_24h: crate::services::StreamHistoryQosSnapshotWindow,
}

#[derive(Clone, PartialEq)]
struct QosDetailRow {
    window_name: String,
    window: crate::services::StreamHistoryQosSnapshotWindow,
}

fn today_start_ts() -> i64 {
    let now = chrono::Utc::now();
    let today = now.date_naive();
    today.and_hms_opt(0, 0, 0).map(|dt| dt.and_utc().timestamp()).unwrap_or(0)
}

fn ts_to_date_str(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0).map_or_else(String::new, |dt| dt.format("%Y-%m-%d").to_string())
}

fn record_details(record: &StreamHistoryRecord) -> String {
    let mut parts = Vec::new();
    if let Some(user_agent) = record.user_agent.as_deref() {
        parts.push(user_agent.to_string());
    }
    if let Some(cluster) = record.cluster.as_deref() {
        parts.push(cluster.to_string());
    }
    if record.shared.unwrap_or(false) {
        parts.push(String::from("shared"));
    }
    if let Some(container) = record.container.as_deref() {
        parts.push(container.to_string());
    }
    if let Some(video_codec) = record.video_codec.as_deref() {
        parts.push(video_codec.to_string());
    }
    if let Some(audio_codec) = record.audio_codec.as_deref() {
        parts.push(audio_codec.to_string());
    }
    if let Some(resolution) = record.resolution.as_deref() {
        parts.push(resolution.to_string());
    }
    if let Some(previous_session_id) = record.previous_session_id {
        parts.push(format!("prev #{previous_session_id}"));
    }
    if parts.is_empty() {
        String::from("-")
    } else {
        parts.join(" | ")
    }
}

fn record_matches(record: &StreamHistoryRecord, filter: &SearchRequest) -> bool {
    match filter {
        SearchRequest::Clear => true,
        SearchRequest::Text(text, _) => {
            let text_lower = text.to_lowercase();
            let fields = [
                record.api_username.as_deref().unwrap_or(""),
                record.title.as_deref().unwrap_or(""),
                record.provider_name.as_deref().unwrap_or(""),
                record.user_agent.as_deref().unwrap_or(""),
                record.cluster.as_deref().unwrap_or(""),
                record.video_codec.as_deref().unwrap_or(""),
                record.audio_codec.as_deref().unwrap_or(""),
                record.resolution.as_deref().unwrap_or(""),
                record.source_addr.as_deref().unwrap_or(""),
                record.disconnect_reason.as_deref().unwrap_or(""),
                record.group.as_deref().unwrap_or(""),
            ];
            fields.iter().any(|f| f.to_lowercase().contains(&text_lower))
        }
        SearchRequest::Regexp(pattern, _) => {
            if let Ok(re) = shared::model::REGEX_CACHE.get_or_compile(pattern) {
                let fields = [
                    record.api_username.as_deref().unwrap_or(""),
                    record.title.as_deref().unwrap_or(""),
                    record.provider_name.as_deref().unwrap_or(""),
                    record.user_agent.as_deref().unwrap_or(""),
                    record.cluster.as_deref().unwrap_or(""),
                    record.video_codec.as_deref().unwrap_or(""),
                    record.audio_codec.as_deref().unwrap_or(""),
                    record.resolution.as_deref().unwrap_or(""),
                    record.source_addr.as_deref().unwrap_or(""),
                    record.disconnect_reason.as_deref().unwrap_or(""),
                    record.group.as_deref().unwrap_or(""),
                ];
                fields.iter().any(|f| re.is_match(f))
            } else {
                false
            }
        }
    }
}

fn qos_snapshot_matches(snapshot: &StreamHistoryQosSnapshot, filter: &SearchRequest) -> bool {
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
                || snapshot.target_id.to_string().contains(&text_lower)
                || snapshot.provider_id.to_string().contains(&text_lower)
                || snapshot.virtual_id.to_string().contains(&text_lower)
        }
        SearchRequest::Regexp(pattern, _) => {
            if let Ok(re) = shared::model::REGEX_CACHE.get_or_compile(pattern) {
                re.is_match(&snapshot.stream_identity_key)
                    || re.is_match(&snapshot.input_name)
                    || re.is_match(&snapshot.provider_name)
                    || re.is_match(&snapshot.item_type)
                    || re.is_match(&snapshot.target_id.to_string())
                    || re.is_match(&snapshot.provider_id.to_string())
                    || re.is_match(&snapshot.virtual_id.to_string())
            } else {
                false
            }
        }
    }
}

fn top_qos_snapshots(
    snapshots: &[StreamHistoryQosSnapshot],
    filter: &SearchRequest,
    limit: usize,
) -> Vec<StreamHistoryQosSnapshot> {
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

fn provider_summary_rows(summaries: &[StreamHistoryProviderSummary]) -> Vec<ProviderSummaryRow> {
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

fn qos_summary_rows(snapshots: &[StreamHistoryQosSnapshot]) -> Vec<QosSummaryRow> {
    snapshots
        .iter()
        .map(|snapshot| QosSummaryRow {
            stream_identity_key: snapshot.stream_identity_key.clone(),
            input_name: snapshot.input_name.clone(),
            provider_name: snapshot.provider_name.clone(),
            provider_id: snapshot.provider_id,
            target_id: snapshot.target_id,
            item_type: snapshot.item_type.clone(),
            window_24h: snapshot.window_24h.clone(),
        })
        .collect()
}

fn qos_detail_rows(snapshot: &StreamHistoryQosSnapshot) -> Vec<QosDetailRow> {
    [("24h", snapshot.window_24h.clone()), ("7d", snapshot.window_7d.clone()), ("30d", snapshot.window_30d.clone())]
        .into_iter()
        .map(|(window_name, window)| QosDetailRow { window_name: window_name.to_string(), window })
        .collect()
}

fn has_any_stream_history_content(
    records: &[Rc<StreamHistoryRecord>],
    summaries: &[StreamHistoryProviderSummary],
    qos_snapshots: &[StreamHistoryQosSnapshot],
) -> bool {
    !records.is_empty() || !summaries.is_empty() || !qos_snapshots.is_empty()
}

#[component]
pub fn StreamHistoryView() -> Html {
    let services = use_service_context();
    let translate = use_translation();
    let from_date = use_state(|| Some(today_start_ts()));
    let to_date = use_state(|| Some(today_start_ts()));
    let all_records = use_state(Vec::<Rc<StreamHistoryRecord>>::new);
    let summaries = use_state(Vec::<StreamHistoryProviderSummary>::new);
    let qos_snapshots = use_state(Vec::<StreamHistoryQosSnapshot>::new);
    let selected_qos_snapshot = use_state(|| None::<StreamHistoryQosSnapshot>);
    let search_filter = use_state(|| SearchRequest::Clear);
    let loading = use_state(|| false);

    // Load on mount with default date range (today)
    {
        let services = services.clone();
        let all_records = all_records.clone();
        let summaries = summaries.clone();
        let qos_snapshots = qos_snapshots.clone();
        let selected_qos_snapshot = selected_qos_snapshot.clone();
        let loading = loading.clone();
        let from = *from_date;
        let to = *to_date;
        use_effect_with((), move |_| {
            let from_str = from.map(ts_to_date_str);
            let to_str = to.map(ts_to_date_str);
            loading.set(true);
            spawn_local(async move {
                let (history_result, summary_result, qos_result) = join!(
                    services.stream_history.get_history(from_str.as_deref(), to_str.as_deref()),
                    services.stream_history.get_summary(from_str.as_deref(), to_str.as_deref()),
                    services.stream_history.get_qos_snapshots()
                );
                match history_result {
                    Ok(Some(records)) => all_records.set(records.into_iter().map(Rc::new).collect()),
                    Ok(None) | Err(_) => all_records.set(Vec::new()),
                }
                match summary_result {
                    Ok(Some(items)) => summaries.set(items),
                    Ok(None) | Err(_) => summaries.set(Vec::new()),
                }
                match qos_result {
                    Ok(Some(items)) => {
                        selected_qos_snapshot.set(None);
                        qos_snapshots.set(items);
                    }
                    Ok(None) | Err(_) => {
                        selected_qos_snapshot.set(None);
                        qos_snapshots.set(Vec::new());
                    }
                }
                loading.set(false);
            });
            || ()
        });
    }

    let handle_from_change = {
        let from_date = from_date.clone();
        Callback::from(move |ts: Option<i64>| from_date.set(ts))
    };

    let handle_to_change = {
        let to_date = to_date.clone();
        Callback::from(move |ts: Option<i64>| to_date.set(ts))
    };

    let handle_load = {
        let services = services.clone();
        let from_date = from_date.clone();
        let to_date = to_date.clone();
        let all_records = all_records.clone();
        let summaries = summaries.clone();
        let qos_snapshots = qos_snapshots.clone();
        let selected_qos_snapshot = selected_qos_snapshot.clone();
        let loading = loading.clone();
        Callback::from(move |_: String| {
            let services = services.clone();
            let all_records = all_records.clone();
            let summaries = summaries.clone();
            let qos_snapshots = qos_snapshots.clone();
            let selected_qos_snapshot = selected_qos_snapshot.clone();
            let loading = loading.clone();
            let from_str = (*from_date).map(ts_to_date_str);
            let to_str = (*to_date).map(ts_to_date_str);
            loading.set(true);
            spawn_local(async move {
                let (history_result, summary_result, qos_result) = join!(
                    services.stream_history.get_history(from_str.as_deref(), to_str.as_deref()),
                    services.stream_history.get_summary(from_str.as_deref(), to_str.as_deref()),
                    services.stream_history.get_qos_snapshots()
                );
                match history_result {
                    Ok(Some(records)) => all_records.set(records.into_iter().map(Rc::new).collect()),
                    Ok(None) | Err(_) => all_records.set(Vec::new()),
                }
                match summary_result {
                    Ok(Some(items)) => summaries.set(items),
                    Ok(None) | Err(_) => summaries.set(Vec::new()),
                }
                match qos_result {
                    Ok(Some(items)) => {
                        selected_qos_snapshot.set(None);
                        qos_snapshots.set(items);
                    }
                    Ok(None) | Err(_) => {
                        selected_qos_snapshot.set(None);
                        qos_snapshots.set(Vec::new());
                    }
                }
                loading.set(false);
            });
        })
    };

    let handle_search = {
        let search_filter = search_filter.clone();
        Callback::from(move |req: SearchRequest| search_filter.set(req))
    };

    let filtered: Rc<Vec<Rc<StreamHistoryRecord>>> =
        use_memo(((*all_records).clone(), (*search_filter).clone()), |(records, filter)| {
            records.iter().filter(|r| record_matches(r, filter)).cloned().collect::<Vec<_>>()
        });
    let has_any_content = has_any_stream_history_content(&all_records, &summaries, &qos_snapshots);

    let visible_qos_snapshots: Rc<Vec<StreamHistoryQosSnapshot>> =
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
    let table_def: Rc<TableDefinition<StreamHistoryRecord>> = use_memo(filtered.clone(), move |filtered| {
        let translate = translate_for_table.clone();
        TableDefinition {
            items: Some(filtered.clone()),
            num_cols: NUM_COLS,
            is_sortable: Callback::from(|_| false),
            on_sort: Callback::noop(),
            render_header_cell: Callback::from(move |col: usize| {
                let label = match col {
                    0 => translate.t("LABEL.STREAM_HISTORY_TIME"),
                    1 => translate.t("LABEL.STREAM_HISTORY_EVENT"),
                    2 => translate.t("LABEL.USERNAME"),
                    3 => translate.t("LABEL.TITLE"),
                    4 => translate.t("LABEL.PROVIDER"),
                    5 => translate.t("LABEL.DURATION"),
                    6 => translate.t("LABEL.STREAM_HISTORY_BYTES"),
                    7 => translate.t("LABEL.STREAM_HISTORY_FIRST_BYTE"),
                    8 => translate.t("LABEL.STREAM_HISTORY_DETAILS"),
                    9 => translate.t("LABEL.STREAM_HISTORY_REASON"),
                    10 => translate.t("LABEL.STREAM_HISTORY_IP"),
                    _ => String::new(),
                };
                html! { <span>{label}</span> }
            }),
            render_data_cell: Callback::from(|(_, col, record): (usize, usize, Rc<StreamHistoryRecord>)| match col {
                0 => html! { <span class="tp__stream-history__cell--time">{format_ts(record.event_ts_utc)}</span> },
                1 => {
                    let is_connect = record.event_type == "connect";
                    let badge_class = if is_connect {
                        "tp__stream-history__badge tp__stream-history__badge--connect"
                    } else {
                        "tp__stream-history__badge tp__stream-history__badge--disconnect"
                    };
                    html! { <span class={badge_class}>{record.event_type.clone()}</span> }
                }
                2 => html! { <span>{record.api_username.as_deref().unwrap_or("-")}</span> },
                3 => {
                    html! { <span class="tp__stream-history__cell--title">{record.title.as_deref().unwrap_or("-")}</span> }
                }
                4 => html! {
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
                5 => html! {
                    <span class="tp__stream-history__cell--mono">
                        {record.session_duration.map(format_duration).unwrap_or_default()}
                    </span>
                },
                6 => html! {
                    <span class="tp__stream-history__cell--mono">
                        {record.bytes_sent.map(format_bytes).unwrap_or_default()}
                    </span>
                },
                7 => html! {
                    <span class="tp__stream-history__cell--mono">
                        {record.first_byte_latency_ms.map(|v| v.to_string()).unwrap_or_default()}
                    </span>
                },
                8 => html! {
                    <span>
                        {record_details(&record)}
                    </span>
                },
                9 => html! {
                    <span>
                        {record.disconnect_reason.as_deref().unwrap_or("-").replace('_', " ")}
                    </span>
                },
                10 => html! {
                    <span class="tp__stream-history__cell--ip">
                        {record.source_addr.as_deref().unwrap_or("-")}
                    </span>
                },
                _ => html! {},
            }),
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
                    2 => html! { <span>{row.target_id}</span> },
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
                <Search onsearch={Some(handle_search)} min_length={1} />
            </div>
            <div class="tp__stream-history__body">
                if *loading {
                    <div class="tp__stream-history__loading">
                        <span>{translate.t("LABEL.STREAM_HISTORY_LOADING")}</span>
                    </div>
                } else if !has_any_content {
                    <NoContent />
                } else {
                    <div class="tp__stream-history__summary">
                        <h2>{translate.t("LABEL.STREAM_HISTORY_SUMMARY")}</h2>
                        <Table::<ProviderSummaryRow> definition={summary_table_def.clone()} />
                    </div>
                    <div class="tp__stream-history__summary">
                        <h2>{translate.t("LABEL.STREAM_HISTORY_QOS")}</h2>
                        <Table::<QosSummaryRow> definition={qos_summary_table_def.clone()} />
                    </div>
                    if (*selected_qos_snapshot).is_some() {
                        <div class="tp__stream-history__summary">
                            <h2>{translate.t("LABEL.STREAM_HISTORY_QOS_DETAIL")}</h2>
                            <Table::<QosDetailRow> definition={qos_detail_table_def.clone()} />
                        </div>
                    }
                    <div class="tp__stream-history__summary">
                        <h2>{translate.t("LABEL.STREAM_HISTORY")}</h2>
                        <Table::<StreamHistoryRecord> definition={table_def} />
                    </div>
                }
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::{provider_summary_rows, qos_detail_rows, qos_score_label, top_qos_snapshots};
    use crate::services::{StreamHistoryProviderSummary, StreamHistoryQosSnapshot, StreamHistoryQosSnapshotWindow};
    use shared::model::SearchRequest;

    fn empty_window() -> StreamHistoryQosSnapshotWindow {
        StreamHistoryQosSnapshotWindow {
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
        }
    }

    fn make_snapshot(
        stream_identity_key: &str,
        provider_name: &str,
        score: u8,
        confidence: u8,
    ) -> StreamHistoryQosSnapshot {
        StreamHistoryQosSnapshot {
            stream_identity_key: stream_identity_key.to_string(),
            input_name: "input-a".to_string(),
            target_id: 1,
            provider_name: provider_name.to_string(),
            provider_id: 10,
            virtual_id: 99,
            item_type: "live".to_string(),
            updated_at: None,
            last_event_at: None,
            window_24h: StreamHistoryQosSnapshotWindow { score, confidence, ..empty_window() },
            window_7d: empty_window(),
            window_30d: empty_window(),
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
        let snapshot = StreamHistoryQosSnapshot {
            stream_identity_key: "stream-a".to_string(),
            input_name: "input-a".to_string(),
            target_id: 1,
            provider_name: "provider-a".to_string(),
            provider_id: 10,
            virtual_id: 99,
            item_type: "live".to_string(),
            updated_at: None,
            last_event_at: None,
            window_24h: StreamHistoryQosSnapshotWindow { score: 90, ..empty_window() },
            window_7d: StreamHistoryQosSnapshotWindow { score: 70, ..empty_window() },
            window_30d: StreamHistoryQosSnapshotWindow { score: 50, ..empty_window() },
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
        let summaries = vec![StreamHistoryProviderSummary {
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
}

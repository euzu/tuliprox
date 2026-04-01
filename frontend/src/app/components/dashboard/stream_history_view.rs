use crate::{
    app::components::{DateInput, Search, Table, TableDefinition, TextButton},
    hooks::use_service_context,
    i18n::use_translation,
    services::{StreamHistoryProviderSummary, StreamHistoryRecord},
    utils::{format_bytes, format_duration, format_ts},
};
use shared::model::SearchRequest;
use std::rc::Rc;
use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;

const NUM_COLS: usize = 11;

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

#[component]
pub fn StreamHistoryView() -> Html {
    let services = use_service_context();
    let translate = use_translation();
    let from_date = use_state(|| Some(today_start_ts()));
    let to_date = use_state(|| Some(today_start_ts()));
    let all_records = use_state(Vec::<Rc<StreamHistoryRecord>>::new);
    let summaries = use_state(Vec::<StreamHistoryProviderSummary>::new);
    let search_filter = use_state(|| SearchRequest::Clear);
    let loading = use_state(|| false);

    // Load on mount with default date range (today)
    {
        let services = services.clone();
        let all_records = all_records.clone();
        let summaries = summaries.clone();
        let loading = loading.clone();
        let from = *from_date;
        let to = *to_date;
        use_effect_with((), move |_| {
            let from_str = from.map(ts_to_date_str);
            let to_str = to.map(ts_to_date_str);
            loading.set(true);
            spawn_local(async move {
                let history_result = services.stream_history.get_history(from_str.as_deref(), to_str.as_deref()).await;
                let summary_result = services.stream_history.get_summary(from_str.as_deref(), to_str.as_deref()).await;
                match history_result {
                    Ok(Some(records)) => all_records.set(records.into_iter().map(Rc::new).collect()),
                    Ok(None) | Err(_) => all_records.set(Vec::new()),
                }
                match summary_result {
                    Ok(Some(items)) => summaries.set(items),
                    Ok(None) | Err(_) => summaries.set(Vec::new()),
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
        let loading = loading.clone();
        Callback::from(move |_: String| {
            let services = services.clone();
            let all_records = all_records.clone();
            let summaries = summaries.clone();
            let loading = loading.clone();
            let from_str = (*from_date).map(ts_to_date_str);
            let to_str = (*to_date).map(ts_to_date_str);
            loading.set(true);
            spawn_local(async move {
                let history_result = services.stream_history.get_history(from_str.as_deref(), to_str.as_deref()).await;
                let summary_result = services.stream_history.get_summary(from_str.as_deref(), to_str.as_deref()).await;
                match history_result {
                    Ok(Some(records)) => all_records.set(records.into_iter().map(Rc::new).collect()),
                    Ok(None) | Err(_) => all_records.set(Vec::new()),
                }
                match summary_result {
                    Ok(Some(items)) => summaries.set(items),
                    Ok(None) | Err(_) => summaries.set(Vec::new()),
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
                } else {
                    <div class="tp__stream-history__summary">
                        <h2>{translate.t("LABEL.STREAM_HISTORY_SUMMARY")}</h2>
                        <table class="tp__stream-history__summary-table">
                            <thead>
                                <tr>
                                    <th>{translate.t("LABEL.PROVIDER")}</th>
                                    <th>{translate.t("LABEL.STREAM_HISTORY_SESSIONS")}</th>
                                    <th>{translate.t("LABEL.STREAM_HISTORY_BYTES")}</th>
                                    <th>{translate.t("LABEL.DURATION")}</th>
                                    <th>{translate.t("LABEL.STREAM_HISTORY_FIRST_BYTE")}</th>
                                    <th>{translate.t("LABEL.STREAM_HISTORY_DISCONNECTS")}</th>
                                </tr>
                            </thead>
                            <tbody>
                                {for summaries.iter().map(|summary| html! {
                                    <tr>
                                        <td>{summary.provider_name.clone()}</td>
                                        <td>{summary.session_count}</td>
                                        <td>{format_bytes(summary.total_bytes_sent)}</td>
                                        <td>{summary.avg_session_duration_secs.map(format_duration).unwrap_or_default()}</td>
                                        <td>{summary.avg_first_byte_latency_ms.map(|v| format!("{v} ms")).unwrap_or_default()}</td>
                                        <td>{summary.disconnect_count}</td>
                                    </tr>
                                })}
                            </tbody>
                        </table>
                    </div>
                    <Table::<StreamHistoryRecord> definition={table_def} />
                }
            </div>
        </div>
    }
}

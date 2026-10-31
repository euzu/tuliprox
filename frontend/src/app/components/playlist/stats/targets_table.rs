use crate::{
    app::components::{make_translated_header_callback, Card, Table, TableDefinition},
    i18n::use_translation,
    model::{
        classify_target_status, format_number, format_relative_time, percent, relative_time, TargetStatus,
        UiTargetAnalyticsEntry,
    },
};
use shared::model::{EpgAnalytics, TargetType};
use std::rc::Rc;
use yew::prelude::*;

crate::app::components::define_table_columns! {
    enum TargetStatsColumn {
        Target => ("target", "LABEL.TARGET") { can_hide: false },
        Output => ("output", "LABEL.PLAYLIST_STATS_OUTPUT"),
        Live => ("live", "LABEL.LIVE"),
        Movies => ("movies", "LABEL.PLAYLIST_STATS_MOVIES"),
        Series => ("series", "LABEL.SERIES"),
        Episodes => ("episodes", "LABEL.PLAYLIST_STATS_EPISODES_HEADER"),
        Epg => ("epg", "LABEL.PLAYLIST_EPG"),
        Updated => ("updated", "LABEL.PLAYLIST_STATS_LAST_UPDATE"),
        Status => ("status", "LABEL.STATUS"),
    }
}

const DASH: &str = "—";

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct TargetsTableProps {
    pub targets: Rc<Vec<Rc<UiTargetAnalyticsEntry>>>,
    pub selected_target_id: Option<u16>,
    pub on_select: Callback<u16>,
    pub now: u64,
}

/// Movies and series are only meaningful for Xtream outputs when the target has none.
fn cluster_count(count: usize, has_xtream: bool) -> String {
    if count == 0 && !has_xtream {
        DASH.to_string()
    } else {
        format_number(count)
    }
}

fn number_cell(value: String, title: Option<String>) -> Html {
    html! { <span class="tp__playlist-stats__table-number" title={title}>{value}</span> }
}

fn epg_cell(epg: Option<&EpgAnalytics>, status: TargetStatus) -> Html {
    let Some(pct) = epg.and_then(|e| percent(e.mapped, e.live_channels)) else {
        return html! { DASH };
    };
    let color = if status == TargetStatus::EpgStale {
        "var(--playlist-stats-warn-color)"
    } else {
        "var(--playlist-stats-epg-color)"
    };
    let fill_style = format!("width: {pct:.1}%; background-color: {color};");
    html! {
        <div class="tp__playlist-stats__table-epg-bar">
            <div class="track"><div class="fill" style={fill_style} /></div>
            <span class="pct">{format!("{pct:.0}%")}</span>
        </div>
    }
}

#[component]
pub fn TargetsTable(props: &TargetsTableProps) -> Html {
    let translate = use_translation();
    let columns = use_memo((), |()| TargetStatsColumn::columns());

    let render_header_cell = make_translated_header_callback(translate.clone(), |index| {
        TargetStatsColumn::from_index(index).map(TargetStatsColumn::header_label)
    });

    let render_data_cell = {
        let translate = translate.clone();
        let now = props.now;
        Callback::<(usize, usize, Rc<UiTargetAnalyticsEntry>), Html>::from(
            move |(_, col, entry): (usize, usize, Rc<UiTargetAnalyticsEntry>)| {
                let analytics = entry.analytics.as_deref();
                let has_xtream = entry.outputs.contains(&TargetType::Xtream);
                match TargetStatsColumn::from_index(col) {
                    Some(TargetStatsColumn::Target) => {
                        html! { <span class="tp__playlist-stats__table-target">{&entry.name}</span> }
                    }
                    Some(TargetStatsColumn::Output) => {
                        let outputs = entry.outputs.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
                        html! { { if outputs.is_empty() { DASH.to_string() } else { outputs } } }
                    }
                    Some(TargetStatsColumn::Live) => {
                        number_cell(analytics.map_or_else(|| DASH.to_string(), |a| format_number(a.live.count)), None)
                    }
                    Some(TargetStatsColumn::Movies) => number_cell(
                        analytics.map_or_else(|| DASH.to_string(), |a| cluster_count(a.vod.count, has_xtream)),
                        None,
                    ),
                    Some(TargetStatsColumn::Series) => number_cell(
                        analytics.map_or_else(|| DASH.to_string(), |a| cluster_count(a.series.count, has_xtream)),
                        None,
                    ),
                    Some(TargetStatsColumn::Episodes) => {
                        let (value, title) = analytics.and_then(|a| a.episodes.map(|count| (a, count))).map_or_else(
                            || (DASH.to_string(), None),
                            |(a, count)| {
                                if a.episodes_complete {
                                    (format_number(count), None)
                                } else {
                                    (
                                        format!("{}+", format_number(count)),
                                        Some(translate.t("LABEL.PLAYLIST_STATS_PARTIALLY_RESOLVED")),
                                    )
                                }
                            },
                        );
                        number_cell(value, title)
                    }
                    Some(TargetStatsColumn::Epg) => {
                        let status = classify_target_status(analytics, now as i64);
                        epg_cell(analytics.and_then(|a| a.epg.as_ref()), status)
                    }
                    Some(TargetStatsColumn::Updated) => html! {
                        { analytics.map_or_else(
                            || DASH.to_string(),
                            |a| format_relative_time(relative_time(a.updated_at, now), &translate),
                        ) }
                    },
                    Some(TargetStatsColumn::Status) => {
                        let status = classify_target_status(analytics, now as i64);
                        html! {
                            <span class={status.badge_class()}>{translate.t(status.label_key())}</span>
                        }
                    }
                    None => Html::default(),
                }
            },
        )
    };

    let table_definition = {
        let columns = columns.clone();
        use_memo((props.targets.clone(), props.selected_target_id, props.now), move |(targets, _, _)| {
            Rc::new(TableDefinition::<UiTargetAnalyticsEntry> {
                table_id: "playlist.stats.targets".into(),
                columns: columns.clone(),
                row_key: Callback::from(|(_, item): (usize, Rc<UiTargetAnalyticsEntry>)| {
                    item.target_id.to_string().into()
                }),
                items: (!targets.is_empty()).then(|| targets.clone()),
                on_sort: Callback::noop(),
                render_header_cell,
                render_data_cell,
            })
        })
    };

    let on_row_click = {
        let on_select = props.on_select.clone();
        Callback::from(move |entry: Rc<UiTargetAnalyticsEntry>| on_select.emit(entry.target_id))
    };

    html! {
        <Card class="tp__playlist-stats__table-card">
            <div class="tp__playlist-stats__table-card-header">
                <h2>{translate.t("LABEL.PLAYLIST_STATS_ALL_TARGETS")}</h2>
                <span>{translate.t("LABEL.PLAYLIST_STATS_ALL_TARGETS_SUBTITLE")}</span>
            </div>
            <Table<UiTargetAnalyticsEntry>
                definition={(*table_definition).clone()}
                on_row_click={on_row_click}
                selected_row_key={props.selected_target_id.map(|id| AttrValue::from(id.to_string()))}
            />
        </Card>
    }
}

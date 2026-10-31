use crate::{
    app::components::{
        playlist::stats::{bar_row::BarRow, emphasize},
        Card,
    },
    i18n::use_translation,
    model::{format_number, percent, t_fmt_split},
};
use shared::model::TargetAnalyticsDto;
use std::rc::Rc;
use yew::prelude::*;

#[derive(Clone, PartialEq, Properties)]
pub struct InputOutputPanelProps {
    pub analytics: Rc<TargetAnalyticsDto>,
}

#[component]
pub fn InputOutputPanel(props: &InputOutputPanelProps) -> Html {
    let translate = use_translation();
    let a = &props.analytics;

    let live_pct = percent(a.live.count, a.live.input_count);
    let live_kept = format_number(a.live.count);
    let live_total = format_number(a.live.input_count);
    let live_pct_str = format!("{:.1}", live_pct.unwrap_or(0.0));
    let live_meta = emphasize(t_fmt_split(
        &translate,
        "LABEL.PLAYLIST_STATS_KEPT_RATIO",
        ("kept", &live_kept),
        &[("total", &live_total), ("percent", &live_pct_str)],
    ));

    let vod_pct = percent(a.vod.count, a.vod.input_count);
    let vod_kept = format_number(a.vod.count);
    let vod_total = format_number(a.vod.input_count);
    let vod_pct_str = format!("{:.1}", vod_pct.unwrap_or(0.0));
    let vod_meta = emphasize(t_fmt_split(
        &translate,
        "LABEL.PLAYLIST_STATS_KEPT_RATIO",
        ("kept", &vod_kept),
        &[("total", &vod_total), ("percent", &vod_pct_str)],
    ));

    let series_pct = percent(a.series.count, a.series.input_count);
    let series_kept = format_number(a.series.count);
    let series_total = format_number(a.series.input_count);
    let series_pct_str = format!("{:.1}", series_pct.unwrap_or(0.0));
    let series_meta = emphasize(t_fmt_split(
        &translate,
        "LABEL.PLAYLIST_STATS_KEPT_RATIO",
        ("kept", &series_kept),
        &[("total", &series_total), ("percent", &series_pct_str)],
    ));

    let max_live_group_count = a.top_live_groups.iter().map(|g| g.count).max().unwrap_or(1);

    html! {
        <Card class="tp__playlist-stats__panel-card">
            <div class="tp__playlist-stats__panel-card-header">
                <h2>{translate.t("LABEL.PLAYLIST_STATS_INPUT_VS_OUTPUT")}</h2>
                <span>{translate.t("LABEL.PLAYLIST_STATS_INPUT_VS_OUTPUT_SUBTITLE")}</span>
            </div>
            <div class="tp__playlist-stats__bar-section">
                <BarRow
                    label={translate.t("LABEL.LIVE")}
                    meta={live_meta}
                    percent={live_pct}
                    color_var="--playlist-stats-live-color"
                />
                <BarRow
                    label={translate.t("LABEL.PLAYLIST_STATS_MOVIES")}
                    meta={vod_meta}
                    percent={vod_pct}
                    color_var="--playlist-stats-vod-color"
                />
                <BarRow
                    label={translate.t("LABEL.SERIES")}
                    meta={series_meta}
                    percent={series_pct}
                    color_var="--playlist-stats-series-color"
                />
            </div>
            if !a.top_live_groups.is_empty() {
                <div class="tp__playlist-stats__live-groups">
                    <span class="tp__playlist-stats__live-groups-title">
                        {translate.t("LABEL.PLAYLIST_STATS_LARGEST_LIVE_GROUPS")}
                    </span>
                    <div class="tp__playlist-stats__live-groups-list">
                    { for a.top_live_groups.iter().map(|g| {
                        let pct = (g.count as f64 / max_live_group_count as f64 * 100.0).clamp(0.0, 100.0);
                        let fill_style = format!("width: {pct:.1}%;");
                        html! {
                            <div class="tp__playlist-stats__live-groups-row" key={g.name.clone()}>
                                <span class="name" title={g.name.clone()}>{&g.name}</span>
                                <div class="track">
                                    <div class="fill" style={fill_style} />
                                </div>
                                <span class="count">{format_number(g.count)}</span>
                            </div>
                        }
                    })}
                    </div>
                </div>
            }
        </Card>
    }
}

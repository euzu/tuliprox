use crate::{
    app::components::playlist::stats::kpi_card::KpiCard,
    i18n::use_translation,
    model::{format_number, percent, t_fmt},
};
use shared::model::TargetAnalyticsDto;
use std::rc::Rc;
use yew::prelude::*;

#[derive(Clone, PartialEq, Properties)]
pub struct KpiRowProps {
    pub analytics: Rc<TargetAnalyticsDto>,
    pub has_xtream: bool,
}

#[component]
pub fn KpiRow(props: &KpiRowProps) -> Html {
    let translate = use_translation();
    let a = &props.analytics;

    // KPI 1: Live channels
    let groups_str = format_number(a.live.groups);
    let live_subtext = t_fmt(&translate, "LABEL.PLAYLIST_STATS_GROUPS", &[("count", &groups_str)]);
    let kpi_live = html! {
        <KpiCard
            label={translate.t("LABEL.PLAYLIST_STATS_LIVE_CHANNELS")}
            value={format_number(a.live.count)}
            subtext={Some(live_subtext)}
            color_var="--playlist-stats-live-color"
        />
    };

    // KPI 2: Movies
    let (movies_val, movies_sub) = if a.vod.count == 0 && !props.has_xtream {
        ("—".to_string(), None)
    } else {
        let vod_groups = format_number(a.vod.groups);
        (
            format_number(a.vod.count),
            Some(t_fmt(&translate, "LABEL.PLAYLIST_STATS_CATEGORIES", &[("count", &vod_groups)])),
        )
    };
    let kpi_movies = html! {
        <KpiCard
            label={translate.t("LABEL.PLAYLIST_STATS_MOVIES")}
            value={movies_val}
            subtext={movies_sub}
            color_var="--playlist-stats-vod-color"
        />
    };

    // KPI 3: Series
    let (series_val, series_sub, series_sub_title) = if a.series.count == 0 && !props.has_xtream {
        ("—".to_string(), None, None)
    } else {
        let sub = a.episodes.map_or_else(
            || ("—".to_string(), Some(translate.t("LABEL.PLAYLIST_STATS_NOT_RESOLVED"))),
            |ep| {
                let ep_str = format_number(ep);
                if a.episodes_complete {
                    (t_fmt(&translate, "LABEL.PLAYLIST_STATS_EPISODES", &[("count", &ep_str)]), None)
                } else {
                    (
                        t_fmt(&translate, "LABEL.PLAYLIST_STATS_EPISODES_PARTIAL", &[("count", &ep_str)]),
                        Some(translate.t("LABEL.PLAYLIST_STATS_PARTIALLY_RESOLVED")),
                    )
                }
            },
        );
        (format_number(a.series.count), Some(sub.0), sub.1)
    };
    let kpi_series = html! {
        <KpiCard
            label={translate.t("LABEL.PLAYLIST_STATS_SERIES")}
            value={series_val}
            subtext={series_sub}
            subtext_title={series_sub_title}
            color_var="--playlist-stats-series-color"
        />
    };

    // KPI 4: EPG coverage
    let (epg_val, epg_sub) = if let Some(ref e) = a.epg {
        if let Some(pct) = percent(e.mapped, e.live_channels) {
            let mapped_str = format_number(e.mapped);
            (
                format!("{pct:.1}%"),
                Some(t_fmt(&translate, "LABEL.PLAYLIST_STATS_CHANNELS_MAPPED", &[("count", &mapped_str)])),
            )
        } else {
            ("—".to_string(), Some(t_fmt(&translate, "LABEL.PLAYLIST_STATS_CHANNELS_MAPPED", &[("count", "0")])))
        }
    } else {
        ("—".to_string(), Some(translate.t("LABEL.PLAYLIST_STATS_NO_EPG_CONFIGURED")))
    };
    let kpi_epg = html! {
        <KpiCard
            label={translate.t("LABEL.PLAYLIST_STATS_EPG_COVERAGE")}
            value={epg_val}
            subtext={epg_sub}
            color_var="--playlist-stats-epg-color"
        />
    };

    // KPI 5: Catch-up channels
    let catchup_sub = if a.catchup_channels == 0 {
        translate.t("LABEL.PLAYLIST_STATS_ARCHIVE_NONE")
    } else if let Some(days) = a.catchup_max_days {
        let days_str = days.to_string();
        t_fmt(&translate, "LABEL.PLAYLIST_STATS_ARCHIVE_DAYS", &[("days", &days_str)])
    } else {
        translate.t("LABEL.PLAYLIST_STATS_ARCHIVE_NONE")
    };
    let kpi_catchup = html! {
        <KpiCard
            label={translate.t("LABEL.PLAYLIST_STATS_CATCHUP_CHANNELS")}
            value={format_number(a.catchup_channels)}
            subtext={Some(catchup_sub)}
            color_var="--playlist-stats-catchup-color"
        />
    };

    html! {
        <div class="tp__playlist-stats__kpis">
            {kpi_live}
            {kpi_movies}
            {kpi_series}
            {kpi_epg}
            {kpi_catchup}
        </div>
    }
}

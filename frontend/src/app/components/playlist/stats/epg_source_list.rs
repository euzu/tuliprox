use crate::{
    i18n::YewI18n,
    model::{format_number, format_relative_time, relative_time, t_fmt},
};
use shared::model::EpgSourceAnalytics;
use yew::prelude::*;

/// Plain render function (no component) so the source rows are borrowed, not cloned into props.
pub fn epg_source_list(sources: &[EpgSourceAnalytics], now: u64, translate: &YewI18n) -> Html {
    html! {
        <div class="tp__playlist-stats__source-list">
            { for sources.iter().map(|src| {
                let is_failed = src.failed_at.is_some();
                let kind = match src.kind {
                    shared::model::EpgSourceTypeDto::Xmltv => "XMLTV",
                    shared::model::EpgSourceTypeDto::Ics => "ICS",
                };

                let meta_text = if let Some(failed_ts) = src.failed_at {
                    let rel = relative_time(failed_ts, now);
                    let time_str = format_relative_time(rel, translate);
                    let failed_part =
                        t_fmt(translate, "LABEL.PLAYLIST_STATS_LAST_FETCH_FAILED", &[("time", &time_str)]);
                    format!("{kind} · {failed_part}")
                } else {
                    let days = src.guide_days.unwrap_or(0);
                    let days_str = if days == 0 {
                        translate.t("LABEL.PLAYLIST_STATS_GUIDE_ENDED")
                    } else {
                        t_fmt(translate, "LABEL.PLAYLIST_STATS_GUIDE_DAYS", &[("days", &days.to_string())])
                    };
                    let fetched_ts = src.fetched_at.unwrap_or(now);
                    let rel = relative_time(fetched_ts, now);
                    let time_str = format_relative_time(rel, translate);
                    let fetched_part = t_fmt(translate, "LABEL.PLAYLIST_STATS_FETCHED_AGO", &[("time", &time_str)]);
                    format!("{kind} · {days_str} · {fetched_part}")
                };

                let badge = if is_failed {
                    let tooltip = src.error.as_deref().unwrap_or("Fetch failed");
                    html! {
                        <span class="tp__playlist-stats__badge tp__playlist-stats__badge--error" title={tooltip.to_string()}>
                            {translate.t("LABEL.PLAYLIST_STATS_STATUS_ERROR")}
                        </span>
                    }
                } else {
                    html! {
                        <span class="tp__playlist-stats__badge tp__playlist-stats__badge--ok">
                            {translate.t("LABEL.PLAYLIST_STATS_STATUS_OK")}
                        </span>
                    }
                };

                html! {
                    <div class="tp__playlist-stats__source-list-row" key={format!("{}/{}", src.input, src.label)}>
                        <div class="source-info">
                            <span class="source-name">{&src.label}</span>
                            <span class="source-meta">{meta_text}</span>
                        </div>
                        <div class="source-stats">
                            <span class="mapped-count">{format_number(src.mapped_channels)}</span>
                            {badge}
                        </div>
                    </div>
                }
            })}
        </div>
    }
}

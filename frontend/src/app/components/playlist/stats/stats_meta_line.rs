use crate::{
    i18n::use_translation,
    model::{format_relative_time, format_scheduled_time, relative_time, t_fmt},
};
use shared::model::{TargetAnalyticsDto, TargetType};
use std::rc::Rc;
use yew::prelude::*;

#[derive(Clone, PartialEq, Properties)]
pub struct StatsMetaLineProps {
    pub outputs: Rc<Vec<TargetType>>,
    pub analytics: Option<Rc<TargetAnalyticsDto>>,
    pub next_scheduled: Option<i64>,
    pub now: u64,
}

#[component]
pub fn StatsMetaLine(props: &StatsMetaLineProps) -> Html {
    let translate = use_translation();
    let outputs_str = if props.outputs.is_empty() {
        "—".to_string()
    } else {
        props.outputs.iter().map(std::string::ToString::to_string).collect::<Vec<_>>().join(", ")
    };

    if let Some(ref a) = props.analytics {
        let inputs_str = if a.inputs.is_empty() { "—".to_string() } else { a.inputs.join(", ") };
        let rel = relative_time(a.updated_at, props.now);
        let time_str = format_relative_time(rel, &translate);
        let took_str = t_fmt(&translate, "LABEL.PLAYLIST_STATS_TOOK_SECS", &[("secs", &a.took_secs.to_string())]);
        let last_update_str = format!("{time_str} {took_str}");

        let next_scheduled_str = props
            .next_scheduled
            .map_or_else(|| "—".to_string(), |next_ts| format_scheduled_time(next_ts, props.now as i64));

        html! {
            <div class="tp__playlist-stats__meta">
                <span class="tp__playlist-stats__meta-item">
                    {translate.t("LABEL.PLAYLIST_STATS_OUTPUT")}{": "}<strong>{outputs_str}</strong>
                </span>
                <span class="tp__playlist-stats__meta-item">
                    {translate.t("LABEL.PLAYLIST_STATS_INPUTS")}{": "}<strong>{inputs_str}</strong>
                </span>
                <span class="tp__playlist-stats__meta-item">
                    {translate.t("LABEL.PLAYLIST_STATS_LAST_UPDATE")}{": "}<strong>{last_update_str}</strong>
                </span>
                <span class="tp__playlist-stats__meta-item">
                    {translate.t("LABEL.PLAYLIST_STATS_NEXT_SCHEDULED")}{": "}<strong>{next_scheduled_str}</strong>
                </span>
            </div>
        }
    } else {
        html! {
            <div class="tp__playlist-stats__meta">
                <span class="tp__playlist-stats__meta-item">
                    {translate.t("LABEL.PLAYLIST_STATS_OUTPUT")}{": "}<strong>{outputs_str}</strong>
                </span>
            </div>
        }
    }
}

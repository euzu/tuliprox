use crate::{
    app::components::{
        playlist::stats::{
            emphasize, epg_coverage_ring::EpgCoverageRing, epg_source_list::epg_source_list,
            missing_epg_chips::missing_epg_chips,
        },
        Card, NoContent,
    },
    i18n::use_translation,
    model::{format_guide_until_date, format_number, percent, t_fmt, t_fmt_split},
};
use shared::model::TargetAnalyticsDto;
use std::rc::Rc;
use yew::prelude::*;

#[derive(Clone, PartialEq, Properties)]
pub struct EpgCoveragePanelProps {
    /// Shared with the other panels; the EPG part is read from `analytics.epg` without cloning.
    pub analytics: Rc<TargetAnalyticsDto>,
    pub now: u64,
    pub on_export_csv: Callback<()>,
}

#[component]
pub fn EpgCoveragePanel(props: &EpgCoveragePanelProps) -> Html {
    let translate = use_translation();

    if let Some(e) = props.analytics.epg.as_ref() {
        let epg_pct = percent(e.mapped, e.live_channels);
        let unmapped = e.live_channels.saturating_sub(e.mapped);
        let guide_expired = e.guide_until.is_some_and(|until| until < props.now as i64);
        let guide_window_text = if let Some(until) = e.guide_until {
            let date_str = format_guide_until_date(until);
            if guide_expired {
                t_fmt(&translate, "LABEL.PLAYLIST_STATS_GUIDE_EXPIRED", &[("date", &date_str)])
            } else {
                let days_str = (until - props.now as i64).max(0).unsigned_abs().div_ceil(86_400).to_string();
                t_fmt(&translate, "LABEL.PLAYLIST_STATS_GUIDE_WINDOW", &[("days", &days_str), ("date", &date_str)])
            }
        } else {
            String::new()
        };

        let mapped_str = format_number(e.mapped);
        let total_str = format_number(e.live_channels);
        let mapped_of_total_text = emphasize(t_fmt_split(
            &translate,
            "LABEL.PLAYLIST_STATS_MAPPED_OF_TOTAL",
            ("mapped", &mapped_str),
            &[("total", &total_str)],
        ));

        let unmapped_str = format_number(unmapped);
        let without_guide_text =
            emphasize(t_fmt_split(&translate, "LABEL.PLAYLIST_STATS_WITHOUT_GUIDE", ("count", &unmapped_str), &[]));

        let on_export = {
            let cb = props.on_export_csv.clone();
            Callback::from(move |_| cb.emit(()))
        };

        html! {
            <Card class="tp__playlist-stats__panel-card">
                <div class="tp__playlist-stats__panel-card-header">
                    <h2>{translate.t("LABEL.PLAYLIST_STATS_EPG_COVERAGE")}</h2>
                    <span>{translate.t("LABEL.PLAYLIST_STATS_EPG_COVERAGE_SUBTITLE")}</span>
                </div>
                <div class="tp__playlist-stats__epg-summary">
                    <EpgCoverageRing percent={epg_pct} />
                    <div class="details">
                        <span class="mapped">{mapped_of_total_text}</span>
                        <span class="unmapped">{without_guide_text}</span>
                        if !guide_window_text.is_empty() {
                            <span class={classes!("window", guide_expired.then_some("expired"))}>{guide_window_text}</span>
                        }
                    </div>
                </div>
                {epg_source_list(&e.sources, props.now, &translate)}
                if !e.missing_by_group.is_empty() || e.missing_other > 0 {
                    <div class="tp__playlist-stats__missing-groups">
                        <span class="tp__playlist-stats__missing-groups-title">
                            {translate.t("LABEL.PLAYLIST_STATS_MISSING_EPG_BY_GROUP")}
                        </span>
                        {missing_epg_chips(&e.missing_by_group, e.missing_other, &translate)}
                    </div>
                }
                <button type="button" class="tp__playlist-stats__export-btn" onclick={on_export}>
                    {translate.t("LABEL.PLAYLIST_STATS_EXPORT_UNMAPPED_CSV")}
                </button>
            </Card>
        }
    } else {
        html! {
            <Card class="tp__playlist-stats__panel-card">
                <div class="tp__playlist-stats__panel-card-header">
                    <h2>{translate.t("LABEL.PLAYLIST_STATS_EPG_COVERAGE")}</h2>
                    <span>{translate.t("LABEL.PLAYLIST_STATS_EPG_COVERAGE_SUBTITLE")}</span>
                </div>
                <NoContent hint={translate.t("LABEL.PLAYLIST_STATS_NO_EPG_HINT")} />
            </Card>
        }
    }
}

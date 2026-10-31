use crate::{i18n::YewI18n, model::format_number};
use shared::model::GroupCount;
use yew::prelude::*;

/// Plain render function (no component) so the group list is borrowed, not cloned into props.
pub fn missing_epg_chips(missing_by_group: &[GroupCount], missing_other: usize, translate: &YewI18n) -> Html {
    html! {
        <div class="tp__playlist-stats__missing-groups-chips">
            { for missing_by_group.iter().map(|g| {
                html! {
                    <span class="chip" key={g.name.clone()}>
                        <span class="name">{&g.name}</span>
                        <span class="count">{format_number(g.count)}</span>
                    </span>
                }
            })}
            { if missing_other > 0 {
                html! {
                    <span class="chip" key="other">
                        <span class="name">{translate.t("LABEL.PLAYLIST_STATS_OTHER")}</span>
                        <span class="count">{format_number(missing_other)}</span>
                    </span>
                }
            } else {
                Html::default()
            }}
        </div>
    }
}

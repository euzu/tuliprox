use crate::i18n::use_translation;
use shared::model::ClusterFlags;
use yew::prelude::*;

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum ClusterFlagsInputMode {
    #[default]
    NoneIsAll,
    NoneIsNone,
}

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct ClusterFlagsInputProps {
    pub name: String,
    #[prop_or_default]
    pub value: Option<ClusterFlags>,
    #[prop_or_default]
    pub on_change: Callback<(String, Option<ClusterFlags>)>,
    #[prop_or_default]
    pub mode: ClusterFlagsInputMode,
    #[prop_or_default]
    pub short_labels: bool,
}

#[component]
pub fn ClusterFlagsInput(props: &ClusterFlagsInputProps) -> Html {
    let translate = use_translation();

    let flags = use_state(|| {
        props.value.unwrap_or_else(|| match props.mode {
            ClusterFlagsInputMode::NoneIsAll => ClusterFlags::all(),
            ClusterFlagsInputMode::NoneIsNone => ClusterFlags::empty(),
        })
    });
    {
        let set_flags = flags.clone();
        use_effect_with((props.value, props.mode), move |(val, cmode)| {
            set_flags.set((*val).unwrap_or_else(|| match cmode {
                ClusterFlagsInputMode::NoneIsAll => ClusterFlags::all(),
                ClusterFlagsInputMode::NoneIsNone => ClusterFlags::empty(),
            }));
        });
    }

    let handle_change = {
        let onchange = props.on_change.clone();
        let name = props.name.clone();
        Callback::from(move |new_flags: Option<ClusterFlags>| {
            let cluster_flags = if new_flags.is_none_or(|f| f.is_empty()) { None } else { new_flags };
            let name = name.clone();
            onchange.emit((name, cluster_flags));
        })
    };

    let handle_flag_click = {
        let current_flags = flags.clone();
        Callback::from(move |new_flag| {
            let mut new_flags = *current_flags;
            new_flags.toggle(new_flag);
            current_flags.set(new_flags);
            if new_flags.is_empty() {
                handle_change.emit(None);
            } else {
                handle_change.emit(Some(new_flags));
            }
        })
    };

    let make_flag_handler = |flag: ClusterFlags| {
        let handle_flag_click = handle_flag_click.clone();
        Callback::from(move |_| handle_flag_click.emit(flag))
    };

    let handle_live_click = make_flag_handler(ClusterFlags::Live);
    let handle_vod_click = make_flag_handler(ClusterFlags::Vod);
    let handle_series_click = make_flag_handler(ClusterFlags::Series);
    let live_label = if props.short_labels { "LABEL.LIVE_SHORT" } else { "LABEL.LIVE" };
    let vod_label = if props.short_labels { "LABEL.VOD_SHORT" } else { "LABEL.VOD" };
    let series_label = if props.short_labels { "LABEL.SERIES_SHORT" } else { "LABEL.SERIES" };

    if props.short_labels {
        html! {
            <div class={classes!("tp__cluster-flags-input", "tp__cluster-flags-input--short", "tp__proxy-type-input")}>
               <span class={classes!("tp__chip", "tp__chip__group", "tp__cluster-flags-input__outer", "active", "tp__proxy-type-input__reverse")}>
                    <span class={"tp__chip__group__sub tp__cluster-flags-input__mixed tp__proxy-type-input__mixed"}>
                       <span onclick={handle_live_click} class={classes!("noselect", "tp__chip", "tp__cluster-flags-input-live", "tp__proxy-type-input__reverse-live", if flags.intersects(ClusterFlags::Live) {"active"} else {"redirect-active"})}>{ translate.t(live_label) }</span>
                       <span onclick={handle_vod_click} class={classes!("noselect", "tp__chip",  "tp__cluster-flags-input-vod", "tp__proxy-type-input__reverse-vod", if flags.intersects(ClusterFlags::Vod)  {"active"} else {"redirect-active"})}>{ translate.t(vod_label) }</span>
                       <span onclick={handle_series_click} class={classes!("noselect", "tp__chip", "tp__cluster-flags-input-series", "tp__proxy-type-input__reverse-series", if flags.intersects(ClusterFlags::Series)  {"active"} else {"redirect-active"})}>{ translate.t(series_label) }</span>
                    </span>
                </span>
            </div>
        }
    } else {
        html! {
            <div class="tp__cluster-flags-input">
               <span onclick={handle_live_click} class={classes!("noselect", "tp__chip", "tp__cluster-flags-input-live", if flags.intersects(ClusterFlags::Live) {"active"} else {""})}>{ translate.t(live_label) }</span>
               <span onclick={handle_vod_click} class={classes!("noselect", "tp__chip",  "tp__cluster-flags-input-vod", if flags.intersects(ClusterFlags::Vod)  {"active"} else {""})}>{ translate.t(vod_label) }</span>
               <span onclick={handle_series_click} class={classes!("noselect", "tp__chip", "tp__cluster-flags-input-series", if flags.intersects(ClusterFlags::Series)  {"active"} else {""})}>{ translate.t(series_label) }</span>
            </div>
        }
    }
}

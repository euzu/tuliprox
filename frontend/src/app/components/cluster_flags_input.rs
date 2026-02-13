use shared::model::ClusterFlags;
use yew::prelude::*;
use yew_i18n::use_translation;

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
}

#[function_component]
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
            let cluster_flags = if new_flags.is_none_or(|f| f.is_empty()) {
                None
            } else {
                new_flags
            };
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

    html! {
        <div class="tp__cluster-flags-input">
           <span onclick={handle_live_click} class={classes!("noselect", "tp__chip", "tp__cluster-flags-input-live", if flags.intersects(ClusterFlags::Live) {"active"} else {""})}>{ translate.t("LABEL.LIVE") }</span>
           <span onclick={handle_vod_click} class={classes!("noselect", "tp__chip",  "tp__cluster-flags-input-vod", if flags.intersects(ClusterFlags::Vod)  {"active"} else {""})}>{ translate.t("LABEL.VOD") }</span>
           <span onclick={handle_series_click} class={classes!("noselect", "tp__chip", "tp__cluster-flags-input-series", if flags.intersects(ClusterFlags::Series)  {"active"} else {""})}>{ translate.t("LABEL.SERIES") }</span>
        </div>
    }
}

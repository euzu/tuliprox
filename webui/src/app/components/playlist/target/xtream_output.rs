use std::rc::Rc;
use yew::prelude::*;
use yew_i18n::use_translation;
use shared::model::XtreamTargetOutputDto;
use crate::app::components::chip::{convert_bool_to_chip_style};
use crate::app::components::Tag;
use crate::app::components::tag_list::{TagList};

#[derive(Properties, PartialEq, Clone)]
pub struct XtreamOutputProps {
    pub output: XtreamTargetOutputDto,
}

#[function_component]
pub fn XtreamOutput(props: &XtreamOutputProps) -> Html {
    let translator = use_translation();

    let tags_skip_direct_source = {
        let output = props.output.clone();
        let translate = translator.clone();
        use_memo(output, move |output| {
            vec![
                Rc::new(Tag { class: convert_bool_to_chip_style(output.skip_live_direct_source), label: translate.t("LABEL.LIVE") }),
                Rc::new(Tag { class: convert_bool_to_chip_style(output.skip_video_direct_source), label: translate.t("LABEL.VOD") }),
                Rc::new(Tag { class: convert_bool_to_chip_style(output.skip_series_direct_source), label: translate.t("LABEL.SERIES") }),
            ]
        })
    };
    let tags_resolve = {
        let output = props.output.clone();
        let translate = translator.clone();
        use_memo(output, move |output| {
            vec![
                Rc::new(Tag { class: convert_bool_to_chip_style(output.resolve_series),
                    label: format!("{} / {}s", translate.t("LABEL.SERIES"), output.resolve_series_delay) }),
                Rc::new(Tag { class: convert_bool_to_chip_style(output.resolve_vod),
                    label: format!("{} / {}s", translate.t("LABEL.VOD"), output.resolve_vod_delay)}),
            ]
        })
    };

    html! {
      <div class="tp__xtream-output tp__target-common">
        <div class="tp__target-common__section">
            <span class="tp__target-common__label">{translator.t("LABEL.SKIP_DIRECT_SOURCE")}</span>
            <TagList tags={(*tags_skip_direct_source).clone()} />
        </div>
        <div class="tp__target-common__section">
          <span class="tp__target-common__label">{translator.t("LABEL.RESOLVE")}</span>
          <TagList tags={(*tags_resolve).clone()} />
        </div>
      </div>
    }
}

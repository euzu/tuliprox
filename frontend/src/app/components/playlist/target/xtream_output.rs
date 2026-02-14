use crate::{
    app::components::{chip::convert_bool_to_chip_style, tag_list::TagList, FilterView, RevealContent, Tag},
    html_if,
};
use shared::model::XtreamTargetOutputDto;
use std::rc::Rc;
use yew::prelude::*;
use yew_i18n::use_translation;

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
                Rc::new(Tag {
                    class: convert_bool_to_chip_style(output.skip_live_direct_source),
                    label: translate.t("LABEL.LIVE"),
                }),
                Rc::new(Tag {
                    class: convert_bool_to_chip_style(output.skip_video_direct_source),
                    label: translate.t("LABEL.VOD"),
                }),
                Rc::new(Tag {
                    class: convert_bool_to_chip_style(output.skip_series_direct_source),
                    label: translate.t("LABEL.SERIES"),
                }),
            ]
        })
    };
    let tags_resolve = {
        let output = props.output.clone();
        let translate = translator.clone();
        use_memo(output, move |output| {
            vec![
                Rc::new(Tag {
                    class: convert_bool_to_chip_style(output.resolve_series),
                    label: format!("{} / {}ms", translate.t("LABEL.SERIES"), output.resolve_delay),
                }),
                Rc::new(Tag {
                    class: convert_bool_to_chip_style(output.resolve_vod),
                    label: format!("{} / {}ms", translate.t("LABEL.VOD"), output.resolve_delay),
                }),
            ]
        })
    };
    let tags_probe = {
        let output = props.output.clone();
        let translate = translator.clone();
        use_memo(output, move |output| {
            vec![
                Rc::new(Tag {
                    class: convert_bool_to_chip_style(output.probe_live),
                    label: format!("{} / {}h", translate.t("LABEL.LIVE"), output.probe_live_interval_hours),
                }),
                Rc::new(Tag {
                    class: convert_bool_to_chip_style(output.probe_vod),
                    label: translate.t("LABEL.VOD").to_string(),
                }),
                Rc::new(Tag {
                    class: convert_bool_to_chip_style(output.probe_series),
                    label: translate.t("LABEL.SERIES").to_string(),
                }),
            ]
        })
    };

    html! {
      <div class="tp__xtream-output tp__target-common">
        { html_if!(props.output.t_filter.is_some(), {
        <div class="tp__target-common__section">
            <RevealContent preview={Some(html!{<FilterView inline={true} filter={props.output.t_filter.clone()} />})}>
               <FilterView pretty={true} filter={props.output.t_filter.clone()} />
            </RevealContent>
        </div>
        }) }
        <div class="tp__target-common__section">
            <span class="tp__target-common__label">{translator.t("LABEL.SKIP_DIRECT_SOURCE")}</span>
            <TagList tags={(*tags_skip_direct_source).clone()} />
        </div>
        <div class="tp__target-common__section">
          <span class="tp__target-common__label">{translator.t("LABEL.RESOLVE")}</span>
          <TagList tags={(*tags_resolve).clone()} />
        </div>
        <div class="tp__target-common__section">
          <span class="tp__target-common__label">{translator.t("LABEL.PROBE")}</span>
          <TagList tags={(*tags_probe).clone()} />
        </div>
      </div>
    }
}

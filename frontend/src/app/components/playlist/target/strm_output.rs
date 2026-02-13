use crate::app::components::{convert_bool_to_chip_style, FilterView, RevealContent, Tag, TagList};
use crate::html_if;
use shared::model::StrmTargetOutputDto;
use std::rc::Rc;
use yew::prelude::*;
use yew_i18n::use_translation;

#[derive(Properties, PartialEq, Clone)]
pub struct StrmOutputProps {
    pub output: StrmTargetOutputDto,
}

#[function_component]
pub fn StrmOutput(props: &StrmOutputProps) -> Html {
    let translator = use_translation();

    let tags = {
        let output = props.output.clone();
        let translate = translator.clone();
        use_memo(output, move |output| {
            vec![
                Rc::new(Tag {
                    class: convert_bool_to_chip_style(output.flat),
                    label: translate.t("LABEL.FLAT"),
                }),
                Rc::new(Tag {
                    class: convert_bool_to_chip_style(output.cleanup),
                    label: translate.t("LABEL.CLEANUP"),
                }),
                Rc::new(Tag {
                    class: convert_bool_to_chip_style(output.underscore_whitespace),
                    label: translate.t("LABEL.UNDERSCORE_WHITESPACE"),
                }),
                Rc::new(Tag {
                    class: convert_bool_to_chip_style(output.add_quality_to_filename),
                    label: translate.t("LABEL.ADD_QUALITY_TO_FILENAME"),
                }),
            ]
        })
    };

    html! {
      <div class="tp__strm-output tp__target-common">
        { html_if!(props.output.t_filter.is_some(), {
        <div class="tp__target-common__section">
            <RevealContent preview={Some(html!{<FilterView inline={true} filter={props.output.t_filter.clone()} />})}><FilterView pretty={true} filter={props.output.t_filter.clone()} /></RevealContent>
        </div>
        }) }
        <div class="tp__target-common__section  tp__target-common__row">
            <span class="tp__target-common__label">{translator.t("LABEL.DIRECTORY")}</span>
            <span>{ props.output.directory.clone() }</span>
        </div>
        <div class="tp__target-common__section tp__target-common__row">
            <span class="tp__target-common__label">{translator.t("LABEL.USERNAME")}</span>
           { props.output.username.as_ref().map(|f| html! {<span>{ f }</span>}) }
        </div>
        <div class="tp__target-common__section">
            <TagList tags={(*tags).clone()} />
        </div>
        { html_if!(props.output.strm_props.is_some(), {
           <div class="tp__target-common__section">
                <span class="tp__target-common__label">{translator.t("LABEL.PROPERTIES")}</span>
                <ul>
                    { props.output.strm_props.as_ref().iter().map(|p| html! { <li>{p}</li> }).collect::<Html>() }
                </ul>
            </div>
        }) }
      </div>
    }
}

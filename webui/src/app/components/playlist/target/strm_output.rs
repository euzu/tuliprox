use std::rc::Rc;
use yew::prelude::*;
use yew_i18n::use_translation;
use shared::model::{StrmTargetOutputDto};
use crate::app::components::{convert_bool_to_chip_style, Tag, TagList};

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
                Rc::new(Tag { class: convert_bool_to_chip_style(output.flat), label: translate.t("LABEL.FLAT") }),
                Rc::new(Tag { class: convert_bool_to_chip_style(output.underscore_whitespace), label: translate.t("LABEL.UNDERSCORE_WHITESPACE") }),
                Rc::new(Tag { class: convert_bool_to_chip_style(output.cleanup), label: translate.t("LABEL.CLEANUP") }),
            ]
        })
    };

    html! {
      <div class="tp__strm-output tp__target-common">
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
        {
            match props.output.strm_props.as_ref() {
                None => html! {},
                Some(props) =>  html! {
                    <div class="tp__target-common__section">
                        <span class="tp__target-common__label">{translator.t("LABEL.PROPERTIES")}</span>
                        <ul>
                            { props.iter().map(|p| html! { <li>{p}</li> }).collect::<Html>() }
                        </ul>
                    </div>
                }
            }
        }
      </div>
    }
}

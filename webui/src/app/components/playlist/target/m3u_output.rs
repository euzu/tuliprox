use std::rc::Rc;
use yew::prelude::*;
use yew_i18n::use_translation;
use shared::model::M3uTargetOutputDto;
use crate::app::components::chip::{convert_bool_to_chip_style};
use crate::app::components::Tag;
use crate::app::components::tag_list::TagList;

#[derive(Properties, PartialEq, Clone)]
pub struct M3uOutputProps {
    pub output: M3uTargetOutputDto,
}
#[function_component]
pub fn M3uOutput(props: &M3uOutputProps) -> Html {
    let translator = use_translation();

    let tags = {
        let output = props.output.clone();
        let translate = translator.clone();
        use_memo(output, move |output| {
            vec![
                Rc::new(Tag { class: convert_bool_to_chip_style(output.include_type_in_url), label: translate.t("LABEL.INCLUDE_TYPE_IN_URL") }),
                Rc::new(Tag { class: convert_bool_to_chip_style(output.mask_redirect_url), label: translate.t("LABEL.MASK_REDIRECT_URL") }),
            ]
        })
    };


    html! {
      <div class="tp__m3u-output tp__target-common">
        <div class="tp__target-common__section">
            <span class="tp__target-common__label">{translator.t("LABEL.FILENAME")}</span>
           { props.output.filename.as_ref().map(|f| html! {<span>{ f }</span>}) }
        </div>
        <div class="tp__target-common__section">
            <TagList tags={(*tags).clone()} />
        </div>
      </div>
    }
}
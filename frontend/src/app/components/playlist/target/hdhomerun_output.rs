use shared::model::HdHomeRunTargetOutputDto;
use yew::prelude::*;
use yew_i18n::use_translation;

#[derive(Properties, PartialEq, Clone)]
pub struct HdHomeRunOutputProps {
    pub output: HdHomeRunTargetOutputDto,
}

#[function_component]
pub fn HdHomeRunOutput(props: &HdHomeRunOutputProps) -> Html {
    let translator = use_translation();
    html! {
      <div class="tp__hdhomerun-output tp__target-common">
        <div class="tp__target-common__section tp__target-common__row">
            <span class="tp__target-common__label">{translator.t("LABEL.DEVICE")}</span>
            <span>{ props.output.device.clone() }</span>
        </div>
        <div class="tp__target-common__section tp__target-common__row">
            <span class="tp__target-common__label">{translator.t("LABEL.USERNAME")}</span>
            <span>{ props.output.username.clone() }</span>
        </div>
        <div class="tp__target-common__section tp__target-common__row">
            <span class="tp__target-common__label">{translator.t("LABEL.USE_OUTPUT")}</span>
            <span>{ props.output.use_output.map_or_else(String::new, |o| o.to_string()) }</span>
        </div>
      </div>
    }
}

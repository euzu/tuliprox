use shared::model::MappingCounter;
use std::sync::atomic::Ordering;
use yew::prelude::*;
use yew_i18n::use_translation;

#[derive(Properties, PartialEq, Clone)]
pub struct MapperCounterViewProps {
    #[prop_or_default]
    pub pretty: bool,
    #[prop_or(false)]
    pub inline: bool,
    pub counter: MappingCounter,
}

#[function_component]
pub fn MapperCounterView(props: &MapperCounterViewProps) -> Html {
    let translate = use_translation();

    html! {
     <div class={classes!("tp__mapper-counter", if props.inline {"tp__mapper-counter__inline"} else {""} )}>
        <div class="tp__mapper-counter__row">
            <label>{translate.t("LABEL.FIELD")}</label>
            {props.counter.field.clone()}
        </div>
        <div class="tp__mapper-counter__row">
          <label>{translate.t("LABEL.CONCAT")}</label>
          {props.counter.concat.clone()}
        </div>
        <div class="tp__mapper-counter__row">
            <label>{translate.t("LABEL.MODIFIER")}</label>
            {props.counter.modifier}
        </div>
        <div class="tp__mapper-counter__row">
            <label>{translate.t("LABEL.VALUE")}</label>
            {props.counter.value.load(Ordering::Relaxed)}
        </div>
        <div class="tp__mapper-counter__row">
            <label>{translate.t("LABEL.PADING")}</label>
            {props.counter.padding}
        </div>
      </div>
    }
}

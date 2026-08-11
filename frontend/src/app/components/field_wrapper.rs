use crate::app::components::FieldLabel;
use yew::prelude::*;

#[derive(Properties, PartialEq)]
pub struct FieldWrapperProps {
    pub field_id: String,
    #[prop_or_default]
    pub label: Option<String>,
    #[prop_or_default]
    pub hint_key: Option<String>,
    #[prop_or_default]
    pub class: Classes,
    #[prop_or(true)]
    pub link_label: bool,
    #[prop_or_default]
    pub required: bool,
    #[prop_or_default]
    pub error: Option<String>,
    pub children: Children,
}

#[component]
pub fn FieldWrapper(props: &FieldWrapperProps) -> Html {
    let for_id = props.link_label.then(|| props.field_id.clone());

    html! {
        <div class={classes!(
            "tp__input",
            props.required.then_some("tp__input--required"),
            props.error.as_ref().map(|_| "tp__input--error"),
            props.class.clone())}>
            { props.label.as_ref().map_or_else(Html::default, |label| html! {
                <FieldLabel
                    label={label.clone()}
                    field_id={props.field_id.clone()}
                    hint_key={props.hint_key.clone()}
                    for_id={for_id.clone()}
                />
            }) }
            <div class="tp__input-wrapper">
                { for props.children.iter() }
            </div>
            { props.error.as_ref().map_or_else(Html::default, |error| html! {
                <span class="tp__input-error" role="alert">{ error.clone() }</span>
            }) }
        </div>
    }
}

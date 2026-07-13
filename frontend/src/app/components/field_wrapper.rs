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
    pub children: Children,
}

#[component]
pub fn FieldWrapper(props: &FieldWrapperProps) -> Html {
    let for_id = props.link_label.then(|| props.field_id.clone());

    html! {
        <div class={classes!("tp__input", props.class.clone())}>
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
        </div>
    }
}

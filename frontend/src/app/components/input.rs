use crate::{
    app::components::{resolve_field_id, AppIcon, FieldWrapper, IconButton},
    html_if,
};
use web_sys::{HtmlInputElement, InputEvent, KeyboardEvent, MouseEvent};
use yew::{component, html, use_effect_with, use_state, Callback, Html, NodeRef, Properties, TargetCast};

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct InputProps {
    #[prop_or_default]
    pub name: String,
    #[prop_or_default]
    pub field_id: Option<String>,
    #[prop_or_default]
    pub label: Option<String>,
    #[prop_or_default]
    pub hidden: bool,
    #[prop_or_default]
    pub input_ref: Option<NodeRef>,
    #[prop_or_default]
    pub autocomplete: bool,
    #[prop_or_default]
    pub onkeydown: Option<Callback<KeyboardEvent>>,
    #[prop_or_default]
    pub on_change: Option<Callback<String>>,
    #[prop_or_default]
    pub value: String,
    #[prop_or_default]
    pub icon: Option<String>,
    #[prop_or_default]
    pub placeholder: Option<String>,
    #[prop_or_default]
    pub hint_key: Option<String>,
    #[prop_or_default]
    pub aria_label: Option<String>,
}

#[component]
pub fn Input(props: &InputProps) -> Html {
    let hide_content = use_state(|| props.hidden);
    let local_ref = props.input_ref.clone().unwrap_or_default();
    let label_text = props.label.clone().unwrap_or_default();
    let resolved_field_id = resolve_field_id(&props.field_id, &props.name, &label_text);

    {
        let local_ref = local_ref.clone();
        use_effect_with(props.value.clone(), move |val| {
            if let Some(input) = local_ref.cast::<HtmlInputElement>() {
                input.set_value(val);
            }
            || ()
        });
    }

    let handle_hide_content = {
        let hide_content = hide_content.clone();
        Callback::from(move |(name, _event): (String, MouseEvent)| {
            if name == "hide" {
                hide_content.set(!*hide_content);
            }
        })
    };

    let handle_oninput = {
        let ontext_clone = props.on_change.clone();
        Callback::from(move |event: InputEvent| {
            if let Some(input) = event.target_dyn_into::<web_sys::HtmlInputElement>() {
                let value = input.value();
                if let Some(cb) = ontext_clone.as_ref() {
                    cb.emit(value);
                }
            }
        })
    };

    let aria_label =
        if props.label.is_some() { None } else { props.aria_label.clone().or_else(|| props.placeholder.clone()) };

    html! {
        <FieldWrapper label={props.label.clone()} field_id={resolved_field_id.clone()} hint_key={props.hint_key.clone()}>
            { html_if!(props.icon.is_some(), {
                <AppIcon name={props.icon.as_ref().unwrap().clone()} />
            })}
            <input
                id={resolved_field_id}
                ref={local_ref.clone()}
                type={if *hide_content { "password".to_string() } else { "text".to_string() }}
                name={props.name.clone()}
                autocomplete={if props.autocomplete { "on".to_string() } else { "off".to_string() }}
                onkeydown={props.onkeydown.clone()}
                oninput={handle_oninput}
                placeholder={props.placeholder.clone()}
                aria-label={aria_label}
                />
            { html_if!(props.hidden, {
                 <IconButton name="hide" icon="Visibility" class={if !*hide_content {"active"} else {""}} onclick={handle_hide_content} />
            })}
        </FieldWrapper>
    }
}

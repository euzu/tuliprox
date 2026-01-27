use web_sys::{HtmlTextAreaElement, InputEvent, KeyboardEvent};
use yew::{function_component, html, use_effect_with, Callback, Html, NodeRef, Properties, TargetCast};
use crate::app::components::CollapsePanel;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct TextAreaProps {
    #[prop_or_default]
    pub name: String,
    #[prop_or_default]
    pub label: Option<String>,
    #[prop_or_default]
    pub input_ref: Option<NodeRef>,
    #[prop_or_default]
    pub onkeydown: Option<Callback<KeyboardEvent>>,
    #[prop_or_default]
    pub on_change: Option<Callback<String>>,
    #[prop_or_default]
    pub value: String,
    #[prop_or_default]
    pub placeholder: Option<String>,
    #[prop_or_default]
    pub rows: Option<u32>,
    #[prop_or_default]
    pub collapse_on_empty: bool,
}

#[function_component]
pub fn TextArea(props: &TextAreaProps) -> Html {
    let local_ref = props.input_ref.clone().unwrap_or_default();

    {
        let local_ref = local_ref.clone();
        use_effect_with(props.value.clone(), move |val| {
            if let Some(input) = local_ref.cast::<HtmlTextAreaElement>() {
                if input.value() != *val {
                    input.set_value(val);
                }
            }
            || ()
        });
    }

    let handle_oninput = {
        let ontext_clone = props.on_change.clone();
        Callback::from(move |event: InputEvent| {
            if let Some(input) = event.target_dyn_into::<web_sys::HtmlTextAreaElement>() {
                let value = input.value();
                if let Some(cb) = ontext_clone.as_ref() {
                    cb.emit(value);
                }
            }
        })
    };

    let text_area = html! {
        <div class="tp__input-wrapper">
            <textarea ref={local_ref} name={props.name.clone()} onkeydown={props.onkeydown.clone()}
                oninput={handle_oninput} placeholder={props.placeholder.clone()}
                rows={props.rows.unwrap_or(5).to_string()} value={props.value.clone()}
            />
        </div>
    };

    if props.collapse_on_empty {
        return html! {
            <CollapsePanel title={props.label.clone().unwrap_or_default()} expanded={!props.value.is_empty()}>
                <div class="tp__input">
                    { text_area }
                </div>
            </CollapsePanel>
        };
    }

    html! {
        <div class="tp__input">
            { if props.label.is_some() {
                   html! {
                       <label>{props.label.clone().unwrap_or_default()}</label>
                   }
                } else { html!{} }
            }
            { text_area }
        </div>
    }
}

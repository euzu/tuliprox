use web_sys::KeyboardEvent;
use yew::{function_component, html, Callback, Html, NodeRef, Properties};

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct InputProps {
    #[prop_or_default]
    pub name: String,
    #[prop_or_default]
    pub label: Option<String>,
    #[prop_or("text".into())]
    pub input_type: String,
    #[prop_or_default]
    pub input_ref: Option<NodeRef>,
    #[prop_or_default]
    pub autocomplete: bool,
    #[prop_or_default]
    pub autofocus: bool,
    #[prop_or_default]
    pub onkeydown: Option<Callback<KeyboardEvent>>,
}

#[function_component]
pub fn Input(props: &InputProps) -> Html {
    html! {
        <div class="input">
            { if props.label.is_some() {
                   html! {
                       <label>{props.label.clone().unwrap_or_default()}</label>
                   }
                } else { html!{} }
            }
            <div class="input-wrapper">
                <input ref={props.input_ref.clone().unwrap_or_default()}
                    type={props.input_type.clone()}
                    name={props.name.clone()}
                    autocomplete={if props.autocomplete { "on".to_string() } else { "off".to_string() }}
                    autofocus={props.autofocus}
                    onkeydown={props.onkeydown.clone().unwrap_or_default()}
                    />
            </div>
        </div>
    }
}
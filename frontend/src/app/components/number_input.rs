use web_sys::{HtmlInputElement, KeyboardEvent};
use yew::prelude::*;
use yew::TargetCast;
use shared::utils::{format_float_localized, parse_localized_float};

#[derive(Properties, Clone, PartialEq)]
pub struct NumberInputProps {
    #[prop_or_default]
    pub name: String,
    #[prop_or_default]
    pub label: Option<String>,
    #[prop_or_default]
    pub value: Option<i64>,
    #[prop_or_default]
    pub float_value: Option<f64>,
    #[prop_or_default]
    pub on_change: Callback<Option<i64>>,
    #[prop_or_default]
    pub on_change_float: Option<Callback<Option<f64>>>,
    #[prop_or_default]
    pub placeholder: String,
}

#[function_component]
pub fn NumberInput(props: &NumberInputProps) -> Html {
    let input_ref = use_node_ref();

    {
        let input_ref = input_ref.clone();
        let deps = (props.value, props.float_value);
        use_effect_with(deps, move |(int_val, float_val)| {
            if let Some(input) = input_ref.cast::<HtmlInputElement>() {
                let new_value = float_val
                    .map(|f| format_float_localized(f, 4, true))
                    .or_else(|| int_val.map(|v| v.to_string()))
                    .unwrap_or_default();
                input.set_value(&new_value);
            }
            || ()
        });
    }

    let prefers_float = props.on_change_float.is_some();
    let on_input = {
        let onchange_int = props.on_change.clone();
        let onchange_float = props.on_change_float.clone();
        Callback::from(move |e: InputEvent| {
            if let Some(input) = e.target_dyn_into::<HtmlInputElement>() {
                let raw = input.value().trim().to_string();
                if raw.is_empty() {
                    if prefers_float {
                        if let Some(cb) = onchange_float.as_ref() {
                            cb.emit(None);
                        }
                    } else {
                        onchange_int.emit(None);
                    }
                    return;
                }

                if prefers_float {
                    if let Some(cb) = onchange_float.as_ref() {
                        let parsed = parse_localized_float(&raw);
                        cb.emit(parsed);
                    }
                } else {
                    let parsed = raw.parse::<i64>().ok();
                    onchange_int.emit(parsed);
                }
            }
        })
    };

    let handle_keydown = {
        Callback::from(move |e: KeyboardEvent| {
            let key = e.key();
            let allowed = key.chars().all(|c| c.is_ascii_digit())
                || key == "Backspace"
                || key == "Delete"
                || key == "ArrowLeft"
                || key == "ArrowRight"
                || key == "Tab"
                || key == "Enter"
                || key == "."
                || key == ","
                || key == "-";

            if !allowed {
                e.prevent_default();
                e.stop_propagation();
            }
        })
    };

    html! {
        <div class="tp__input">
            { if let Some(label) = &props.label {
                html! { <label>{ label }</label> }
            } else { html!{} } }
            <div class="tp__input-wrapper">
                <input
                    ref={input_ref.clone()}
                    type="text"
                    name={props.name.clone()}
                    placeholder={props.placeholder.clone()}
                    onkeydown={handle_keydown.clone()}
                    oninput={on_input.clone()}
                />
            </div>
        </div>
    }
}
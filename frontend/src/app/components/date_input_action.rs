use crate::app::components::{FieldLabel, IconButton};
use web_sys::{HtmlInputElement, MouseEvent};
use yew::{component, html, use_effect_with, Callback, Html, NodeRef, Properties, TargetCast};

#[derive(Clone, PartialEq, Debug)]
pub struct ToolAction {
    pub name: Option<String>,
    pub icon: String,
    pub hint: Option<String>,
    pub class: Option<String>,
    pub onclick: Callback<MouseEvent>,
}

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct DateInputActionProps {
    #[prop_or_default]
    pub name: String,
    #[prop_or_default]
    pub field_id: Option<String>,
    #[prop_or_default]
    pub label: Option<String>,
    #[prop_or_default]
    pub input_ref: Option<NodeRef>,
    #[prop_or_default]
    pub value: Option<i64>,
    #[prop_or_default]
    pub on_change: Option<Callback<Option<i64>>>,
    #[prop_or_default]
    pub tool_action: Option<ToolAction>,
}

#[component]
pub fn DateInputAction(props: &DateInputActionProps) -> Html {
    let local_ref = props.input_ref.clone().unwrap_or_default();

    {
        let local_ref = local_ref.clone();
        let value = props.value;
        use_effect_with(value, move |val| {
            if let Some(input) = local_ref.cast::<HtmlInputElement>() {
                if let Some(ts) = val {
                    if let Some(date) = chrono::DateTime::from_timestamp(*ts, 0) {
                        input.set_value(&date.format("%Y-%m-%d").to_string());
                    }
                } else {
                    input.set_value("");
                }
            }
            || ()
        });
    }

    let handle_change = {
        let onchange_cb = props.on_change.clone();
        Callback::from(move |event: yew::events::Event| {
            if let Some(input) = event.target_dyn_into::<HtmlInputElement>() {
                let value = input.value();
                let ts = if value.is_empty() {
                    None
                } else {
                    chrono::NaiveDate::parse_from_str(&value, "%Y-%m-%d")
                        .ok()
                        .and_then(|date| date.and_hms_opt(0, 0, 0))
                        .map(|dt| dt.and_utc().timestamp())
                };
                if let Some(cb) = onchange_cb.as_ref() {
                    cb.emit(ts);
                }
            }
        })
    };

    let handle_tool_click = {
        let tool_action = props.tool_action.clone();
        Callback::from(move |(_, event): (String, MouseEvent)| {
            if let Some(action) = tool_action.as_ref() {
                action.onclick.emit(event);
            }
        })
    };

    html! {
        <div class="tp__input tp__input-date tp__input-date-tools">
            { if let Some(label) = &props.label {
                html! {
                    <FieldLabel
                        label={label.clone()}
                        field_id={props.field_id.clone().unwrap_or_else(|| {
                            if props.name.trim().is_empty() {
                                label.clone()
                            } else {
                                props.name.clone()
                            }
                        })}
                    />
                }
            } else { html!{} } }
            <div class="tp__input-wrapper">
                <input
                    ref={local_ref.clone()}
                    type="date"
                    name={props.name.clone()}
                    onchange={handle_change}
                />
            <div class="tp__input-tools">
                {
                    if let Some(action) = props.tool_action.as_ref() {
                        html! {
                            <IconButton
                                class={if let Some(tool_class) = action.class.as_ref() {
                                    format!("tp__input-tool {tool_class}")
                                } else {
                                    "tp__input-tool".to_string()
                                }}
                                name={action.name.clone().unwrap_or_else(|| props.name.clone())}
                                icon={action.icon.clone()}
                                hint={action.hint.clone().unwrap_or_default()}
                                onclick={handle_tool_click}
                            />
                        }
                    } else {
                        Html::default()
                    }
                }
            </div>
            </div>
        </div>
    }
}

use crate::app::components::FieldLabel;
use web_sys::HtmlInputElement;
use yew::{classes, component, html, use_effect_with, Callback, Html, NodeRef, Properties, TargetCast};

pub(crate) fn format_date_input_value(value: Option<i64>) -> String {
    value
        .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
        .map_or_else(String::new, |date| date.format("%Y-%m-%d").to_string())
}

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct DateInputProps {
    #[prop_or_default]
    pub name: String,
    #[prop_or_default]
    pub field_id: Option<String>,
    #[prop_or_default]
    pub label: Option<String>,
    #[prop_or_default]
    pub input_ref: Option<NodeRef>,
    #[prop_or_default]
    pub value: Option<i64>, // Unix Timestamp
    #[prop_or_default]
    pub on_change: Option<Callback<Option<i64>>>, // None or Some(timestamp)
}

#[derive(Properties, Clone, PartialEq, Debug)]
pub(crate) struct DateInputBaseProps {
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
    pub tools: Html,
    #[prop_or_default]
    pub extra_class: Option<&'static str>,
}

#[component]
pub(crate) fn DateInputBase(props: &DateInputBaseProps) -> Html {
    let local_ref = props.input_ref.clone().unwrap_or_default();

    {
        let local_ref = local_ref.clone();
        let value = props.value;
        use_effect_with(value, move |val| {
            if let Some(input) = local_ref.cast::<HtmlInputElement>() {
                input.set_value(&format_date_input_value(*val));
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

    html! {
        <div class={classes!("tp__input", "tp__input-date", props.extra_class)}>
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
               {props.tools.clone()}
            </div>
        </div>
    }
}

#[component]
pub fn DateInput(props: &DateInputProps) -> Html {
    html! {
        <DateInputBase
            name={props.name.clone()}
            field_id={props.field_id.clone()}
            label={props.label.clone()}
            input_ref={props.input_ref.clone()}
            value={props.value}
            on_change={props.on_change.clone()}
        />
    }
}

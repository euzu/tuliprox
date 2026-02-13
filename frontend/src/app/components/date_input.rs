use web_sys::HtmlInputElement;
use yew::{
    function_component, html, use_effect_with, Callback, Html, NodeRef, Properties, TargetCast,
};

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct DateInputProps {
    #[prop_or_default]
    pub name: String,
    #[prop_or_default]
    pub label: Option<String>,
    #[prop_or_default]
    pub input_ref: Option<NodeRef>,
    #[prop_or_default]
    pub value: Option<i64>, // Unix Timestamp
    #[prop_or_default]
    pub on_change: Option<Callback<Option<i64>>>, // None or Some(timestamp)
}

#[function_component]
pub fn DateInput(props: &DateInputProps) -> Html {
    let local_ref = props.input_ref.clone().unwrap_or_default();

    {
        let local_ref = local_ref.clone();
        let value = props.value;
        use_effect_with(value, move |val| {
            if let Some(input) = local_ref.cast::<HtmlInputElement>() {
                if let Some(ts) = val {
                    // Timestamp -> yyyy-mm-dd
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
                let value = input.value(); // "2025-08-22"
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
        <div class="tp__input tp__input-date">
            { if let Some(label) = &props.label {
                html! { <label>{ label }</label> }
            } else { html!{} } }
            <div class="tp__input-wrapper">
                <input
                    ref={local_ref.clone()}
                    type="date"
                    name={props.name.clone()}
                    onchange={handle_change}
                />
            </div>
        </div>
    }
}

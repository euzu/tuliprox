use crate::app::components::FieldLabel;
use chrono::TimeZone;
use web_sys::HtmlInputElement;
use yew::{classes, component, html, use_effect_with, Callback, Html, NodeRef, Properties, TargetCast};

pub(crate) fn format_datetime_input_value(value: Option<i64>) -> String {
    value
        .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
        .map_or_else(String::new, |dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%dT%H:%M").to_string())
}

pub(crate) fn parse_datetime_input_change(value: &str) -> Option<i64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M")
        .ok()
        .and_then(|naive| chrono::Local.from_local_datetime(&naive).latest())
        .map(|local| local.timestamp())
}

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct DateTimeInputProps {
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
pub(crate) struct DateTimeInputBaseProps {
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
pub(crate) fn DateTimeInputBase(props: &DateTimeInputBaseProps) -> Html {
    let local_ref = props.input_ref.clone().unwrap_or_default();

    {
        let local_ref = local_ref.clone();
        let value = props.value;
        use_effect_with(value, move |val| {
            if let Some(input) = local_ref.cast::<HtmlInputElement>() {
                input.set_value(&format_datetime_input_value(*val));
            }
            || ()
        });
    }

    let handle_change = {
        let onchange_cb = props.on_change.clone();
        let current_value = props.value;
        Callback::from(move |event: yew::events::Event| {
            if let Some(input) = event.target_dyn_into::<HtmlInputElement>() {
                match parse_datetime_input_change(&input.value()) {
                    Some(ts) => {
                        if let Some(cb) = onchange_cb.as_ref() {
                            cb.emit(Some(ts));
                        }
                    }
                    None => {
                        // Re-display the last valid value so the user sees
                        // the input reset on invalid or empty entries.
                        if trimmed_is_empty(&input.value()) {
                            if let Some(cb) = onchange_cb.as_ref() {
                                cb.emit(None);
                            }
                        } else {
                            input.set_value(&format_datetime_input_value(current_value));
                        }
                    }
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
                    type="datetime-local"
                    name={props.name.clone()}
                    onchange={handle_change}
                />
               {props.tools.clone()}
            </div>
        </div>
    }
}

fn trimmed_is_empty(value: &str) -> bool {
    value.trim().is_empty()
}

#[component]
pub fn DateTimeInput(props: &DateTimeInputProps) -> Html {
    html! {
        <DateTimeInputBase
            name={props.name.clone()}
            field_id={props.field_id.clone()}
            label={props.label.clone()}
            input_ref={props.input_ref.clone()}
            value={props.value}
            on_change={props.on_change.clone()}
        />
    }
}

#[cfg(test)]
mod tests {
    use super::{format_datetime_input_value, parse_datetime_input_change};
    use chrono::TimeZone;

    #[test]
    fn format_datetime_input_value_returns_empty_for_none() {
        assert_eq!(format_datetime_input_value(None), String::new());
    }

    #[test]
    fn format_datetime_input_value_formats_utc_timestamp_in_local_time() {
        // Pick a known local datetime, derive the UTC timestamp that
        // corresponds to it, and assert the formatter renders the same
        // local wall-clock value. Stable across any timezone the test
        // happens to run in.
        let local_naive = chrono::NaiveDate::from_ymd_opt(2026, 8, 6).unwrap().and_hms_opt(14, 30, 0).unwrap();
        let local = chrono::Local.from_local_datetime(&local_naive).single().unwrap();
        let utc = local.timestamp();
        let expected = local.format("%Y-%m-%dT%H:%M").to_string();
        assert_eq!(format_datetime_input_value(Some(utc)), expected);
    }

    #[test]
    fn parse_datetime_input_change_round_trips_known_local_datetime() {
        let local_naive = chrono::NaiveDate::from_ymd_opt(2026, 8, 6).unwrap().and_hms_opt(14, 30, 0).unwrap();
        let local = chrono::Local.from_local_datetime(&local_naive).single().unwrap();
        let formatted = local.format("%Y-%m-%dT%H:%M").to_string();
        let parsed = parse_datetime_input_change(&formatted).expect("parses");
        assert_eq!(parsed, local.timestamp());
    }

    #[test]
    fn parse_datetime_input_change_returns_none_for_empty() {
        assert!(parse_datetime_input_change("").is_none());
        assert!(parse_datetime_input_change("   ").is_none());
    }

    #[test]
    fn parse_datetime_input_change_returns_none_for_partial_or_invalid() {
        assert!(parse_datetime_input_change("2026-08-06T25:00").is_none()); // invalid hour
        assert!(parse_datetime_input_change("01.08.2026 14:30").is_none()); // wrong format
    }
}

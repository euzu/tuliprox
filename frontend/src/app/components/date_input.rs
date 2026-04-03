use crate::app::components::FieldLabel;
use web_sys::HtmlInputElement;
use yew::{classes, component, html, use_effect_with, Callback, Html, NodeRef, Properties, TargetCast};

pub(crate) fn format_date_input_value(value: Option<i64>) -> String {
    value
        .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
        .map_or_else(String::new, |date| date.format("%Y-%m-%d").to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DateInputChange {
    Clear,
    Set(i64),
    IgnoreInvalid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DateInputAction {
    Emit(Option<i64>),
    ResetDisplay(String),
}

fn parse_date_input_change(value: &str) -> DateInputChange {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        DateInputChange::Clear
    } else {
        chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d")
            .ok()
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .map(|dt| DateInputChange::Set(dt.and_utc().timestamp()))
            .unwrap_or(DateInputChange::IgnoreInvalid)
    }
}

fn resolve_date_input_action(raw_value: &str, current_value: Option<i64>) -> DateInputAction {
    match parse_date_input_change(raw_value) {
        DateInputChange::Clear => DateInputAction::Emit(None),
        DateInputChange::Set(ts) => DateInputAction::Emit(Some(ts)),
        DateInputChange::IgnoreInvalid => DateInputAction::ResetDisplay(format_date_input_value(current_value)),
    }
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
        let current_value = props.value;
        Callback::from(move |event: yew::events::Event| {
            if let Some(input) = event.target_dyn_into::<HtmlInputElement>() {
                match resolve_date_input_action(&input.value(), current_value) {
                    DateInputAction::Emit(value) => {
                        if let Some(cb) = onchange_cb.as_ref() {
                            cb.emit(value);
                        }
                    }
                    DateInputAction::ResetDisplay(value) => input.set_value(&value),
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

#[cfg(test)]
mod tests {
    use super::{
        format_date_input_value, parse_date_input_change, resolve_date_input_action, DateInputAction, DateInputChange,
    };

    #[test]
    fn empty_date_input_clears_value() {
        assert_eq!(parse_date_input_change(""), DateInputChange::Clear);
        assert_eq!(parse_date_input_change("   "), DateInputChange::Clear);
    }

    #[test]
    fn valid_iso_date_input_sets_start_of_day_timestamp() {
        assert_eq!(parse_date_input_change("2026-03-01"), DateInputChange::Set(1_772_323_200));
    }

    #[test]
    fn partial_or_locale_formatted_input_is_ignored_until_valid() {
        assert_eq!(parse_date_input_change("2026-03-"), DateInputChange::IgnoreInvalid);
        assert_eq!(parse_date_input_change("tt.03.jjjj"), DateInputChange::IgnoreInvalid);
        assert_eq!(parse_date_input_change("01.04.2026"), DateInputChange::IgnoreInvalid);
    }

    #[test]
    fn invalid_input_resets_display_to_last_valid_value() {
        assert_eq!(
            resolve_date_input_action("tt.03.jjjj", Some(1_775_001_600)),
            DateInputAction::ResetDisplay("2026-04-01".to_string())
        );
    }

    #[test]
    fn formatter_keeps_iso_input_value() {
        assert_eq!(format_date_input_value(Some(1_775_001_600)), "2026-04-01");
        assert_eq!(format_date_input_value(None), "");
    }
}

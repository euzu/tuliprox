use web_sys::HtmlInputElement;
use yew::prelude::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NormalizedRange {
    min: u8,
    max: u8,
    value: u8,
    fill_percent: u16,
}

fn normalize_range(min: u8, max: u8, value: u8) -> NormalizedRange {
    let (min, max) = if min <= max { (min, max) } else { (max, min) };
    let value = value.clamp(min, max);
    let span = u16::from(max - min);
    let fill_percent = (u16::from(value - min) * 100).checked_div(span).unwrap_or_default();
    NormalizedRange { min, max, value, fill_percent }
}

fn parse_slider_value(value: &str, min: u8, max: u8) -> Option<u8> {
    let parsed = value.parse::<i64>().ok()?;
    let bounded = parsed.clamp(i64::from(min), i64::from(max));
    u8::try_from(bounded).ok()
}

fn emit_slider_value<F>(disabled: bool, range: NormalizedRange, read_value: F, on_change: &Callback<u8>)
where
    F: FnOnce() -> String,
{
    if disabled {
        return;
    }
    if let Some(value) = parse_slider_value(&read_value(), range.min, range.max) {
        on_change.emit(value);
    }
}

#[derive(Properties, Clone, PartialEq)]
pub struct RangeSliderProps {
    pub name: AttrValue,
    pub value: u8,
    #[prop_or(0)]
    pub min: u8,
    #[prop_or(100)]
    pub max: u8,
    #[prop_or(false)]
    pub disabled: bool,
    pub on_change: Callback<u8>,
}

/// Neutral bounded range input with an editable numeric value and visual fill level.
#[component]
pub fn RangeSlider(props: &RangeSliderProps) -> Html {
    let range = normalize_range(props.min, props.max, props.value);
    let value = range.value.to_string();
    let value_label = format!("{} value", props.name.as_str());
    let slider_style = format!("--tp-range-slider-fill: {}%;", range.fill_percent);
    let disabled = props.disabled;
    let on_change = props.on_change.clone();
    let oninput = Callback::from(move |event: InputEvent| {
        emit_slider_value(
            disabled,
            range,
            || {
                let input: HtmlInputElement = event.target_unchecked_into();
                input.value()
            },
            &on_change,
        );
    });
    let on_value_blur = {
        let value = value.clone();
        Callback::from(move |event: FocusEvent| {
            let input: HtmlInputElement = event.target_unchecked_into();
            input.set_value(&value);
        })
    };

    html! {
        <div class={classes!("tp__range-slider", disabled.then_some("tp__range-slider--disabled"))}>
            <input
                class="tp__range-slider__range"
                type="range"
                name={props.name.clone()}
                min={range.min.to_string()}
                max={range.max.to_string()}
                value={value.clone()}
                style={slider_style}
                disabled={disabled}
                aria-readonly={disabled.to_string()}
                oninput={oninput.clone()}
            />
            <input
                class="tp__range-slider__value"
                type="number"
                min={range.min.to_string()}
                max={range.max.to_string()}
                step="1"
                value={value}
                disabled={disabled}
                aria-label={value_label}
                aria-readonly={disabled.to_string()}
                oninput={oninput}
                onblur={on_value_blur}
            />
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::{emit_slider_value, normalize_range, parse_slider_value, NormalizedRange};
    use std::{cell::Cell, rc::Rc};
    use yew::Callback;

    #[test]
    fn range_normalization_clamps_values_and_calculates_fill() {
        let cases = [
            (0, 100, 90, NormalizedRange { min: 0, max: 100, value: 90, fill_percent: 90 }),
            (0, 100, 101, NormalizedRange { min: 0, max: 100, value: 100, fill_percent: 100 }),
            (20, 60, 10, NormalizedRange { min: 20, max: 60, value: 20, fill_percent: 0 }),
            (20, 60, 40, NormalizedRange { min: 20, max: 60, value: 40, fill_percent: 50 }),
            (50, 50, 50, NormalizedRange { min: 50, max: 50, value: 50, fill_percent: 0 }),
            (100, 0, 25, NormalizedRange { min: 0, max: 100, value: 25, fill_percent: 25 }),
        ];

        for (min, max, value, expected) in cases {
            assert_eq!(normalize_range(min, max, value), expected);
        }
    }

    #[test]
    fn parsed_values_are_bounded_to_the_normalized_range() {
        assert_eq!(parse_slider_value("101", 0, 100), Some(100));
        assert_eq!(parse_slider_value("-1", 0, 100), Some(0));
        assert_eq!(parse_slider_value("42", 20, 60), Some(42));
        assert_eq!(parse_slider_value("", 0, 100), None);
        assert_eq!(parse_slider_value("invalid", 0, 100), None);
    }

    #[test]
    fn range_slider_controls_emit_direct_and_bounded_values_only_while_active() {
        let emitted = Rc::new(Cell::new(None));
        let captured = Rc::clone(&emitted);
        let callback = Callback::from(move |value| captured.set(Some(value)));
        let range = normalize_range(0, 100, 0);

        emit_slider_value(false, range, || "42".to_string(), &callback);
        assert_eq!(emitted.get(), Some(42));

        emit_slider_value(false, range, || "101".to_string(), &callback);
        assert_eq!(emitted.get(), Some(100));

        emitted.set(None);
        emit_slider_value(false, range, String::new, &callback);
        assert_eq!(emitted.get(), None);

        emitted.set(None);
        emit_slider_value(true, range, || panic!("disabled slider must not read the event value"), &callback);
        assert_eq!(emitted.get(), None);
    }
}

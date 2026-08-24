use crate::app::components::{resolve_field_id, FieldWrapper};
use shared::utils::{format_float_localized, parse_localized_float};
use web_sys::{HtmlInputElement, KeyboardEvent};
use yew::{prelude::*, TargetCast};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnsignedInputError {
    SyntaxOrOverflow,
    Range,
}

fn parse_unsigned_input(
    raw: &str,
    allow_empty: bool,
    min: Option<u64>,
    max: Option<u64>,
) -> Result<Option<u64>, UnsignedInputError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return if allow_empty { Ok(None) } else { Err(UnsignedInputError::SyntaxOrOverflow) };
    }

    let value = raw.parse::<u64>().map_err(|_| UnsignedInputError::SyntaxOrOverflow)?;
    if min.is_some_and(|min| value < min) || max.is_some_and(|max| value > max) {
        Err(UnsignedInputError::Range)
    } else {
        Ok(Some(value))
    }
}

#[derive(Properties, Clone, PartialEq)]
pub struct NumberInputProps {
    #[prop_or_default]
    pub name: String,
    #[prop_or_default]
    pub field_id: Option<String>,
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
    #[prop_or_default]
    pub min_i64: Option<i64>,
    #[prop_or_default]
    pub max_i64: Option<i64>,
    #[prop_or_default]
    pub u64_value: Option<u64>,
    #[prop_or_default]
    pub on_change_u64: Option<Callback<Option<u64>>>,
    #[prop_or_default]
    pub min_u64: Option<u64>,
    #[prop_or_default]
    pub max_u64: Option<u64>,
    #[prop_or_default]
    pub allow_empty_u64: bool,
    #[prop_or_default]
    pub on_invalid_u64: Option<Callback<UnsignedInputError>>,
}

#[component]
pub fn NumberInput(props: &NumberInputProps) -> Html {
    let input_ref = use_node_ref();
    let label_text = props.label.clone().unwrap_or_default();
    let resolved_field_id = resolve_field_id(&props.field_id, &props.name, &label_text);

    {
        let input_ref = input_ref.clone();
        let deps = (props.value, props.float_value, props.u64_value, props.on_change_u64.is_some());
        use_effect_with(deps, move |(int_val, float_val, u64_val, prefers_u64)| {
            if let Some(input) = input_ref.cast::<HtmlInputElement>() {
                let new_value = if *prefers_u64 {
                    u64_val.map(|value| value.to_string()).unwrap_or_default()
                } else {
                    float_val
                        .map(|f| format_float_localized(f, 4, true))
                        .or_else(|| int_val.map(|v| v.to_string()))
                        .unwrap_or_default()
                };
                input.set_value(&new_value);
            }
            || ()
        });
    }

    let prefers_float = props.on_change_float.is_some();
    let prefers_u64 = props.on_change_u64.is_some();
    let on_input = {
        let onchange_int = props.on_change.clone();
        let onchange_float = props.on_change_float.clone();
        let onchange_u64 = props.on_change_u64.clone();
        let on_invalid_u64 = props.on_invalid_u64.clone();
        let int_value = props.value;
        let min_i64 = props.min_i64;
        let max_i64 = props.max_i64;
        let u64_value = props.u64_value;
        let min_u64 = props.min_u64;
        let max_u64 = props.max_u64;
        let allow_empty_u64 = props.allow_empty_u64;
        Callback::from(move |e: InputEvent| {
            if let Some(input) = e.target_dyn_into::<HtmlInputElement>() {
                let raw = input.value().trim().to_string();
                if prefers_u64 {
                    match parse_unsigned_input(&raw, allow_empty_u64, min_u64, max_u64) {
                        Ok(value) => {
                            if let Some(cb) = onchange_u64.as_ref() {
                                cb.emit(value);
                            }
                        }
                        Err(error) => {
                            input.set_value(&u64_value.map(|value| value.to_string()).unwrap_or_default());
                            if let Some(cb) = on_invalid_u64.as_ref() {
                                cb.emit(error);
                            }
                        }
                    }
                    return;
                }

                if raw.is_empty() {
                    if prefers_float {
                        if let Some(cb) = onchange_float.as_ref() {
                            cb.emit(None);
                        }
                    } else if min_i64.is_some() || max_i64.is_some() {
                        input.set_value(&int_value.map(|value| value.to_string()).unwrap_or_default());
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
                    if min_i64.is_some() || max_i64.is_some() {
                        if parsed.is_some_and(|value| {
                            min_i64.is_none_or(|min| value >= min) && max_i64.is_none_or(|max| value <= max)
                        }) {
                            onchange_int.emit(parsed);
                        } else {
                            input.set_value(&int_value.map(|value| value.to_string()).unwrap_or_default());
                        }
                    } else {
                        onchange_int.emit(parsed);
                    }
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
        <FieldWrapper label={props.label.clone()} field_id={resolved_field_id.clone()}>
            <input
                id={resolved_field_id}
                ref={input_ref.clone()}
                type="text"
                name={props.name.clone()}
                placeholder={props.placeholder.clone()}
                onkeydown={handle_keydown.clone()}
                oninput={on_input.clone()}
            />
        </FieldWrapper>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_unsigned_input_handles_empty_values() {
        assert_eq!(parse_unsigned_input("", true, None, None), Ok(None));
        assert_eq!(parse_unsigned_input("", false, None, None), Err(UnsignedInputError::SyntaxOrOverflow));
    }

    #[test]
    fn parse_unsigned_input_accepts_u64_max() {
        assert_eq!(parse_unsigned_input("18446744073709551615", false, None, None), Ok(Some(u64::MAX)));
    }

    #[test]
    fn parse_unsigned_input_rejects_invalid_or_overflowing_values() {
        assert_eq!(
            parse_unsigned_input("18446744073709551616", false, None, None),
            Err(UnsignedInputError::SyntaxOrOverflow)
        );
        assert_eq!(parse_unsigned_input("-1", false, None, None), Err(UnsignedInputError::SyntaxOrOverflow));
    }

    #[test]
    fn parse_unsigned_input_enforces_inclusive_range() {
        assert_eq!(parse_unsigned_input("9", false, Some(10), None), Err(UnsignedInputError::Range));
        assert_eq!(parse_unsigned_input("101", false, None, Some(100)), Err(UnsignedInputError::Range));
        assert_eq!(parse_unsigned_input("10", false, Some(10), Some(10)), Ok(Some(10)));
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod browser_tests {
    use super::*;
    use gloo_timers::future::TimeoutFuture;
    use std::{cell::RefCell, rc::Rc};
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};
    use web_sys::{Element, Event, HtmlInputElement};
    use yew::{AppHandle, Renderer};

    wasm_bindgen_test_configure!(run_in_browser);

    async fn settle_render() {
        TimeoutFuture::new(0).await;
        TimeoutFuture::new(0).await;
    }

    async fn render_input(
        props: NumberInputProps,
    ) -> Result<(Element, HtmlInputElement, AppHandle<NumberInput>), wasm_bindgen::JsValue> {
        let document = gloo_utils::document();
        let body = document.body().ok_or_else(|| wasm_bindgen::JsValue::from_str("test document has no body"))?;
        let root = document.create_element("div")?;
        body.append_child(&root)?;
        let handle = Renderer::<NumberInput>::with_root_and_props(root.clone(), props).render();
        settle_render().await;
        let input = root
            .query_selector("input")?
            .ok_or_else(|| wasm_bindgen::JsValue::from_str("number input was not rendered"))?
            .dyn_into::<HtmlInputElement>()
            .map_err(|_| wasm_bindgen::JsValue::from_str("number input has the wrong element type"))?;
        Ok((root, input, handle))
    }

    fn dispatch_input(input: &HtmlInputElement, value: &str) -> Result<(), wasm_bindgen::JsValue> {
        input.set_value(value);
        input.dispatch_event(&Event::new("input")?)?;
        Ok(())
    }

    fn unsigned_props(
        value: u64,
        max: Option<u64>,
        on_change: Callback<Option<u64>>,
        on_invalid: Callback<UnsignedInputError>,
    ) -> NumberInputProps {
        NumberInputProps {
            name: "unsigned-test".to_string(),
            field_id: None,
            label: None,
            value: None,
            float_value: None,
            on_change: Callback::noop(),
            on_change_float: None,
            placeholder: String::new(),
            min_i64: None,
            max_i64: None,
            u64_value: Some(value),
            on_change_u64: Some(on_change),
            min_u64: None,
            max_u64: max,
            allow_empty_u64: false,
            on_invalid_u64: Some(on_invalid),
        }
    }

    fn bounded_signed_props(value: i64, on_change: Callback<Option<i64>>) -> NumberInputProps {
        NumberInputProps {
            name: "bounded-signed-test".to_string(),
            field_id: None,
            label: None,
            value: Some(value),
            float_value: None,
            on_change,
            on_change_float: None,
            placeholder: String::new(),
            min_i64: Some(i64::from(i8::MIN)),
            max_i64: Some(i64::from(i8::MAX)),
            u64_value: None,
            on_change_u64: None,
            min_u64: None,
            max_u64: None,
            allow_empty_u64: false,
            on_invalid_u64: None,
        }
    }

    #[wasm_bindgen_test(async)]
    async fn bounded_signed_mode_accepts_inclusive_min_and_max() -> Result<(), wasm_bindgen::JsValue> {
        let emitted = Rc::new(RefCell::new(Vec::new()));
        let props = bounded_signed_props(7, {
            let emitted = Rc::clone(&emitted);
            Callback::from(move |value| emitted.borrow_mut().push(value))
        });
        let (root, input, handle) = render_input(props).await?;

        dispatch_input(&input, "-128")?;
        dispatch_input(&input, "127")?;

        assert_eq!(&*emitted.borrow(), &[Some(-128), Some(127)]);
        handle.destroy();
        root.remove();
        Ok(())
    }

    #[wasm_bindgen_test(async)]
    async fn bounded_signed_mode_rolls_back_clear_range_and_overflow_without_emitting(
    ) -> Result<(), wasm_bindgen::JsValue> {
        for raw in ["", "-129", "128", "9223372036854775808"] {
            let emitted = Rc::new(RefCell::new(Vec::new()));
            let props = bounded_signed_props(7, {
                let emitted = Rc::clone(&emitted);
                Callback::from(move |value| emitted.borrow_mut().push(value))
            });
            let (root, input, handle) = render_input(props).await?;

            dispatch_input(&input, raw)?;

            assert_eq!(input.value(), "7");
            assert!(emitted.borrow().is_empty());
            handle.destroy();
            root.remove();
        }
        Ok(())
    }

    #[wasm_bindgen_test(async)]
    async fn unsigned_mode_emits_u64_max() -> Result<(), wasm_bindgen::JsValue> {
        let emitted = Rc::new(RefCell::new(Vec::new()));
        let props = unsigned_props(
            0,
            None,
            {
                let emitted = Rc::clone(&emitted);
                Callback::from(move |value| emitted.borrow_mut().push(value))
            },
            Callback::noop(),
        );
        let (root, input, handle) = render_input(props).await?;

        dispatch_input(&input, "18446744073709551615")?;

        assert_eq!(&*emitted.borrow(), &[Some(u64::MAX)]);
        handle.destroy();
        root.remove();
        Ok(())
    }

    #[wasm_bindgen_test(async)]
    async fn unsigned_mode_rolls_back_invalid_values_without_emitting() -> Result<(), wasm_bindgen::JsValue> {
        for (raw, max, error) in [
            ("18446744073709551616", None, UnsignedInputError::SyntaxOrOverflow),
            ("-1", None, UnsignedInputError::SyntaxOrOverflow),
            ("101", Some(100), UnsignedInputError::Range),
        ] {
            let emitted = Rc::new(RefCell::new(Vec::new()));
            let invalid = Rc::new(RefCell::new(Vec::new()));
            let props = unsigned_props(
                50,
                max,
                {
                    let emitted = Rc::clone(&emitted);
                    Callback::from(move |value| emitted.borrow_mut().push(value))
                },
                {
                    let invalid = Rc::clone(&invalid);
                    Callback::from(move |kind| invalid.borrow_mut().push(kind))
                },
            );
            let (root, input, handle) = render_input(props).await?;

            dispatch_input(&input, raw)?;

            assert_eq!(input.value(), "50");
            assert!(emitted.borrow().is_empty());
            assert_eq!(&*invalid.borrow(), &[error]);
            handle.destroy();
            root.remove();
        }
        Ok(())
    }

    #[wasm_bindgen_test(async)]
    async fn unsigned_mode_rejects_values_above_wasm_usize_max_without_dispatch() -> Result<(), wasm_bindgen::JsValue> {
        let dispatched = Rc::new(RefCell::new(Vec::<usize>::new()));
        let props = unsigned_props(
            7,
            Some(usize::MAX as u64),
            {
                let dispatched = Rc::clone(&dispatched);
                Callback::from(move |value: Option<u64>| {
                    if let Some(value) = value.and_then(|value| usize::try_from(value).ok()) {
                        dispatched.borrow_mut().push(value);
                    }
                })
            },
            Callback::noop(),
        );
        let (root, input, handle) = render_input(props).await?;

        let above_usize_max = (usize::MAX as u64) + 1;
        assert!(usize::try_from(above_usize_max).is_err());
        dispatch_input(&input, &above_usize_max.to_string())?;

        assert_eq!(input.value(), "7");
        assert!(dispatched.borrow().is_empty());
        handle.destroy();
        root.remove();
        Ok(())
    }
}

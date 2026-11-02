use crate::app::components::{resolve_field_id, FieldWrapper};
use gloo_timers::callback::Timeout;
use web_sys::{FocusEvent, HtmlInputElement, InputEvent, KeyboardEvent, MouseEvent};
use yew::{
    classes, component, html, use_effect_with, use_mut_ref, use_node_ref, use_state, Callback, Html, Properties,
    TargetCast,
};

#[derive(Clone, PartialEq, Debug)]
pub struct AutocompleteOption {
    pub value: String,
    pub label: String,
    pub detail: Option<String>,
}

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct AutocompleteInputProps {
    #[prop_or_default]
    pub name: String,
    #[prop_or_default]
    pub field_id: Option<String>,
    #[prop_or_default]
    pub label: Option<String>,
    #[prop_or_default]
    pub value: String,
    #[prop_or_default]
    pub placeholder: Option<String>,
    #[prop_or_default]
    pub suggestions: Vec<AutocompleteOption>,
    #[prop_or_default]
    pub loading: bool,
    #[prop_or_default]
    pub disabled: bool,
    #[prop_or_default]
    pub required: bool,
    #[prop_or_default]
    pub error: Option<String>,
    #[prop_or_default]
    pub hint_key: Option<String>,
    #[prop_or_default]
    pub aria_label: Option<String>,
    #[prop_or_default]
    pub on_query: Callback<String>,
    #[prop_or_default]
    pub on_select: Callback<String>,
}

#[component]
pub fn AutocompleteInput(props: &AutocompleteInputProps) -> Html {
    let input_ref = use_node_ref();
    let label_text = props.label.clone().unwrap_or_default();
    let resolved_field_id = resolve_field_id(&props.field_id, &props.name, &label_text);

    let query = use_state(|| props.value.clone());
    let is_open = use_state(|| false);
    let is_focused = use_state(|| false);
    let selected_index = use_state(|| None::<usize>);
    let blur_handle = use_mut_ref(|| None::<Timeout>);

    // Clean up pending blur timer on unmount
    {
        let blur_handle = blur_handle.clone();
        use_effect_with((), move |()| {
            move || {
                *blur_handle.borrow_mut() = None;
            }
        });
    }

    // Synchronize query when props.value changes externally
    {
        let query = query.clone();
        let input_ref = input_ref.clone();
        use_effect_with(props.value.clone(), move |val| {
            query.set(val.clone());
            if let Some(input) = input_ref.cast::<HtmlInputElement>() {
                input.set_value(val);
            }
            || ()
        });
    }

    // Open dropdown when new suggestions arrive while focused
    {
        let is_open = is_open.clone();
        let selected_index = selected_index.clone();
        let is_focused = is_focused.clone();
        use_effect_with((props.suggestions.clone(), props.loading), move |(suggestions, loading)| {
            if !*loading && !suggestions.is_empty() && *is_focused {
                is_open.set(true);
                selected_index.set(None);
            } else if suggestions.is_empty() {
                is_open.set(false);
            }
            || ()
        });
    }

    let handle_oninput = {
        let query = query.clone();
        let is_open = is_open.clone();
        let selected_index = selected_index.clone();
        let on_query = props.on_query.clone();
        Callback::from(move |e: InputEvent| {
            let Some(input) = e.target_dyn_into::<HtmlInputElement>() else {
                return;
            };
            let text = input.value();
            query.set(text.clone());
            selected_index.set(None);
            if text.trim().is_empty() {
                is_open.set(false);
            }
            on_query.emit(text);
        })
    };

    let handle_onfocus = {
        let is_focused = is_focused.clone();
        let is_open = is_open.clone();
        let suggestions_len = props.suggestions.len();
        Callback::from(move |_: FocusEvent| {
            is_focused.set(true);
            if suggestions_len > 0 {
                is_open.set(true);
            }
        })
    };

    let handle_onblur = {
        let is_focused = is_focused.clone();
        let is_open = is_open.clone();
        let blur_handle = blur_handle.clone();
        let query = query.clone();
        let on_select = props.on_select.clone();
        let current_value = props.value.clone();
        Callback::from(move |_: FocusEvent| {
            is_focused.set(false);
            if *query != current_value {
                on_select.emit((*query).clone());
            }
            let is_open = is_open.clone();
            let timeout = Timeout::new(200, move || {
                is_open.set(false);
            });
            *blur_handle.borrow_mut() = Some(timeout);
        })
    };

    let handle_onkeydown = {
        let is_open = is_open.clone();
        let selected_index = selected_index.clone();
        let query = query.clone();
        let on_select = props.on_select.clone();
        let suggestions = props.suggestions.clone();
        let input_ref = input_ref.clone();
        Callback::from(move |e: KeyboardEvent| match e.key().as_str() {
            "ArrowDown" => {
                e.prevent_default();
                if !*is_open && !suggestions.is_empty() {
                    is_open.set(true);
                    selected_index.set(Some(0));
                } else if *is_open && !suggestions.is_empty() {
                    let next = match *selected_index {
                        Some(idx) if idx + 1 < suggestions.len() => Some(idx + 1),
                        Some(_) | None => Some(0),
                    };
                    selected_index.set(next);
                }
            }
            "ArrowUp" => {
                e.prevent_default();
                if *is_open && !suggestions.is_empty() {
                    let prev = match *selected_index {
                        Some(0) | None => Some(suggestions.len().saturating_sub(1)),
                        Some(idx) => Some(idx - 1),
                    };
                    selected_index.set(prev);
                }
            }
            "Enter" => {
                e.prevent_default();
                if *is_open {
                    if let Some(idx) = *selected_index {
                        if let Some(item) = suggestions.get(idx) {
                            query.set(item.value.clone());
                            if let Some(input) = input_ref.cast::<HtmlInputElement>() {
                                input.set_value(&item.value);
                            }
                            on_select.emit(item.value.clone());
                            is_open.set(false);
                            return;
                        }
                    }
                }
                on_select.emit((*query).clone());
                is_open.set(false);
            }
            "Escape" if *is_open => {
                e.prevent_default();
                is_open.set(false);
            }
            _ => {}
        })
    };

    let make_on_suggestion_mousedown = |item_val: String| {
        let query = query.clone();
        let on_select = props.on_select.clone();
        let is_open = is_open.clone();
        let blur_handle = blur_handle.clone();
        let input_ref = input_ref.clone();
        Callback::from(move |e: MouseEvent| {
            e.prevent_default();
            *blur_handle.borrow_mut() = None;
            query.set(item_val.clone());
            if let Some(input) = input_ref.cast::<HtmlInputElement>() {
                input.set_value(&item_val);
            }
            on_select.emit(item_val.clone());
            is_open.set(false);
        })
    };

    let listbox_id = format!("tp-autocomplete-{resolved_field_id}");
    let active_descendant = selected_index.map(|idx| format!("{listbox_id}-opt-{idx}"));
    let aria_label =
        if props.label.is_some() { None } else { props.aria_label.clone().or_else(|| props.placeholder.clone()) };

    html! {
        <div class="tp__autocomplete">
            <FieldWrapper
                label={props.label.clone()}
                field_id={resolved_field_id.clone()}
                hint_key={props.hint_key.clone()}
                required={props.required}
                error={props.error.clone()}
            >
                <input
                    id={resolved_field_id}
                    ref={input_ref.clone()}
                    type="text"
                    name={props.name.clone()}
                    autocomplete="off"
                    role="combobox"
                    aria-autocomplete="list"
                    aria-expanded={is_open.to_string()}
                    aria-controls={listbox_id.clone()}
                    aria-activedescendant={active_descendant}
                    aria-label={aria_label}
                    aria-required={props.required.then(|| "true".to_string())}
                    aria-invalid={props.error.as_ref().map(|_| "true".to_string())}
                    placeholder={props.placeholder.clone()}
                    disabled={props.disabled}
                    value={(*query).clone()}
                    oninput={handle_oninput}
                    onfocus={handle_onfocus}
                    onblur={handle_onblur}
                    onkeydown={handle_onkeydown}
                />
                {
                    if props.loading {
                        html! { <span class="tp__autocomplete-spinner">{ "⏳" }</span> }
                    } else {
                        html! {}
                    }
                }
            </FieldWrapper>

            {
                if *is_open && !props.suggestions.is_empty() {
                    html! {
                        <ul class="tp__autocomplete-dropdown" id={listbox_id.clone()} role="listbox">
                            {
                                for props.suggestions.iter().enumerate().map(|(idx, opt)| {
                                    let is_active = *selected_index == Some(idx);
                                    let on_mousedown = make_on_suggestion_mousedown(opt.value.clone());
                                    let opt_id = format!("{listbox_id}-opt-{idx}");
                                    html! {
                                        <li
                                            id={opt_id}
                                            key={opt.value.clone()}
                                            class={classes!("tp__autocomplete-item", if is_active { "tp__autocomplete-item--active" } else { "" })}
                                            role="option"
                                            aria-selected={is_active.to_string()}
                                            onmousedown={on_mousedown}
                                        >
                                            <span class="tp__autocomplete-item-name">{ &opt.label }</span>
                                            {
                                              if let Some(ref detail) = opt.detail {
                                                  html! { <small class="tp__autocomplete-item-id">{ detail }</small> }
                                              } else {
                                                  html! {}
                                              }
                                            }
                                        </li>
                                    }
                                })
                            }
                        </ul>
                    }
                } else {
                    html! {}
                }
            }
        </div>
    }
}

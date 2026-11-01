use crate::{hooks::use_service_context, i18n::use_translation};
use gloo_timers::callback::Timeout;
use shared::model::EpgSearchResultDto;
use web_sys::{HtmlInputElement, KeyboardEvent, MouseEvent};
use yew::{platform::spawn_local, prelude::*};

#[derive(Properties, PartialEq, Clone)]
pub struct EpgPickerProps {
    pub value: String,
    pub on_select: Callback<String>,
    #[prop_or_default]
    pub target_id: Option<u16>,
    #[prop_or_default]
    pub input: Option<String>,
    #[prop_or(false)]
    pub disabled: bool,
    #[prop_or_default]
    pub field_id: Option<String>,
    #[prop_or_default]
    pub placeholder: Option<String>,
}

#[component]
pub fn EpgPicker(props: &EpgPickerProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();

    let query = use_state(|| props.value.clone());
    let results = use_state(Vec::<EpgSearchResultDto>::new);
    let is_open = use_state(|| false);
    let is_searching = use_state(|| false);
    let selected_index = use_state(|| None::<usize>);
    let debounce_handle = use_mut_ref(|| None::<Timeout>);
    let blur_handle = use_mut_ref(|| None::<Timeout>);
    let container_ref = use_node_ref();

    // Dropping a `Timeout` cancels it; leaving the panel must not keep a pending search alive.
    {
        let debounce_handle = debounce_handle.clone();
        let blur_handle = blur_handle.clone();
        use_effect_with((), move |()| {
            move || {
                *debounce_handle.borrow_mut() = None;
                *blur_handle.borrow_mut() = None;
            }
        });
    }

    // Synchronize query when props.value changes externally
    {
        let query = query.clone();
        use_effect_with(props.value.clone(), move |val| {
            query.set(val.clone());
            || ()
        });
    }

    let on_input = {
        let query = query.clone();
        let on_select = props.on_select.clone();
        let results = results.clone();
        let is_open = is_open.clone();
        let is_searching = is_searching.clone();
        let selected_index = selected_index.clone();
        let debounce_handle = debounce_handle.clone();
        let services = services.clone();
        let translate = translate.clone();
        let target_id = props.target_id;
        let input = props.input.clone();

        Callback::from(move |e: InputEvent| {
            let Some(input_el) = e.target_dyn_into::<HtmlInputElement>() else {
                return;
            };
            let text = input_el.value();
            query.set(text.clone());
            on_select.emit(text.clone());

            *debounce_handle.borrow_mut() = None;

            if text.trim().is_empty() {
                results.set(Vec::new());
                is_open.set(false);
                selected_index.set(None);
                is_searching.set(false);
                return;
            }

            let results = results.clone();
            let is_open = is_open.clone();
            let is_searching = is_searching.clone();
            let selected_index = selected_index.clone();
            let services = services.clone();
            let translate = translate.clone();
            let input = input.clone();
            let search_query = text.clone();

            let timeout = Timeout::new(300, move || {
                is_searching.set(true);
                spawn_local(async move {
                    let res = services.playlist.search_epg(&search_query, target_id, input.as_deref(), Some(50)).await;
                    match res {
                        Ok(items) => {
                            let has_items = !items.is_empty();
                            results.set(items);
                            is_open.set(has_items);
                            selected_index.set(None);
                        }
                        Err(err) => {
                            // An empty dropdown would look like "no matches"; say what happened.
                            results.set(Vec::new());
                            is_open.set(false);
                            services.toastr.error(
                                translate
                                    .t("MESSAGES.PLAYLIST_EXPLORER.EPG_SEARCH_FAILED")
                                    .replace("{error}", &err.to_string()),
                            );
                        }
                    }
                    is_searching.set(false);
                });
            });

            *debounce_handle.borrow_mut() = Some(timeout);
        })
    };

    let on_focus = {
        let results = results.clone();
        let is_open = is_open.clone();
        Callback::from(move |_: FocusEvent| {
            if !results.is_empty() {
                is_open.set(true);
            }
        })
    };

    let on_blur = {
        let is_open = is_open.clone();
        let blur_handle = blur_handle.clone();
        Callback::from(move |_: FocusEvent| {
            let is_open = is_open.clone();
            // Short timeout so item clicks register before closing; storing it keeps it
            // cancelable and cleaned up on unmount.
            let timeout = Timeout::new(200, move || {
                is_open.set(false);
            });
            *blur_handle.borrow_mut() = Some(timeout);
        })
    };

    let on_key_down = {
        let is_open = is_open.clone();
        let results = results.clone();
        let selected_index = selected_index.clone();
        let query = query.clone();
        let on_select = props.on_select.clone();

        Callback::from(move |e: KeyboardEvent| match e.key().as_str() {
            "ArrowDown" => {
                e.prevent_default();
                if !*is_open && !results.is_empty() {
                    is_open.set(true);
                    selected_index.set(Some(0));
                } else if *is_open && !results.is_empty() {
                    let next = match *selected_index {
                        Some(idx) if idx + 1 < results.len() => Some(idx + 1),
                        Some(_) => Some(0),
                        None => Some(0),
                    };
                    selected_index.set(next);
                }
            }
            "ArrowUp" => {
                e.prevent_default();
                if *is_open && !results.is_empty() {
                    let prev = match *selected_index {
                        Some(0) | None => Some(results.len().saturating_sub(1)),
                        Some(idx) => Some(idx - 1),
                    };
                    selected_index.set(prev);
                }
            }
            "Enter" if *is_open => {
                if let Some(idx) = *selected_index {
                    if let Some(item) = results.get(idx) {
                        e.prevent_default();
                        query.set(item.id.clone());
                        on_select.emit(item.id.clone());
                        is_open.set(false);
                    }
                }
            }
            "Escape" if *is_open => {
                e.prevent_default();
                is_open.set(false);
            }
            _ => {}
        })
    };

    let placeholder = props.placeholder.clone().unwrap_or_else(|| translate.t("LABEL.EXPLORER_SEARCH_EPG_HINT"));
    let listbox_id = format!("tp-epg-picker-{}", props.field_id.as_deref().unwrap_or("listbox"));

    html! {
        <div ref={container_ref} class="tp__epg-picker">
            <div class="tp__epg-picker__input-wrap">
                <input
                    type="text"
                    class="tp__input tp__epg-picker__input"
                    role="combobox"
                    aria-autocomplete="list"
                    aria-expanded={is_open.to_string()}
                    aria-controls={listbox_id.clone()}
                    aria-label={translate.t("LABEL.EPG_CHANNEL_ID")}
                    value={(*query).clone()}
                    placeholder={placeholder}
                    disabled={props.disabled}
                    oninput={on_input}
                    onfocus={on_focus}
                    onblur={on_blur}
                    onkeydown={on_key_down}
                />
                {
                    if *is_searching {
                        html! {
                            <span class="tp__epg-picker__spinner">{ "⏳" }</span>
                        }
                    } else {
                        html! {}
                    }
                }
            </div>

            {
                if *is_open && !results.is_empty() {
                    html! {
                        <ul class="tp__epg-picker__dropdown" id={listbox_id.clone()} role="listbox">
                            {
                                for results.iter().enumerate().map(|(idx, item)| {
                                    let is_active = *selected_index == Some(idx);
                                    let item_id = item.id.clone();
                                    let on_item_click = {
                                        let query = query.clone();
                                        let on_select = props.on_select.clone();
                                        let is_open = is_open.clone();
                                        let id = item_id.clone();
                                        Callback::from(move |e: MouseEvent| {
                                            e.prevent_default();
                                            query.set(id.clone());
                                            on_select.emit(id.clone());
                                            is_open.set(false);
                                        })
                                    };

                                    html! {
                                        <li
                                            key={item.id.clone()}
                                            class={classes!("tp__epg-picker__item", if is_active { "tp__epg-picker__item--active" } else { "" })}
                                            role="option"
                                            aria-selected={is_active.to_string()}
                                            onmousedown={on_item_click}
                                        >
                                            <span class="tp__epg-picker__item-name">{ &item.display_name }</span>
                                            <small class="tp__epg-picker__item-id">{ &item.id }</small>
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

use crate::{
    app::components::{AppIcon, DropDownIconButton, DropDownOption, DropDownSelection, IconButton},
    hooks::{is_text_input_focused, use_key_down},
    html_if,
    i18n::use_translation,
};
use gloo_timers::callback::Timeout;
use shared::model::SearchRequest;
use std::rc::Rc;
use web_sys::HtmlInputElement;
use yew::prelude::*;

const DEBOUNCE_TIMEOUT_MS: u32 = 500;

type SearchEmitter = Rc<dyn Fn(Option<Rc<Vec<String>>>)>;

enum RegexState {
    Active,
    Inactive,
    Invalid,
}

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct SearchProps {
    #[prop_or_default]
    pub class: String,
    #[prop_or_default]
    pub options: Option<Rc<Vec<DropDownOption>>>,
    pub onsearch: Option<Callback<SearchRequest>>,
    #[prop_or_default]
    pub on_fields_change: Option<Callback<Option<Rc<Vec<String>>>>>,
    #[prop_or(3)]
    pub min_length: usize,
}

#[component]
pub fn Search(props: &SearchProps) -> Html {
    let translate = use_translation();
    let search_fields = use_state(|| {
        // Preselected options (e.g. restored from local storage) apply immediately.
        props.options.as_ref().and_then(|options| {
            let selected: Vec<String> =
                options.iter().filter(|option| option.selected).map(|option| option.id.clone()).collect();
            if selected.is_empty() {
                None
            } else {
                Some(Rc::new(selected))
            }
        })
    });
    let input_ref = use_node_ref();
    let invalid_search = use_state(|| false);
    let regex_active = use_state(|| RegexState::Inactive);

    // Global '/' shortcut focuses the search input
    {
        let input = input_ref.clone();
        use_key_down((), move |event: &KeyboardEvent| {
            if event.key() == "/" && !is_text_input_focused(event) {
                if let Some(input) = input.cast::<HtmlInputElement>() {
                    event.prevent_default();
                    let _ = input.focus();
                }
            }
        });
    }

    let handle_regex_click = {
        let regex_active = regex_active.clone();
        let input = input_ref.clone();
        Callback::from(move |_: (String, MouseEvent)| match *regex_active {
            RegexState::Active | RegexState::Invalid => {
                regex_active.set(RegexState::Inactive);
            }
            RegexState::Inactive => {
                if let Some(input) = input.cast::<HtmlInputElement>() {
                    let text = input.value();
                    if shared::model::REGEX_CACHE.get_or_compile(&text).is_ok() {
                        regex_active.set(RegexState::Active);
                    } else {
                        regex_active.set(RegexState::Invalid);
                    }
                    shared::model::REGEX_CACHE.sweep();
                }
            }
        })
    };

    let debounce_timeout = use_mut_ref(|| None::<Timeout>);

    let emit_search: SearchEmitter = {
        let on_search = props.onsearch.clone();
        let min_length = props.min_length;
        let invalid_search = invalid_search.clone();
        let input = input_ref.clone();
        let regex = regex_active.clone();
        Rc::new(move |selected_fields: Option<Rc<Vec<String>>>| {
            invalid_search.set(false);
            if let Some(cb_search) = on_search.as_ref() {
                if let Some(input) = input.cast::<HtmlInputElement>() {
                    let text = input.value();
                    if text.len() >= min_length {
                        if matches!(*regex, RegexState::Inactive) {
                            cb_search.emit(SearchRequest::Text(text, selected_fields));
                        } else if shared::model::REGEX_CACHE.get_or_compile(&text).is_ok() {
                            regex.set(RegexState::Active);
                            cb_search.emit(SearchRequest::Regexp(text, selected_fields));
                        } else {
                            regex.set(RegexState::Invalid);
                        }
                    } else if text.is_empty() {
                        cb_search.emit(SearchRequest::Clear);
                    } else {
                        invalid_search.set(true);
                    }
                }
            }
        })
    };

    let handle_key_down = {
        let emit_search = emit_search.clone();
        let search_fields = search_fields.clone();
        Callback::from(move |e: KeyboardEvent| {
            if let Some(timeout) = debounce_timeout.borrow_mut().take() {
                timeout.cancel();
            }

            if e.code() == "Enter" {
                emit_search((*search_fields).clone());
            } else {
                let emit_search = emit_search.clone();
                let selected_fields = (*search_fields).clone();
                *debounce_timeout.borrow_mut() = Some(Timeout::new(DEBOUNCE_TIMEOUT_MS, move || {
                    emit_search(selected_fields);
                }));
            }
        })
    };

    let handle_options_click = {
        let search_fields = search_fields.clone();
        let emit_search = emit_search.clone();
        let on_fields_change = props.on_fields_change.clone();
        Callback::from(move |(_name, selections)| {
            let selected = match selections {
                DropDownSelection::Empty => None,
                DropDownSelection::Multi(options) => Some(Rc::new(options)),
                DropDownSelection::Single(option) => Some(Rc::new(vec![option])),
            };
            search_fields.set(selected.clone());
            if let Some(cb_fields) = on_fields_change.as_ref() {
                cb_fields.emit(selected.clone());
            }
            emit_search(selected);
        })
    };

    html! {
        <div class={classes!("tp__search", if *invalid_search { "invalid" } else { "" })}>
            <div class="tp__search-wrapper">
               <AppIcon name="Search" />
                <input ref={input_ref.clone()} type="text"
                    name="search"
                    autocomplete={"on"}
                    placeholder={translate.t("LABEL.SEARCH")}
                    aria-label={translate.t("LABEL.SEARCH")}
                    aria-invalid={(*invalid_search || matches!(*regex_active, RegexState::Invalid)).to_string()}
                    onkeydown={handle_key_down}
                    />
                <IconButton class={match *regex_active {
                    RegexState::Active => "option-active",
                    RegexState::Invalid => "option-invalid",
                    RegexState::Inactive => ""}}
                 name="regex" icon="Regexp"
                 hint={translate.t("LABEL.REGEXP")}
                 aria_label={translate.t("LABEL.REGEXP")}
                 onclick={handle_regex_click} />
                {
                  html_if!(
                    props.options.is_some(),
                     {
                      <DropDownIconButton multi_select={true}
                        class={if search_fields.as_ref().is_some_and(|fields| !fields.is_empty()) { "option-active" } else { "" }}
                        options={props.options.as_ref().unwrap().clone()} name="fields" icon="Popup" on_select={handle_options_click} />
                     }
                  )
                }
            </div>
        </div>
    }
}

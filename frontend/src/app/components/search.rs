use std::rc::Rc;
use gloo_timers::callback::Timeout;
use web_sys::HtmlInputElement;
use yew::prelude::*;
use shared::model::SearchRequest;
use crate::app::components::{AppIcon, DropDownIconButton, DropDownOption, DropDownSelection, IconButton};
use crate::html_if;

const DEBOUNCE_TIMEOUT_MS:  u32 = 500;

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
    #[prop_or(3)]
    pub min_length: usize,
}

#[function_component]
pub fn Search(props: &SearchProps) -> Html {

    let search_fields = use_state(|| None::<Rc<Vec<String>>>);
    let input_ref = use_node_ref();
    let invalid_search = use_state(|| false);
    let regex_active = use_state(|| RegexState::Inactive);

    let handle_regex_click = {
        let regex_active = regex_active.clone();
        let input = input_ref.clone();
        Callback::from(move |_: (String, MouseEvent)| {
            match *regex_active {
                RegexState::Active
                | RegexState::Invalid => {
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
                    }
                }
            }
        })
    };


    let debounce_timeout = use_mut_ref(|| None::<Timeout>);

    let handle_key_down = {
        let regex = regex_active.clone();
        let input = input_ref.clone();
        let on_search = props.onsearch.clone();
        let search_fields = search_fields.clone();
        let min_length = props.min_length;
        let invalid_search = invalid_search.clone();
        Callback::from(move |e: KeyboardEvent| {
            let regex = regex.clone();
            let input = input.clone();
            let on_search = on_search.clone();
            if let Some(timeout) = debounce_timeout.borrow_mut().take() {
                timeout.cancel();
            }

            let search_fields = search_fields.clone();
            let invalid_search = invalid_search.clone();
            invalid_search.set(false);
            let do_search = move || {
                if let Some(cb_search) = on_search.as_ref() {
                    if let Some(input) = input.cast::<HtmlInputElement>() {
                        let text = input.value();
                        if text.len() >= min_length {
                            if !matches!(*regex, RegexState::Inactive) {
                                if shared::model::REGEX_CACHE.get_or_compile(&text).is_ok() {
                                    regex.set(RegexState::Active);
                                    cb_search.emit(SearchRequest::Regexp(text, (*search_fields).clone()));
                                } else {
                                    regex.set(RegexState::Invalid);
                                }
                            } else {
                                cb_search.emit(SearchRequest::Text(text, (*search_fields).clone()));
                            }
                        } else if text.is_empty() {
                            cb_search.emit(SearchRequest::Clear);
                        } else {
                            invalid_search.set(true);
                        }
                    }
                }
            };

            if e.code() == "Enter" {
                do_search();
            } else {
                // Set new timeout
                *debounce_timeout.borrow_mut() = Some(Timeout::new(DEBOUNCE_TIMEOUT_MS, move || {
                    do_search();
                }));
            }
        })
    };

    let handle_options_click = {
        let search_fields = search_fields.clone();
        Callback::from(move |(_name, selections)| {
            match selections {
                DropDownSelection::Empty => {
                    search_fields.set(None);
                }
                DropDownSelection::Multi(options) => {
                    search_fields.set(Some(Rc::new(options)));
                }
                DropDownSelection::Single(option) => {
                    search_fields.set(Some(Rc::new(vec![option])));
                }
            }
        })
    };

    html! {
        <div class={classes!("tp__search", if *invalid_search { "invalid" } else { "" })}>
            <div class="tp__search-wrapper">
               <AppIcon name="Search" />
                <input ref={input_ref.clone()} type="text"
                    name="search"
                    autocomplete={"on"}
                    onkeydown={handle_key_down}
                    />
                <IconButton class={match *regex_active {
                    RegexState::Active => "option-active",
                    RegexState::Invalid => "option-invalid",
                    RegexState::Inactive => ""}}
                 name="regex" icon="Regexp" onclick={handle_regex_click} />
                {
                  html_if!(
                    props.options.is_some(),
                     {
                      <DropDownIconButton multi_select={true} options={props.options.as_ref().unwrap().clone()} name="fields" icon="Popup" on_select={handle_options_click} />
                     }
                  )
                }
            </div>
        </div>
    }
}
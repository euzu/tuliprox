use crate::{
    app::components::{AutocompleteInput, AutocompleteOption},
    hooks::use_service_context,
    i18n::use_translation,
};
use gloo_timers::callback::Timeout;
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
    pub label: Option<String>,
    #[prop_or_default]
    pub placeholder: Option<String>,
}

#[component]
pub fn EpgPicker(props: &EpgPickerProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();

    let suggestions = use_state(Vec::<AutocompleteOption>::new);
    let is_searching = use_state(|| false);
    let debounce_handle = use_mut_ref(|| None::<Timeout>);
    let search_generation = use_mut_ref(|| 0u64);

    // Dropping a Timeout cancels it; leaving the panel cancels pending search.
    {
        let debounce_handle = debounce_handle.clone();
        use_effect_with((), move |()| {
            move || {
                *debounce_handle.borrow_mut() = None;
            }
        });
    }

    let on_query = {
        let suggestions = suggestions.clone();
        let is_searching = is_searching.clone();
        let debounce_handle = debounce_handle.clone();
        let search_generation = search_generation.clone();
        let services = services.clone();
        let translate = translate.clone();
        let target_id = props.target_id;
        let input = props.input.clone();

        Callback::from(move |text: String| {
            *debounce_handle.borrow_mut() = None;
            let current_gen = {
                let mut g = search_generation.borrow_mut();
                *g = g.wrapping_add(1);
                *g
            };

            if text.trim().is_empty() {
                suggestions.set(Vec::new());
                is_searching.set(false);
                return;
            }

            let suggestions = suggestions.clone();
            let is_searching = is_searching.clone();
            let search_generation = search_generation.clone();
            let services = services.clone();
            let translate = translate.clone();
            let input = input.clone();
            let search_query = text;

            let timeout = Timeout::new(300, move || {
                is_searching.set(true);
                spawn_local(async move {
                    let res = services.playlist.search_epg(&search_query, target_id, input.as_deref(), Some(50)).await;
                    if *search_generation.borrow() != current_gen {
                        return;
                    }
                    match res {
                        Ok(items) => {
                            let mapped: Vec<AutocompleteOption> = items
                                .into_iter()
                                .map(|item| AutocompleteOption {
                                    value: item.id.clone(),
                                    label: item.display_name,
                                    detail: Some(item.id),
                                })
                                .collect();
                            suggestions.set(mapped);
                        }
                        Err(err) => {
                            suggestions.set(Vec::new());
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

    let placeholder = props.placeholder.clone().unwrap_or_else(|| translate.t("LABEL.EXPLORER_SEARCH_EPG_HINT"));

    html! {
        <AutocompleteInput
            name="epg_channel_id"
            field_id={props.field_id.clone()}
            label={props.label.clone()}
            value={props.value.clone()}
            placeholder={placeholder}
            aria_label={Some(translate.t("LABEL.EPG_CHANNEL_ID"))}
            disabled={props.disabled}
            suggestions={(*suggestions).clone()}
            loading={*is_searching}
            {on_query}
            on_select={props.on_select.clone()}
        />
    }
}

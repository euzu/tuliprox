use super::parse_filter_preview;
use crate::{
    app::{
        components::{AppIcon, ContentDialog, FilterEditor, FilterView},
        ConfigContext,
    },
    hooks::use_service_context,
    model::{DialogAction, DialogActions, DialogResult},
};
use shared::{foundation::get_filter, model::PatternTemplate};
use web_sys::window;
use yew::{create_portal, prelude::*};

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct FilterInputProps {
    #[prop_or_default]
    pub icon: String,
    #[prop_or_default]
    pub filter: Option<String>,
    #[prop_or_default]
    pub on_change: Callback<Option<String>>,
    #[prop_or_default]
    pub validate_on_server: bool,
}

#[component]
pub fn FilterInput(props: &FilterInputProps) -> Html {
    let services = use_service_context();
    let config_ctx = use_context::<ConfigContext>().expect("Config context not found");

    let filter_state = use_state(|| None);
    let parsed_filter_state = use_state(|| None);
    let templates_state = use_state(|| None);
    let dialog_open = use_state(|| false);
    let editor_filter_state = use_state(|| None);
    let editor_templates_state = use_state(|| None);
    let editor_valid_state = use_state(|| true);
    let latest_filter = use_mut_ref(|| props.filter.clone());
    let validating = use_state(|| false);
    let validation_pending = use_mut_ref(|| false);

    {
        let templates = templates_state.clone();
        let cfg_templates = config_ctx.config.as_ref().and_then(|c| {
            c.templates.as_ref().map(|definition| definition.templates.clone()).or_else(|| c.sources.templates.clone())
        });
        use_effect_with(cfg_templates, move |templ| {
            templates.set(templ.clone());
        });
    }

    {
        let filter = filter_state.clone();
        use_effect_with(props.filter.clone(), move |flt| {
            filter.set(flt.clone());
        });
    }

    {
        let parsed_filter = parsed_filter_state.clone();
        use_effect_with(((*filter_state).clone(), (*templates_state).clone()), move |(flt, templates)| {
            let parsed =
                if let Some(new_fltr) = flt.as_ref() { get_filter(new_fltr, templates.as_deref()).ok() } else { None };
            parsed_filter.set(parsed);
        });
    }

    let handle_templates_edit = {
        let templates = editor_templates_state.clone();
        Callback::from(move |templ: Option<Vec<PatternTemplate>>| {
            templates.set(templ);
        })
    };

    let handle_click = {
        let filter_state = filter_state.clone();
        let templates_state = templates_state.clone();
        let dialog_open = dialog_open.clone();
        let editor_filter_state = editor_filter_state.clone();
        let editor_templates_state = editor_templates_state.clone();
        let editor_valid_state = editor_valid_state.clone();
        let latest_filter = latest_filter.clone();
        Callback::from(move |e: MouseEvent| {
            e.prevent_default();
            e.stop_propagation();
            let current_filter = (*filter_state).clone();
            let current_templates = (*templates_state).clone();
            let (_, valid) = parse_filter_preview(current_filter.as_deref(), current_templates.as_deref());
            latest_filter.replace(current_filter.clone());
            editor_filter_state.set(current_filter);
            editor_templates_state.set(current_templates);
            editor_valid_state.set(valid);
            dialog_open.set(true);
        })
    };

    let handle_dialog_result = {
        let dialog_open = dialog_open.clone();
        let filter_state = filter_state.clone();
        let templates_state = templates_state.clone();
        let editor_templates_state = editor_templates_state.clone();
        let latest_filter = latest_filter.clone();
        let validating = validating.clone();
        let validation_pending = validation_pending.clone();
        let services = services.clone();
        let validate_on_server = props.validate_on_server;
        let on_change = props.on_change.clone();
        Callback::from(move |result: DialogResult| {
            if *validation_pending.borrow() {
                return;
            }
            if result != DialogResult::Ok {
                dialog_open.set(false);
                return;
            }
            let next_filter = latest_filter.borrow().clone();
            let next_templates = (*editor_templates_state).clone();
            if validate_on_server {
                validation_pending.replace(true);
                let validation_pending = validation_pending.clone();
                validating.set(true);
                let validating = validating.clone();
                let services = services.clone();
                let dialog_open = dialog_open.clone();
                let filter_state = filter_state.clone();
                let templates_state = templates_state.clone();
                let on_change = on_change.clone();
                yew::platform::spawn_local(async move {
                    match services.user.validate_filter(next_filter.clone()).await {
                        Ok(()) => {
                            filter_state.set(next_filter.clone());
                            templates_state.set(next_templates);
                            on_change.emit(next_filter);
                            dialog_open.set(false);
                        }
                        Err(err) => services.toastr.error(err.to_string()),
                    }
                    validation_pending.replace(false);
                    validating.set(false);
                });
            } else if parse_filter_preview(next_filter.as_deref(), next_templates.as_deref()).1 {
                filter_state.set(next_filter.clone());
                templates_state.set(next_templates);
                on_change.emit(next_filter);
                dialog_open.set(false);
            }
        })
    };

    let dialog_actions = DialogActions {
        left: Some(vec![DialogAction::new(
            "close",
            "LABEL.CLOSE",
            DialogResult::Cancel,
            Some("Close".to_owned()),
            Some("secondary".to_string()),
        )
        .with_disabled(*validating)]),
        right: vec![DialogAction::new(
            "submit",
            "LABEL.OK",
            DialogResult::Ok,
            Some("Accept".to_owned()),
            Some("primary".to_string()),
        )
        .with_disabled(*validating || (!props.validate_on_server && !*editor_valid_state))],
    };

    html! {
        <>
            <div class={"tp__filter-input tp__input"} onclick={handle_click} tabindex="0">
            <div class={"tp__input-wrapper"}>
            <span class="tp__filter-input__preview">
            {
                match (*parsed_filter_state).as_ref() {
                  None => html! { <>{(*filter_state).clone().unwrap_or_default()}</> },
                  Some(preview) => html! {
                        <FilterView inline={true} filter={preview.clone()} />
                  }
                }
            }
            </span>
             <AppIcon name={if props.icon.is_empty() { "Edit".to_owned() } else {  props.icon.clone()} } />
            </div>
            </div>
            if *dialog_open {
                {{
                    let dialog = html! {
                        <ContentDialog
                            content={html! {
                                <FilterEditor
                                    filter={(*editor_filter_state).clone()}
                                    validate_on_server={props.validate_on_server}
                                    disabled={*validating}
                                    on_filter_change={{
                                        let editor_filter_state = editor_filter_state.clone();
                                        let latest_filter = latest_filter.clone();
                                        Callback::from(move |flt: Option<String>| {
                                            latest_filter.replace(flt.clone());
                                            editor_filter_state.set(flt);
                                        })
                                    }}
                                    on_valid_change={{
                                        let editor_valid_state = editor_valid_state.clone();
                                        Callback::from(move |valid: bool| editor_valid_state.set(valid))
                                    }}
                                    on_templates_change={handle_templates_edit}
                                />
                            }}
                            actions={dialog_actions}
                            close_on_backdrop_click={false}
                            close_on_confirm={false}
                            on_confirm={handle_dialog_result}
                        />
                    };
                    // Keep the overlay outside scrolling forms so WebKit cannot clip it.
                    if let Some(body) = window().and_then(|win| win.document()).and_then(|document| document.body()) {
                        create_portal(dialog, body.into())
                    } else {
                        dialog
                    }
                }}
            }
        </>
    }
}

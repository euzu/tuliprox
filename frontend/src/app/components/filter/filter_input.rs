use super::parse_filter_preview;
use crate::{
    app::{
        components::{AppIcon, ContentDialog, FilterEditor, FilterView},
        ConfigContext,
    },
    model::{DialogAction, DialogActions, DialogResult},
};
use shared::{foundation::get_filter, model::PatternTemplate};
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct FilterInputProps {
    #[prop_or_default]
    pub icon: String,
    #[prop_or_default]
    pub filter: Option<String>,
    #[prop_or_default]
    pub on_change: Callback<Option<String>>,
}

#[component]
pub fn FilterInput(props: &FilterInputProps) -> Html {
    let config_ctx = use_context::<ConfigContext>().expect("Config context not found");

    let filter_state = use_state(|| None);
    let parsed_filter_state = use_state(|| None);
    let templates_state = use_state(|| None);
    let dialog_open = use_state(|| false);
    let editor_filter_state = use_state(|| None);
    let editor_templates_state = use_state(|| None);
    let editor_valid_state = use_state(|| true);

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
        Callback::from(move |e: MouseEvent| {
            e.prevent_default();
            e.stop_propagation();
            let current_filter = (*filter_state).clone();
            let current_templates = (*templates_state).clone();
            let (_, valid) = parse_filter_preview(current_filter.as_deref(), current_templates.as_deref());
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
        let editor_filter_state = editor_filter_state.clone();
        let editor_templates_state = editor_templates_state.clone();
        let editor_valid_state = editor_valid_state.clone();
        let on_change = props.on_change.clone();
        Callback::from(move |result: DialogResult| {
            if result == DialogResult::Ok && *editor_valid_state {
                let next_filter = (*editor_filter_state).clone();
                filter_state.set(next_filter.clone());
                templates_state.set((*editor_templates_state).clone());
                on_change.emit(next_filter);
            }
            dialog_open.set(false);
        })
    };

    let dialog_actions = DialogActions {
        left: Some(vec![DialogAction::new(
            "close",
            "LABEL.CLOSE",
            DialogResult::Cancel,
            Some("Close".to_owned()),
            None,
        )]),
        right: vec![DialogAction::new(
            "submit",
            "LABEL.OK",
            DialogResult::Ok,
            Some("Accept".to_owned()),
            Some("primary".to_string()),
        )
        .with_disabled(!*editor_valid_state)],
    };

    html! {
        <>
            <div class={"tp__filter-input tp__input"} onclick={handle_click} tabindex="0">
            <div class={"tp__input-wrapper"}>
            <span class="tp__filter-input__preview">
            {
                match (*parsed_filter_state).as_ref() {
                  None => html! {},
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
                <ContentDialog
                    content={html! {
                        <FilterEditor
                            filter={(*editor_filter_state).clone()}
                            on_filter_change={{
                                let editor_filter_state = editor_filter_state.clone();
                                Callback::from(move |flt: Option<String>| editor_filter_state.set(flt))
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
                    on_confirm={handle_dialog_result}
                />
            }
        </>
    }
}

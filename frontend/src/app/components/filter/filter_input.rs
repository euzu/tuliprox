use std::cell::RefCell;
use std::rc::Rc;
use crate::app::components::{AppIcon, FilterEditor, FilterView};
use crate::app::ConfigContext;
use crate::model::{DialogAction, DialogActions, DialogResult};
use crate::services::DialogService;
use shared::foundation::get_filter;
use shared::model::PatternTemplate;
use yew::platform::spawn_local;
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

#[function_component]
pub fn FilterInput(props: &FilterInputProps) -> Html {
    let config_ctx = use_context::<ConfigContext>().expect("Config context not found");
    let dialog = use_context::<DialogService>().expect("Dialog service not found");
    let dialog_actions = use_memo((), |()| {
        Some(DialogActions {
            left: Some(vec![DialogAction::new("close", "LABEL.CLOSE", DialogResult::Cancel, Some("Close".to_owned()), None)]),
            right: vec![DialogAction::new("submit", "LABEL.OK", DialogResult::Ok, Some("Accept".to_owned()), Some("primary".to_string()))],
        })
    });

    let filter_state = use_state(|| None);
    let parsed_filter_state = use_state(|| None);
    let templates_state = use_state(|| None);

    {
        let templates = templates_state.clone();
        let cfg_templates = config_ctx.config.as_ref().and_then(|c| c.sources.templates.clone());
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
        let templates = templates_state.clone();
        use_effect_with((*filter_state).clone(), move |flt| {
            let parsed = if let Some(new_fltr) = flt.as_ref() {
                get_filter(new_fltr, (*templates).as_ref()).ok()
            } else {
                None
            };
            parsed_filter.set(parsed);
        });
    }

    let handle_templates_edit = {
        let templates = templates_state.clone();
        Callback::from(move |templ: Option<Vec<PatternTemplate>>| {
            templates.set(templ);
        })
    };

    let handle_click = {
        let dialog = dialog.clone();
        let filter_state = filter_state.clone();
        let templates_state = templates_state.clone();
        let on_change = props.on_change.clone();
        let handle_templates_edit = handle_templates_edit.clone();
        let dialog_actions = dialog_actions.clone();
        Callback::from(move |e: MouseEvent| {
            e.prevent_default();
            e.stop_propagation();
            let original_filter = (*filter_state).clone();
            let original_templates = (*templates_state).clone();
            let current_filter = (*filter_state).clone();
            let handle_templates_edit = handle_templates_edit.clone();
            let actions = dialog_actions.clone();
            let dlg = dialog.clone();
            let filter_state = filter_state.clone();
            let templates_state = templates_state.clone();
            let on_change = on_change.clone();
            spawn_local(async move {
                // we need this refcell because the state hook does not update
                // when we close the dialog
                let filter_ref = Rc::new(RefCell::new((*filter_state).clone()));
                let handle_filter_edit = {
                    let filter_ref_set = filter_ref.clone();
                    let filter = filter_state.clone();
                    Callback::from(move |flt: Option<String>| {
                        *filter_ref_set.borrow_mut() = flt.clone();
                        filter.set(flt);
                    })
                };

                let filter_view = html! {<FilterEditor filter={current_filter}
                    on_filter_change={handle_filter_edit}
                    on_templates_change={handle_templates_edit} />};
                let result = dlg.content(filter_view, (*actions).clone(), false).await;
                match result {
                    DialogResult::Ok => on_change.emit(filter_ref.borrow().clone()),
                    _ => {
                        filter_state.set(original_filter);
                        templates_state.set(original_templates);
                    }
                }
            });
        })
    };

    html! {
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
    }
}
use crate::{
    app::{
        components::{CollapsePanel, FilterView},
        ConfigContext,
    },
    i18n::use_translation,
};
use shared::{
    foundation::{get_filter, Filter},
    model::PatternTemplate,
};
use web_sys::InputEvent;
use yew::{
    classes, component, html, use_context, use_effect_with, use_mut_ref, use_state, Callback, Html, Properties,
    TargetCast,
};
use yew_hooks::use_debounce;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct FilterEditorProps {
    #[prop_or_default]
    pub filter: Option<String>,
    #[prop_or_default]
    pub on_filter_change: Callback<Option<String>>,
    #[prop_or_default]
    pub on_valid_change: Callback<bool>,
    pub on_templates_change: Callback<Option<Vec<PatternTemplate>>>,
}

const FILTER_INPUT_DEBOUNCE_MS: u32 = 300;

pub(crate) fn parse_filter_preview(
    filter: Option<&str>,
    templates: Option<&[PatternTemplate]>,
) -> (Option<Filter>, bool) {
    match filter {
        Some(filter) => match get_filter(filter, templates) {
            Ok(parsed) => (Some(parsed), true),
            Err(_) => (None, false),
        },
        None => (None, true),
    }
}

#[component]
pub fn FilterEditor(props: &FilterEditorProps) -> Html {
    let config_ctx = use_context::<ConfigContext>().expect("Config context not found");
    let translate = use_translation();
    let cfg_templates = config_ctx.config.as_ref().and_then(|c| {
        c.templates.as_ref().map(|definition| definition.templates.clone()).or_else(|| c.sources.templates.clone())
    });

    let templates_state = use_state(|| cfg_templates.clone());
    let filter_state = use_state(|| props.filter.clone());
    // Raw textarea value, updated immediately on every keystroke so typing stays responsive.
    let input_value = use_state(|| props.filter.clone().unwrap_or_default());
    // Latest pending value awaiting the debounced parse/emit.
    let pending_value = use_mut_ref(|| props.filter.clone());

    {
        let templates = templates_state.clone();
        let on_templates_change = props.on_templates_change.clone();
        use_effect_with(cfg_templates, move |templ| {
            templates.set(templ.clone());
            on_templates_change.emit(templ.clone());
        });
    }

    {
        let filter = filter_state.clone();
        let input_value = input_value.clone();
        let pending_value = pending_value.clone();
        use_effect_with(props.filter.clone(), move |flt| {
            filter.set(flt.clone());
            input_value.set(flt.clone().unwrap_or_default());
            *pending_value.borrow_mut() = flt.clone();
        });
    }

    let (parsed_filter, valid_filter) = parse_filter_preview((*filter_state).as_deref(), (*templates_state).as_deref());

    {
        let on_valid_change = props.on_valid_change.clone();
        use_effect_with(valid_filter, move |valid| {
            on_valid_change.emit(*valid);
            || ()
        });
    }

    let debounce = {
        let filter = filter_state.clone();
        let on_filter_change = props.on_filter_change.clone();
        let pending_value = pending_value.clone();
        use_debounce(
            move || {
                let next = pending_value.borrow().clone();
                filter.set(next.clone());
                on_filter_change.emit(next);
            },
            FILTER_INPUT_DEBOUNCE_MS,
        )
    };

    let handle_filter_input = {
        let input_value = input_value.clone();
        let pending_value = pending_value.clone();
        let debounce = debounce.clone();
        Callback::from(move |event: InputEvent| {
            if let Some(input) = event.target_dyn_into::<web_sys::HtmlTextAreaElement>() {
                let value = input.value();
                input_value.set(value.clone());
                *pending_value.borrow_mut() = if value.is_empty() { None } else { Some(value) };
                debounce.run();
            }
        })
    };

    html! {
        <div class={classes!("tp__filter-editor", if valid_filter {"tp__filter-editor-valid"} else {"tp__filter-editor-invalid"})}>
          <CollapsePanel class="tp__filter-editor__templates-container" expanded={false} title={translate.t("LABEL.TEMPLATES")}>
            <div class="tp__filter-editor__templates">
                <div class="tp__filter-editor__templates-content">
                 { if let Some(templ_vec) = &*templates_state {
                      html! {
                            for templ in templ_vec.iter() {
                             <div key={templ.name.clone()} class="tp__filter-editor__templates-template">
                                <div class="tp__filter-editor__templates-template-name">
                                    { templ.name.clone() }
                                </div>
                                <div class="tp__filter-editor__templates-template-value">
                                    { templ.value.to_string() }
                                </div>
                             </div>
                         }
                        }
                    } else {
                        html! {}
                    }
                 }
                </div>
              </div>
            </CollapsePanel>
            <div class="tp__filter-editor__editor">
                <textarea class="tp__filter-editor__editor-input" value={(*input_value).clone()} oninput={handle_filter_input}/>
            </div>
            <div class="tp__filter-editor__preview">
                <FilterView inline={false} pretty={true} filter={parsed_filter} />
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::parse_filter_preview;

    #[test]
    fn parse_filter_preview_accepts_empty_filter() {
        let (parsed, valid) = parse_filter_preview(None, None);
        assert!(parsed.is_none());
        assert!(valid);
    }

    #[test]
    fn parse_filter_preview_accepts_valid_filter() {
        let (parsed, valid) = parse_filter_preview(Some("Group ~ \".*\""), None);
        assert!(parsed.is_some());
        assert!(valid);
    }

    #[test]
    fn parse_filter_preview_rejects_invalid_filter() {
        let (parsed, valid) = parse_filter_preview(Some("("), None);
        assert!(parsed.is_none());
        assert!(!valid);
    }
}

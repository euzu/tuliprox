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
use std::cell::RefCell;
use web_sys::InputEvent;
use yew::{
    classes, component, html, use_context, use_effect_with, use_memo, use_mut_ref, use_state, Callback, Html,
    Properties, TargetCast,
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
    #[prop_or_default]
    pub validate_on_server: bool,
    #[prop_or_default]
    pub disabled: bool,
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

fn emit_filter_input(
    value: String,
    pending_value: &RefCell<Option<String>>,
    on_filter_change: &Callback<Option<String>>,
) {
    let next = if value.trim().is_empty() { None } else { Some(value) };
    pending_value.replace(next.clone());
    on_filter_change.emit(next);
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
    // Latest value awaiting the debounced preview.
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
            if *pending_value.borrow() == *flt {
                return;
            }
            filter.set(flt.clone());
            input_value.set(flt.clone().unwrap_or_default());
            *pending_value.borrow_mut() = flt.clone();
        });
    }

    let preview = use_memo(((*filter_state).clone(), (*templates_state).clone()), |(filter, templates)| {
        parse_filter_preview(filter.as_deref(), templates.as_deref())
    });
    let (parsed_filter, valid_filter) = &*preview;

    {
        let on_valid_change = props.on_valid_change.clone();
        use_effect_with(*valid_filter, move |valid| {
            on_valid_change.emit(*valid);
            || ()
        });
    }

    let debounce = {
        let filter = filter_state.clone();
        let pending_value = pending_value.clone();
        use_debounce(
            move || {
                let next = pending_value.borrow().clone();
                filter.set(next);
            },
            FILTER_INPUT_DEBOUNCE_MS,
        )
    };

    let handle_filter_input = {
        let input_value = input_value.clone();
        let pending_value = pending_value.clone();
        let debounce = debounce.clone();
        let on_filter_change = props.on_filter_change.clone();
        Callback::from(move |event: InputEvent| {
            if let Some(input) = event.target_dyn_into::<web_sys::HtmlTextAreaElement>() {
                let value = input.value();
                input_value.set(value.clone());
                emit_filter_input(value, &pending_value, &on_filter_change);
                debounce.run();
            }
        })
    };

    html! {
        <div class={classes!("tp__filter-editor", if *valid_filter {Some("tp__filter-editor-valid")} else if props.validate_on_server {None} else {Some("tp__filter-editor-invalid")})}>
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
                <textarea class="tp__filter-editor__editor-input" value={(*input_value).clone()} oninput={handle_filter_input} disabled={props.disabled}/>
            </div>
            <div class="tp__filter-editor__preview">
                if props.validate_on_server && parsed_filter.is_none() {
                    <pre class="tp__filter__code">{(*input_value).clone()}</pre>
                } else {
                    <FilterView inline={false} pretty={true} filter={parsed_filter.clone()} />
                }
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::{emit_filter_input, parse_filter_preview};
    use std::{cell::RefCell, rc::Rc};
    use yew::Callback;

    #[test]
    fn filter_input_emits_latest_value_before_preview_debounce() {
        let previous = Some(r#"Group = "Sports""#.to_owned());
        let pending = RefCell::new(previous.clone());
        let submitted = Rc::new(RefCell::new(previous));
        let captured = submitted.clone();
        let on_change = Callback::from(move |filter| {
            captured.replace(filter);
        });
        let latest = r#"Group = "News""#.to_owned();
        emit_filter_input(latest.clone(), &pending, &on_change);
        assert_eq!(submitted.borrow().as_ref(), Some(&latest));
        assert_eq!(*submitted.borrow(), *pending.borrow());
        emit_filter_input("(".to_owned(), &pending, &on_change);
        assert!(!parse_filter_preview(submitted.borrow().as_deref(), None).1);
        emit_filter_input(String::new(), &pending, &on_change);
        assert!(submitted.borrow().is_none());
        assert!(pending.borrow().is_none());
        emit_filter_input("  ".to_owned(), &pending, &on_change);
        assert!(submitted.borrow().is_none());
    }

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

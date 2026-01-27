use web_sys::InputEvent;
use crate::app::ConfigContext;
use shared::model::PatternTemplate;
use yew::{classes, function_component, html, use_context, use_effect_with, use_state, Callback, Html, Properties, TargetCast};
use yew_i18n::use_translation;
use shared::foundation::{get_filter};
use crate::app::components::{CollapsePanel, FilterView};

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct FilterEditorProps {
    #[prop_or_default]
    pub filter: Option<String>,
    #[prop_or_default]
    pub on_filter_change: Callback<Option<String>>,
    pub on_templates_change: Callback<Option<Vec<PatternTemplate>>>,
}

#[function_component]
pub fn FilterEditor(props: &FilterEditorProps) -> Html {
    let config_ctx = use_context::<ConfigContext>().expect("Config context not found");
    let translate = use_translation();

    let templates_state = use_state(|| None);
    let filter_state = use_state(|| None);
    let parsed_filter_state = use_state(|| None);
    let valid_filter_state = use_state(|| true);

    {
        let templates = templates_state.clone();
        let cfg_templates = config_ctx.config.as_ref().and_then(|c| c.sources.templates.clone());
        use_effect_with(cfg_templates,  move |templ| {
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
        let filter = filter_state.clone();
        let parsed_filter = parsed_filter_state.clone();
        let templates = templates_state.clone();
        let valid_filter = valid_filter_state.clone();
        use_effect_with(filter.clone(), move |flt| {
            let parsed = if let Some(new_fltr) = flt.as_ref() {
                match get_filter(new_fltr, (*templates).as_ref()) {
                    Ok(fltr) => {
                        valid_filter.set(true);
                        Some(fltr)
                    }
                    Err(_) => {
                        valid_filter.set(false);
                        None
                    }
                }
            } else {
                valid_filter.set(true);
                None
            };
            parsed_filter.set(parsed);
        });
    }

    let handle_filter_input = {
      let filter = filter_state.clone();
      let on_filter_change = props.on_filter_change.clone();
      Callback::from(move |event: InputEvent| {
          if let Some(input) = event.target_dyn_into::<web_sys::HtmlTextAreaElement>() {
              let value = input.value();
              if value.is_empty() {
                  filter.set(None);
                  on_filter_change.emit(None);
              } else {
                  filter.set(Some(value.clone()));
                  on_filter_change.emit(Some(value));
              }
          }
      })
    };

    html! {
        <div class={classes!("tp__filter-editor", if *valid_filter_state {"tp__filter-editor-valid"} else {"tp__filter-editor-invalid"})}>
          <CollapsePanel class="tp__filter-editor__templates-container" expanded={false} title={translate.t("LABEL.TEMPLATES")}>
            <div class="tp__filter-editor__templates">
                <div class="tp__filter-editor__templates-content">
                 { if let Some(templ_vec) = &*templates_state {
                      html! {
                            for templ_vec.iter().map(|templ| html! {
                             <>
                                <div class="tp__filter-editor__templates-template-name">
                                    { &templ.name }
                                </div>
                                <div class="tp__filter-editor__templates-template-value">
                                    { templ.value.to_string() }
                                </div>
                             </>
                         })
                        }
                    } else {
                        html! {}
                    }
                 }
                </div>
              </div>
            </CollapsePanel>
            <div class="tp__filter-editor__editor">
                <textarea class="tp__filter-editor__editor-input" value={(*filter_state).clone()} oninput={handle_filter_input}/>
            </div>
            <div class="tp__filter-editor__preview">
                <FilterView inline={false} pretty={true} filter={(*parsed_filter_state).clone()} />
            </div>
        </div>
    }
}
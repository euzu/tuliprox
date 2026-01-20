use crate::app::components::config::HasFormData;
use crate::app::components::key_value_editor::KeyValueEditor;
use crate::app::components::select::Select;
use crate::app::components::{AliasItemForm, BlockId, BlockInstance, Card, DropDownOption, DropDownSelection, EditMode, EpgSourceItemForm, IconButton, Panel, RadioButtonGroup, SourceEditorContext, TextButton, TitledCard};
use crate::{config_field_child, edit_field_bool, edit_field_date, edit_field_number_i16, edit_field_number_u16, edit_field_text, edit_field_text_option, generate_form_reducer};
use shared::model::{ConfigInputAliasDto, ConfigInputDto, ConfigInputOptionsDto, EpgConfigDto, EpgSourceDto, InputFetchMethod, InputType, StagedInputDto};
use std::collections::HashMap;
use std::fmt::Display;
use std::rc::Rc;
use std::str::FromStr;
use web_sys::MouseEvent;
use yew::{function_component, html, use_context, use_effect_with, use_memo, use_reducer, use_state, Callback, Html, Properties, UseReducerHandle};
use yew_i18n::use_translation;
use shared::error::TuliproxError;
use shared::info_err_res;

const LABEL_NAME: &str = "LABEL.NAME";
const LABEL_INPUT_TYPE: &str = "LABEL.INPUT_TYPE";
const LABEL_FETCH_METHOD: &str = "LABEL.METHOD";
const LABEL_HEADERS: &str = "LABEL.HEADERS";
const LABEL_URL: &str = "LABEL.URL";
const LABEL_EPG_SOURCES: &str = "LABEL.EPG_SOURCES";
const LABEL_USERNAME: &str = "LABEL.USERNAME";
const LABEL_PASSWORD: &str = "LABEL.PASSWORD";
const LABEL_PERSIST: &str = "LABEL.PERSIST";
const LABEL_ENABLED: &str = "LABEL.ENABLED";
const LABEL_ALIASES: &str = "LABEL.ALIASES";
const LABEL_PRIORITY: &str = "LABEL.PRIORITY";
const LABEL_MAX_CONNECTIONS: &str = "LABEL.MAX_CONNECTIONS";
const LABEL_EXP_DATE: &str = "LABEL.EXP_DATE";
const LABEL_ADD_EPG_SOURCE: &str = "LABEL.ADD_EPG_SOURCE";
const LABEL_ADD_ALIAS: &str = "LABEL.ADD_ALIAS";
const LABEL_SKIP: &str = "LABEL.SKIP";
const LABEL_XTREAM_SKIP_LIVE: &str = "LABEL.LIVE";
const LABEL_XTREAM_SKIP_VOD: &str = "LABEL.VOD";
const LABEL_XTREAM_SKIP_SERIES: &str = "LABEL.SERIES";
const LABEL_XTREAM_LIVE_STREAM_USE_PREFIX: &str = "LABEL.LIVE_STREAM_USE_PREFIX";
const LABEL_XTREAM_LIVE_STREAM_WITHOUT_EXTENSION: &str = "LABEL.LIVE_STREAM_WITHOUT_EXTENSION";
const LABEL_CACHE_DURATION: &str = "LABEL.CACHE_DURATION";

const LABEL_MAIN: &str = "LABEL.MAIN_CONFIG";
const LABEL_OPTIONS: &str = "LABEL.OPTIONS";
const LABEL_STAGED: &str = "LABEL.STAGED";
const LABEL_ADVANCED: &str = "LABEL.ADVANCED";
const LABEL_ALIAS: &str = "LABEL.ALIAS";


#[derive(Copy, Clone, PartialEq, Eq)]
enum InputFormPage {
    Main,
    Options,
    Staged,
    Advanced,
    Alias
}

impl InputFormPage {
    const MAIN: &str = "Main";
    const OPTIONS: &str = "Options";
    const STAGED: &str = "Staged";
    const ADVANCED: &str = "Advanced";
    const ALIAS: &str = "Alias";
}

impl FromStr for InputFormPage {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        match s {
            Self::MAIN => Ok(InputFormPage::Main),
            Self::OPTIONS => Ok(InputFormPage::Options),
            Self::STAGED => Ok(InputFormPage::Staged),
            Self::ADVANCED => Ok(InputFormPage::Advanced),
            Self::ALIAS => Ok(InputFormPage::Alias),
            _ => info_err_res!("Unknown input form page: {s}"),
        }
    }
}

impl Display for InputFormPage {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", match *self {
            InputFormPage::Main => Self::MAIN,
            InputFormPage::Options => Self::OPTIONS,
            InputFormPage::Staged => Self::STAGED,
            InputFormPage::Advanced => Self::ADVANCED,
            InputFormPage::Alias => Self::ALIAS,
        })
    }
}

generate_form_reducer!(
    state: ConfigInputOptionsDtoFormState { form: ConfigInputOptionsDto },
    action_name: ConfigInputOptionsFormAction,
    fields {
      XtreamSkipLive => xtream_skip_live: bool,
      XtreamSkipVod => xtream_skip_vod: bool,
      XtreamSkipSeries => xtream_skip_series: bool,
      XtreamLiveStreamUsePrefix => xtream_live_stream_use_prefix: bool,
      XtreamLiveStreamWithoutExtension => xtream_live_stream_without_extension: bool,
    }
);

generate_form_reducer!(
    state: StagedInputDtoFormState { form: StagedInputDto },
    action_name: StagedInputFormAction,
    fields {
        Url => url: String,
        Username => username: Option<String>,
        Password => password: Option<String>,
        Method => method: InputFetchMethod,
        InputType => input_type: InputType,
        // Headers => headers: HashMap<String, String>,
    }
);

generate_form_reducer!(
    state: ConfigInputFormState { form: ConfigInputDto },
    action_name: ConfigInputFormAction,
    fields {
        Name => name: String,
        Url => url: String,
        Username => username: Option<String>,
        Password => password: Option<String>,
        Persist => persist: Option<String>,
        Enabled => enabled: bool,
        Priority => priority: i16,
        MaxConnections => max_connections: u16,
        Method => method: InputFetchMethod,
        ExpDate => exp_date: Option<i64>,
        CacheDuration => cache_duration: Option<String>,
    }
);

#[derive(Properties, PartialEq, Clone)]
pub struct ConfigInputViewProps {
    #[prop_or_default]
    pub(crate) block_id: Option<BlockId>,
    pub(crate) input: Option<Rc<ConfigInputDto>>,
    #[prop_or_default]
    pub(crate) on_apply: Option<Callback<ConfigInputDto>>,
    #[prop_or_default]
    pub(crate) on_cancel: Option<Callback<()>>,
}

#[function_component]
pub fn ConfigInputView(props: &ConfigInputViewProps) -> Html {
    let translate = use_translation();
    let source_editor_ctx = use_context::<SourceEditorContext>();
    let fetch_methods = use_memo((), |_| {
        [InputFetchMethod::GET, InputFetchMethod::POST]
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<String>>()
    });
    let view_visible = use_state(|| InputFormPage::Main);

    // let on_tab_click = {
    //     let view_visible = view_visible.clone();
    //     Callback::from(move |page: InputFormPage| view_visible.set(page))
    // };

    let handle_menu_click = {
        let active_menu = view_visible.clone();
        Callback::from(move |(name, _): (String, _)| {
            if let Ok(view_type) = InputFormPage::from_str(&name) {
                active_menu.set(view_type);
            }
        })
    };

    let input_form_state: UseReducerHandle<ConfigInputFormState> =
        use_reducer(|| ConfigInputFormState {
            form: ConfigInputDto::default(),
            modified: false,
        });
    let input_options_state: UseReducerHandle<ConfigInputOptionsDtoFormState> =
        use_reducer(|| ConfigInputOptionsDtoFormState {
            form: ConfigInputOptionsDto::default(),
            modified: false,
        });
    let staged_input_state: UseReducerHandle<StagedInputDtoFormState> =
        use_reducer(|| StagedInputDtoFormState {
            form: StagedInputDto::default(),
            modified: false,
        });

    // State for EPG sources, Aliases, and Headers
    let epg_sources_state = use_state(Vec::<EpgSourceDto>::new);
    let aliases_state = use_state(Vec::<ConfigInputAliasDto>::new);
    let headers_state = use_state(HashMap::<String, String>::new);

    // State for showing item forms
    let show_epg_form_state = use_state(|| false);
    let show_alias_form_state = use_state(|| false);
    let edit_alias = use_state(|| None::<ConfigInputAliasDto>);

    let staged_input_types = use_memo(staged_input_state.form.input_type, |input_type| {
        let default_it = input_type;
        [
            InputType::M3u,
            InputType::Xtream,
            // InputType::M3uBatch,
            // InputType::XtreamBatch,
        ]
            .iter()
            .map(|t| DropDownOption {
                id: t.to_string(),
                label: html! { t.to_string() },
                selected: t == default_it,
            }).collect::<Vec<DropDownOption>>()
    });

    {
        let input_form_state = input_form_state.clone();
        let input_options_state = input_options_state.clone();
        let staged_input_state = staged_input_state.clone();
        let epg_sources_state = epg_sources_state.clone();
        let aliases_state = aliases_state.clone();
        let headers_state = headers_state.clone();

        let config_input = props.input.clone();

        use_effect_with(config_input, move |cfg| {
            if let Some(input) = cfg {
                input_form_state.dispatch(ConfigInputFormAction::SetAll(input.as_ref().clone()));

                input_options_state.dispatch(ConfigInputOptionsFormAction::SetAll(
                    input.options.as_ref().map_or_else(ConfigInputOptionsDto::default, |d| d.clone()),
                ));

                staged_input_state.dispatch(StagedInputFormAction::SetAll(
                    input.staged.as_ref().map_or_else(StagedInputDto::default, |c| c.clone()),
                ));

                // Load headers
                headers_state.set(input.headers.clone());

                // Load EPG sources
                epg_sources_state.set(input.epg.as_ref().and_then(|epg| epg.sources.clone()).unwrap_or_default());

                // Load aliases
                aliases_state.set(input.aliases.clone().unwrap_or_default());
            } else {
                input_form_state.dispatch(ConfigInputFormAction::SetAll(ConfigInputDto::default()));
                input_options_state.dispatch(ConfigInputOptionsFormAction::SetAll(ConfigInputOptionsDto::default()));
                staged_input_state.dispatch(StagedInputFormAction::SetAll(StagedInputDto::default()));
                headers_state.set(HashMap::new());
                epg_sources_state.set(Vec::new());
                aliases_state.set(Vec::new());
            }
            || ()
        });
    }

    let handle_add_epg_item = {
        let epg_sources = epg_sources_state.clone();
        let show_epg_form = show_epg_form_state.clone();
        Callback::from(move |source: EpgSourceDto| {
            let mut sources = (*epg_sources).clone();
            sources.push(source);
            epg_sources.set(sources);
            show_epg_form.set(false);
        })
    };

    let handle_close_add_epg_item = {
        let show_epg_form = show_epg_form_state.clone();
        Callback::from(move |_| {
            show_epg_form.set(false);
        })
    };

    let handle_show_add_epg_item = {
        let show_epg_form = show_epg_form_state.clone();
        Callback::from(move |_| {
            show_epg_form.set(true);
        })
    };

    let handle_add_alias_item = {
        let aliases = aliases_state.clone();
        let show_alias_form = show_alias_form_state.clone();
        let edit_alias = edit_alias.clone();
        Callback::from(move |alias: ConfigInputAliasDto| {
            let mut items = (*aliases).clone();
            if let Some(e) = edit_alias.as_ref() {
                if let Some(pos) = items.iter().position(|x| x.name == e.name) {
                    if let Some(slot) = items.get_mut(pos) {
                        *slot = alias;
                    }
                } else {
                    items.push(alias);
                }
                edit_alias.set(None);
            } else {
                items.push(alias);
            }
            aliases.set(items);
            show_alias_form.set(false);
        })
    };

    let handle_close_add_alias_item = {
        let show_alias_form = show_alias_form_state.clone();
        let edit_alias = edit_alias.clone();
        Callback::from(move |()| {
            show_alias_form.set(false);
            edit_alias.set(None);
        })
    };

    let handle_show_add_alias_item = {
        let show_alias_form = show_alias_form_state.clone();
        let edit_alias = edit_alias.clone();
        Callback::from(move |_| {
            show_alias_form.set(true);
            edit_alias.set(None);
        })
    };


    let handle_remove_alias_list_item = {
        let alias_list = aliases_state.clone();
        Callback::from(move |(idx, e): (String, MouseEvent)| {
            e.prevent_default();
            e.stop_propagation();
            if let Ok(index) = idx.parse::<usize>() {
                let mut items = (*alias_list).clone();
                if index < items.len() {
                    items.remove(index);
                    alias_list.set(items);
                }
            }
        })
    };

    let handle_edit_alias_list_item = {
        let alias_list = aliases_state.clone();
        let show_alias_form = show_alias_form_state.clone();
        let edit_alias = edit_alias.clone();

        Callback::from(move |(idx, e): (String, MouseEvent)| {
            e.prevent_default();
            e.stop_propagation();
            if let Ok(index) = idx.parse::<usize>() {
                let items = (*alias_list).clone();
                if index < items.len() {
                    let item = items.get(index).cloned();
                    edit_alias.set(item);
                    show_alias_form.set(true);
                }
            }
        })
    };

    let handle_remove_epg_source = {
        let epg_list = epg_sources_state.clone();
        Callback::from(move |(idx, e): (String, MouseEvent)| {
            e.prevent_default();
            e.stop_propagation();
            if let Ok(index) = idx.parse::<usize>() {
                let mut items = (*epg_list).clone();
                if index < items.len() {
                    items.remove(index);
                    epg_list.set(items);
                }
            }
        })
    };

    let render_options = || {
        html! {
            <Card class="tp__config-view__card">
            <TitledCard title={translate.t(LABEL_SKIP)}>
              <div class="tp__config-view__cols-3">
                { edit_field_bool!(input_options_state, translate.t(LABEL_XTREAM_SKIP_LIVE), xtream_skip_live, ConfigInputOptionsFormAction::XtreamSkipLive) }
                { edit_field_bool!(input_options_state, translate.t(LABEL_XTREAM_SKIP_VOD), xtream_skip_vod, ConfigInputOptionsFormAction::XtreamSkipVod) }
                { edit_field_bool!(input_options_state, translate.t(LABEL_XTREAM_SKIP_SERIES), xtream_skip_series, ConfigInputOptionsFormAction::XtreamSkipSeries) }
              </div>
            </TitledCard>
            { edit_field_bool!(input_options_state, translate.t(LABEL_XTREAM_LIVE_STREAM_USE_PREFIX), xtream_live_stream_use_prefix, ConfigInputOptionsFormAction::XtreamLiveStreamUsePrefix) }
            { edit_field_bool!(input_options_state, translate.t(LABEL_XTREAM_LIVE_STREAM_WITHOUT_EXTENSION), xtream_live_stream_without_extension, ConfigInputOptionsFormAction::XtreamLiveStreamWithoutExtension) }
            </Card>
        }
    };

    let render_staged = || {
        let staged_method_selection = Rc::new(vec![staged_input_state.form.method.to_string()]);
        let staged_input_state_1 = staged_input_state.clone();
        let staged_input_state_2 = staged_input_state.clone();
        html! {
            <Card class="tp__config-view__card">
                { edit_field_text!(staged_input_state, translate.t(LABEL_URL),  url, StagedInputFormAction::Url) }
                <div class="tp__config-view__cols-2">
                { edit_field_text_option!(staged_input_state, translate.t(LABEL_USERNAME), username, StagedInputFormAction::Username) }
                { edit_field_text_option!(staged_input_state, translate.t(LABEL_PASSWORD), password, StagedInputFormAction::Password, true) }
                { config_field_child!(translate.t(LABEL_FETCH_METHOD), {

                   html! {
                       <RadioButtonGroup
                        multi_select={false} none_allowed={false}
                        on_select={Callback::from(move |selections: Rc<Vec<String>>| {
                            if let Some(first) = selections.first() {
                                staged_input_state_1.dispatch(StagedInputFormAction::Method(first.parse::<InputFetchMethod>().unwrap_or(InputFetchMethod::GET)));
                            }
                        })}
                        options={&fetch_methods}
                        selected={staged_method_selection}
                    />
               }})}
               { config_field_child!(translate.t(LABEL_INPUT_TYPE), {
                   html! {
                       <Select
                        name={"staged_input_types"}
                        multi_select={false}
                        on_select={Callback::from(move |(_, selections):(String, DropDownSelection)| {
                           match selections {
                            DropDownSelection::Empty => {
                                   staged_input_state_2.dispatch(StagedInputFormAction::InputType(InputType::Xtream));
                            }
                            DropDownSelection::Single(option) => {
                                staged_input_state_2.dispatch(StagedInputFormAction::InputType(option.parse::<InputType>().unwrap_or(InputType::Xtream)));
                            }
                            DropDownSelection::Multi(options) => {
                              if let Some(first) = options.first() {
                                staged_input_state_2.dispatch(StagedInputFormAction::InputType(first.parse::<InputType>().unwrap_or(InputType::Xtream)));
                               }
                             }
                           }
                        })}
                        options={staged_input_types.clone()}
                    />
               }})}
                </div>

                //{ edit_field_list!(staged_input_state, translate.t(LABEL_HEADERS), headers, StagedInputFormAction::Headers, translate.t(LABEL_ADD_HEADER)) }
            </Card>
        }
    };

    let render_input = || {
        let input_method_selection = Rc::new(vec![input_form_state.form.method.to_string()]);
        let input_form_state_disp = input_form_state.clone();

        html! {
             <Card class="tp__config-view__card">
               <div class="tp__config-view__cols-2">
               { edit_field_text!(input_form_state, translate.t(LABEL_NAME),  name, ConfigInputFormAction::Name) }
               { edit_field_bool!(input_form_state, translate.t(LABEL_ENABLED), enabled, ConfigInputFormAction::Enabled) }
               </div>
               { edit_field_text!(input_form_state, translate.t(LABEL_URL),  url, ConfigInputFormAction::Url) }
               <div class="tp__config-view__cols-2">
               { edit_field_text_option!(input_form_state, translate.t(LABEL_USERNAME), username, ConfigInputFormAction::Username) }
               { edit_field_text_option!(input_form_state, translate.t(LABEL_PASSWORD), password, ConfigInputFormAction::Password, true) }
               { edit_field_number_u16!(input_form_state, translate.t(LABEL_MAX_CONNECTIONS), max_connections, ConfigInputFormAction::MaxConnections) }
               { edit_field_number_i16!(input_form_state, translate.t(LABEL_PRIORITY), priority, ConfigInputFormAction::Priority) }
               { edit_field_date!(input_form_state, translate.t(LABEL_EXP_DATE), exp_date, ConfigInputFormAction::ExpDate) }
               { edit_field_text_option!(input_form_state, translate.t(LABEL_CACHE_DURATION), cache_duration, ConfigInputFormAction::CacheDuration) }
               { config_field_child!(translate.t(LABEL_FETCH_METHOD), {
                   html! {
                       <RadioButtonGroup
                        multi_select={false} none_allowed={false}
                        on_select={Callback::from(move |selections: Rc<Vec<String>>| {
                            if let Some(first) = selections.first() {
                                input_form_state_disp.dispatch(ConfigInputFormAction::Method(first.parse::<InputFetchMethod>().unwrap_or(InputFetchMethod::GET)));
                            }
                        })}
                        options={fetch_methods.clone()}
                        selected={input_method_selection}
                    />
               }})}
               </div>
               { edit_field_text_option!(input_form_state, translate.t(LABEL_PERSIST), persist, ConfigInputFormAction::Persist) }
            </Card>
        }
    };

    let render_alias = || {
        let aliases = aliases_state.clone();
        let show_alias_form = show_alias_form_state.clone();
        let edit_alias = edit_alias.clone();

        html! {
             <Card class="tp__config-view__card">
              if *show_alias_form {
                    <AliasItemForm
                        initial={(*edit_alias).clone()}
                        on_submit={handle_add_alias_item}
                        on_cancel={handle_close_add_alias_item}
                    />
              } else {
                  { config_field_child!(translate.t(LABEL_ALIASES), {
                      let aliases_list = aliases.clone();
                      html! {
                        <div class="tp__form-list">
                            <div class="tp__form-list__items">
                            {
                                for (*aliases_list).iter().enumerate().map(|(idx, alias)| {
                                    html! {
                                        <div class="tp__form-list__item" key={format!("alias-{idx}")}>
                                            <div class="tp__form-list__item-toolbar">
                                                <IconButton
                                                name={idx.to_string()}
                                                icon="Delete"
                                                onclick={handle_remove_alias_list_item.clone()}/>
                                                <IconButton
                                                name={idx.to_string()}
                                                icon="Edit"
                                                onclick={handle_edit_alias_list_item.clone()}/>
                                            </div>
                                            <div class="tp__form-list__item-content">
                                                <span><strong>{&alias.name}</strong>{" - "}{&alias.url}</span>
                                            </div>
                                        </div>
                                    }
                                })
                            }
                            </div>
                            <div class="tp__form-list__toolbar">
                                <TextButton
                                    class="primary"
                                    name="add_alias"
                                    icon="Add"
                                    title={translate.t(LABEL_ADD_ALIAS)}
                                    onclick={handle_show_add_alias_item}
                                />
                            </div>
                          </div>
                      }
                  })}
              }
            </Card>
        }
    };

    let render_advanced = || {
        let headers = headers_state.clone();
        let epg_sources = epg_sources_state.clone();
        let show_epg_form = show_epg_form_state.clone();

        html! {
            <Card class="tp__config-view__card">
               if *show_epg_form {
                    <EpgSourceItemForm
                        on_submit={handle_add_epg_item}
                        on_cancel={handle_close_add_epg_item}
                    />
               } else  {
                  // Headers Section
                  { config_field_child!(translate.t(LABEL_HEADERS), {
                      let headers_set = headers.clone();
                      html! {
                        <KeyValueEditor
                            entries={(*headers).clone()}
                            readonly={false}
                            key_placeholder={translate.t("LABEL.HEADER_NAME")}
                           value_placeholder={translate.t("LABEL.HEADER_VALUE")}
                            on_change={Callback::from(move |new_headers: HashMap<String, String>| {
                                headers_set.set(new_headers);
                            })}
                        />
                      }
                  })}

                  // EPG Sources Section
                  { config_field_child!(translate.t(LABEL_EPG_SOURCES), {
                      let epg_sources_list = epg_sources.clone();

                      html! {
                        <div class="tp__form-list">
                            <div class="tp__form-list__items">
                            {
                                for (*epg_sources_list).iter().enumerate().map(|(idx, source)| {
                                    html! {
                                        <div class="tp__form-list__item" key={format!("epg-{idx}")}>
                                            <IconButton
                                                name={idx.to_string()}
                                                icon="Delete"
                                                onclick={handle_remove_epg_source.clone()} />
                                            <div class="tp__form-list__item-content">
                                                <span>{&source.url}</span>
                                            </div>
                                        </div>
                                    }
                                })
                            }
                            </div>
                            <TextButton
                                class="primary"
                                name="add_epg_source"
                                icon="Add"
                                title={translate.t(LABEL_ADD_EPG_SOURCE)}
                                onclick={handle_show_add_epg_item}
                            />
                        </div>
                      }
                  })}
                }
            </Card>
        }
    };

    let handle_apply_input = {
        let on_apply = props.on_apply.clone();
        let block_id = props.block_id;
        let source_editor_ctx = source_editor_ctx.clone();
        let input_form_state = input_form_state.clone();
        let input_options_state = input_options_state.clone();
        let staged_input_state = staged_input_state.clone();
        let headers_state = headers_state.clone();
        let epg_sources_state = epg_sources_state.clone();
        let aliases_state = aliases_state.clone();

        Callback::from(move |_| {
            let mut input = input_form_state.data().clone();

            let options = input_options_state.data();
            input.options = if options.is_empty() {
                None
            } else {
                Some(options.clone())
            };

            let staged_input = staged_input_state.data();
            input.staged = if staged_input.is_empty() {
                None
            } else {
                Some(staged_input.clone())
            };

            // Handle Headers
            input.headers = (*headers_state).clone();

            // Handle EPG: update sources but preserve other fields if present
            let epg_sources = (*epg_sources_state).clone();
            if let Some(mut epg_cfg) = input.epg.take() {
                epg_cfg.sources = if epg_sources.is_empty() { None } else { Some(epg_sources) };
                input.epg = if epg_cfg.sources.is_some() || epg_cfg.smart_match.is_some() {
                    Some(epg_cfg)
                } else {
                    None
                };
            } else if !epg_sources.is_empty() {
                input.epg = Some(EpgConfigDto {
                    sources: Some(epg_sources),
                    ..EpgConfigDto::default()
                });
            }

            // Handle Aliases
            let aliases = (*aliases_state).clone();
            input.aliases = if aliases.is_empty() {
                None
            } else {
                Some(aliases)
            };

            if let Some(on_apply) = &on_apply {
                on_apply.emit(input);
            } else if let (Some(ctx), Some(block_id)) = (&source_editor_ctx, block_id) {
                ctx.on_form_change.emit((block_id, BlockInstance::Input(Rc::new(input))));
                ctx.edit_mode.set(EditMode::Inactive);
            }
        })
    };
    let handle_cancel = {
        let source_editor_ctx = source_editor_ctx.clone();
        let on_cancel = props.on_cancel.clone();
        Callback::from(move |_| {
            if let Some(on_cancel) = &on_cancel {
                on_cancel.emit(());
            } else if let Some(ctx) = &source_editor_ctx {
                ctx.edit_mode.set(EditMode::Inactive);
            }
        })
    };

    let render_edit_mode = || {
        html! {
            <div class="tp__source-editor-form__body">
                // <div class="tp__tab-header">
                // {
                //     for [
                //         InputFormPage::Main,
                //         InputFormPage::Options,
                //         InputFormPage::Staged,
                //         InputFormPage::Advanced
                //     ].iter().map(|page| {
                //         let active = &*view_visible == page;
                //         let on_tab_click = {
                //             let on_tab_click = on_tab_click.clone();
                //             let page = *page;
                //             Callback::from(move |_| on_tab_click.emit(page))
                //         };
                //         html! {
                //             <button
                //                 class={classes!("tp__tab-button", if active { "active" } else { "" })}
                //                 onclick={on_tab_click}
                //             >
                //                 { page.to_string() }
                //             </button>
                //         }
                //     })
                // }
                // </div>
            <div class="tp__source-editor-form__body__pages">
                <Panel value={InputFormPage::Main.to_string()} active={view_visible.to_string()}>
                {render_input()}
                </Panel>
                <Panel value={InputFormPage::Alias.to_string()} active={view_visible.to_string()}>
                {render_alias()}
                </Panel>
                <Panel value={InputFormPage::Options.to_string()} active={view_visible.to_string()}>
                {render_options()}
                </Panel>
                <Panel value={InputFormPage::Staged.to_string()} active={view_visible.to_string()}>
                {render_staged()}
                </Panel>
                <Panel value={InputFormPage::Advanced.to_string()} active={view_visible.to_string()}>
                {render_advanced()}
                </Panel>
            </div>
            </div>
        }
    };

    let render_sidebar = || {
        html! {
            <div class="tp__source-editor-form__sidebar">
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", InputFormPage::Main, if *view_visible == InputFormPage::Main { " active" } else {""})}  icon="Settings" hint={translate.t(LABEL_MAIN)} name={InputFormPage::Main.to_string()} onclick={&handle_menu_click}></IconButton>
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", InputFormPage::Alias, if *view_visible == InputFormPage::Alias { " active" } else {""})}  icon="Alias" hint={translate.t(LABEL_ALIAS)} name={InputFormPage::Alias.to_string()} onclick={&handle_menu_click}></IconButton>
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", InputFormPage::Options, if *view_visible == InputFormPage::Options { " active" } else {""})}  icon="Options" hint={translate.t(LABEL_OPTIONS)} name={InputFormPage::Options.to_string()} onclick={&handle_menu_click}></IconButton>
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", InputFormPage::Staged, if *view_visible == InputFormPage::Staged { " active" } else {""})}  icon="Staged" hint={translate.t(LABEL_STAGED)} name={InputFormPage::Staged.to_string()} onclick={&handle_menu_click}></IconButton>
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", InputFormPage::Advanced, if *view_visible == InputFormPage::Advanced { " active" } else {""})}  icon="Advanced" hint={translate.t(LABEL_ADVANCED)} name={InputFormPage::Advanced.to_string()} onclick={&handle_menu_click}></IconButton>
          </div>
        }
    };

    html! {
        <div class="tp__source-editor-form tp__config-view-page">
          <div class="tp__source-editor-form__toolbar tp__form-page__toolbar">
             <TextButton class="secondary" name="cancel_input"
                icon="Cancel"
                title={ translate.t("LABEL.CANCEL")}
                onclick={handle_cancel}></TextButton>
             <TextButton class="primary" name="apply_input"
                icon="Accept"
                title={ translate.t("LABEL.OK")}
                onclick={handle_apply_input}></TextButton>
          </div>
        <div class="tp__source-editor-form__content">
            { render_sidebar() }
            { render_edit_mode() }
        </div>
        </div>
    }
}

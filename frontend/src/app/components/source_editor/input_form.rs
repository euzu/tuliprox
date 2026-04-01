use crate::{
    app::{
        components::{
            config::HasFormData, key_value_editor::KeyValueEditor, select::Select, AliasItemForm, BlockId,
            BlockInstance, Card, DropDownOption, DropDownSelection, EditMode, EpgSmartMatchForm, EpgSourceItemForm,
            IconButton, Panel, ProviderItemForm, RadioButtonGroup, SourceEditorContext, TextButton, TitledCard,
            ToolAction,
        },
        ConfigContext,
    },
    config_field, config_field_bool, config_field_child, config_field_custom, config_field_optional,
    config_field_optional_hide, edit_field_bool, edit_field_exp_date, edit_field_number_i16, edit_field_number_u16,
    edit_field_number_u32, edit_field_text, edit_field_text_option, generate_form_reducer,
    hooks::use_service_context,
    html_if,
    i18n::use_translation,
};
use shared::{
    concat_string,
    error::TuliproxError,
    info_err_res,
    model::{
        ClusterSource, ConfigInputAliasDto, ConfigInputDto, ConfigInputOptionsDto, ConfigProviderDto,
        EpgSmartMatchConfigDto, EpgSourceDto, InputFetchMethod, InputType, StagedInputDto, XtreamLoginRequest,
    },
};
use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
    rc::Rc,
    str::FromStr,
};
use web_sys::MouseEvent;
use yew::{
    component, html, platform::spawn_local, use_context, use_effect_with, use_memo, use_mut_ref, use_reducer,
    use_state, Callback, Html, Properties, UseReducerHandle,
};

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
const LABEL_ADD_PROVIDER: &str = "LABEL.ADD_PROVIDER";
const LABEL_PROVIDERS: &str = "LABEL.PROVIDER";
const LABEL_SKIP: &str = "LABEL.SKIP";
const LABEL_XTREAM_SKIP_LIVE: &str = "LABEL.LIVE";
const LABEL_XTREAM_SKIP_VOD: &str = "LABEL.VOD";
const LABEL_XTREAM_SKIP_SERIES: &str = "LABEL.SERIES";
const LABEL_XTREAM_LIVE_STREAM_USE_PREFIX: &str = "LABEL.LIVE_STREAM_USE_PREFIX";
const LABEL_XTREAM_LIVE_STREAM_WITHOUT_EXTENSION: &str = "LABEL.LIVE_STREAM_WITHOUT_EXTENSION";
const LABEL_RESOLVE_TMDB: &str = "LABEL.RESOLVE_TMDB";
const LABEL_RESOLVE: &str = "LABEL.RESOLVE";
const LABEL_PROBE: &str = "LABEL.PROBE";
const LABEL_RESOLVE_DELAY_SEC: &str = "LABEL.RESOLVE_DELAY_SEC";
const LABEL_PROBE_DELAY_SEC: &str = "LABEL.PROBE_DELAY_SEC";
const LABEL_RESOLVE_BACKGROUND: &str = "LABEL.RESOLVE_BACKGROUND";
const LABEL_PROBE_LIVE_INTERVAL_HOURS: &str = "LABEL.PROBE_LIVE_INTERVAL_HOURS";
const LABEL_METADATA: &str = "LABEL.METADATA";
const LABEL_CACHE_DURATION: &str = "LABEL.CACHE_DURATION";
const LABEL_MAIN: &str = "LABEL.MAIN_CONFIG";
const LABEL_OPTIONS: &str = "LABEL.OPTIONS";
const LABEL_STAGED: &str = "LABEL.STAGED";
const LABEL_LIVE_SOURCE: &str = "LABEL.LIVE_SOURCE";
const LABEL_VOD_SOURCE: &str = "LABEL.VOD_SOURCE";
const LABEL_SERIES_SOURCE: &str = "LABEL.SERIES_SOURCE";
const LABEL_EPG: &str = "LABEL.EPG";
const LABEL_ALIAS: &str = "LABEL.ALIAS";
const LABEL_PROVIDER: &str = "LABEL.PROVIDER";
const LABEL_LIVE_STREAMS: &str = "LABEL.LIVE_STREAMS";
const LABEL_EPG_SMART_MATCH: &str = "LABEL.EPG_SMART_MATCH";
const LABEL_EDIT_EPG_SMART_MATCH: &str = "LABEL.EDIT_EPG_SMART_MATCH";

#[derive(Copy, Clone, PartialEq, Eq)]
enum InputFormPage {
    Main,
    Options,
    Staged,
    Epg,
    Alias,
    Provider,
}

impl InputFormPage {
    const MAIN: &str = "Main";
    const OPTIONS: &str = "Options";
    const STAGED: &str = "Staged";
    const EPG: &str = "Epg";
    const ALIAS: &str = "Alias";
    const PROVIDER: &str = "Provider";
}

impl FromStr for InputFormPage {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        match s {
            Self::MAIN => Ok(InputFormPage::Main),
            Self::OPTIONS => Ok(InputFormPage::Options),
            Self::STAGED => Ok(InputFormPage::Staged),
            Self::EPG => Ok(InputFormPage::Epg),
            Self::ALIAS => Ok(InputFormPage::Alias),
            Self::PROVIDER => Ok(InputFormPage::Provider),
            _ => info_err_res!("Unknown input form page: {s}"),
        }
    }
}

impl Display for InputFormPage {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match *self {
                InputFormPage::Main => Self::MAIN,
                InputFormPage::Options => Self::OPTIONS,
                InputFormPage::Staged => Self::STAGED,
                InputFormPage::Epg => Self::EPG,
                InputFormPage::Alias => Self::ALIAS,
                InputFormPage::Provider => Self::PROVIDER,
            }
        )
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
      ResolveTmdb => resolve_tmdb: bool,
      ResolveBackground => resolve_background: bool,
      ResolveSeries => resolve_series: bool,
      ResolveVod => resolve_vod: bool,
      ResolveDelay => resolve_delay: u16,
      ProbeDelay => probe_delay: u16,
      ProbeLive => probe_live: bool,
      ProbeVod => probe_vod: bool,
      ProbeSeries => probe_series: bool,
      ProbeLiveIntervalHours => probe_live_interval_hours: u32,
    }
);

generate_form_reducer!(
    state: StagedInputDtoFormState { form: StagedInputDto },
    action_name: StagedInputFormAction,
    fields {
        Enabled => enabled: bool,
        Url => url: String,
        Username => username: Option<String>,
        Password => password: Option<String>,
        Method => method: InputFetchMethod,
        InputType => input_type: InputType,
        LiveSource => live_source: Option<ClusterSource>,
        VodSource => vod_source: Option<ClusterSource>,
        SeriesSource => series_source: Option<ClusterSource>,
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
fn cluster_source_options(current: Option<ClusterSource>) -> Rc<Vec<DropDownOption>> {
    let options: [(Option<ClusterSource>, &str); 3] = [
        (Some(ClusterSource::Staged), "staged"),
        (Some(ClusterSource::Input), "input"),
        (Some(ClusterSource::Skip), "skip"),
    ];
    Rc::new(
        options
            .iter()
            .map(|(val, label)| DropDownOption {
                id: label.to_string(),
                label: html! { label.to_string() },
                selected: *val == current,
            })
            .collect(),
    )
}

fn apply_parsed_input_type(staged_input_state: &UseReducerHandle<StagedInputDtoFormState>, selected: Option<&str>) {
    let input_type =
        selected.and_then(|value| value.parse::<InputType>().ok()).unwrap_or(staged_input_state.form.input_type);
    if staged_input_state.form.input_type != input_type {
        staged_input_state.dispatch(StagedInputFormAction::InputType(input_type));
        // Clear Xtream-specific credentials when switching to a non-Xtream type
        // so they don't persist invisibly in the DTO.
        if !input_type.is_xtream() {
            staged_input_state.dispatch(StagedInputFormAction::Username(None));
            staged_input_state.dispatch(StagedInputFormAction::Password(None));
        }
    }
}

#[derive(Properties, PartialEq, Clone)]
pub struct ConfigInputViewProps {
    #[prop_or_default]
    pub(crate) block_id: Option<BlockId>,
    pub(crate) input: Option<Rc<ConfigInputDto>>,
    #[prop_or(true)]
    pub(crate) allow_write: bool,
    #[prop_or_default]
    pub(crate) on_apply: Option<Callback<ConfigInputDto>>,
    #[prop_or_default]
    pub(crate) on_cancel: Option<Callback<()>>,
}

#[component]
pub fn ConfigInputView(props: &ConfigInputViewProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();
    let source_editor_ctx = use_context::<SourceEditorContext>();
    let config_ctx = use_context::<ConfigContext>();
    let fetch_methods = use_memo((), |_| {
        [InputFetchMethod::GET, InputFetchMethod::POST].iter().map(ToString::to_string).collect::<Vec<String>>()
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
        use_reducer(|| ConfigInputFormState { form: ConfigInputDto::default(), modified: false });
    let input_options_state: UseReducerHandle<ConfigInputOptionsDtoFormState> =
        use_reducer(|| ConfigInputOptionsDtoFormState { form: ConfigInputOptionsDto::default(), modified: false });
    let staged_input_state: UseReducerHandle<StagedInputDtoFormState> =
        use_reducer(|| StagedInputDtoFormState { form: StagedInputDto::default(), modified: false });

    // State for EPG sources, Aliases, Headers, and Providers
    let epg_sources_state = use_state(Vec::<EpgSourceDto>::new);
    let aliases_state = use_state(Vec::<ConfigInputAliasDto>::new);
    let headers_state = use_state(HashMap::<String, String>::new);
    let providers_state = use_state(Vec::<ConfigProviderDto>::new);
    let providers_dirty_state = use_state(|| false);

    let epg_smart_match_state = use_state(|| None::<EpgSmartMatchConfigDto>);
    let show_smart_match_form_state = use_state(|| false);

    // State for showing item forms
    let show_epg_form_state = use_state(|| false);
    let show_alias_form_state = use_state(|| false);
    let show_provider_form_state = use_state(|| false);
    let edit_alias = use_state(|| None::<ConfigInputAliasDto>);
    let edit_provider = use_state(|| None::<ConfigProviderDto>);
    let edit_epg_source = use_state(|| None::<EpgSourceDto>);
    let exp_date_loading = use_state(|| false);
    let exp_date_request_in_flight = use_mut_ref(|| false);
    let exp_date_request_token = use_mut_ref(|| 0_u64);

    let staged_input_types = use_memo(staged_input_state.form.input_type, |input_type| {
        let default_it = input_type;
        [
            InputType::M3u,
            InputType::Xtream,
            // InputType::M3uBatch,
            // InputType::XtreamBatch,
        ]
        .iter()
        .map(|t| DropDownOption { id: t.to_string(), label: html! { t.to_string() }, selected: t == default_it })
        .collect::<Vec<DropDownOption>>()
    });

    {
        let input_form_state = input_form_state.clone();
        let input_options_state = input_options_state.clone();
        let staged_input_state = staged_input_state.clone();
        let epg_sources_state = epg_sources_state.clone();
        let epg_smart_match_state = epg_smart_match_state.clone();
        let aliases_state = aliases_state.clone();
        let headers_state = headers_state.clone();
        let providers_state = providers_state.clone();
        let providers_dirty_state = providers_dirty_state.clone();
        let deps = (props.block_id, props.input.clone(), config_ctx.clone());
        let view_visible = view_visible.clone();
        use_effect_with(deps, move |(_, cfg, config_ctx)| {
            let global_providers = config_ctx
                .as_ref()
                .and_then(|ctx| ctx.config.as_ref())
                .and_then(|cfg| cfg.sources.provider.clone())
                .unwrap_or_default();
            if let Some(input) = cfg {
                if input.input_type.is_library() && matches!(*view_visible, InputFormPage::Staged | InputFormPage::Epg)
                {
                    view_visible.set(InputFormPage::Main);
                }

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

                // Load EPG smart match
                epg_smart_match_state.set(input.epg.as_ref().and_then(|epg| epg.smart_match.clone()));

                // Load aliases
                aliases_state.set(input.aliases.clone().unwrap_or_default());

                // Load providers:
                // - Prefer explicit input-level providers.
                // - If missing, fall back to source-level providers from source.yml.
                // - If both exist, keep input providers first and append missing source-level providers.
                let mut display_providers = if let Some(input_providers) = input.provider.as_ref() {
                    input_providers.clone()
                } else {
                    global_providers.clone()
                };
                if input.provider.is_some() && !display_providers.is_empty() && !global_providers.is_empty() {
                    let mut seen: HashSet<String> =
                        display_providers.iter().map(|provider| provider.name.to_string()).collect();
                    for provider in global_providers {
                        if seen.insert(provider.name.to_string()) {
                            display_providers.push(provider);
                        }
                    }
                }
                providers_state.set(display_providers);
                providers_dirty_state.set(false);
            } else {
                input_form_state.dispatch(ConfigInputFormAction::SetAll(ConfigInputDto::default()));
                input_options_state.dispatch(ConfigInputOptionsFormAction::SetAll(ConfigInputOptionsDto::default()));
                staged_input_state.dispatch(StagedInputFormAction::SetAll(StagedInputDto::default()));
                headers_state.set(HashMap::new());
                epg_sources_state.set(Vec::new());
                epg_smart_match_state.set(None);
                aliases_state.set(Vec::new());
                providers_state.set(Vec::new());
                providers_dirty_state.set(false);
            }
            || ()
        });
    }

    let handle_add_epg_item = {
        let epg_sources = epg_sources_state.clone();
        let show_epg_form = show_epg_form_state.clone();
        let edit_epg_source = edit_epg_source.clone();
        Callback::from(move |source: EpgSourceDto| {
            let mut sources = (*epg_sources).clone();
            if let Some(existing) = edit_epg_source.as_ref() {
                if let Some(position) = sources.iter().position(|item| item == existing) {
                    if let Some(slot) = sources.get_mut(position) {
                        *slot = source;
                    }
                } else {
                    sources.push(source);
                }
                edit_epg_source.set(None);
            } else {
                sources.push(source);
            }
            epg_sources.set(sources);
            show_epg_form.set(false);
        })
    };

    let handle_close_add_epg_item = {
        let show_epg_form = show_epg_form_state.clone();
        let edit_epg_source = edit_epg_source.clone();
        Callback::from(move |_| {
            show_epg_form.set(false);
            edit_epg_source.set(None);
        })
    };

    let handle_show_add_epg_item = {
        let show_epg_form = show_epg_form_state.clone();
        let edit_epg_source = edit_epg_source.clone();
        Callback::from(move |_| {
            show_epg_form.set(true);
            edit_epg_source.set(None);
        })
    };

    let handle_edit_epg_source = {
        let epg_list = epg_sources_state.clone();
        let show_epg_form = show_epg_form_state.clone();
        let edit_epg_source = edit_epg_source.clone();
        Callback::from(move |(idx, e): (String, MouseEvent)| {
            e.prevent_default();
            e.stop_propagation();
            if let Ok(index) = idx.parse::<usize>() {
                let items = (*epg_list).clone();
                if let Some(item) = items.get(index).cloned() {
                    edit_epg_source.set(Some(item));
                    show_epg_form.set(true);
                }
            }
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

    let handle_move_alias_up = {
        let alias_list = aliases_state.clone();
        Callback::from(move |(idx, e): (String, MouseEvent)| {
            e.prevent_default();
            e.stop_propagation();
            if let Ok(index) = idx.parse::<usize>() {
                let mut items = (*alias_list).clone();
                if index > 0 && index < items.len() {
                    items.swap(index, index - 1);
                    alias_list.set(items);
                }
            }
        })
    };

    let handle_move_alias_down = {
        let alias_list = aliases_state.clone();
        Callback::from(move |(idx, e): (String, MouseEvent)| {
            e.prevent_default();
            e.stop_propagation();
            if let Ok(index) = idx.parse::<usize>() {
                let mut items = (*alias_list).clone();
                if index + 1 < items.len() {
                    items.swap(index, index + 1);
                    alias_list.set(items);
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

    let handle_submit_smart_match = {
        let epg_smart_match_state = epg_smart_match_state.clone();
        let show_smart_match_form = show_smart_match_form_state.clone();
        Callback::from(move |cfg: EpgSmartMatchConfigDto| {
            epg_smart_match_state.set(Some(cfg));
            show_smart_match_form.set(false);
        })
    };

    let handle_close_smart_match_form = {
        let show_smart_match_form = show_smart_match_form_state.clone();
        Callback::from(move |_| show_smart_match_form.set(false))
    };

    let handle_show_smart_match_form = {
        let show_smart_match_form = show_smart_match_form_state.clone();
        Callback::from(move |_: String| show_smart_match_form.set(true))
    };

    let handle_edit_smart_match = {
        let show_smart_match_form = show_smart_match_form_state.clone();
        Callback::from(move |(_, e): (String, MouseEvent)| {
            e.prevent_default();
            e.stop_propagation();
            show_smart_match_form.set(true);
        })
    };

    let handle_remove_smart_match = {
        let epg_smart_match_state = epg_smart_match_state.clone();
        Callback::from(move |(_, e): (String, MouseEvent)| {
            e.prevent_default();
            e.stop_propagation();
            epg_smart_match_state.set(None);
        })
    };

    let handle_add_provider_item = {
        let providers = providers_state.clone();
        let providers_dirty_state = providers_dirty_state.clone();
        let show_provider_form = show_provider_form_state.clone();
        let edit_provider = edit_provider.clone();
        Callback::from(move |provider: ConfigProviderDto| {
            let mut items = (*providers).clone();
            if let Some(e) = edit_provider.as_ref() {
                if let Some(pos) = items.iter().position(|x| x.name == e.name) {
                    if let Some(slot) = items.get_mut(pos) {
                        *slot = provider;
                    }
                } else {
                    items.push(provider);
                }
                edit_provider.set(None);
            } else {
                items.push(provider);
            }
            providers.set(items);
            providers_dirty_state.set(true);
            show_provider_form.set(false);
        })
    };

    let handle_close_add_provider_item = {
        let show_provider_form = show_provider_form_state.clone();
        let edit_provider = edit_provider.clone();
        Callback::from(move |()| {
            show_provider_form.set(false);
            edit_provider.set(None);
        })
    };

    let handle_show_add_provider_item = {
        let show_provider_form = show_provider_form_state.clone();
        let edit_provider = edit_provider.clone();
        Callback::from(move |_| {
            show_provider_form.set(true);
            edit_provider.set(None);
        })
    };

    let handle_remove_provider_list_item = {
        let provider_list = providers_state.clone();
        let providers_dirty_state = providers_dirty_state.clone();
        Callback::from(move |(idx, e): (String, MouseEvent)| {
            e.prevent_default();
            e.stop_propagation();
            if let Ok(index) = idx.parse::<usize>() {
                let mut items = (*provider_list).clone();
                if index < items.len() {
                    items.remove(index);
                    provider_list.set(items);
                    providers_dirty_state.set(true);
                }
            }
        })
    };

    let handle_edit_provider_list_item = {
        let provider_list = providers_state.clone();
        let show_provider_form = show_provider_form_state.clone();
        let edit_provider = edit_provider.clone();
        Callback::from(move |(idx, e): (String, MouseEvent)| {
            e.prevent_default();
            e.stop_propagation();
            if let Ok(index) = idx.parse::<usize>() {
                let items = (*provider_list).clone();
                if index < items.len() {
                    let item = items.get(index).cloned();
                    edit_provider.set(item);
                    show_provider_form.set(true);
                }
            }
        })
    };

    let library_input = input_form_state.form.input_type.is_library();
    let xtream_input = input_form_state.form.input_type.is_xtream();

    let render_options = || {
        let headers = headers_state.clone();
        if !props.allow_write {
            return html! {
                <Card class="tp__config-view__card">
                { html_if!(xtream_input, {
                    <>
                    <TitledCard title={translate.t(LABEL_SKIP)}>
                      <div class="tp__config-view__cols-3">
                        { config_field_bool!(input_options_state.form, translate.t(LABEL_XTREAM_SKIP_LIVE), xtream_skip_live) }
                        { config_field_bool!(input_options_state.form, translate.t(LABEL_XTREAM_SKIP_VOD), xtream_skip_vod) }
                        { config_field_bool!(input_options_state.form, translate.t(LABEL_XTREAM_SKIP_SERIES), xtream_skip_series) }
                      </div>
                    </TitledCard>
                    <TitledCard title={translate.t(LABEL_LIVE_STREAMS)}>
                      <div class="tp__config-view__cols-2">
                        { config_field_bool!(input_options_state.form, translate.t(LABEL_XTREAM_LIVE_STREAM_USE_PREFIX), xtream_live_stream_use_prefix) }
                        { config_field_bool!(input_options_state.form, translate.t(LABEL_XTREAM_LIVE_STREAM_WITHOUT_EXTENSION), xtream_live_stream_without_extension) }
                      </div>
                    </TitledCard>
                    <TitledCard title={translate.t(LABEL_RESOLVE)}>
                        <div class="tp__config-view__cols-3">
                        { config_field_bool!(input_options_state.form, translate.t(LABEL_XTREAM_SKIP_VOD), resolve_vod) }
                        { config_field_bool!(input_options_state.form, translate.t(LABEL_XTREAM_SKIP_SERIES), resolve_series) }
                        </div>
                        <div class="tp__config-view__cols-2">
                        { config_field_custom!(translate.t(LABEL_RESOLVE_DELAY_SEC), input_options_state.form.resolve_delay.to_string()) }
                        </div>
                        { config_field_bool!(input_options_state.form, translate.t(LABEL_RESOLVE_BACKGROUND), resolve_background) }
                    </TitledCard>
                    <TitledCard title={translate.t(LABEL_PROBE)}>
                        <div class="tp__config-view__cols-3">
                        { config_field_bool!(input_options_state.form, translate.t(LABEL_XTREAM_SKIP_LIVE), probe_live) }
                        { config_field_bool!(input_options_state.form, translate.t(LABEL_XTREAM_SKIP_VOD), probe_vod) }
                        { config_field_bool!(input_options_state.form, translate.t(LABEL_XTREAM_SKIP_SERIES), probe_series) }
                        </div>
                        <div class="tp__config-view__cols-2">
                        { config_field_custom!(translate.t(LABEL_PROBE_DELAY_SEC), input_options_state.form.probe_delay.to_string()) }
                        { config_field_custom!(translate.t(LABEL_PROBE_LIVE_INTERVAL_HOURS), input_options_state.form.probe_live_interval_hours.to_string()) }
                        </div>
                    </TitledCard>
                    </>
                })}
                <TitledCard title={translate.t(LABEL_METADATA)}>
                  { config_field_bool!(input_options_state.form, translate.t(LABEL_RESOLVE_TMDB), resolve_tmdb) }
                </TitledCard>
                { config_field_child!(translate.t(LABEL_HEADERS), "INPUT_FORM.HEADERS", {
                    let headers_set = headers.clone();
                    html! {
                        <KeyValueEditor
                            entries={(*headers).clone()}
                            readonly={!props.allow_write}
                            key_placeholder={translate.t("LABEL.HEADER_NAME")}
                            value_placeholder={translate.t("LABEL.HEADER_VALUE")}
                            on_change={Callback::from(move |new_headers: HashMap<String, String>| {
                                headers_set.set(new_headers);
                            })}
                        />
                    }
                })}
                </Card>
            };
        }
        html! {
            <Card class="tp__config-view__card">
            { html_if!(xtream_input, {
                <>
                <TitledCard title={translate.t(LABEL_SKIP)}>
                  <div class="tp__config-view__cols-3">
                    { edit_field_bool!(input_options_state, translate.t(LABEL_XTREAM_SKIP_LIVE), xtream_skip_live, ConfigInputOptionsFormAction::XtreamSkipLive) }
                    { edit_field_bool!(input_options_state, translate.t(LABEL_XTREAM_SKIP_VOD), xtream_skip_vod, ConfigInputOptionsFormAction::XtreamSkipVod) }
                    { edit_field_bool!(input_options_state, translate.t(LABEL_XTREAM_SKIP_SERIES), xtream_skip_series, ConfigInputOptionsFormAction::XtreamSkipSeries) }
                  </div>
                </TitledCard>
                <TitledCard title={translate.t(LABEL_LIVE_STREAMS)}>
                  <div class="tp__config-view__cols-2">
                    { edit_field_bool!(input_options_state, translate.t(LABEL_XTREAM_LIVE_STREAM_USE_PREFIX), xtream_live_stream_use_prefix, ConfigInputOptionsFormAction::XtreamLiveStreamUsePrefix) }
                    { edit_field_bool!(input_options_state, translate.t(LABEL_XTREAM_LIVE_STREAM_WITHOUT_EXTENSION), xtream_live_stream_without_extension, ConfigInputOptionsFormAction::XtreamLiveStreamWithoutExtension) }
                  </div>
                </TitledCard>
                <TitledCard title={translate.t(LABEL_RESOLVE)}>
                    <div class="tp__config-view__cols-3">
                    { edit_field_bool!(input_options_state, translate.t(LABEL_XTREAM_SKIP_VOD), resolve_vod,  ConfigInputOptionsFormAction::ResolveVod) }
                    { edit_field_bool!(input_options_state, translate.t(LABEL_XTREAM_SKIP_SERIES), resolve_series,  ConfigInputOptionsFormAction::ResolveSeries) }
                    </div>
                    <div class="tp__config-view__cols-2">
                    { edit_field_number_u16!(input_options_state, translate.t(LABEL_RESOLVE_DELAY_SEC), resolve_delay,  ConfigInputOptionsFormAction::ResolveDelay) }
                    </div>
                    { edit_field_bool!(input_options_state, translate.t(LABEL_RESOLVE_BACKGROUND), resolve_background,  ConfigInputOptionsFormAction::ResolveBackground) }
                </TitledCard>
                <TitledCard title={translate.t(LABEL_PROBE)}>
                    <div class="tp__config-view__cols-3">
                    { edit_field_bool!(input_options_state, translate.t(LABEL_XTREAM_SKIP_LIVE), probe_live,  ConfigInputOptionsFormAction::ProbeLive) }
                    { edit_field_bool!(input_options_state, translate.t(LABEL_XTREAM_SKIP_VOD), probe_vod,  ConfigInputOptionsFormAction::ProbeVod) }
                    { edit_field_bool!(input_options_state, translate.t(LABEL_XTREAM_SKIP_SERIES), probe_series,  ConfigInputOptionsFormAction::ProbeSeries) }
                    </div>
                    <div class="tp__config-view__cols-2">
                    { edit_field_number_u16!(input_options_state, translate.t(LABEL_PROBE_DELAY_SEC), probe_delay,  ConfigInputOptionsFormAction::ProbeDelay) }
                    { edit_field_number_u32!(input_options_state, translate.t(LABEL_PROBE_LIVE_INTERVAL_HOURS), probe_live_interval_hours,  ConfigInputOptionsFormAction::ProbeLiveIntervalHours) }
                    </div>
                </TitledCard>
                </>
            })}
            <TitledCard title={translate.t(LABEL_METADATA)}>
              { edit_field_bool!(input_options_state, translate.t(LABEL_RESOLVE_TMDB), resolve_tmdb, ConfigInputOptionsFormAction::ResolveTmdb) }
            </TitledCard>
            { config_field_child!(translate.t(LABEL_HEADERS), "INPUT_FORM.HEADERS", {
                let headers_set = headers.clone();
                html! {
                    <KeyValueEditor
                        entries={(*headers).clone()}
                        readonly={!props.allow_write}
                        key_placeholder={translate.t("LABEL.HEADER_NAME")}
                        value_placeholder={translate.t("LABEL.HEADER_VALUE")}
                        on_change={Callback::from(move |new_headers: HashMap<String, String>| {
                            headers_set.set(new_headers);
                        })}
                    />
                }
            })}
            </Card>
        }
    };

    let render_staged = || {
        let staged_method_selection = Rc::new(vec![staged_input_state.form.method.to_string()]);
        let staged_input_state_1 = staged_input_state.clone();
        let staged_input_state_2 = staged_input_state.clone();
        let staged_input_state_live = staged_input_state.clone();
        let staged_input_state_vod = staged_input_state.clone();
        let staged_input_state_series = staged_input_state.clone();
        let show_cluster_sources = input_form_state.form.input_type.is_xtream();
        let live_source_options = cluster_source_options(staged_input_state.form.live_source);
        let vod_source_options = cluster_source_options(staged_input_state.form.vod_source);
        let series_source_options = cluster_source_options(staged_input_state.form.series_source);
        let staged_is_xtream = staged_input_state.form.input_type.is_xtream();
        if !props.allow_write {
            return html! {
                <Card class="tp__config-view__card">
                    { config_field_bool!(staged_input_state.form, translate.t(LABEL_ENABLED), enabled) }
                    { config_field!(staged_input_state.form, translate.t(LABEL_URL), url) }
                    { html_if!(staged_input_state.form.input_type.is_xtream(), {
                        <div class="tp__config-view__cols-2">
                        { config_field_optional!(staged_input_state.form, translate.t(LABEL_USERNAME), username) }
                        { config_field_optional_hide!(staged_input_state.form, translate.t(LABEL_PASSWORD), password) }
                        </div>
                    })}
                    <div class="tp__config-view__cols-2">
                    { config_field_custom!(translate.t(LABEL_FETCH_METHOD), staged_input_state.form.method.to_string()) }
                    { config_field_custom!(translate.t(LABEL_INPUT_TYPE), staged_input_state.form.input_type.to_string()) }
                    </div>
                    {
                        html_if!(show_cluster_sources, {
                        <div class="tp__config-view__cols-2">
                        { config_field_custom!(translate.t(LABEL_LIVE_SOURCE), staged_input_state.form.live_source.map_or_else(String::new, |source| source.to_string())) }
                        { config_field_custom!(translate.t(LABEL_VOD_SOURCE), staged_input_state.form.vod_source.map_or_else(String::new, |source| source.to_string())) }
                        { config_field_custom!(translate.t(LABEL_SERIES_SOURCE), staged_input_state.form.series_source.map_or_else(String::new, |source| source.to_string())) }
                        </div>
                        })
                    }
                </Card>
            };
        }
        html! {
            <Card class="tp__config-view__card">
                { edit_field_bool!(staged_input_state, translate.t(LABEL_ENABLED),  enabled, StagedInputFormAction::Enabled) }
                { edit_field_text!(staged_input_state, translate.t(LABEL_URL),  url, StagedInputFormAction::Url) }
                { html_if!(staged_input_state.form.input_type.is_xtream(), {
                  <div class="tp__config-view__cols-2">
                  { edit_field_text_option!(staged_input_state, translate.t(LABEL_USERNAME), username, StagedInputFormAction::Username) }
                  { edit_field_text_option!(staged_input_state, translate.t(LABEL_PASSWORD), password, StagedInputFormAction::Password, true) }
                  </div>
                })}
                <div class="tp__config-view__cols-2">
                { config_field_child!(translate.t(LABEL_FETCH_METHOD), "INPUT_FORM.FETCH_METHOD", {

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
               { config_field_child!(translate.t(LABEL_INPUT_TYPE), "INPUT_FORM.INPUT_TYPE", {
                   html! {
                       <Select
                        name={"staged_input_types"}
                        multi_select={false}
                        on_select={Callback::from(move |(_, selections):(String, DropDownSelection)| {
                           match selections {
                            DropDownSelection::Empty => {}
                            DropDownSelection::Single(option) => {
                                apply_parsed_input_type(
                                    &staged_input_state_2,
                                    Some(option.as_str()),
                                );
                            }
                            DropDownSelection::Multi(options) => {
                                apply_parsed_input_type(
                                    &staged_input_state_2,
                                    options.first().map(String::as_str),
                                );
                             }
                           }
                        })}
                        options={staged_input_types.clone()}
                    />
               }})}
                </div>
                {
                    html_if!(show_cluster_sources, {
                    <div class="tp__config-view__cols-2">
                    { config_field_child!(translate.t(LABEL_LIVE_SOURCE), "INPUT_FORM.LIVE_SOURCE", {
                        html! {
                            <Select
                                name={"live_source"}
                                multi_select={false}
                                on_select={Callback::from(move |(_, selections):(String, DropDownSelection)| {
                                    if let DropDownSelection::Single(option) = selections {
                                        staged_input_state_live.dispatch(StagedInputFormAction::LiveSource(
                                            ClusterSource::from_str(option.as_str()).ok()
                                        ));
                                    }
                                })}
                                options={live_source_options}
                            />
                        }
                    })}
                    { config_field_child!(translate.t(LABEL_VOD_SOURCE), "INPUT_FORM.VOD_SOURCE", {
                        html! {
                            <Select
                                name={"vod_source"}
                                multi_select={false}
                                on_select={Callback::from(move |(_, selections):(String, DropDownSelection)| {
                                    if let DropDownSelection::Single(option) = selections {
                                        staged_input_state_vod.dispatch(StagedInputFormAction::VodSource(
                                            ClusterSource::from_str(option.as_str()).ok()
                                        ));
                                    }
                                })}
                                options={vod_source_options}
                            />
                        }
                    })}
                    { config_field_child!(translate.t(LABEL_SERIES_SOURCE), "INPUT_FORM.SERIES_SOURCE", {
                        html! {
                            <Select
                                name={"series_source"}
                                multi_select={false}
                                on_select={Callback::from(move |(_, selections):(String, DropDownSelection)| {
                                    if let DropDownSelection::Single(option) = selections {
                                        staged_input_state_series.dispatch(StagedInputFormAction::SeriesSource(
                                            ClusterSource::from_str(option.as_str()).ok()
                                        ));
                                    }
                                })}
                                options={series_source_options}
                            />
                        }
                    })}
                    </div>
                    })
                }

                //{ edit_field_list!(staged_input_state, translate.t(LABEL_HEADERS), headers, StagedInputFormAction::Headers, translate.t(LABEL_ADD_HEADER)) }
            </Card>
        }
    };

    let render_input = || {
        let input_method_selection = Rc::new(vec![input_form_state.form.method.to_string()]);
        let input_form_state_disp = input_form_state.clone();
        let exp_date_tool_action = if input_form_state.form.input_type.is_xtream() {
            let services = services.clone();
            let input_form_state = input_form_state.clone();
            let exp_date_loading = exp_date_loading.clone();
            let exp_date_request_in_flight = exp_date_request_in_flight.clone();
            let exp_date_request_token = exp_date_request_token.clone();
            let translate = translate.clone();

            Some(ToolAction {
                name: Some("RefreshExpDate".to_string()),
                icon: "Refresh".to_string(),
                hint: Some(translate.t(LABEL_RESOLVE)),
                class: (*exp_date_loading).then(|| "loading".to_string()),
                onclick: Callback::from(move |_event: MouseEvent| {
                    if *exp_date_request_in_flight.borrow() {
                        return;
                    }

                    let url = input_form_state.form.url.clone();
                    let username = input_form_state.form.username.clone().unwrap_or_default();
                    let password = input_form_state.form.password.clone().unwrap_or_default();

                    if url.trim().is_empty() || username.trim().is_empty() || password.trim().is_empty() {
                        services
                            .toastr
                            .error(translate.t("MESSAGES.SOURCE_EDITOR.URL_USERNAME_AND_PASSWORD_MANDATORY"));
                        return;
                    }

                    *exp_date_request_in_flight.borrow_mut() = true;
                    let request_token = {
                        let mut token = exp_date_request_token.borrow_mut();
                        *token += 1;
                        *token
                    };
                    exp_date_loading.set(true);
                    let services = services.clone();
                    let input_form_state = input_form_state.clone();
                    let exp_date_loading = exp_date_loading.clone();
                    let exp_date_request_in_flight = exp_date_request_in_flight.clone();
                    let exp_date_request_token = exp_date_request_token.clone();
                    let request = XtreamLoginRequest { url, username, password };

                    spawn_local(async move {
                        let current_snapshot = || {
                            (
                                input_form_state.form.url.clone(),
                                input_form_state.form.username.clone().unwrap_or_default(),
                                input_form_state.form.password.clone().unwrap_or_default(),
                            )
                        };
                        match services.config.get_xtream_login_info(&request).await {
                            Ok(login_info) => {
                                if *exp_date_request_token.borrow() == request_token {
                                    let snapshot_matches = current_snapshot()
                                        == (request.url.clone(), request.username.clone(), request.password.clone());
                                    if snapshot_matches {
                                        if let Some(exp_date) = login_info.exp_date {
                                            input_form_state.dispatch(ConfigInputFormAction::ExpDate(Some(exp_date)));
                                        } else {
                                            services.toastr.warning("No expiration date returned by provider");
                                        }
                                    }
                                }
                            }
                            Err(err) => {
                                if *exp_date_request_token.borrow() == request_token {
                                    services.toastr.error(err.to_string());
                                }
                            }
                        }
                        if *exp_date_request_token.borrow() == request_token {
                            *exp_date_request_in_flight.borrow_mut() = false;
                            exp_date_loading.set(false);
                        }
                    });
                }),
            })
        } else {
            None
        };

        if !props.allow_write {
            return html! {
                 <Card class="tp__config-view__card">
                   <div class="tp__config-view__cols-2">
                   { config_field!(input_form_state.form, translate.t(LABEL_NAME), name) }
                   { config_field_bool!(input_form_state.form, translate.t(LABEL_ENABLED), enabled) }
                   </div>
                   { html_if!(!library_input, {
                    <>
                     { config_field!(input_form_state.form, translate.t(LABEL_URL), url) }
                     <div class="tp__config-view__cols-2">
                     { html_if!(xtream_input, {
                       <>
                       { config_field_optional!(input_form_state.form, translate.t(LABEL_USERNAME), username) }
                       { config_field_optional_hide!(input_form_state.form, translate.t(LABEL_PASSWORD), password) }
                       </>
                     })}
                     { config_field_custom!(translate.t(LABEL_MAX_CONNECTIONS), input_form_state.form.max_connections.to_string()) }
                     { config_field_custom!(translate.t(LABEL_PRIORITY), input_form_state.form.priority.to_string()) }
                     { config_field_custom!(translate.t(LABEL_EXP_DATE), input_form_state.form.exp_date.map_or_else(String::new, |exp_date| exp_date.to_string())) }
                     { config_field_optional!(input_form_state.form, translate.t(LABEL_CACHE_DURATION), cache_duration) }
                     { config_field_custom!(translate.t(LABEL_FETCH_METHOD), input_form_state.form.method.to_string()) }
                     </div>
                     { config_field_optional!(input_form_state.form, translate.t(LABEL_PERSIST), persist) }
                    </>
                   })}
                </Card>
            };
        }
        html! {
             <Card class="tp__config-view__card">
               <div class="tp__config-view__cols-2">
               { edit_field_text!(input_form_state, translate.t(LABEL_NAME),  name, ConfigInputFormAction::Name) }
               { edit_field_bool!(input_form_state, translate.t(LABEL_ENABLED), enabled, ConfigInputFormAction::Enabled) }
               </div>
               { html_if!(!library_input, {
                <>
                 { edit_field_text!(input_form_state, translate.t(LABEL_URL),  url, ConfigInputFormAction::Url) }
                 <div class="tp__config-view__cols-2">
                 { html_if!(xtream_input, {
                   <>
                   { edit_field_text_option!(input_form_state, translate.t(LABEL_USERNAME), username, ConfigInputFormAction::Username) }
                   { edit_field_text_option!(input_form_state, translate.t(LABEL_PASSWORD), password, ConfigInputFormAction::Password, true) }
                   </>
                 })}
                 { edit_field_number_u16!(input_form_state, translate.t(LABEL_MAX_CONNECTIONS), max_connections, ConfigInputFormAction::MaxConnections) }
                 { edit_field_number_i16!(input_form_state, translate.t(LABEL_PRIORITY), priority, ConfigInputFormAction::Priority) }
                 { edit_field_exp_date!(input_form_state, translate.t(LABEL_EXP_DATE), exp_date, ConfigInputFormAction::ExpDate, exp_date_tool_action) }
                 { edit_field_text_option!(input_form_state, translate.t(LABEL_CACHE_DURATION), cache_duration, ConfigInputFormAction::CacheDuration) }
                 { config_field_child!(translate.t(LABEL_FETCH_METHOD), "INPUT_FORM.FETCH_METHOD", {
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
                </>
               })}
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
                        input_type={input_form_state.form.input_type}
                        initial={(*edit_alias).clone()}
                        on_submit={handle_add_alias_item}
                        on_cancel={handle_close_add_alias_item}
                        readonly={!props.allow_write}
                    />
              } else {
                  { config_field_child!(translate.t(LABEL_ALIASES), "INPUT_FORM.ALIASES", {
                      let aliases_list = aliases.clone();
                      let alias_count = aliases_list.len();
                      html! {
                        <div class="tp__form-list">
                            <div class="tp__form-list__items">
                            {
                                for (*aliases_list).iter().enumerate().map(|(idx, alias)| {
                                    html! {
                                        <div class="tp__form-list__item" key={format!("alias-{idx}")}>
                                            <div class="tp__form-list__item-toolbar">
                                                if props.allow_write && idx > 0 {
                                                    <IconButton
                                                        class="tp__form-list__item-arrow-btn"
                                                        name={idx.to_string()}
                                                        icon="ArrowUp"
                                                        onclick={handle_move_alias_up.clone()}
                                                    />
                                                } else if props.allow_write && alias_count > 2 {
                                                    <span class="tp__form-list__item-placeholder-btn"/>
                                                }
                                                if props.allow_write && idx + 1 < alias_count {
                                                    <IconButton
                                                        class="tp__form-list__item-arrow-btn"
                                                        name={idx.to_string()}
                                                        icon="ArrowDown"
                                                        onclick={handle_move_alias_down.clone()}
                                                    />
                                                } else if props.allow_write && alias_count > 2 {
                                                    <span class="tp__form-list__item-placeholder-btn"/>
                                                }
                                                <IconButton
                                                name={idx.to_string()}
                                                icon="Edit"
                                                onclick={handle_edit_alias_list_item.clone()}/>
                                                if props.allow_write {
                                                    <IconButton
                                                    name={idx.to_string()}
                                                    icon="Delete"
                                                    onclick={handle_remove_alias_list_item.clone()}/>
                                                }
                                            </div>
                                            <div class="tp__form-list__item-content">
                                                <span class={if alias.enabled {""} else {"inactive"}}>
                                                    {
                                                        if alias.name.is_empty() {
                                                            html! { alias.url.as_str() }
                                                        } else {
                                                            html! { <><strong>{alias.name.as_ref()}</strong>{" - "}{alias.url.as_str()}</> }
                                                        }
                                                    }
                                                </span>
                                            </div>
                                        </div>
                                    }
                                })
                            }
                            </div>
                            if props.allow_write {
                                <div class="tp__form-list__toolbar">
                                    <TextButton
                                        class="primary"
                                        name="add_alias"
                                        icon="Add"
                                        title={translate.t(LABEL_ADD_ALIAS)}
                                        onclick={handle_show_add_alias_item}
                                    />
                                </div>
                            }
                          </div>
                      }
                  })}
              }
            </Card>
        }
    };

    let render_provider = || {
        let providers = providers_state.clone();
        let show_provider_form = show_provider_form_state.clone();
        let edit_provider = edit_provider.clone();

        html! {
            <Card class="tp__config-view__card">
              if *show_provider_form {
                    <ProviderItemForm
                        initial={(*edit_provider).clone()}
                        on_submit={handle_add_provider_item.clone()}
                        on_cancel={handle_close_add_provider_item.clone()}
                        readonly={!props.allow_write}
                    />
              } else {
                  { config_field_child!(translate.t(LABEL_PROVIDERS), "INPUT_FORM.PROVIDERS", {
                      let provider_list = providers.clone();
                      html! {
                        <div class="tp__form-list">
                            <div class="tp__form-list__items">
                            {
                                for (*provider_list).iter().enumerate().map(|(idx, provider)| {
                                    html! {
                                        <div class="tp__form-list__item" key={format!("provider-{idx}")}>
                                            <div class="tp__form-list__item-toolbar">
                                                <IconButton
                                                    name={idx.to_string()}
                                                    icon="Edit"
                                                    onclick={handle_edit_provider_list_item.clone()}
                                                />
                                                if props.allow_write {
                                                    <IconButton
                                                        name={idx.to_string()}
                                                        icon="Delete"
                                                        onclick={handle_remove_provider_list_item.clone()}
                                                    />
                                                }
                                            </div>
                                            <div class="tp__form-list__item-content">
                                                <span>
                                                    <strong>{provider.name.as_ref()}</strong>
                                                </span>
                                            </div>
                                        </div>
                                    }
                                })
                            }
                            </div>
                            if props.allow_write {
                                <div class="tp__form-list__toolbar">
                                    <TextButton
                                        class="primary"
                                        name="add_provider"
                                        icon="Add"
                                        title={translate.t(LABEL_ADD_PROVIDER)}
                                        onclick={handle_show_add_provider_item.clone()}
                                    />
                                </div>
                            }
                          </div>
                      }
                  })}
              }
            </Card>
        }
    };

    let render_epg = || {
        let epg_sources = epg_sources_state.clone();
        let epg_smart_match = epg_smart_match_state.clone();
        let show_epg_form = show_epg_form_state.clone();
        let show_smart_match_form = show_smart_match_form_state.clone();
        let edit_epg_source = edit_epg_source.clone();

        html! {
            <Card class="tp__config-view__card">
               if *show_epg_form {
                    <EpgSourceItemForm
                        on_submit={handle_add_epg_item}
                        on_cancel={handle_close_add_epg_item}
                        initial={(*edit_epg_source).clone()}
                        readonly={!props.allow_write}
                    />
               } else if *show_smart_match_form {
                    <EpgSmartMatchForm
                        on_submit={handle_submit_smart_match}
                        on_cancel={handle_close_smart_match_form}
                        initial={(*epg_smart_match).clone()}
                        readonly={!props.allow_write}
                    />
               } else  {
                  // EPG Sources Section
                  { config_field_child!(translate.t(LABEL_EPG_SOURCES), "INPUT_FORM.EPG_SOURCES", {
                      let epg_sources_list = epg_sources.clone();

                      html! {
                        <div class="tp__form-list">
                            <div class="tp__form-list__items">
                            {
                                for (*epg_sources_list).iter().enumerate().map(|(idx, source)| {
                                    html! {
                                        <div class="tp__form-list__item" key={format!("epg-{idx}")}>
                                            <div class="tp__form-list__item-toolbar">
                                                <IconButton
                                                    name={idx.to_string()}
                                                    icon="Edit"
                                                    onclick={handle_edit_epg_source.clone()} />
                                                if props.allow_write {
                                                    <IconButton
                                                        name={idx.to_string()}
                                                        icon="Delete"
                                                        onclick={handle_remove_epg_source.clone()} />
                                                }
                                            </div>
                                            <div class="tp__form-list__item-content">
                                                <span>
                                                    {&source.url}
                                                    {" ("}
                                                    {source.priority}
                                                    {", "}
                                                    {if source.logo_override { "logo_override" } else { "no_logo_override" }}
                                                    {")"}
                                                </span>
                                            </div>
                                        </div>
                                    }
                                })
                            }
                            </div>
                        if props.allow_write {
                            <TextButton
                                    class="primary"
                                    name="add_epg_source"
                                    icon="Add"
                                    title={translate.t(LABEL_ADD_EPG_SOURCE)}
                                    onclick={handle_show_add_epg_item}
                                />
                        }
                        </div>
                      }
                  })
                  }

                  // EPG Smart Match Section
                  { config_field_child!(translate.t(LABEL_EPG_SMART_MATCH), "INPUT_FORM.EPG_SMART_MATCH", {
                      let smart_match = epg_smart_match.clone();
                      let smart_match_entry = (*smart_match).clone();

                      html! {
                        <div class="tp__form-list">
                            if let Some(cfg) = smart_match_entry {
                                <div class="tp__form-list__items">
                                    <div class="tp__form-list__item" key={"epg-smart-match"}>
                                        <div class="tp__form-list__item-toolbar">
                                            <IconButton
                                                name={"edit_smart_match"}
                                                icon="Edit"
                                                onclick={handle_edit_smart_match.clone()}
                                            />
                                            if props.allow_write {
                                                <IconButton
                                                    name={"remove_smart_match"}
                                                    icon="Delete"
                                                    onclick={handle_remove_smart_match.clone()}
                                                />
                                            }
                                        </div>
                                        <div class="tp__form-list__item-content">
                                            <span>
                                                {if cfg.enabled { "enabled" } else { "disabled" }}
                                                {" | "}
                                                {if cfg.fuzzy_matching { "fuzzy" } else { "exact" }}
                                                {" | "}
                                                {cfg.match_threshold}
                                                {" / "}
                                                {cfg.best_match_threshold}
                                                {"%"}
                                            </span>
                                        </div>
                                    </div>
                                </div>
                            }
                            if props.allow_write {
                                <div class="tp__form-list__toolbar">
                                    <TextButton
                                        class="primary"
                                        name="edit_epg_smart_match"
                                        icon={if (*smart_match).is_some() { "Edit" } else { "Add" }}
                                        title={
                                            if (*smart_match).is_some() {
                                                translate.t(LABEL_EDIT_EPG_SMART_MATCH)
                                            } else {
                                                translate.t(LABEL_EPG_SMART_MATCH)
                                            }
                                        }
                                        onclick={handle_show_smart_match_form.clone()}
                                    />
                                </div>
                            }
                        </div>
                      }
                  })
               }}
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
        let epg_smart_match_state = epg_smart_match_state.clone();
        let aliases_state = aliases_state.clone();
        let providers_state = providers_state.clone();
        let providers_dirty_state = providers_dirty_state.clone();

        Callback::from(move |_| {
            let mut input = input_form_state.data().clone();

            let options = input_options_state.data();
            input.options = if options.is_empty() { None } else { Some(options.clone()) };

            let staged_input = staged_input_state.data();
            input.staged = if staged_input.is_empty() { None } else { Some(staged_input.clone()) };

            // Handle Headers
            input.headers = (*headers_state).clone();

            // Handle EPG: update sources and smart_match
            let epg_sources = (*epg_sources_state).clone();
            let smart_match = (*epg_smart_match_state).clone();
            let sources_opt = if epg_sources.is_empty() { None } else { Some(epg_sources) };
            input.epg = if sources_opt.is_some() || smart_match.is_some() {
                let mut epg_cfg = input.epg.take().unwrap_or_default();
                epg_cfg.sources = sources_opt;
                epg_cfg.smart_match = smart_match;
                Some(epg_cfg)
            } else {
                None
            };

            // Handle Aliases
            let aliases = (*aliases_state).clone();
            input.aliases = if aliases.is_empty() { None } else { Some(aliases) };

            // Handle Providers
            if *providers_dirty_state {
                let providers = (*providers_state).clone();
                // Keep explicit empty overrides (Some(vec![])) so deleting the last provider
                // survives source-level fallback logic during save.
                input.provider = Some(providers);
            }

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

            <div class="tp__source-editor-form__body__pages">
                <Panel value={InputFormPage::Main.to_string()} active={view_visible.to_string()}>
                {render_input()}
                </Panel>
                { html_if!(!library_input, {
                    <Panel value={InputFormPage::Alias.to_string()} active={view_visible.to_string()}>
                    {render_alias()}
                    </Panel>
                })}
                { html_if!(!library_input, {
                 <>
                  <Panel value={InputFormPage::Options.to_string()} active={view_visible.to_string()}>
                   {render_options()}
                  </Panel>
                  <Panel value={InputFormPage::Provider.to_string()} active={view_visible.to_string()}>
                   {render_provider()}
                   </Panel>
                  <Panel value={InputFormPage::Epg.to_string()} active={view_visible.to_string()}>
                   {render_epg()}
                  </Panel>
                  <Panel value={InputFormPage::Staged.to_string()} active={view_visible.to_string()}>
                   {render_staged()}
                   </Panel>
                    </>
                })}
            </div>
            </div>
        }
    };

    let button_disabled =
        *show_alias_form_state || *show_epg_form_state || *show_provider_form_state || *show_smart_match_form_state;

    let render_sidebar = || {
        html! {
            <div class={concat_string!("tp__source-editor-form__sidebar", if button_disabled {" disabled"} else {""})}>
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", InputFormPage::Main, if *view_visible == InputFormPage::Main { " active" } else {""})}  icon="Settings" hint={translate.t(LABEL_MAIN)} name={InputFormPage::Main.to_string()} onclick={&handle_menu_click}></IconButton>
            {html_if!(!library_input, {
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", InputFormPage::Alias, if *view_visible == InputFormPage::Alias { " active" } else {""})}  icon="Alias" hint={translate.t(LABEL_ALIAS)} name={InputFormPage::Alias.to_string()} onclick={&handle_menu_click}></IconButton>
            })}
            { html_if!(!library_input, {
                <>
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", InputFormPage::Options, if *view_visible == InputFormPage::Options { " active" } else {""})}  icon="Options" hint={translate.t(LABEL_OPTIONS)} name={InputFormPage::Options.to_string()} onclick={&handle_menu_click}></IconButton>
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", InputFormPage::Epg, if *view_visible == InputFormPage::Epg { " active" } else {""})}  icon="Epg" hint={translate.t(LABEL_EPG)} name={InputFormPage::Epg.to_string()} onclick={&handle_menu_click}></IconButton>
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", InputFormPage::Provider, if *view_visible == InputFormPage::Provider { " active" } else {""})}  icon="Dns" hint={translate.t(LABEL_PROVIDER)} name={InputFormPage::Provider.to_string()} onclick={&handle_menu_click}></IconButton>
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", InputFormPage::Staged, if *view_visible == InputFormPage::Staged { " active" } else {""})}  icon="Staged" hint={translate.t(LABEL_STAGED)} name={InputFormPage::Staged.to_string()} onclick={&handle_menu_click}></IconButton>
                </>
             })}
          </div>
        }
    };

    html! {
        <div class="tp__source-editor-form tp__config-view-page">
          <div class="tp__source-editor-form__toolbar tp__form-page__toolbar">
             <TextButton class={concat_string!("secondary", if button_disabled {" disabled"} else {""} )} name="cancel_input"
                icon="Cancel"
                title={ translate.t("LABEL.CANCEL")}
                onclick={handle_cancel}></TextButton>
             if props.allow_write {
                 <TextButton class={concat_string!("primary", if button_disabled {" disabled"} else {""} )} name="apply_input"
                    icon="Accept"
                    title={ translate.t("LABEL.OK")}
                    onclick={handle_apply_input}></TextButton>
             }
          </div>
        <div class="tp__source-editor-form__content">
            { render_sidebar() }
            { render_edit_mode() }
        </div>
        </div>
    }
}

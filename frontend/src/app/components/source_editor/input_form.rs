use crate::{
    app::{
        components::{
            config::HasFormData, input::Input, key_value_editor::KeyValueEditor, AliasItemForm, BlockId, BlockInstance,
            Card, ClusterFlagsInput, ClusterFlagsInputMode, EditMode, EpgSmartMatchForm, EpgSourceItemForm,
            FilterInput, HideContent, IconButton, Panel, ProviderItemForm, RadioButtonGroup, SourceEditorContext,
            TextButton, TitledCard, ToggleSwitch, ToolAction,
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
    model::{
        ClusterFlags, ConfigInputAliasDto, ConfigInputDto, ConfigInputOptionsDto, ConfigInputStagedDto,
        ConfigProviderDto, EpgSmartMatchConfigDto, EpgSourceDto, InputFetchMethod, MediaServerInputConfigDto,
        MediaServerLibrarySelector, OnConnectErrorPolicy, ProviderUrlSelectionPolicy, StagedInputType,
        XtreamLoginRequest,
    },
    utils::{Internable, BATCH_SCHEME_PREFIX},
};
use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
    rc::Rc,
    str::FromStr,
    sync::Arc,
};
use web_sys::MouseEvent;
use yew::{
    component, html, platform::spawn_local, use_context, use_effect_with, use_memo, use_mut_ref, use_reducer,
    use_state, Callback, Html, Properties, UseReducerHandle,
};

const LABEL_NAME: &str = "LABEL.NAME";
const LABEL_FETCH_METHOD: &str = "LABEL.METHOD";
const LABEL_HEADERS: &str = "LABEL.HEADERS";
const LABEL_URL: &str = "LABEL.URL";
const LABEL_EPG_SOURCES: &str = "LABEL.EPG_SOURCES";
const LABEL_USERNAME: &str = "LABEL.USERNAME";
const LABEL_PASSWORD: &str = "LABEL.PASSWORD";
const LABEL_PERSIST: &str = "LABEL.PERSIST";
const LABEL_ENABLED: &str = "LABEL.ENABLED";
const LABEL_DISABLED: &str = "LABEL.DISABLED";
const LABEL_ALIASES: &str = "LABEL.ALIASES";
const LABEL_PRIORITY: &str = "LABEL.PRIORITY";
const LABEL_MAX_CONNECTIONS: &str = "LABEL.MAX_CONNECTIONS";
const LABEL_EXP_DATE: &str = "LABEL.EXP_DATE";
const LABEL_SELECTION_POLICY: &str = "LABEL.SELECTION_POLICY";
const LABEL_PROVIDER_URL_SELECTION_RESUME_LAST_WORKING: &str = "LABEL.PROVIDER_URL_SELECTION_RESUME_LAST_WORKING";
const LABEL_PROVIDER_URL_SELECTION_RESTART_FROM_FIRST: &str = "LABEL.PROVIDER_URL_SELECTION_RESTART_FROM_FIRST";
const LABEL_PROVIDER_DNS: &str = "LABEL.PROVIDER_DNS";
const LABEL_DNS_ON_CONNECT_ERROR: &str = "LABEL.DNS_ON_CONNECT_ERROR";
const LABEL_DNS_CONNECT_TRY_NEXT_IP: &str = "LABEL.DNS_CONNECT_TRY_NEXT_IP";
const LABEL_DNS_CONNECT_ROTATE_PROVIDER_URL: &str = "LABEL.DNS_CONNECT_ROTATE_PROVIDER_URL";
const LABEL_DNS_REFRESH_SECS: &str = "LABEL.DNS_REFRESH_SECS";
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
const LABEL_RESOLVE_FILTER: &str = "LABEL.RESOLVE_FILTER";
const LABEL_PROBE_FILTER: &str = "LABEL.PROBE_FILTER";
const LABEL_PROBE_LIVE_INTERVAL_HOURS: &str = "LABEL.PROBE_LIVE_INTERVAL_HOURS";
const LABEL_METADATA: &str = "LABEL.METADATA";
const LABEL_CACHE_DURATION: &str = "LABEL.CACHE_DURATION";
const LABEL_MAIN: &str = "LABEL.MAIN_CONFIG";
const LABEL_OPTIONS: &str = "LABEL.OPTIONS";
const LABEL_EPG: &str = "LABEL.EPG";
const LABEL_ALIAS: &str = "LABEL.ALIAS";
const LABEL_TYPE: &str = "LABEL.TYPE";
const LABEL_CLUSTER: &str = "LABEL.CLUSTER";
const LABEL_MEDIA_SERVER: &str = "LABEL.MEDIA_SERVER";
const LABEL_LIBRARIES: &str = "LABEL.LIBRARIES";
const LABEL_TOKEN: &str = "LABEL.TOKEN";
const LABEL_API_KEY: &str = "LABEL.API_KEY";
const LABEL_USER_ID: &str = "LABEL.USER_ID";
const LABEL_ACCOUNT_TOKEN: &str = "LABEL.ACCOUNT_TOKEN";
const LABEL_SERVER_ID: &str = "LABEL.SERVER_ID";
const LABEL_SERVER_NAME: &str = "LABEL.SERVER_NAME";
const LABEL_PREFER_HTTPS: &str = "LABEL.PREFER_HTTPS";
const LABEL_ALLOW_RELAY: &str = "LABEL.ALLOW_RELAY";

fn input_persist_hint_key(staged_input: bool) -> &'static str {
    if staged_input {
        "INPUT_FORM.STAGED_PERSIST"
    } else {
        "INPUT_FORM.PERSIST"
    }
}

fn input_url_hint_key(simple_input: bool) -> &'static str {
    if simple_input {
        "INPUT_FORM.SIMPLE_INPUT.URL"
    } else {
        "INPUT_FORM.URL"
    }
}

fn staged_type_options() -> Rc<Vec<String>> {
    Rc::new(vec![StagedInputType::M3u.to_string(), StagedInputType::Xtream.to_string()])
}

fn staged_type_from_selection(selections: &[String]) -> StagedInputType {
    selections.first().and_then(|value| value.parse::<StagedInputType>().ok()).unwrap_or_default()
}

fn libraries_to_text(libraries: &[MediaServerLibrarySelector]) -> String {
    libraries
        .iter()
        .map(|library| match library {
            MediaServerLibrarySelector::Name(name) => name.as_str(),
            MediaServerLibrarySelector::Detailed(details) => details.name.as_deref().unwrap_or_default(),
        })
        .filter(|name| !name.trim().is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

fn libraries_from_text(value: &str, previous: &[MediaServerLibrarySelector]) -> Vec<MediaServerLibrarySelector> {
    value
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| {
            previous
                .iter()
                .find(|library| match library {
                    MediaServerLibrarySelector::Name(existing) => existing.trim() == name,
                    MediaServerLibrarySelector::Detailed(details) => {
                        details.name.as_deref().is_some_and(|n| n.trim() == name)
                    }
                })
                .cloned()
                .unwrap_or_else(|| MediaServerLibrarySelector::Name(name.to_string()))
        })
        .collect()
}

fn mutate_media_server(
    current: &Option<MediaServerInputConfigDto>,
    change: impl FnOnce(&mut MediaServerInputConfigDto),
) -> Option<MediaServerInputConfigDto> {
    let mut media_server = current.clone().unwrap_or_default();
    change(&mut media_server);
    Some(media_server)
}

fn mutate_staged(
    current: &Option<ConfigInputStagedDto>,
    change: impl FnOnce(&mut ConfigInputStagedDto),
) -> Option<ConfigInputStagedDto> {
    let mut staged = current.clone().unwrap_or_default();
    change(&mut staged);
    Some(staged)
}

fn provider_url_selection_policy_label_key(policy: ProviderUrlSelectionPolicy) -> &'static str {
    match policy {
        ProviderUrlSelectionPolicy::ResumeLastWorking => LABEL_PROVIDER_URL_SELECTION_RESUME_LAST_WORKING,
        ProviderUrlSelectionPolicy::RestartFromFirst => LABEL_PROVIDER_URL_SELECTION_RESTART_FROM_FIRST,
    }
}

fn provider_dns_enabled_text(provider: &ConfigProviderDto) -> &'static str {
    if provider.dns.as_ref().is_some_and(|dns| dns.enabled) {
        LABEL_ENABLED
    } else {
        LABEL_DISABLED
    }
}

fn provider_on_connect_error_text(provider: &ConfigProviderDto) -> &'static str {
    provider.dns.as_ref().filter(|dns| dns.enabled).map_or("-", |dns| match dns.on_connect_error {
        OnConnectErrorPolicy::TryNextIp => LABEL_DNS_CONNECT_TRY_NEXT_IP,
        OnConnectErrorPolicy::RotateProviderUrl => LABEL_DNS_CONNECT_ROTATE_PROVIDER_URL,
    })
}

fn provider_refresh_secs_text(provider: &ConfigProviderDto) -> String {
    provider.dns.as_ref().filter(|dns| dns.enabled).map_or_else(|| "-".to_string(), |dns| dns.refresh_secs.to_string())
}
const LABEL_PROVIDER: &str = "LABEL.PROVIDER";
const LABEL_LIVE_STREAMS: &str = "LABEL.LIVE_STREAMS";
const LABEL_EPG_SMART_MATCH: &str = "LABEL.EPG_SMART_MATCH";
const LABEL_EDIT_EPG_SMART_MATCH: &str = "LABEL.EDIT_EPG_SMART_MATCH";

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum InputFormPage {
    Main,
    Options,
    Libraries,
    Epg,
    Alias,
    Provider,
}

impl InputFormPage {
    const MAIN: &str = "Main";
    const OPTIONS: &str = "Options";
    const LIBRARIES: &str = "Libraries";
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
            Self::LIBRARIES => Ok(InputFormPage::Libraries),
            Self::EPG => Ok(InputFormPage::Epg),
            Self::ALIAS => Ok(InputFormPage::Alias),
            Self::PROVIDER => Ok(InputFormPage::Provider),
            _ => Err(TuliproxError::Config(format!("Unknown input form page: {s}"))),
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
                InputFormPage::Libraries => Self::LIBRARIES,
                InputFormPage::Epg => Self::EPG,
                InputFormPage::Alias => Self::ALIAS,
                InputFormPage::Provider => Self::PROVIDER,
            }
        )
    }
}

impl Internable for InputFormPage {
    fn intern(self) -> Arc<str> {
        match self {
            Self::Main => Self::MAIN,
            Self::Options => Self::OPTIONS,
            Self::Libraries => Self::LIBRARIES,
            Self::Epg => Self::EPG,
            Self::Alias => Self::ALIAS,
            Self::Provider => Self::PROVIDER,
        }
        .intern()
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
      ResolveFilter => resolve_filter: Option<String>,
      ProbeFilter => probe_filter: Option<String>,
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
        StagedType => staged_type: StagedInputType,
        Staged => staged: Option<ConfigInputStagedDto>,
        MediaServer => media_server: Option<MediaServerInputConfigDto>,
        ExpDate => exp_date: Option<i64>,
        CacheDuration => cache_duration: Option<String>,
    }
);

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

    {
        let input_form_state = input_form_state.clone();
        let input_options_state = input_options_state.clone();
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
                let is_library = input.input_type.is_library();
                let is_media_server = input.input_type.is_media_server();
                let is_staged = input.input_type.is_staged();
                let current_page = *view_visible;
                let page_allowed = match current_page {
                    InputFormPage::Main => true,
                    InputFormPage::Libraries => is_media_server,
                    InputFormPage::Alias | InputFormPage::Options | InputFormPage::Provider => {
                        !is_library && !is_staged && !is_media_server
                    }
                    InputFormPage::Epg => !is_library && !is_staged && !is_media_server,
                };
                if !page_allowed {
                    view_visible.set(InputFormPage::Main);
                }

                input_form_state.dispatch(ConfigInputFormAction::SetAll(input.as_ref().clone()));

                input_options_state.dispatch(ConfigInputOptionsFormAction::SetAll(
                    input.options.as_ref().map_or_else(ConfigInputOptionsDto::default, |d| d.clone()),
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

    {
        let input_form_state = input_form_state.clone();
        use_effect_with(
            (
                input_form_state.form.url.clone(),
                input_form_state.form.username.clone(),
                input_form_state.form.password.clone(),
            ),
            move |(url, username, password)| {
                if url.starts_with(BATCH_SCHEME_PREFIX) {
                    if username.is_some() {
                        input_form_state.dispatch(ConfigInputFormAction::Username(None));
                    }
                    if password.is_some() {
                        input_form_state.dispatch(ConfigInputFormAction::Password(None));
                    }
                }
                || ()
            },
        );
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
    let media_server_input = input_form_state.form.input_type.is_media_server();
    let xtream_input = input_form_state.form.input_type.is_xtream();
    let staged_input = input_form_state.form.input_type.is_staged();
    let staged_xtream_input = staged_input && input_form_state.form.staged_type == StagedInputType::Xtream;
    let credentials_input = xtream_input || staged_xtream_input || media_server_input;
    let options_input = !library_input && !media_server_input;

    let render_options = || {
        let headers = headers_state.clone();
        if !props.allow_write {
            return html! {
                <Card class="tp__config-view__card">
                { html_if!(xtream_input || staged_xtream_input, {
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
                        { config_field_optional!(input_options_state.form, translate.t(LABEL_RESOLVE_FILTER), resolve_filter) }
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
                        { config_field_optional!(input_options_state.form, translate.t(LABEL_PROBE_FILTER), probe_filter) }
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
            { html_if!(xtream_input || staged_xtream_input, {
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
                    { config_field_child!(translate.t(LABEL_RESOLVE_FILTER), "INPUT_FORM.RESOLVE_FILTER", {
                        let input_options_state_filter = input_options_state.clone();
                        html! {
                            <FilterInput filter={input_options_state.form.resolve_filter.clone().unwrap_or_default()} on_change={Callback::from(move |new_filter: Option<String>| {
                                input_options_state_filter.dispatch(ConfigInputOptionsFormAction::ResolveFilter(new_filter));
                            })} />
                        }
                    })}
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
                    { config_field_child!(translate.t(LABEL_PROBE_FILTER), "INPUT_FORM.PROBE_FILTER", {
                        let input_options_state_filter = input_options_state.clone();
                        html! {
                            <FilterInput filter={input_options_state.form.probe_filter.clone().unwrap_or_default()} on_change={Callback::from(move |new_filter: Option<String>| {
                                input_options_state_filter.dispatch(ConfigInputOptionsFormAction::ProbeFilter(new_filter));
                            })} />
                        }
                    })}
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

    let render_input = || {
        let providers_state = providers_state.clone();
        let input_method_selection = Rc::new(vec![input_form_state.form.method.to_string()]);
        let staged_type_selection = Rc::new(vec![input_form_state.form.staged_type.to_string()]);
        let staged_type_options = staged_type_options();
        let staged_type_labels = Rc::new(vec![translate.t("LABEL.M3U"), translate.t("LABEL.XTREAM")]);
        let staged_clusters = input_form_state.form.staged.as_ref().map(|staged| staged.clusters);
        let is_csv_batch = input_form_state.form.url.starts_with(BATCH_SCHEME_PREFIX);
        let simple_input = staged_input || media_server_input;
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
                    let providers = (!(*providers_state).is_empty()).then_some((*providers_state).clone());
                    let request = XtreamLoginRequest { url, username, password, providers };

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
                   { html_if!(staged_input, {
                     <>
                     { config_field_custom!(translate.t(LABEL_TYPE), input_form_state.form.staged_type.to_string()) }
                     { config_field_custom!(translate.t(LABEL_CLUSTER), staged_clusters.map_or_else(String::new, |clusters| clusters.to_string())) }
                     </>
                   })}
                   { html_if!(!library_input, {
                    <>
                     { config_field!(input_form_state.form, translate.t(LABEL_URL), url, Some(input_url_hint_key(simple_input).to_string())) }
                     <div class="tp__config-view__cols-2">
                     { html_if!(credentials_input && !is_csv_batch, {
                       <>
                       { config_field_optional!(input_form_state.form, translate.t(LABEL_USERNAME), username) }
                       { config_field_optional_hide!(input_form_state.form, translate.t(LABEL_PASSWORD), password) }
                       </>
                     })}
                     { html_if!(!staged_input && !is_csv_batch, {
                         <>
                         { config_field_custom!(translate.t(LABEL_MAX_CONNECTIONS), input_form_state.form.max_connections.to_string()) }
                         { config_field_custom!(translate.t(LABEL_PRIORITY), input_form_state.form.priority.to_string()) }
                         { config_field_custom!(translate.t(LABEL_EXP_DATE), input_form_state.form.exp_date.map_or_else(String::new, |exp_date| exp_date.to_string())) }
                         </>
                     })}
                     { html_if!(!staged_input, {
                         { config_field_optional!(input_form_state.form, translate.t(LABEL_CACHE_DURATION), cache_duration) }
                     })}
                     { config_field_custom!(translate.t(LABEL_FETCH_METHOD), input_form_state.form.method.to_string()) }
                     </div>
                     { config_field_optional!(input_form_state.form, translate.t(LABEL_PERSIST), persist, Some(input_persist_hint_key(staged_input).to_string())) }
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
               { html_if!(staged_input, {
                 <>
                 { config_field_child!(translate.t(LABEL_TYPE), "INPUT_FORM.STAGED_TYPE", {
                   let input_form_state = input_form_state.clone();
                   html! {
                       <RadioButtonGroup
                        multi_select={false}
                        none_allowed={false}
                        options={staged_type_options}
                        labels={Some(staged_type_labels)}
                        selected={staged_type_selection}
                        on_select={Callback::from(move |selections: Rc<Vec<String>>| {
                            input_form_state.dispatch(ConfigInputFormAction::StagedType(staged_type_from_selection(&selections)));
                        })}
                    />
                 }})}
                 { config_field_child!(translate.t(LABEL_CLUSTER), "INPUT_FORM.STAGED_CLUSTERS", {
                   let input_form_state = input_form_state.clone();
                   html! {
                       <ClusterFlagsInput
                        name={"staged_clusters"}
                        value={staged_clusters}
                        mode={ClusterFlagsInputMode::NoneIsAll}
                        on_change={Callback::from(move |(_, flags): (String, Option<ClusterFlags>)| {
                            // Staged inputs must keep at least one cluster; mirror ConfigInputStagedDto::default() when the user clears the last chip.
                            let clusters = flags.filter(|f| !f.is_empty()).unwrap_or_else(ClusterFlags::all);
                            let next = mutate_staged(&input_form_state.form.staged, |staged| {
                                staged.clusters = clusters;
                            });
                            input_form_state.dispatch(ConfigInputFormAction::Staged(next));
                        })}
                    />
                 }})}
                 </>
               })}
               { html_if!(!library_input, {
                <>
                 <div class="tp__form-field tp__form-field__text">
                     <Input
                        label={translate.t(LABEL_URL)}
                        name={"url"}
                        field_id={Some(crate::app::components::dto_field_id(&input_form_state.form, "url"))}
                        autocomplete={true}
                        value={input_form_state.form.url.clone()}
                        hint_key={Some(input_url_hint_key(simple_input).to_string())}
                        on_change={Callback::from({
                            let input_form_state = input_form_state.clone();
                            move |value: String| input_form_state.dispatch(ConfigInputFormAction::Url(value))
                        })}
                     />
                 </div>
                 <div class="tp__config-view__cols-2">
                 { html_if!(credentials_input && !is_csv_batch, {
                   <>
                   { edit_field_text_option!(input_form_state, translate.t(LABEL_USERNAME), username, ConfigInputFormAction::Username) }
                   { edit_field_text_option!(input_form_state, translate.t(LABEL_PASSWORD), password, ConfigInputFormAction::Password, true) }
                   </>
                 })}
                 { html_if!(!staged_input && !is_csv_batch, {
                   <>
                   { edit_field_number_u16!(input_form_state, translate.t(LABEL_MAX_CONNECTIONS), max_connections, ConfigInputFormAction::MaxConnections) }
                   { edit_field_number_i16!(input_form_state, translate.t(LABEL_PRIORITY), priority, ConfigInputFormAction::Priority) }
                   { edit_field_exp_date!(input_form_state, translate.t(LABEL_EXP_DATE), exp_date, ConfigInputFormAction::ExpDate, exp_date_tool_action) }
                   </>
                 })}
                 { html_if!(!staged_input, {
                   { edit_field_text_option!(input_form_state, translate.t(LABEL_CACHE_DURATION), cache_duration, ConfigInputFormAction::CacheDuration) }
                 })}
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
                 <div class="tp__form-field tp__form-field__text">
                     <Input
                        label={translate.t(LABEL_PERSIST)}
                        name={"persist"}
                        field_id={Some(crate::app::components::dto_field_id(&input_form_state.form, "persist"))}
                        autocomplete={true}
                        value={input_form_state.form.persist.clone().unwrap_or_default()}
                        hint_key={Some(input_persist_hint_key(staged_input).to_string())}
                        on_change={Callback::from({
                            let input_form_state = input_form_state.clone();
                            move |value: String| {
                                input_form_state.dispatch(ConfigInputFormAction::Persist((!value.is_empty()).then_some(value)));
                            }
                        })}
                     />
                 </div>
                </>
               })}
            </Card>
        }
    };

    let render_media_server = || {
        let media_server = input_form_state.form.media_server.clone().unwrap_or_default();
        let token = media_server.token.clone().unwrap_or_default();
        let api_key = media_server.api_key.clone().unwrap_or_default();
        let user_id = media_server.user_id.clone().unwrap_or_default();
        let account_token = media_server.account_token.clone().unwrap_or_default();
        let server_id = media_server.server_id.clone().unwrap_or_default();
        let server_name = media_server.server_name.clone().unwrap_or_default();

        if !props.allow_write {
            return html! {
                <Card class="tp__config-view__card">
                    <TitledCard title={translate.t(LABEL_MEDIA_SERVER)}>
                        { config_field_child!(translate.t(LABEL_TOKEN), "MEDIA_SERVER.TOKEN", {
                            html! { <span class="tp__form-field__value"><HideContent content={token} /></span> }
                        })}
                        { config_field_child!(translate.t(LABEL_API_KEY), "MEDIA_SERVER.API_KEY", {
                            html! { <span class="tp__form-field__value"><HideContent content={api_key} /></span> }
                        })}
                        { config_field_child!(translate.t(LABEL_USER_ID), "MEDIA_SERVER.USER_ID", {
                            html! { <span class="tp__form-field__value">{user_id}</span> }
                        })}
                        { config_field_child!(translate.t(LABEL_ACCOUNT_TOKEN), "MEDIA_SERVER.ACCOUNT_TOKEN", {
                            html! { <span class="tp__form-field__value"><HideContent content={account_token} /></span> }
                        })}
                        { config_field_child!(translate.t(LABEL_SERVER_ID), "MEDIA_SERVER.SERVER_ID", {
                            html! { <span class="tp__form-field__value">{server_id}</span> }
                        })}
                        { config_field_child!(translate.t(LABEL_SERVER_NAME), "MEDIA_SERVER.SERVER_NAME", {
                            html! { <span class="tp__form-field__value">{server_name}</span> }
                        })}
                        <div class="tp__config-view__cols-2">
                            { config_field_child!(translate.t(LABEL_PREFER_HTTPS), "MEDIA_SERVER.PREFER_HTTPS", {
                                html! { <ToggleSwitch value={media_server.prefer_https} readonly={true} /> }
                            })}
                            { config_field_child!(translate.t(LABEL_ALLOW_RELAY), "MEDIA_SERVER.ALLOW_RELAY", {
                                html! { <ToggleSwitch value={media_server.allow_relay} readonly={true} /> }
                            })}
                        </div>
                    </TitledCard>
                </Card>
            };
        }

        let dispatch_media = |change: fn(&mut MediaServerInputConfigDto, String)| {
            let state = input_form_state.clone();
            Callback::from(move |value: String| {
                let next = mutate_media_server(&state.form.media_server, |media_server| change(media_server, value));
                state.dispatch(ConfigInputFormAction::MediaServer(next));
            })
        };
        let dispatch_media_bool = |change: fn(&mut MediaServerInputConfigDto, bool)| {
            let state = input_form_state.clone();
            Callback::from(move |value: bool| {
                let next = mutate_media_server(&state.form.media_server, |media_server| change(media_server, value));
                state.dispatch(ConfigInputFormAction::MediaServer(next));
            })
        };

        let on_token = dispatch_media(|media_server, value| {
            media_server.token = (!value.is_empty()).then_some(value);
        });
        let on_api_key = dispatch_media(|media_server, value| {
            media_server.api_key = (!value.is_empty()).then_some(value);
        });
        let on_user_id = dispatch_media(|media_server, value| {
            media_server.user_id = (!value.is_empty()).then_some(value);
        });
        let on_account_token = dispatch_media(|media_server, value| {
            media_server.account_token = (!value.is_empty()).then_some(value);
        });
        let on_server_id = dispatch_media(|media_server, value| {
            media_server.server_id = (!value.is_empty()).then_some(value);
        });
        let on_server_name = dispatch_media(|media_server, value| {
            media_server.server_name = (!value.is_empty()).then_some(value);
        });
        let on_prefer_https = dispatch_media_bool(|media_server, value| media_server.prefer_https = value);
        let on_allow_relay = dispatch_media_bool(|media_server, value| media_server.allow_relay = value);

        html! {
            <Card class="tp__config-view__card">
                <TitledCard title={translate.t(LABEL_MEDIA_SERVER)}>
                    <div class="tp__config-view__cols-2">
                        <Input name="media_server_token" field_id={Some("MEDIA_SERVER.TOKEN".to_string())} label={Some(translate.t(LABEL_TOKEN))} value={token} hidden={true} on_change={Some(on_token)} />
                        <Input name="media_server_api_key" field_id={Some("MEDIA_SERVER.API_KEY".to_string())} label={Some(translate.t(LABEL_API_KEY))} value={api_key} hidden={true} on_change={Some(on_api_key)} />
                        <Input name="media_server_user_id" field_id={Some("MEDIA_SERVER.USER_ID".to_string())} label={Some(translate.t(LABEL_USER_ID))} value={user_id} on_change={Some(on_user_id)} />
                        <Input name="media_server_account_token" field_id={Some("MEDIA_SERVER.ACCOUNT_TOKEN".to_string())} label={Some(translate.t(LABEL_ACCOUNT_TOKEN))} value={account_token} hidden={true} on_change={Some(on_account_token)} />
                        <Input name="media_server_server_id" field_id={Some("MEDIA_SERVER.SERVER_ID".to_string())} label={Some(translate.t(LABEL_SERVER_ID))} value={server_id} on_change={Some(on_server_id)} />
                        <Input name="media_server_server_name" field_id={Some("MEDIA_SERVER.SERVER_NAME".to_string())} label={Some(translate.t(LABEL_SERVER_NAME))} value={server_name} on_change={Some(on_server_name)} />
                    </div>
                    <div class="tp__config-view__cols-2">
                        { config_field_child!(translate.t(LABEL_PREFER_HTTPS), "MEDIA_SERVER.PREFER_HTTPS", {
                            html! { <ToggleSwitch value={media_server.prefer_https} on_change={on_prefer_https} /> }
                        })}
                        { config_field_child!(translate.t(LABEL_ALLOW_RELAY), "MEDIA_SERVER.ALLOW_RELAY", {
                            html! { <ToggleSwitch value={media_server.allow_relay} on_change={on_allow_relay} /> }
                        })}
                    </div>
                </TitledCard>
            </Card>
        }
    };

    let render_media_server_libraries = || {
        let media_server = input_form_state.form.media_server.clone().unwrap_or_default();
        let libraries = libraries_to_text(&media_server.libraries);

        if !props.allow_write {
            return html! {
                <Card class="tp__config-view__card">
                    { config_field_child!(translate.t(LABEL_LIBRARIES), "MEDIA_SERVER.LIBRARIES", {
                        html! { <span class="tp__form-field__value">{libraries}</span> }
                    })}
                </Card>
            };
        }

        let input_form_state = input_form_state.clone();
        let on_libraries = Callback::from(move |value: String| {
            let next = mutate_media_server(&input_form_state.form.media_server, |media_server| {
                media_server.libraries = libraries_from_text(&value, &media_server.libraries);
            });
            input_form_state.dispatch(ConfigInputFormAction::MediaServer(next));
        });

        html! {
            <Card class="tp__config-view__card">
                <Input name="media_server_libraries" field_id={Some("MEDIA_SERVER.LIBRARIES".to_string())} label={Some(translate.t(LABEL_LIBRARIES))} value={libraries} on_change={Some(on_libraries)} />
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
                        providers={(*providers_state).clone()}
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
                                        <div class="tp__form-list__item tp__provider-list-item" key={format!("provider-{idx}")}>
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
                                                <span class="tp__provider-list-item__name">
                                                    <strong>{provider.name.as_ref()}</strong>
                                                </span>
                                                <div class="tp__provider-list-item__meta">
                                                    <div class="tp__provider-list-item__meta-row">
                                                        <span class="tp__provider-list-item__meta-label">{"URLs: "}</span>
                                                        <span class="tp__provider-list-item__meta-value">{provider.urls.len()}</span>
                                                    </div>
                                                    <div class="tp__provider-list-item__meta-row">
                                                        <span class="tp__provider-list-item__meta-label">{format!("{}: ", translate.t(LABEL_SELECTION_POLICY))}</span>
                                                        <span class="tp__provider-list-item__meta-value">
                                                            {translate.t(provider_url_selection_policy_label_key(provider.provider_url_selection_policy))}
                                                        </span>
                                                    </div>
                                                    <div class="tp__provider-list-item__meta-row">
                                                        <span class="tp__provider-list-item__meta-label">{format!("{}: ", translate.t(LABEL_PROVIDER_DNS))}</span>
                                                        <span class="tp__provider-list-item__meta-value">
                                                            {translate.t(provider_dns_enabled_text(provider))}
                                                        </span>
                                                    </div>
                                                    <div class="tp__provider-list-item__meta-row">
                                                        <span class="tp__provider-list-item__meta-label">{format!("{}: ", translate.t(LABEL_DNS_ON_CONNECT_ERROR))}</span>
                                                        <span class="tp__provider-list-item__meta-value">
                                                            {
                                                                if provider_on_connect_error_text(provider) == "-" {
                                                                    "-".into()
                                                                } else {
                                                                    translate.t(provider_on_connect_error_text(provider))
                                                                }
                                                            }
                                                        </span>
                                                    </div>
                                                    <div class="tp__provider-list-item__meta-row">
                                                        <span class="tp__provider-list-item__meta-label">{format!("{}: ", translate.t(LABEL_DNS_REFRESH_SECS))}</span>
                                                        <span class="tp__provider-list-item__meta-value">
                                                            {provider_refresh_secs_text(provider)}
                                                        </span>
                                                    </div>
                                                </div>
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

            if input.input_type.is_staged() {
                input.staged.get_or_insert_with(ConfigInputStagedDto::default);
            } else {
                input.staged = None;
            }
            if input.input_type.is_media_server() {
                input.media_server.get_or_insert_with(MediaServerInputConfigDto::default);
            } else {
                input.media_server = None;
            }

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
                <Panel value={InputFormPage::Main.intern()} active={view_visible.intern()}>
                    {render_input()}
                </Panel>
                { html_if!(media_server_input, {
                    <Panel value={InputFormPage::Libraries.intern()} active={view_visible.intern()}>
                    {render_media_server()}
                    {render_media_server_libraries()}
                    </Panel>
                })}
                { html_if!(!library_input && !staged_input && !media_server_input, {
                    <Panel value={InputFormPage::Alias.intern()} active={view_visible.intern()}>
                    {render_alias()}
                    </Panel>
                })}
                { html_if!(options_input, {
                 <>
                  <Panel value={InputFormPage::Options.intern()} active={view_visible.intern()}>
                   {render_options()}
                  </Panel>
                    </>
                })}
                { html_if!(!library_input && !staged_input && !media_server_input, {
                 <>
                  <Panel value={InputFormPage::Provider.intern()} active={view_visible.intern()}>
                   {render_provider()}
                   </Panel>
                  <Panel value={InputFormPage::Epg.intern()} active={view_visible.intern()}>
                   {render_epg()}
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
            {html_if!(media_server_input, {
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", InputFormPage::Libraries, if *view_visible == InputFormPage::Libraries { " active" } else {""})}  icon="VideoConfig" hint={translate.t(LABEL_MEDIA_SERVER)} name={InputFormPage::Libraries.to_string()} onclick={&handle_menu_click}></IconButton>
            })}
            {html_if!(!library_input && !staged_input && !media_server_input, {
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", InputFormPage::Alias, if *view_visible == InputFormPage::Alias { " active" } else {""})}  icon="Alias" hint={translate.t(LABEL_ALIAS)} name={InputFormPage::Alias.to_string()} onclick={&handle_menu_click}></IconButton>
            })}
            { html_if!(options_input, {
                <>
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", InputFormPage::Options, if *view_visible == InputFormPage::Options { " active" } else {""})}  icon="Options" hint={translate.t(LABEL_OPTIONS)} name={InputFormPage::Options.to_string()} onclick={&handle_menu_click}></IconButton>
                </>
             })}
            { html_if!(!library_input && !staged_input && !media_server_input, {
                <>
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", InputFormPage::Epg, if *view_visible == InputFormPage::Epg { " active" } else {""})}  icon="Epg" hint={translate.t(LABEL_EPG)} name={InputFormPage::Epg.to_string()} onclick={&handle_menu_click}></IconButton>
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", InputFormPage::Provider, if *view_visible == InputFormPage::Provider { " active" } else {""})}  icon="Dns" hint={translate.t(LABEL_PROVIDER)} name={InputFormPage::Provider.to_string()} onclick={&handle_menu_click}></IconButton>
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

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::{MediaServerLibraryKind, MediaServerLibrarySelectorDetailsDto};

    #[test]
    fn media_server_libraries_page_round_trips() {
        assert_eq!(InputFormPage::from_str(InputFormPage::LIBRARIES).ok(), Some(InputFormPage::Libraries));
        assert_eq!(InputFormPage::Libraries.to_string(), InputFormPage::LIBRARIES);
    }

    #[test]
    fn simple_inputs_use_simple_url_hint() {
        assert_eq!(input_url_hint_key(true), "INPUT_FORM.SIMPLE_INPUT.URL");
        assert_eq!(input_url_hint_key(false), "INPUT_FORM.URL");
    }

    #[test]
    fn staged_inputs_use_staged_persist_hint() {
        assert_eq!(input_persist_hint_key(true), "INPUT_FORM.STAGED_PERSIST");
        assert_eq!(input_persist_hint_key(false), "INPUT_FORM.PERSIST");
    }

    #[test]
    fn libraries_from_text_preserves_existing_detailed_selector() {
        let detailed = MediaServerLibrarySelector::Detailed(MediaServerLibrarySelectorDetailsDto {
            id: Some("id-1".to_string()),
            key: Some("key-1".to_string()),
            name: Some("Movies".to_string()),
            kind: Some(MediaServerLibraryKind::Movies),
        });
        let previous = vec![detailed.clone()];

        let libraries = libraries_from_text("Movies, Kids", &previous);

        assert_eq!(libraries, vec![detailed, MediaServerLibrarySelector::Name("Kids".to_string())]);
    }
}

use crate::{
    app::components::{
        config::HasFormData, key_value_editor::KeyValueEditor, select::Select, AliasItemForm, BlockId, BlockInstance,
        Card, DropDownOption, DropDownSelection, EditMode, EpgSourceItemForm, IconButton, Panel, ProviderItemForm,
        RadioButtonGroup, SourceEditorContext, TextButton, TitledCard,
    },
    config_field_child, edit_field_bool, edit_field_date, edit_field_number_i16, edit_field_number_u16,
    edit_field_number_u32, edit_field_text, edit_field_text_option, generate_form_reducer, html_if,
    i18n::use_translation,
};
use shared::{
    concat_string,
    error::TuliproxError,
    info_err_res,
    model::{
        ConfigInputAliasDto, ConfigInputDto, ConfigInputOptionsDto, ConfigProviderDto, EpgConfigDto, EpgSourceDto,
        InputFetchMethod, InputType, StagedInputDto,
    },
};
use std::{collections::HashMap, fmt::Display, rc::Rc, str::FromStr};
use web_sys::MouseEvent;
use yew::{
    component, html, use_context, use_effect_with, use_memo, use_reducer, use_state, Callback, Html, Properties,
    UseReducerHandle,
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
const LABEL_ADVANCED: &str = "LABEL.ADVANCED";
const LABEL_ALIAS: &str = "LABEL.ALIAS";
const LABEL_PROVIDER: &str = "LABEL.PROVIDER";
const LABEL_LIVE_STREAMS: &str = "LABEL.LIVE_STREAMS";

#[derive(Copy, Clone, PartialEq, Eq)]
enum InputFormPage {
    Main,
    Options,
    Staged,
    Advanced,
    Alias,
    Provider,
}

impl InputFormPage {
    const MAIN: &str = "Main";
    const OPTIONS: &str = "Options";
    const STAGED: &str = "Staged";
    const ADVANCED: &str = "Advanced";
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
            Self::ADVANCED => Ok(InputFormPage::Advanced),
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
                InputFormPage::Advanced => Self::ADVANCED,
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

#[component]
pub fn ConfigInputView(props: &ConfigInputViewProps) -> Html {
    let translate = use_translation();
    let source_editor_ctx = use_context::<SourceEditorContext>();
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

    // State for showing item forms
    let show_epg_form_state = use_state(|| false);
    let show_alias_form_state = use_state(|| false);
    let show_provider_form_state = use_state(|| false);
    let edit_alias = use_state(|| None::<ConfigInputAliasDto>);
    let edit_provider = use_state(|| None::<ConfigProviderDto>);

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
        let aliases_state = aliases_state.clone();
        let headers_state = headers_state.clone();
        let providers_state = providers_state.clone();

        let deps = (props.block_id, props.input.clone());
        let view_visible = view_visible.clone();
        use_effect_with(deps, move |(_, cfg)| {
            if let Some(input) = cfg {
                if input.input_type.is_library()
                    && matches!(*view_visible, InputFormPage::Staged | InputFormPage::Advanced)
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

                // Load aliases
                aliases_state.set(input.aliases.clone().unwrap_or_default());

                // Load providers
                providers_state.set(input.provider.clone().unwrap_or_default());
            } else {
                input_form_state.dispatch(ConfigInputFormAction::SetAll(ConfigInputDto::default()));
                input_options_state.dispatch(ConfigInputOptionsFormAction::SetAll(ConfigInputOptionsDto::default()));
                staged_input_state.dispatch(StagedInputFormAction::SetAll(StagedInputDto::default()));
                headers_state.set(HashMap::new());
                epg_sources_state.set(Vec::new());
                aliases_state.set(Vec::new());
                providers_state.set(Vec::new());
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

    let handle_add_provider_item = {
        let providers = providers_state.clone();
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
        Callback::from(move |(idx, e): (String, MouseEvent)| {
            e.prevent_default();
            e.stop_propagation();
            if let Ok(index) = idx.parse::<usize>() {
                let mut items = (*provider_list).clone();
                if index < items.len() {
                    items.remove(index);
                    provider_list.set(items);
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
               { html_if!(!library_input, {
                <>
                 { edit_field_text!(input_form_state, translate.t(LABEL_URL),  url, ConfigInputFormAction::Url) }
                 <div class="tp__config-view__cols-2">
                 { edit_field_text_option!(input_form_state, translate.t(LABEL_USERNAME), username, ConfigInputFormAction::Username) }
                 { edit_field_text_option!(input_form_state, translate.t(LABEL_PASSWORD), password, ConfigInputFormAction::Password, true) }
                 { edit_field_number_u16!(input_form_state, translate.t(LABEL_MAX_CONNECTIONS), max_connections, ConfigInputFormAction::MaxConnections) }
                 { edit_field_number_i16!(input_form_state, translate.t(LABEL_PRIORITY), priority, ConfigInputFormAction::Priority) }
                 { edit_field_date!(input_form_state, translate.t(LABEL_EXP_DATE), exp_date, ConfigInputFormAction::ExpDate) }
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
                        initial={(*edit_alias).clone()}
                        on_submit={handle_add_alias_item}
                        on_cancel={handle_close_add_alias_item}
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
                                                if idx > 0 {
                                                    <IconButton
                                                        class="tp__form-list__item-arrow-btn"
                                                        name={idx.to_string()}
                                                        icon="ArrowUp"
                                                        onclick={handle_move_alias_up.clone()}
                                                    />
                                                } else if alias_count > 2 {
                                                    <span class="tp__form-list__item-placeholder-btn"/>
                                                }
                                                if idx + 1 < alias_count {
                                                    <IconButton
                                                        class="tp__form-list__item-arrow-btn"
                                                        name={idx.to_string()}
                                                        icon="ArrowDown"
                                                        onclick={handle_move_alias_down.clone()}
                                                    />
                                                } else if alias_count > 2 {
                                                    <span class="tp__form-list__item-placeholder-btn"/>
                                                }
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
                                                    icon="Delete"
                                                    onclick={handle_remove_provider_list_item.clone()}
                                                />
                                                <IconButton
                                                    name={idx.to_string()}
                                                    icon="Edit"
                                                    onclick={handle_edit_provider_list_item.clone()}
                                                />
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
                            <div class="tp__form-list__toolbar">
                                <TextButton
                                    class="primary"
                                    name="add_provider"
                                    icon="Add"
                                    title={translate.t(LABEL_ADD_PROVIDER)}
                                    onclick={handle_show_add_provider_item.clone()}
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
                  { config_field_child!(translate.t(LABEL_HEADERS), "INPUT_FORM.HEADERS", {
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
                  { config_field_child!(translate.t(LABEL_EPG_SOURCES), "INPUT_FORM.EPG_SOURCES", {
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
        let aliases_state = aliases_state.clone();
        let providers_state = providers_state.clone();

        Callback::from(move |_| {
            let mut input = input_form_state.data().clone();

            let options = input_options_state.data();
            input.options = if options.is_empty() { None } else { Some(options.clone()) };

            let staged_input = staged_input_state.data();
            input.staged = if staged_input.is_empty() { None } else { Some(staged_input.clone()) };

            // Handle Headers
            input.headers = (*headers_state).clone();

            // Handle EPG: update sources but preserve other fields if present
            let epg_sources = (*epg_sources_state).clone();
            if let Some(mut epg_cfg) = input.epg.take() {
                epg_cfg.sources = if epg_sources.is_empty() { None } else { Some(epg_sources) };
                input.epg =
                    if epg_cfg.sources.is_some() || epg_cfg.smart_match.is_some() { Some(epg_cfg) } else { None };
            } else if !epg_sources.is_empty() {
                input.epg = Some(EpgConfigDto { sources: Some(epg_sources), ..EpgConfigDto::default() });
            }

            // Handle Aliases
            let aliases = (*aliases_state).clone();
            input.aliases = if aliases.is_empty() { None } else { Some(aliases) };

            // Handle Providers
            let providers = (*providers_state).clone();
            input.provider = if providers.is_empty() { None } else { Some(providers) };

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
                  <Panel value={InputFormPage::Advanced.to_string()} active={view_visible.to_string()}>
                   {render_advanced()}
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

    let button_disabled = *show_alias_form_state || *show_epg_form_state || *show_provider_form_state;

    let render_sidebar = || {
        html! {
            <div class={concat_string!("tp__source-editor-form__sidebar", if button_disabled {" disabled"} else {""})}>
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", InputFormPage::Main, if *view_visible == InputFormPage::Main { " active" } else {""})}  icon="Settings" hint={translate.t(LABEL_MAIN)} name={InputFormPage::Main.to_string()} onclick={&handle_menu_click}></IconButton>
            {html_if!(!library_input, {
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", InputFormPage::Alias, if *view_visible == InputFormPage::Alias { " active" } else {""})}  icon="Alias" hint={translate.t(LABEL_ALIAS)} name={InputFormPage::Alias.to_string()} onclick={&handle_menu_click}></IconButton>
            })}
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", InputFormPage::Options, if *view_visible == InputFormPage::Options { " active" } else {""})}  icon="Options" hint={translate.t(LABEL_OPTIONS)} name={InputFormPage::Options.to_string()} onclick={&handle_menu_click}></IconButton>
            { html_if!(!library_input, {
                <>
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", InputFormPage::Advanced, if *view_visible == InputFormPage::Advanced { " active" } else {""})}  icon="Advanced" hint={translate.t(LABEL_ADVANCED)} name={InputFormPage::Advanced.to_string()} onclick={&handle_menu_click}></IconButton>
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
             <TextButton class={concat_string!("primary", if button_disabled {" disabled"} else {""} )} name="apply_input"
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

use super::{
    input_persist_hint_key, input_url_hint_key, ConfigInputFormAction, ConfigInputFormState,
    ConfigInputOptionsDtoFormState, ConfigInputOptionsFormAction, LABEL_CACHE_DURATION, LABEL_ENABLED, LABEL_EXP_DATE,
    LABEL_FETCH_METHOD, LABEL_HEADERS, LABEL_LIVE_STREAMS, LABEL_MAX_CONNECTIONS, LABEL_METADATA, LABEL_NAME,
    LABEL_PASSWORD, LABEL_PERSIST, LABEL_PRIORITY, LABEL_PROBE, LABEL_PROBE_DELAY_SEC, LABEL_PROBE_FILTER,
    LABEL_PROBE_LIVE_INTERVAL_HOURS, LABEL_RESOLVE, LABEL_RESOLVE_BACKGROUND, LABEL_RESOLVE_DELAY_SEC,
    LABEL_RESOLVE_FILTER, LABEL_RESOLVE_TMDB, LABEL_SKIP, LABEL_SKIP_LIVE, LABEL_SKIP_SERIES, LABEL_SKIP_VOD,
    LABEL_URL, LABEL_USERNAME, LABEL_XTREAM_LIVE_STREAM_USE_PREFIX, LABEL_XTREAM_LIVE_STREAM_WITHOUT_EXTENSION,
};
use crate::{
    app::components::{input::Input, Card, FilterInput, KeyValueEditor, RadioButtonGroup, TitledCard, ToolAction},
    config_field, config_field_bool, config_field_child, config_field_custom, config_field_optional,
    config_field_optional_hide, edit_field_bool, edit_field_exp_date, edit_field_number_i16, edit_field_number_u16,
    edit_field_number_u32, edit_field_text, edit_field_text_option, html_if,
    i18n::{use_translation, YewI18n},
};
use shared::{model::InputFetchMethod, utils::BATCH_SCHEME_PREFIX};
use std::{collections::HashMap, rc::Rc};
use yew::{component, html, use_memo, Callback, Html, Properties, UseReducerHandle, UseStateHandle};

#[derive(Properties, Clone, PartialEq)]
pub(super) struct CommonInputFormProps {
    pub state: UseReducerHandle<ConfigInputFormState>,
    pub allow_write: bool,
    #[prop_or(true)]
    pub show_url: bool,
    #[prop_or(false)]
    pub simple_url: bool,
    #[prop_or(false)]
    pub credentials: bool,
    #[prop_or(false)]
    pub connection: bool,
    #[prop_or(false)]
    pub cache_duration: bool,
    #[prop_or(false)]
    pub staged_persist: bool,
    #[prop_or_default]
    pub extra: Html,
    #[prop_or_default]
    pub exp_date_tool_action: Option<ToolAction>,
}

#[component]
pub(super) fn CommonInputForm(props: &CommonInputFormProps) -> Html {
    let translate = use_translation();
    let state = props.state.clone();
    let fetch_methods = use_memo((), |_| {
        [InputFetchMethod::GET, InputFetchMethod::POST].iter().map(ToString::to_string).collect::<Vec<_>>()
    });
    let method_selection = Rc::new(vec![state.form.method.to_string()]);
    let csv_batch = state.form.url.starts_with(BATCH_SCHEME_PREFIX);
    let credentials = props.credentials && !csv_batch;
    let connection = props.connection && !csv_batch;

    if !props.allow_write {
        return html! {
            <Card class="tp__config-view__card">
                <div class="tp__config-view__cols-2">
                    { config_field!(state.form, translate.t(LABEL_NAME), name) }
                    { config_field_bool!(state.form, translate.t(LABEL_ENABLED), enabled) }
                </div>
                if props.show_url {
                    { config_field!(state.form, translate.t(LABEL_URL), url, Some(input_url_hint_key(props.simple_url).to_string())) }
                    { html_if!(credentials, {
                        <div class="tp__config-view__cols-2">
                            { config_field_optional!(state.form, translate.t(LABEL_USERNAME), username) }
                            { config_field_optional_hide!(state.form, translate.t(LABEL_PASSWORD), password) }
                        </div>
                    })}
                    { html_if!(connection, {
                        <>
                            <div class="tp__config-view__cols-2">
                                { config_field_custom!(translate.t(LABEL_MAX_CONNECTIONS), state.form.max_connections.to_string()) }
                                { config_field_custom!(translate.t(LABEL_PRIORITY), state.form.priority.to_string()) }
                            </div>
                            <div class="tp__config-view__cols-2">
                                { config_field_custom!(translate.t(LABEL_EXP_DATE), state.form.exp_date.map_or_else(String::new, |value| value.to_string())) }
                                { html_if!(props.cache_duration, {
                                    { config_field_optional!(state.form, translate.t(LABEL_CACHE_DURATION), cache_duration) }
                                })}
                            </div>
                        </>
                    })}
                    { html_if!(!connection && props.cache_duration, {
                        { config_field_optional!(state.form, translate.t(LABEL_CACHE_DURATION), cache_duration) }
                    })}
                    <div class="tp__config-view__cols-2">
                        { config_field_custom!(translate.t(LABEL_FETCH_METHOD), state.form.method.to_string()) }
                        { config_field_optional!(state.form, translate.t(LABEL_PERSIST), persist, Some(input_persist_hint_key(props.staged_persist).to_string())) }
                    </div>
                }
                {props.extra.clone()}
            </Card>
        };
    }

    html! {
        <Card class="tp__config-view__card">
            <div class="tp__config-view__cols-2">
                { edit_field_text!(state, translate.t(LABEL_NAME), name, ConfigInputFormAction::Name) }
                { edit_field_bool!(state, translate.t(LABEL_ENABLED), enabled, ConfigInputFormAction::Enabled) }
            </div>
            if props.show_url {
                <div class="tp__form-field tp__form-field__text">
                    <Input label={translate.t(LABEL_URL)} name="url"
                        field_id={Some(crate::app::components::dto_field_id(&state.form, "url"))}
                        autocomplete={true} value={state.form.url.clone()}
                        hint_key={Some(input_url_hint_key(props.simple_url).to_string())}
                        on_change={Callback::from({
                            let state = state.clone();
                            move |value| state.dispatch(ConfigInputFormAction::Url(value))
                        })} />
                </div>
                { html_if!(credentials, {
                    <div class="tp__config-view__cols-2">
                        { edit_field_text_option!(state, translate.t(LABEL_USERNAME), username, ConfigInputFormAction::Username) }
                        { edit_field_text_option!(state, translate.t(LABEL_PASSWORD), password, ConfigInputFormAction::Password, true) }
                    </div>
                })}
                { html_if!(connection, {
                    <>
                        <div class="tp__config-view__cols-2">
                            { edit_field_number_u16!(state, translate.t(LABEL_MAX_CONNECTIONS), max_connections, ConfigInputFormAction::MaxConnections) }
                            { edit_field_number_i16!(state, translate.t(LABEL_PRIORITY), priority, ConfigInputFormAction::Priority) }
                        </div>
                        <div class="tp__config-view__cols-2">
                            { edit_field_exp_date!(state, translate.t(LABEL_EXP_DATE), exp_date, ConfigInputFormAction::ExpDate, props.exp_date_tool_action.clone()) }
                            { html_if!(props.cache_duration, {
                                { edit_field_text_option!(state, translate.t(LABEL_CACHE_DURATION), cache_duration, ConfigInputFormAction::CacheDuration) }
                            })}
                        </div>
                    </>
                })}
                { html_if!(!connection && props.cache_duration, {
                    { edit_field_text_option!(state, translate.t(LABEL_CACHE_DURATION), cache_duration, ConfigInputFormAction::CacheDuration) }
                })}
                <div class="tp__config-view__cols-2">
                    { config_field_child!(translate.t(LABEL_FETCH_METHOD), "INPUT_FORM.FETCH_METHOD", {
                        let state = state.clone();
                        html! {
                            <RadioButtonGroup multi_select={false} none_allowed={false}
                                on_select={Callback::from(move |selections: Rc<Vec<String>>| {
                                    if let Some(method) = selections.first().and_then(|value| value.parse().ok()) {
                                        state.dispatch(ConfigInputFormAction::Method(method));
                                    }
                                })}
                                options={fetch_methods.clone()} selected={method_selection} />
                        }
                    })}
                    <div class="tp__form-field tp__form-field__text">
                        <Input label={translate.t(LABEL_PERSIST)} name="persist"
                            field_id={Some(crate::app::components::dto_field_id(&state.form, "persist"))}
                            autocomplete={true} value={state.form.persist.clone().unwrap_or_default()}
                            hint_key={Some(input_persist_hint_key(props.staged_persist).to_string())}
                            on_change={Callback::from({
                                let state = state.clone();
                                move |value: String| state.dispatch(ConfigInputFormAction::Persist((!value.is_empty()).then_some(value)))
                            })} />
                    </div>
                </div>
            }
            {props.extra.clone()}
        </Card>
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(super) enum OptionsKind {
    Basic,
    Xtream,
    Stalker,
}

#[derive(Properties, Clone, PartialEq)]
pub(super) struct InputOptionsFormProps {
    pub state: UseReducerHandle<ConfigInputOptionsDtoFormState>,
    pub headers: UseStateHandle<HashMap<String, String>>,
    pub allow_write: bool,
    pub kind: OptionsKind,
}

#[component]
pub(super) fn InputOptionsForm(props: &InputOptionsFormProps) -> Html {
    let translate = use_translation();
    let state = props.state.clone();
    let headers = props.headers.clone();
    let type_options = match (props.allow_write, props.kind) {
        (_, OptionsKind::Basic) => Html::default(),
        (false, OptionsKind::Stalker) => html! {
            <TitledCard title={translate.t("LABEL.STALKER")}>
                <div class="tp__config-view__cols-3">
                    { config_field_bool!(state.form, translate.t(LABEL_SKIP_LIVE), skip_live) }
                    { config_field_bool!(state.form, translate.t(LABEL_SKIP_VOD), skip_vod) }
                    { config_field_bool!(state.form, translate.t(LABEL_SKIP_SERIES), skip_series) }
                </div>
                { config_field_bool!(state.form, translate.t("LABEL.STALKER_BULK_EPG"), stalker_bulk_epg) }
            </TitledCard>
        },
        (true, OptionsKind::Stalker) => html! {
            <TitledCard title={translate.t("LABEL.STALKER")}>
                <div class="tp__config-view__cols-3">
                    { edit_field_bool!(state, translate.t(LABEL_SKIP_LIVE), skip_live, ConfigInputOptionsFormAction::SkipLive) }
                    { edit_field_bool!(state, translate.t(LABEL_SKIP_VOD), skip_vod, ConfigInputOptionsFormAction::SkipVod) }
                    { edit_field_bool!(state, translate.t(LABEL_SKIP_SERIES), skip_series, ConfigInputOptionsFormAction::SkipSeries) }
                </div>
                { edit_field_bool!(state, translate.t("LABEL.STALKER_BULK_EPG"), stalker_bulk_epg, ConfigInputOptionsFormAction::StalkerBulkEpg) }
            </TitledCard>
        },
        (false, OptionsKind::Xtream) => xtream_options_readonly(&state, &translate),
        (true, OptionsKind::Xtream) => xtream_options_editable(&state, &translate),
    };
    html! {
        <Card class="tp__config-view__card">
            {type_options}
            <TitledCard title={translate.t(LABEL_METADATA)}>
                if props.allow_write {
                    { edit_field_bool!(state, translate.t(LABEL_RESOLVE_TMDB), resolve_tmdb, ConfigInputOptionsFormAction::ResolveTmdb) }
                } else {
                    { config_field_bool!(state.form, translate.t(LABEL_RESOLVE_TMDB), resolve_tmdb) }
                }
            </TitledCard>
            { config_field_child!(translate.t(LABEL_HEADERS), "INPUT_FORM.HEADERS", {
                let headers = headers.clone();
                html! { <KeyValueEditor entries={(*headers).clone()} readonly={!props.allow_write}
                    key_placeholder={translate.t("LABEL.HEADER_NAME")} value_placeholder={translate.t("LABEL.HEADER_VALUE")}
                    on_change={Callback::from(move |value| headers.set(value))} /> }
            })}
        </Card>
    }
}

fn xtream_options_readonly(state: &UseReducerHandle<ConfigInputOptionsDtoFormState>, translate: &YewI18n) -> Html {
    html! { <>
        <TitledCard title={translate.t(LABEL_SKIP)}><div class="tp__config-view__cols-3">
            { config_field_bool!(state.form, translate.t(LABEL_SKIP_LIVE), skip_live) }
            { config_field_bool!(state.form, translate.t(LABEL_SKIP_VOD), skip_vod) }
            { config_field_bool!(state.form, translate.t(LABEL_SKIP_SERIES), skip_series) }
        </div></TitledCard>
        <TitledCard title={translate.t(LABEL_LIVE_STREAMS)}><div class="tp__config-view__cols-2">
            { config_field_bool!(state.form, translate.t(LABEL_XTREAM_LIVE_STREAM_USE_PREFIX), xtream_live_stream_use_prefix) }
            { config_field_bool!(state.form, translate.t(LABEL_XTREAM_LIVE_STREAM_WITHOUT_EXTENSION), xtream_live_stream_without_extension) }
        </div></TitledCard>
        <TitledCard title={translate.t(LABEL_RESOLVE)}>
            <div class="tp__config-view__cols-2">
                { config_field_bool!(state.form, translate.t(LABEL_SKIP_VOD), resolve_vod) }
                { config_field_bool!(state.form, translate.t(LABEL_SKIP_SERIES), resolve_series) }
            </div>
            { config_field_custom!(translate.t(LABEL_RESOLVE_DELAY_SEC), state.form.resolve_delay.to_string()) }
            { config_field_bool!(state.form, translate.t(LABEL_RESOLVE_BACKGROUND), resolve_background) }
            { config_field_optional!(state.form, translate.t(LABEL_RESOLVE_FILTER), resolve_filter) }
        </TitledCard>
        <TitledCard title={translate.t(LABEL_PROBE)}>
            <div class="tp__config-view__cols-3">
                { config_field_bool!(state.form, translate.t(LABEL_SKIP_LIVE), probe_live) }
                { config_field_bool!(state.form, translate.t(LABEL_SKIP_VOD), probe_vod) }
                { config_field_bool!(state.form, translate.t(LABEL_SKIP_SERIES), probe_series) }
            </div>
            <div class="tp__config-view__cols-2">
                { config_field_custom!(translate.t(LABEL_PROBE_DELAY_SEC), state.form.probe_delay.to_string()) }
                { config_field_custom!(translate.t(LABEL_PROBE_LIVE_INTERVAL_HOURS), state.form.probe_live_interval_hours.to_string()) }
            </div>
            { config_field_optional!(state.form, translate.t(LABEL_PROBE_FILTER), probe_filter) }
        </TitledCard>
    </> }
}

fn xtream_options_editable(state: &UseReducerHandle<ConfigInputOptionsDtoFormState>, translate: &YewI18n) -> Html {
    html! { <>
        <TitledCard title={translate.t(LABEL_SKIP)}><div class="tp__config-view__cols-3">
            { edit_field_bool!(state, translate.t(LABEL_SKIP_LIVE), skip_live, ConfigInputOptionsFormAction::SkipLive) }
            { edit_field_bool!(state, translate.t(LABEL_SKIP_VOD), skip_vod, ConfigInputOptionsFormAction::SkipVod) }
            { edit_field_bool!(state, translate.t(LABEL_SKIP_SERIES), skip_series, ConfigInputOptionsFormAction::SkipSeries) }
        </div></TitledCard>
        <TitledCard title={translate.t(LABEL_LIVE_STREAMS)}><div class="tp__config-view__cols-2">
            { edit_field_bool!(state, translate.t(LABEL_XTREAM_LIVE_STREAM_USE_PREFIX), xtream_live_stream_use_prefix, ConfigInputOptionsFormAction::XtreamLiveStreamUsePrefix) }
            { edit_field_bool!(state, translate.t(LABEL_XTREAM_LIVE_STREAM_WITHOUT_EXTENSION), xtream_live_stream_without_extension, ConfigInputOptionsFormAction::XtreamLiveStreamWithoutExtension) }
        </div></TitledCard>
        <TitledCard title={translate.t(LABEL_RESOLVE)}>
            <div class="tp__config-view__cols-2">
                { edit_field_bool!(state, translate.t(LABEL_SKIP_VOD), resolve_vod, ConfigInputOptionsFormAction::ResolveVod) }
                { edit_field_bool!(state, translate.t(LABEL_SKIP_SERIES), resolve_series, ConfigInputOptionsFormAction::ResolveSeries) }
            </div>
            { edit_field_number_u16!(state, translate.t(LABEL_RESOLVE_DELAY_SEC), resolve_delay, ConfigInputOptionsFormAction::ResolveDelay) }
            { edit_field_bool!(state, translate.t(LABEL_RESOLVE_BACKGROUND), resolve_background, ConfigInputOptionsFormAction::ResolveBackground) }
            { config_field_child!(translate.t(LABEL_RESOLVE_FILTER), "INPUT_FORM.RESOLVE_FILTER", {
                let state = state.clone();
                html! { <FilterInput filter={state.form.resolve_filter.clone().unwrap_or_default()} on_change={Callback::from(move |value| state.dispatch(ConfigInputOptionsFormAction::ResolveFilter(value)))} /> }
            })}
        </TitledCard>
        <TitledCard title={translate.t(LABEL_PROBE)}>
            <div class="tp__config-view__cols-3">
                { edit_field_bool!(state, translate.t(LABEL_SKIP_LIVE), probe_live, ConfigInputOptionsFormAction::ProbeLive) }
                { edit_field_bool!(state, translate.t(LABEL_SKIP_VOD), probe_vod, ConfigInputOptionsFormAction::ProbeVod) }
                { edit_field_bool!(state, translate.t(LABEL_SKIP_SERIES), probe_series, ConfigInputOptionsFormAction::ProbeSeries) }
            </div>
            <div class="tp__config-view__cols-2">
                { edit_field_number_u16!(state, translate.t(LABEL_PROBE_DELAY_SEC), probe_delay, ConfigInputOptionsFormAction::ProbeDelay) }
                { edit_field_number_u32!(state, translate.t(LABEL_PROBE_LIVE_INTERVAL_HOURS), probe_live_interval_hours, ConfigInputOptionsFormAction::ProbeLiveIntervalHours) }
            </div>
            { config_field_child!(translate.t(LABEL_PROBE_FILTER), "INPUT_FORM.PROBE_FILTER", {
                let state = state.clone();
                html! { <FilterInput filter={state.form.probe_filter.clone().unwrap_or_default()} on_change={Callback::from(move |value| state.dispatch(ConfigInputOptionsFormAction::ProbeFilter(value)))} /> }
            })}
        </TitledCard>
    </> }
}

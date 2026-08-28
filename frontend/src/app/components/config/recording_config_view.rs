use super::config_page::ConfigForm;
use crate::{
    app::{
        components::{
            config::{
                config_page::LABEL_RECORDING_CONFIG, config_view_context::ConfigViewContext, use_emit_mapped,
                RecordingConfigCards,
            },
            Card, KeyValueEditor,
        },
        context::ConfigContext,
    },
    config_field, config_field_bool, config_field_child, config_field_optional, edit_field_bool, edit_field_list,
    edit_field_number_f64, edit_field_number_u64, edit_field_number_u8, edit_field_text_option, generate_form_reducer,
    hooks::use_service_context,
    i18n::use_translation,
    use_default_form_reducer,
};
use shared::model::{RecordingConfigDto, VideoConfigDto};
use std::collections::HashMap;
use yew::prelude::*;

const LABEL_ORGANIZE_INTO_DIRECTORIES: &str = "LABEL.ORGANIZE_INTO_DIRECTORIES";
const LABEL_DIRECTORY: &str = "LABEL.DIRECTORY";
const LABEL_EPISODE_PATTERN: &str = "LABEL.EPISODE_PATTERN";
const LABEL_HEADERS: &str = "LABEL.HEADERS";
const LABEL_EXTENSIONS: &str = "LABEL.EXTENSIONS";
const LABEL_ADD_EXTENSION: &str = "LABEL.ADD_EXTENSION";
const LABEL_RECORDING_QUEUE: &str = "LABEL.DOWNLOAD_QUEUE";
const LABEL_RETRY_BACKOFF_INITIAL: &str = "LABEL.DOWNLOAD_RETRY_BACKOFF_INITIAL";
const LABEL_RETRY_BACKOFF_MULTIPLIER: &str = "LABEL.DOWNLOAD_RETRY_BACKOFF_MULTIPLIER";
const LABEL_RETRY_BACKOFF_MAX: &str = "LABEL.DOWNLOAD_RETRY_BACKOFF_MAX";
const LABEL_RETRY_BACKOFF_JITTER_PERCENT: &str = "LABEL.DOWNLOAD_RETRY_BACKOFF_JITTER_PERCENT";
const LABEL_RETRY_MAX_ATTEMPTS: &str = "LABEL.DOWNLOAD_RETRY_MAX_ATTEMPTS";
const LABEL_RESERVE_SLOTS_FOR_USERS: &str = "LABEL.RESERVE_SLOTS_FOR_USERS";
const LABEL_MAX_BACKGROUND_PER_PROVIDER: &str = "LABEL.MAX_BACKGROUND_PER_PROVIDER";

generate_form_reducer!(
    state: RecordingConfigFormState { form: RecordingConfigDto },
    action_name: RecordingConfigFormAction,
    fields {
        OrganizeIntoDirectories => organize_into_directories: bool,
        Directory => directory: Option<String>,
        EpisodePattern => episode_pattern: Option<String>,
        Headers => headers: HashMap<String, String>,
        ReserveSlotsForUsers => reserve_slots_for_users: u8,
        MaxBackgroundPerProvider => max_background_per_provider: u8,
        RetryBackoffInitialSecs => retry_backoff_initial_secs: u64,
        RetryBackoffMultiplier => retry_backoff_multiplier: f64,
        RetryBackoffMaxSecs => retry_backoff_max_secs: u64,
        RetryBackoffJitterPercent => retry_backoff_jitter_percent: u8,
        RetryMaxAttempts => retry_max_attempts: u8,
    }
);

generate_form_reducer!(
    state: VideoConfigFormState { form: VideoConfigDto },
    action_name: VideoConfigFormAction,
    fields {
        Extensions => extensions: Vec<String>,
    }
);

#[component]
pub fn RecordingConfigView() -> Html {
    let translate = use_translation();
    let services = use_service_context();
    let config_ctx = use_context::<ConfigContext>().expect("Recording ConfigContext not found");
    let config_view_ctx = use_context::<ConfigViewContext>().expect("ConfigViewContext not found");
    let state: UseReducerHandle<RecordingConfigFormState> =
        use_default_form_reducer!(RecordingConfigFormState { form: RecordingConfigDto::default() });
    let video_state: UseReducerHandle<VideoConfigFormState> =
        use_default_form_reducer!(VideoConfigFormState { form: VideoConfigDto::default() });
    let reload_generation = use_state(|| 0_u64);

    {
        let state = state.clone();
        let video_state = video_state.clone();
        let reload_generation = reload_generation.clone();
        let video = config_ctx.config.as_ref().and_then(|config| config.config.video.as_ref()).cloned();
        let edit_mode = *config_view_ctx.edit_mode;
        use_effect_with((video, edit_mode), move |(video, _)| {
            video_state.dispatch(VideoConfigFormAction::SetAll(video.clone().unwrap_or_default()));
            state.dispatch(RecordingConfigFormAction::SetAll(
                video
                    .as_ref()
                    .and_then(|video| video.recording.clone())
                    .unwrap_or_else(|| RecordingConfigDto { enabled: false, ..Default::default() }),
            ));
            reload_generation.set((*reload_generation).wrapping_add(1));
            || ()
        });
    }

    use_emit_mapped(
        (state.form.clone(), video_state.form.clone(), state.modified || video_state.modified),
        config_view_ctx.on_form_change.clone(),
        |(recording, mut video, modified)| {
            video.recording = Some(recording);
            ConfigForm::Recording(modified, video)
        },
    );

    let handle_headers = {
        let state = state.clone();
        Callback::from(move |headers| state.dispatch(RecordingConfigFormAction::Headers(headers)))
    };
    let handle_recording = {
        let state = state.clone();
        Callback::from(move |(_, recording)| state.dispatch(RecordingConfigFormAction::SetAll(recording)))
    };
    let transfer_view = html! {
        <>
            <Card>
                { config_field_child!(translate.t(LABEL_EXTENSIONS), "RECORDING_CONFIG.EXTENSIONS", {
                    html! { <div class="tp__config-view__tags">{ for video_state.form.extensions.iter().map(|extension| html! { <span>{extension}</span> }) }</div> }
                }) }
            </Card>
            <Card class="tp__config-view__card">
                { config_field_bool!(state.form, translate.t(LABEL_ORGANIZE_INTO_DIRECTORIES), organize_into_directories) }
                { config_field_optional!(state.form, translate.t(LABEL_DIRECTORY), directory) }
                { config_field_optional!(state.form, translate.t(LABEL_EPISODE_PATTERN), episode_pattern) }
                { config_field_child!(translate.t(LABEL_HEADERS), "RECORDING_CONFIG.HEADERS", {
                    html! { <ul>{ for state.form.headers.iter().map(|(key, value)| html! { <li key={key.clone()}>{format!("- {key}: {value}")}</li> }) }</ul> }
                }) }
            </Card>
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_RECORDING_QUEUE)}</h1>
                { config_field!(state.form, translate.t(LABEL_RESERVE_SLOTS_FOR_USERS), reserve_slots_for_users, "RECORDING_CONFIG.RESERVE_SLOTS_FOR_USERS") }
                { config_field!(state.form, translate.t(LABEL_MAX_BACKGROUND_PER_PROVIDER), max_background_per_provider, "RECORDING_CONFIG.MAX_BACKGROUND_PER_PROVIDER") }
                { config_field!(state.form, translate.t(LABEL_RETRY_BACKOFF_INITIAL), retry_backoff_initial_secs, "RECORDING_CONFIG.RETRY_BACKOFF_INITIAL") }
                { config_field!(state.form, translate.t(LABEL_RETRY_BACKOFF_MULTIPLIER), retry_backoff_multiplier, "RECORDING_CONFIG.RETRY_BACKOFF_MULTIPLIER") }
                { config_field!(state.form, translate.t(LABEL_RETRY_BACKOFF_MAX), retry_backoff_max_secs, "RECORDING_CONFIG.RETRY_BACKOFF_MAX") }
                { config_field!(state.form, translate.t(LABEL_RETRY_BACKOFF_JITTER_PERCENT), retry_backoff_jitter_percent, "RECORDING_CONFIG.RETRY_BACKOFF_JITTER_PERCENT") }
                { config_field!(state.form, translate.t(LABEL_RETRY_MAX_ATTEMPTS), retry_max_attempts, "RECORDING_CONFIG.RETRY_MAX_ATTEMPTS") }
            </Card>
        </>
    };

    let transfer_edit = html! {
        <>
            <Card>{ edit_field_list!(video_state, translate.t(LABEL_EXTENSIONS), extensions, VideoConfigFormAction::Extensions, translate.t(LABEL_ADD_EXTENSION)) }</Card>
            <Card class="tp__config-view__card">
                { edit_field_bool!(state, translate.t(LABEL_ORGANIZE_INTO_DIRECTORIES), organize_into_directories, RecordingConfigFormAction::OrganizeIntoDirectories) }
                { edit_field_text_option!(state, translate.t(LABEL_DIRECTORY), directory, RecordingConfigFormAction::Directory) }
                { edit_field_text_option!(state, translate.t(LABEL_EPISODE_PATTERN), episode_pattern, RecordingConfigFormAction::EpisodePattern) }
                <KeyValueEditor label={Some(translate.t(LABEL_HEADERS))} entries={state.form.headers.clone()} readonly={false} on_change={handle_headers} />
            </Card>
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_RECORDING_QUEUE)}</h1>
                { edit_field_number_u8!(state, translate.t(LABEL_RESERVE_SLOTS_FOR_USERS), reserve_slots_for_users, RecordingConfigFormAction::ReserveSlotsForUsers) }
                { edit_field_number_u8!(state, translate.t(LABEL_MAX_BACKGROUND_PER_PROVIDER), max_background_per_provider, RecordingConfigFormAction::MaxBackgroundPerProvider) }
                { edit_field_number_u64!(state, translate.t(LABEL_RETRY_BACKOFF_INITIAL), retry_backoff_initial_secs, RecordingConfigFormAction::RetryBackoffInitialSecs) }
                { edit_field_number_f64!(state, translate.t(LABEL_RETRY_BACKOFF_MULTIPLIER), retry_backoff_multiplier, RecordingConfigFormAction::RetryBackoffMultiplier) }
                { edit_field_number_u64!(state, translate.t(LABEL_RETRY_BACKOFF_MAX), retry_backoff_max_secs, RecordingConfigFormAction::RetryBackoffMaxSecs) }
                { edit_field_number_u8!(state, translate.t(LABEL_RETRY_BACKOFF_JITTER_PERCENT), retry_backoff_jitter_percent, RecordingConfigFormAction::RetryBackoffJitterPercent) }
                { edit_field_number_u8!(state, translate.t(LABEL_RETRY_MAX_ATTEMPTS), retry_max_attempts, RecordingConfigFormAction::RetryMaxAttempts) }
            </Card>
        </>
    };

    html! {
        <div class="tp__recording-config-view tp__config-view-page">
            <div class="tp__config-view-page__title">{translate.t(LABEL_RECORDING_CONFIG)}</div>
            <div class="tp__recording-config-view__body tp__config-view-page__body">
                { if *config_view_ctx.edit_mode { transfer_edit } else { transfer_view } }
                <RecordingConfigCards
                    recording={state.form.clone()}
                    reload_generation={*reload_generation}
                    edit_mode={*config_view_ctx.edit_mode}
                    on_change={handle_recording}
                    on_error={{ let toastr = services.toastr.clone(); Callback::from(move |message| toastr.error(message)) }}
                />
            </div>
        </div>
    }
}

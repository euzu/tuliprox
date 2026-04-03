use crate::{
    app::{
        components::{
            config::{
                config_page::{ConfigForm, LABEL_VIDEO_CONFIG},
                config_view_context::ConfigViewContext,
                use_emit_mapped,
            },
            Card, Chip, KeyValueEditor,
        },
        context::ConfigContext,
    },
    config_field_bool, config_field_child, config_field_optional, edit_field_bool, edit_field_list,
    edit_field_number_f64, edit_field_number_u64, edit_field_number_u8, edit_field_text_option, generate_form_reducer,
    i18n::use_translation,
};
use shared::model::{VideoConfigDto, VideoDownloadConfigDto};
use std::collections::HashMap;
use yew::prelude::*;

const LABEL_DOWNLOAD: &str = "LABEL.DOWNLOAD";
const LABEL_ORGANIZE_INTO_DIRECTORIES: &str = "LABEL.ORGANIZE_INTO_DIRECTORIES";
const LABEL_DIRECTORY: &str = "LABEL.DIRECTORY";
const LABEL_EPISODE_PATTERN: &str = "LABEL.EPISODE_PATTERN";
const LABEL_HEADERS: &str = "LABEL.HEADERS";
const LABEL_EXTENSIONS: &str = "LABEL.EXTENSIONS";
const LABEL_WEB_SEARCH: &str = "LABEL.WEB_SEARCH";
const LABEL_ADD_EXTENSION: &str = "LABEL.ADD_EXTENSION";
const LABEL_DOWNLOAD_QUEUE: &str = "LABEL.DOWNLOAD_QUEUE";
const LABEL_DOWNLOAD_RETRY_BACKOFF_INITIAL: &str = "LABEL.DOWNLOAD_RETRY_BACKOFF_INITIAL";
const LABEL_DOWNLOAD_RETRY_BACKOFF_MULTIPLIER: &str = "LABEL.DOWNLOAD_RETRY_BACKOFF_MULTIPLIER";
const LABEL_DOWNLOAD_RETRY_BACKOFF_MAX: &str = "LABEL.DOWNLOAD_RETRY_BACKOFF_MAX";
const LABEL_DOWNLOAD_RETRY_BACKOFF_JITTER_PERCENT: &str = "LABEL.DOWNLOAD_RETRY_BACKOFF_JITTER_PERCENT";
const LABEL_DOWNLOAD_RETRY_MAX_ATTEMPTS: &str = "LABEL.DOWNLOAD_RETRY_MAX_ATTEMPTS";
const LABEL_RESERVE_SLOTS_FOR_USERS: &str = "LABEL.RESERVE_SLOTS_FOR_USERS";
const LABEL_MAX_BACKGROUND_PER_PROVIDER: &str = "LABEL.MAX_BACKGROUND_PER_PROVIDER";

generate_form_reducer!(
    state: VideoDownloadConfigFormState { form: VideoDownloadConfigDto },
    action_name: VideoDownloadConfigFormAction,
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
        WebSearch => web_search: Option<String>,
        Extensions => extensions: Vec<String>,
    }
);

#[component]
pub fn VideoConfigView() -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let config_view_ctx = use_context::<ConfigViewContext>().expect("ConfigViewContext not found");

    let download_state: UseReducerHandle<VideoDownloadConfigFormState> =
        use_reducer(|| VideoDownloadConfigFormState { form: VideoDownloadConfigDto::default(), modified: false });
    let video_state: UseReducerHandle<VideoConfigFormState> =
        use_reducer(|| VideoConfigFormState { form: VideoConfigDto::default(), modified: false });

    let handle_headers = {
        let download_state = download_state.clone();
        Callback::from(move |headers: HashMap<String, String>| {
            download_state.dispatch(VideoDownloadConfigFormAction::Headers(headers));
        })
    };

    {
        let video_state = video_state.clone();
        let download_state = download_state.clone();
        let video_cfg = config_ctx.config.as_ref().and_then(|c| c.config.video.clone());
        use_effect_with(video_cfg, move |video_cfg| {
            if let Some(video) = video_cfg {
                video_state.dispatch(VideoConfigFormAction::SetAll(video.clone()));
                download_state.dispatch(VideoDownloadConfigFormAction::SetAll(
                    video.download.as_ref().map_or_else(VideoDownloadConfigDto::default, |d| d.clone()),
                ));
            } else {
                video_state.dispatch(VideoConfigFormAction::SetAll(VideoConfigDto::default()));
                download_state.dispatch(VideoDownloadConfigFormAction::SetAll(VideoDownloadConfigDto::default()));
            }
            || ()
        });
    }

    {
        let deps =
            (video_state.form.clone(), download_state.form.clone(), video_state.modified, download_state.modified);
        use_emit_mapped(
            deps,
            config_view_ctx.on_form_change.clone(),
            |(video_form, download_form, video_modified, download_modified)| {
                let mut form = video_form;
                form.download = if download_modified { Some(download_form) } else { form.download };
                let modified = video_modified || download_modified;
                ConfigForm::Video(modified, form)
            },
        );
    }

    let render_extensions = |extensions: &Vec<String>| {
        html! {
            <Card>
            { config_field_child!(translate.t(LABEL_EXTENSIONS), "VIDEO_CONFIG.EXTENSIONS", {
               html! {
                 <div class="tp__config-view__tags">
                 for t in extensions.iter() { <Chip label={t.clone()} /> }
                 </div>
                }})}
            </Card>
        }
    };

    let render_download_view = || {
        html! {
            <>
                <Card class="tp__config-view__card">
                    <h1>{translate.t(LABEL_DOWNLOAD)}</h1>
                    { config_field_bool!(download_state.form, translate.t(LABEL_ORGANIZE_INTO_DIRECTORIES), organize_into_directories) }
                    { config_field_optional!(download_state.form, translate.t(LABEL_DIRECTORY), directory) }
                    { config_field_optional!(download_state.form, translate.t(LABEL_EPISODE_PATTERN), episode_pattern) }
                    { config_field_child!(translate.t(LABEL_HEADERS), "VIDEO_CONFIG.HEADERS", {
                        html! {
                            <div class="tp__config-view__tags">
                              <ul>
                                for (k, v) in download_state.form.headers.iter() { <li key={k.clone()}>{"- "}{k}{": "} {v}</li> }
                              </ul>
                            </div>
                        }
                    })}
                </Card>
                <Card class="tp__config-view__card">
                    <h1>{translate.t(LABEL_DOWNLOAD_QUEUE)}</h1>
                    { config_field_child!(translate.t(LABEL_RESERVE_SLOTS_FOR_USERS), "VIDEO_CONFIG.RESERVE_SLOTS_FOR_USERS", { html! { download_state.form.reserve_slots_for_users } }) }
                    { config_field_child!(translate.t(LABEL_MAX_BACKGROUND_PER_PROVIDER), "VIDEO_CONFIG.MAX_BACKGROUND_PER_PROVIDER", { html! { download_state.form.max_background_per_provider } }) }
                    { config_field_child!(translate.t(LABEL_DOWNLOAD_RETRY_BACKOFF_INITIAL), "VIDEO_CONFIG.DOWNLOAD_RETRY_BACKOFF_INITIAL", { html! { download_state.form.retry_backoff_initial_secs } }) }
                    { config_field_child!(translate.t(LABEL_DOWNLOAD_RETRY_BACKOFF_MULTIPLIER), "VIDEO_CONFIG.DOWNLOAD_RETRY_BACKOFF_MULTIPLIER", { html! { download_state.form.retry_backoff_multiplier } }) }
                    { config_field_child!(translate.t(LABEL_DOWNLOAD_RETRY_BACKOFF_MAX), "VIDEO_CONFIG.DOWNLOAD_RETRY_BACKOFF_MAX", { html! { download_state.form.retry_backoff_max_secs } }) }
                    { config_field_child!(translate.t(LABEL_DOWNLOAD_RETRY_BACKOFF_JITTER_PERCENT), "VIDEO_CONFIG.DOWNLOAD_RETRY_BACKOFF_JITTER_PERCENT", { html! { download_state.form.retry_backoff_jitter_percent } }) }
                    { config_field_child!(translate.t(LABEL_DOWNLOAD_RETRY_MAX_ATTEMPTS), "VIDEO_CONFIG.DOWNLOAD_RETRY_MAX_ATTEMPTS", { html! { download_state.form.retry_max_attempts } }) }
                </Card>
            </>
        }
    };

    let render_view_mode = || {
        html! {
          <>
            <div class="tp__video-config-view__body tp__config-view-page__body">
              { config_field_optional!(video_state.form, translate.t(LABEL_WEB_SEARCH), web_search) }
            </div>
            <div class="tp__video-config-view__body tp__config-view-page__body">
              { render_extensions(&video_state.form.extensions) }
              { render_download_view() }
            </div>
          </>
        }
    };

    let render_edit_mode = || {
        html! {
        <>
          <div class="tp__video-config-view__body tp__config-view-page__body">
            { edit_field_text_option!(video_state, translate.t(LABEL_WEB_SEARCH), web_search, VideoConfigFormAction::WebSearch) }
          </div>
          <div class="tp__video-config-view__body tp__config-view-page__body">
            <Card class="tp__config-view__card">
                { edit_field_list!(video_state, translate.t(LABEL_EXTENSIONS), extensions, VideoConfigFormAction::Extensions, translate.t(LABEL_ADD_EXTENSION)) }
            </Card>
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_DOWNLOAD)}</h1>
                { edit_field_bool!(download_state, translate.t(LABEL_ORGANIZE_INTO_DIRECTORIES), organize_into_directories, VideoDownloadConfigFormAction::OrganizeIntoDirectories) }
                { edit_field_text_option!(download_state, translate.t(LABEL_DIRECTORY), directory, VideoDownloadConfigFormAction::Directory) }
                { edit_field_text_option!(download_state, translate.t(LABEL_EPISODE_PATTERN), episode_pattern, VideoDownloadConfigFormAction::EpisodePattern) }
                <KeyValueEditor
                    label={Some(translate.t(LABEL_HEADERS))}
                    entries={download_state.form.headers.clone()}
                    readonly={false}
                    on_change={handle_headers.clone()}
                />
            </Card>
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_DOWNLOAD_QUEUE)}</h1>
                { edit_field_number_u8!(download_state, translate.t(LABEL_RESERVE_SLOTS_FOR_USERS), reserve_slots_for_users, VideoDownloadConfigFormAction::ReserveSlotsForUsers) }
                { edit_field_number_u8!(download_state, translate.t(LABEL_MAX_BACKGROUND_PER_PROVIDER), max_background_per_provider, VideoDownloadConfigFormAction::MaxBackgroundPerProvider) }
                { edit_field_number_u64!(download_state, translate.t(LABEL_DOWNLOAD_RETRY_BACKOFF_INITIAL), retry_backoff_initial_secs, VideoDownloadConfigFormAction::RetryBackoffInitialSecs) }
                { edit_field_number_f64!(download_state, translate.t(LABEL_DOWNLOAD_RETRY_BACKOFF_MULTIPLIER), retry_backoff_multiplier, VideoDownloadConfigFormAction::RetryBackoffMultiplier) }
                { edit_field_number_u64!(download_state, translate.t(LABEL_DOWNLOAD_RETRY_BACKOFF_MAX), retry_backoff_max_secs, VideoDownloadConfigFormAction::RetryBackoffMaxSecs) }
                { edit_field_number_u8!(download_state, translate.t(LABEL_DOWNLOAD_RETRY_BACKOFF_JITTER_PERCENT), retry_backoff_jitter_percent, VideoDownloadConfigFormAction::RetryBackoffJitterPercent) }
                { edit_field_number_u8!(download_state, translate.t(LABEL_DOWNLOAD_RETRY_MAX_ATTEMPTS), retry_max_attempts, VideoDownloadConfigFormAction::RetryMaxAttempts) }
            </Card>
          </div>
        </>
        }
    };

    html! {
        <div class="tp__video-config-view tp__config-view-page">
            <div class="tp__config-view-page__title">{translate.t(LABEL_VIDEO_CONFIG)}</div>
            { if *config_view_ctx.edit_mode { render_edit_mode() } else { render_view_mode() } }
        </div>
    }
}

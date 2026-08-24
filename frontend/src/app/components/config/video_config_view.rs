use crate::{
    app::{
        components::{
            config::{
                config_page::{ConfigForm, LABEL_VIDEO_CONFIG},
                config_view_context::ConfigViewContext,
                recording_config_for_video, use_emit_mapped, RecordingConfigCards,
            },
            Card, Chip, KeyValueEditor,
        },
        context::ConfigContext,
    },
    config_field, config_field_bool, config_field_child, config_field_optional, edit_field_bool, edit_field_list,
    edit_field_number_f64, edit_field_number_u64, edit_field_number_u8, edit_field_text_option, generate_form_reducer,
    hooks::use_service_context,
    i18n::use_translation,
    use_default_form_reducer,
};
use shared::model::{RecordingConfigDto, VideoConfigDto, VideoDownloadConfigDto};
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
        RecordingPriority => recording_priority: i8,
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
    let services = use_service_context();
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let config_view_ctx = use_context::<ConfigViewContext>().expect("ConfigViewContext not found");

    let download_state: UseReducerHandle<VideoDownloadConfigFormState> =
        use_default_form_reducer!(VideoDownloadConfigFormState { form: VideoDownloadConfigDto::default() });
    let video_state: UseReducerHandle<VideoConfigFormState> =
        use_default_form_reducer!(VideoConfigFormState { form: VideoConfigDto::default() });
    let recording_form = use_state(|| (false, RecordingConfigDto { enabled: false, ..Default::default() }));
    let reload_generation = use_state(|| 0_u64);
    let download_configured = use_state(|| false);

    let handle_headers = {
        let download_state = download_state.clone();
        Callback::from(move |headers: HashMap<String, String>| {
            download_state.dispatch(VideoDownloadConfigFormAction::Headers(headers));
        })
    };

    {
        let video_state = video_state.clone();
        let download_state = download_state.clone();
        let recording_form = recording_form.clone();
        let reload_generation = reload_generation.clone();
        let download_configured = download_configured.clone();
        let video_cfg = config_ctx.config.as_ref().and_then(|c| c.config.video.clone());
        let edit_mode = *config_view_ctx.edit_mode;
        use_effect_with((video_cfg.clone(), edit_mode), move |(video_cfg, _)| {
            if let Some(video) = video_cfg {
                video_state.dispatch(VideoConfigFormAction::SetAll(video.clone()));
                download_configured.set(video.download.is_some());
                let download_form =
                    video.download.as_ref().map_or_else(VideoDownloadConfigDto::default, std::clone::Clone::clone);
                download_state.dispatch(VideoDownloadConfigFormAction::SetAll(download_form));
                recording_form.set((false, recording_config_for_video(Some(video))));
            } else {
                video_state.dispatch(VideoConfigFormAction::SetAll(VideoConfigDto::default()));
                download_state.dispatch(VideoDownloadConfigFormAction::SetAll(VideoDownloadConfigDto::default()));
                download_configured.set(false);
                recording_form.set((false, recording_config_for_video(None)));
            }
            reload_generation.set((*reload_generation).wrapping_add(1));
            || ()
        });
    }

    {
        let deps = (
            video_state.form.clone(),
            download_state.form.clone(),
            (*recording_form).clone(),
            video_state.modified,
            download_state.modified,
            *download_configured,
        );
        use_emit_mapped(
            deps,
            config_view_ctx.on_form_change.clone(),
            |(
                video_form,
                download_form,
                (recording_modified, recording_form),
                video_modified,
                download_modified,
                originally_configured,
            )| {
                let form = merge_video_recording(
                    video_form,
                    download_form,
                    recording_form,
                    download_modified,
                    recording_modified,
                    originally_configured,
                );
                let modified = video_modified || download_modified || recording_modified;
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
                <RecordingConfigCards
                    recording={recording_form.1.clone()}
                    recording_priority={download_state.form.recording_priority}
                    reload_generation={*reload_generation}
                    edit_mode={false}
                    on_change={{ let recording_form = recording_form.clone(); Callback::from(move |value| recording_form.set(value)) }}
                    on_recording_priority_change={Callback::noop()}
                    on_error={Callback::noop()}
                />
                <Card class="tp__config-view__card">
                    <h1>{translate.t(LABEL_DOWNLOAD_QUEUE)}</h1>
                    { config_field!(download_state.form, translate.t(LABEL_RESERVE_SLOTS_FOR_USERS), reserve_slots_for_users, "VIDEO_CONFIG.RESERVE_SLOTS_FOR_USERS") }
                    { config_field!(download_state.form, translate.t(LABEL_MAX_BACKGROUND_PER_PROVIDER), max_background_per_provider, "VIDEO_CONFIG.MAX_BACKGROUND_PER_PROVIDER") }
                    { config_field!(download_state.form, translate.t(LABEL_DOWNLOAD_RETRY_BACKOFF_INITIAL), retry_backoff_initial_secs, "VIDEO_CONFIG.DOWNLOAD_RETRY_BACKOFF_INITIAL") }
                    { config_field!(download_state.form, translate.t(LABEL_DOWNLOAD_RETRY_BACKOFF_MULTIPLIER), retry_backoff_multiplier, "VIDEO_CONFIG.DOWNLOAD_RETRY_BACKOFF_MULTIPLIER") }
                    { config_field!(download_state.form, translate.t(LABEL_DOWNLOAD_RETRY_BACKOFF_MAX), retry_backoff_max_secs, "VIDEO_CONFIG.DOWNLOAD_RETRY_BACKOFF_MAX") }
                    { config_field!(download_state.form, translate.t(LABEL_DOWNLOAD_RETRY_BACKOFF_JITTER_PERCENT), retry_backoff_jitter_percent, "VIDEO_CONFIG.DOWNLOAD_RETRY_BACKOFF_JITTER_PERCENT") }
                    { config_field!(download_state.form, translate.t(LABEL_DOWNLOAD_RETRY_MAX_ATTEMPTS), retry_max_attempts, "VIDEO_CONFIG.DOWNLOAD_RETRY_MAX_ATTEMPTS") }
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
            <RecordingConfigCards
                recording={recording_form.1.clone()}
                recording_priority={download_state.form.recording_priority}
                reload_generation={*reload_generation}
                edit_mode={true}
                on_change={{ let recording_form = recording_form.clone(); Callback::from(move |value| recording_form.set(value)) }}
                on_recording_priority_change={{ let download_state = download_state.clone(); Callback::from(move |value| download_state.dispatch(VideoDownloadConfigFormAction::RecordingPriority(value))) }}
                on_error={{ let toastr = services.toastr.clone(); Callback::from(move |message| toastr.error(message)) }}
            />
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

/// Effective DVR state for the toggle: a missing `video.download`
/// block means no DVR engine exists yet, so the visible switch starts
/// off. Once a download block is present, an absent or `enabled: true`
/// nested `recording` block reads as "DVR on" (the default).
#[cfg(test)]
pub fn effective_recording_enabled(video: Option<&VideoConfigDto>) -> bool {
    video
        .and_then(|video| video.download.as_ref())
        .is_some_and(|download| download.recording.as_ref().is_none_or(|recording| recording.enabled))
}

/// Apply the recording toggle to a download block. Preserves every
/// existing nested field (`directory`, retention, etc.) — the toggle
/// only owns the `enabled` flag.
pub fn set_recording_enabled(download: &mut VideoDownloadConfigDto, enabled: bool) {
    let recording = download.recording.get_or_insert_with(RecordingConfigDto::default);
    recording.enabled = enabled;
}

pub fn merge_video_recording(
    mut video: VideoConfigDto,
    mut download: VideoDownloadConfigDto,
    recording: RecordingConfigDto,
    download_modified: bool,
    recording_modified: bool,
    originally_configured: bool,
) -> VideoConfigDto {
    if recording_modified {
        download.recording = Some(recording);
    } else if download_modified && !originally_configured {
        set_recording_enabled(&mut download, false);
    }
    if recording_modified || download_modified {
        video.download = Some(download);
    }
    video
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_recording_enabled_requires_download_block() {
        assert!(!effective_recording_enabled(None));

        let without_recording =
            VideoConfigDto { download: Some(VideoDownloadConfigDto::default()), ..Default::default() };
        assert!(effective_recording_enabled(Some(&without_recording)));

        let disabled = VideoConfigDto {
            download: Some(VideoDownloadConfigDto {
                recording: Some(RecordingConfigDto { enabled: false, ..Default::default() }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(!effective_recording_enabled(Some(&disabled)));
    }

    #[test]
    fn set_recording_enabled_preserves_existing_recording_fields() {
        let mut download = VideoDownloadConfigDto {
            recording: Some(RecordingConfigDto {
                directory: Some("custom-recordings".to_string()),
                enabled: false,
                ..Default::default()
            }),
            ..Default::default()
        };

        set_recording_enabled(&mut download, true);

        assert!(download.recording.is_some());
        let Some(recording) = download.recording.as_ref() else { return };
        assert!(recording.enabled);
        assert_eq!(recording.directory.as_deref(), Some("custom-recordings"));
    }

    #[test]
    fn recording_edit_preserves_complete_download_config() {
        let original_recording = RecordingConfigDto { directory: Some("before".to_string()), ..Default::default() };
        let download = VideoDownloadConfigDto {
            headers: HashMap::from([("User-Agent".to_string(), "agent".to_string())]),
            directory: Some("downloads".to_string()),
            organize_into_directories: true,
            episode_pattern: Some("episode".to_string()),
            download_priority: -3,
            recording_priority: 4,
            reserve_slots_for_users: 5,
            max_background_per_provider: 6,
            retry_backoff_initial_secs: 7,
            retry_backoff_multiplier: 2.5,
            retry_backoff_max_secs: 8,
            retry_backoff_jitter_percent: 9,
            retry_max_attempts: 10,
            recording: Some(original_recording.clone()),
        };
        let video = VideoConfigDto { download: Some(download.clone()), ..Default::default() };
        let recording = RecordingConfigDto { timezone: Some("UTC".to_string()), ..original_recording };

        let merged = merge_video_recording(video, download.clone(), recording.clone(), false, true, true);

        assert_eq!(merged.download, Some(VideoDownloadConfigDto { recording: Some(recording), ..download }));
    }

    #[test]
    fn missing_download_stays_absent_until_recording_changes() {
        let video = VideoConfigDto::default();
        let download = VideoDownloadConfigDto::default();

        let unchanged = merge_video_recording(
            video.clone(),
            download.clone(),
            RecordingConfigDto { enabled: false, ..Default::default() },
            false,
            false,
            false,
        );
        assert!(unchanged.download.is_none());

        let enabled = merge_video_recording(video, download, RecordingConfigDto::default(), false, true, false);
        assert!(enabled
            .download
            .as_ref()
            .and_then(|value| value.recording.as_ref())
            .is_some_and(|value| value.enabled));
    }

    #[test]
    fn disabling_recording_preserves_all_recording_and_download_siblings() {
        let recording = RecordingConfigDto {
            directory: Some("recordings".to_string()),
            timezone: Some("UTC".to_string()),
            retention: Some(shared::model::RecordingRetentionConfigDto {
                keep_last_per_channel: Some(3),
                ..Default::default()
            }),
            ..Default::default()
        };
        let download = VideoDownloadConfigDto {
            headers: HashMap::from([("X-Test".to_string(), "yes".to_string())]),
            recording_priority: 7,
            recording: Some(recording.clone()),
            ..Default::default()
        };
        let video = VideoConfigDto { download: Some(download.clone()), ..Default::default() };
        let disabled = RecordingConfigDto { enabled: false, ..recording };

        let merged = merge_video_recording(video, download.clone(), disabled.clone(), false, true, true);

        assert_eq!(merged.download, Some(VideoDownloadConfigDto { recording: Some(disabled), ..download }));
    }
}

use crate::{
    app::{
        components::{
            config::{
                config_page::{ConfigForm, LABEL_VIDEO_CONFIG},
                config_view_context::ConfigViewContext,
            },
            Card, Chip, KeyValueEditor,
        },
        context::ConfigContext,
    },
    config_field_bool, config_field_child, config_field_optional, edit_field_bool, edit_field_list,
    edit_field_text_option, generate_form_reducer,
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

generate_form_reducer!(
    state: VideoDownloadConfigFormState { form: VideoDownloadConfigDto },
    action_name: VideoDownloadConfigFormAction,
    fields {
        OrganizeIntoDirectories => organize_into_directories: bool,
        Directory => directory: Option<String>,
        EpisodePattern => episode_pattern: Option<String>,
        Headers => headers: HashMap<String, String>,
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
        // Sync form changes with parent context
        let on_form_change = config_view_ctx.on_form_change.clone();
        let download_state = download_state.clone();
        let video_state = video_state.clone();
        let deps = (video_state.modified, download_state.modified, video_state, download_state);
        use_effect_with(deps, move |(vm, dm, v, d)| {
            let mut form = v.form.clone();
            form.download = if *dm { Some(d.form.clone()) } else { form.download };
            let modified = *vm || *dm;
            on_form_change.emit(ConfigForm::Video(modified, form));
        });
    }

    let render_extensions = |extensions: &Vec<String>| {
        html! {
            <Card>
            { config_field_child!(translate.t(LABEL_EXTENSIONS), {
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
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_DOWNLOAD)}</h1>
                { config_field_bool!(download_state.form, translate.t(LABEL_ORGANIZE_INTO_DIRECTORIES), organize_into_directories) }
                { config_field_optional!(download_state.form, translate.t(LABEL_DIRECTORY), directory) }
                { config_field_optional!(download_state.form, translate.t(LABEL_EPISODE_PATTERN), episode_pattern) }
                { config_field_child!(translate.t(LABEL_HEADERS), {
                    html! {
                        <div class="tp__config-view__tags">
                          <ul>
                            for (k, v) in download_state.form.headers.iter() { <li key={k.clone()}>{"- "}{k}{": "} {v}</li> }
                          </ul>
                        </div>
                    }
                })}
            </Card>
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
                    on_change={handle_headers}
                />
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

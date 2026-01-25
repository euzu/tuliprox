use crate::app::components::config::config_page::{ConfigForm, LABEL_LIBRARY_CONFIG};
use crate::app::components::config::config_view_context::ConfigViewContext;
use crate::app::components::input::Input;
use crate::app::components::select::Select;
use crate::app::components::{Card, Chip, DropDownOption, DropDownSelection, IconButton, ToggleSwitch};
use crate::app::context::ConfigContext;
use crate::{config_field, config_field_bool, config_field_child, config_field_optional,
            edit_field_bool, edit_field_list, edit_field_number, edit_field_number_u64, edit_field_text, edit_field_text_option, generate_form_reducer};
use shared::model::{LibraryConfigDto, LibraryContentType, LibraryMetadataConfigDto,
                    LibraryMetadataReadConfigDto, LibraryPlaylistConfigDto, LibraryScanDirectoryDto,
                    LibraryTmdbConfigDto, LibraryMetadataFormat};
use std::rc::Rc;
use yew::prelude::*;
use yew_i18n::use_translation;

const LABEL_ENABLED: &str = "LABEL.ENABLED";
const LABEL_SCAN_DIRECTORIES: &str = "LABEL.SCAN_DIRECTORIES";
const LABEL_SUPPORTED_EXTENSIONS: &str = "LABEL.SUPPORTED_EXTENSIONS";
const LABEL_METADATA: &str = "LABEL.METADATA";
const LABEL_METADATA_PATH: &str = "LABEL.METADATA_PATH";
const LABEL_PLAYLIST: &str = "LABEL.PLAYLIST";
const LABEL_MOVIE_CATEGORY: &str = "LABEL.MOVIE_CATEGORY";
const LABEL_SERIES_CATEGORY: &str = "LABEL.SERIES_CATEGORY";
const LABEL_TMDB: &str = "LABEL.TMDB";
const LABEL_API_KEY: &str = "LABEL.API_KEY";
const LABEL_READ_EXISTING: &str = "LABEL.READ_EXISTING";
const LABEL_KODI: &str = "LABEL.KODI";
const LABEL_JELLYFIN: &str = "LABEL.JELLYFIN";
const LABEL_PLEX: &str = "LABEL.PLEX";
const LABEL_FALLBACK_TO_FILENAME: &str = "LABEL.FALLBACK_TO_FILENAME";
const LABEL_ADD_EXTENSION: &str = "LABEL.ADD_EXTENSION";
const LABEL_RECURSIVE: &str = "LABEL.RECURSIVE";
const LABEL_PATH: &str = "LABEL.PATH";
const LABEL_CONTENT_TYPE: &str = "LABEL.CONTENT_TYPE";
const LABEL_AUTO: &str = "LABEL.AUTO";
const LABEL_MOVIE: &str = "LABEL.MOVIE";
const LABEL_SERIES: &str = "LABEL.SERIES";
const LABEL_RATE_LIMIT_MS: &str = "LABEL.RATE_LIMIT_MS";
const LABEL_CACHE_DURATION_DAYS: &str = "LABEL.CACHE_DURATION_DAYS";
const LABEL_LANGUAGE: &str = "LABEL.LANGUAGE";
const LABEL_FORMATS: &str = "LABEL.FORMATS";
const LABEL_ADD_FORMAT: &str = "LABEL.ADD_FORMAT";

const TYPE_AUTO: &str = "Auto";
const TYPE_MOVIE: &str = "Movie";
const TYPE_SERIES: &str = "Series";


generate_form_reducer!(
    state: LibraryConfigFormState { form: LibraryConfigDto },
    action_name: LibraryConfigFormAction,
    fields {
        Enabled => enabled: bool,
        SupportedExtensions => supported_extensions: Vec<String>,
        ScanDirectories => scan_directories: Vec<LibraryScanDirectoryDto>,
    }
);

generate_form_reducer!(
    state: LibraryPlaylistConfigFormState { form: LibraryPlaylistConfigDto },
    action_name: LibraryPlaylistConfigFormAction,
    fields {
        MovieCategory => movie_category: String,
        SeriesCategory => series_category: String,
    }
);

generate_form_reducer!(
    state: LibraryMetadataConfigFormState { form: LibraryMetadataConfigDto },
    action_name: LibraryMetadataConfigFormAction,
    fields {
        Path => path: String,
        FallbackToFilename => fallback_to_filename: bool,
        Formats => formats: Vec<LibraryMetadataFormat>,
    }
);

generate_form_reducer!(
    state: LibraryMetadataReadConfigFormState { form: LibraryMetadataReadConfigDto },
    action_name: LibraryMetadataReadConfigFormAction,
    fields {
        Kodi => kodi: bool,
        Jellyfin => jellyfin: bool,
        Plex => plex: bool,
    }
);

generate_form_reducer!(
    state: LibraryTmdbConfigFormState { form: LibraryTmdbConfigDto },
    action_name: LibraryTmdbConfigFormAction,
    fields {
        Enabled => enabled: bool,
        ApiKey => api_key: Option<String>,
        RateLimitMs => rate_limit_ms: u64,
        CacheDurationDays => cache_duration_days: u32,
        Language => language: String,
    }
);

#[function_component]
pub fn LibraryConfigView() -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let config_view_ctx = use_context::<ConfigViewContext>().expect("ConfigViewContext not found");

    let form_state: UseReducerHandle<LibraryConfigFormState> = use_reducer(|| {
        LibraryConfigFormState { form: LibraryConfigDto::default(), modified: false }
    });

    let playlist_state: UseReducerHandle<LibraryPlaylistConfigFormState> = use_reducer(|| {
        LibraryPlaylistConfigFormState { form: LibraryPlaylistConfigDto::default(), modified: false }
    });

    let metadata_state: UseReducerHandle<LibraryMetadataConfigFormState> = use_reducer(|| {
        LibraryMetadataConfigFormState { form: LibraryMetadataConfigDto::default(), modified: false }
    });

    let metadata_read_state: UseReducerHandle<LibraryMetadataReadConfigFormState> = use_reducer(|| {
        LibraryMetadataReadConfigFormState { form: LibraryMetadataReadConfigDto::default(), modified: false }
    });

    let tmdb_state: UseReducerHandle<LibraryTmdbConfigFormState> = use_reducer(|| {
        LibraryTmdbConfigFormState { form: LibraryTmdbConfigDto::default(), modified: false }
    });

    {
        let form_state = form_state.clone();
        let playlist_state = playlist_state.clone();
        let metadata_state = metadata_state.clone();
        let metadata_read_state = metadata_read_state.clone();
        let tmdb_state = tmdb_state.clone();

        let library_cfg = config_ctx.config.as_ref().and_then(|c| c.config.library.clone());
        use_effect_with((library_cfg, config_view_ctx.edit_mode.clone()), move |(library_cfg, _mode)| {
            if let Some(library) = library_cfg {
                form_state.dispatch(LibraryConfigFormAction::SetAll(library.clone()));
                playlist_state.dispatch(LibraryPlaylistConfigFormAction::SetAll(library.playlist.clone()));
                metadata_state.dispatch(LibraryMetadataConfigFormAction::SetAll(library.metadata.clone()));
                metadata_read_state.dispatch(LibraryMetadataReadConfigFormAction::SetAll(library.metadata.read_existing.clone()));
                tmdb_state.dispatch(LibraryTmdbConfigFormAction::SetAll(library.metadata.tmdb.clone()));
            } else {
                form_state.dispatch(LibraryConfigFormAction::SetAll(LibraryConfigDto::default()));
                playlist_state.dispatch(LibraryPlaylistConfigFormAction::SetAll(LibraryPlaylistConfigDto::default()));
                metadata_state.dispatch(LibraryMetadataConfigFormAction::SetAll(LibraryMetadataConfigDto::default()));
                metadata_read_state.dispatch(LibraryMetadataReadConfigFormAction::SetAll(LibraryMetadataReadConfigDto::default()));
                tmdb_state.dispatch(LibraryTmdbConfigFormAction::SetAll(LibraryTmdbConfigDto::default()));
            }
            || ()
        });
    }

    {
        let on_form_change = config_view_ctx.on_form_change.clone();
        let form_state = form_state.clone();
        let playlist_state = playlist_state.clone();
        let metadata_state = metadata_state.clone();
        let metadata_read_state = metadata_read_state.clone();
        let tmdb_state = tmdb_state.clone();

        use_effect_with(
            (
                form_state,
                playlist_state,
                metadata_state,
                metadata_read_state,
                tmdb_state,
            ),
            move |(form, playlist, metadata, metadata_read, tmdb)| {
                let mut new_form = form.form.clone();
                new_form.playlist = playlist.form.clone();
                new_form.metadata = metadata.form.clone();
                new_form.metadata.read_existing = metadata_read.form.clone();
                new_form.metadata.tmdb = tmdb.form.clone();

                let modified = form.modified
                    || playlist.modified
                    || metadata.modified
                    || metadata_read.modified
                    || tmdb.modified;

                on_form_change.emit(ConfigForm::Library(modified, new_form));
            },
        );
    }


    let get_content_type_options = {
        let translate = translate.clone();
        Callback::from(move |content_type: LibraryContentType|
            vec![
                DropDownOption { id: TYPE_AUTO.to_string(), label: html! { translate.t(LABEL_AUTO) }, selected: content_type == LibraryContentType::Auto },
                DropDownOption { id: TYPE_MOVIE.to_string(), label: html! { translate.t(LABEL_MOVIE) }, selected: content_type == LibraryContentType::Movie },
                DropDownOption { id: TYPE_SERIES.to_string(), label: html! { translate.t(LABEL_SERIES) }, selected: content_type == LibraryContentType::Series },
            ]
        )
    };

    let handle_path_change = {
        let form = form_state.clone();
        Callback::from(move |(idx, value): (usize, String)| {
            let mut scan_directories = form.form.scan_directories.clone();
            if idx >= scan_directories.len() {
               return;
            }
            scan_directories[idx].path = value;
            form.dispatch(LibraryConfigFormAction::ScanDirectories(scan_directories));
        })
    };

    let handle_enabled_change = {
        let form = form_state.clone();
        Callback::from(move |(idx, value): (usize, bool)| {
            let mut scan_directories = form.form.scan_directories.clone();
            if idx >= scan_directories.len() {
                return;
            }
            scan_directories[idx].enabled = value;
            form.dispatch(LibraryConfigFormAction::ScanDirectories(scan_directories));
        })
    };

    let handle_recursive_change = {
        let form = form_state.clone();
        Callback::from(move |(idx, value): (usize, bool)| {
            let mut scan_directories = form.form.scan_directories.clone();
            if idx >= scan_directories.len() {
                return;
            }
            scan_directories[idx].recursive = value;
            form.dispatch(LibraryConfigFormAction::ScanDirectories(scan_directories));
        })
    };

    let handle_type_change = {
        let form = form_state.clone();
        Callback::from(move |(idx, selection): (usize, DropDownSelection)| {
            if let DropDownSelection::Single(val) = selection {
                let content_type = match val.as_str() {
                    TYPE_MOVIE => LibraryContentType::Movie,
                    TYPE_SERIES => LibraryContentType::Series,
                    _ => LibraryContentType::Auto,
                };
                let mut scan_directories = form.form.scan_directories.clone();
                if idx >= scan_directories.len() {
                    return;
                }
                scan_directories[idx].content_type = content_type;
                form.dispatch(LibraryConfigFormAction::ScanDirectories(scan_directories));
            }
        })
    };

    let handle_remove_directory = {
        let form_state = form_state.clone();
        Callback::from(move |idx: usize| {
            let mut current_list = form_state.form.scan_directories.clone();
            if idx >= current_list.len() {
               return;
            }
            current_list.remove(idx);
            form_state.dispatch(LibraryConfigFormAction::ScanDirectories(current_list));
        })
    };

    let handle_add_directory = {
        let form_state = form_state.clone();

        Callback::from(move |_| {
            let new_dir = LibraryScanDirectoryDto::default();
            let mut current_list = form_state.form.scan_directories.clone();
            current_list.push(new_dir);
            form_state.dispatch(LibraryConfigFormAction::ScanDirectories(current_list));
        })
    };

    let render_extensions = |extensions: &Vec<String>| html! {
        <Card class="tp__config-view__card">
        { config_field_child!(translate.t(LABEL_SUPPORTED_EXTENSIONS), {
           html! {
             <div class="tp__config-view__tags">
             { html! { for extensions.iter().map(|t| html! { <Chip label={t.clone()} /> }) } }
             </div>
            }})}
        </Card>
    };

    let render_scan_directories_view = |directories: &Vec<LibraryScanDirectoryDto>| html! {
         <>
            <h1>{translate.t(LABEL_SCAN_DIRECTORIES)}</h1>
            <ul>
            { for directories.iter().map(|dir| html! {
                <li class="tp__library-config-view__list"><span class="tp__library-config-view__list-item">{&dir.path}</span>
                    <span>{format!("({}, {}, {}: {})",
                        if dir.enabled { translate.t("LABEL.ENABLED") } else {translate.t("LABEL.DISABLED")},
                        if dir.recursive { translate.t("LABEL.RECURSIVE") } else {translate.t("LABEL.NON_RECURSIVE")},
                        translate.t(LABEL_CONTENT_TYPE),
                        match dir.content_type {
                            LibraryContentType::Auto => translate.t(LABEL_AUTO),
                            LibraryContentType::Movie => translate.t(LABEL_MOVIE),
                            LibraryContentType::Series => translate.t(LABEL_SERIES),
                        })}</span>
                </li>
            }) }
            </ul>
         </>
    };
    let render_scan_directories_edit = || {
        html! {
           <Card class="tp__config-view__card">
               <h1>{translate.t(LABEL_SCAN_DIRECTORIES)}</h1>
               <IconButton name="Add" icon="Add" onclick={handle_add_directory} />
                <table class="tp__config-view__table tp__table__table">
                   <thead>
                       <tr>
                            <th style="width: 50px;"></th>
                            <th>{translate.t(LABEL_PATH)}</th>
                            <th>{translate.t(LABEL_CONTENT_TYPE)}</th>
                            <th>{translate.t(LABEL_ENABLED)}</th>
                            <th>{translate.t(LABEL_RECURSIVE)}</th>
                       </tr>
                   </thead>
                   <tbody>
                   { for form_state.form.scan_directories.iter().enumerate().map(|(idx, dir)| {
                       let on_remove = handle_remove_directory.clone();
                       let path_change = handle_path_change.clone();
                       let enabled_change = handle_enabled_change.clone();
                       let recursive_change = handle_recursive_change.clone();
                       let type_change = handle_type_change.clone();
                       let options = Rc::new(get_content_type_options.emit(dir.content_type));
                       html! {
                           <tr>
                               <td>
                                   <IconButton name="Delete" icon="Delete" onclick={Callback::from(move |_| on_remove.emit(idx))} />
                               </td>
                               <td><Input name="path" value={dir.path.clone()} on_change={Some(Callback::from(move |value| path_change.emit((idx, value))))} /></td>
                               <td>
                                  <Select name="type" options={options} on_select={Callback::from(move |(_, selection)| type_change.emit((idx, selection)))} />
                               </td>
                               <td>
                                  <ToggleSwitch value={dir.enabled} readonly={false} on_change={Callback::from(move |value| enabled_change.emit((idx, value)))} />
                               </td>
                               <td>
                                  <ToggleSwitch value={dir.recursive} readonly={false} on_change={Callback::from(move |value| recursive_change.emit((idx, value)))} />
                               </td>
                           </tr>
                       }
                   })}
                   </tbody>
               </table>
           </Card>
        }
    };

    let render_view_mode = || {
        let metadata = &metadata_state.form;
        let playlist = &playlist_state.form;
        let tmdb = &tmdb_state.form;
        let read_existing = &metadata_read_state.form;

        html! {
            <>
            <div class="tp__library-config-view__header">
                { config_field_bool!(form_state.form, translate.t(LABEL_ENABLED), enabled) }
                { render_scan_directories_view(&form_state.form.scan_directories) }
            </div>
            <div class="tp__library-config-view__body tp__config-view-page__body">
                { render_extensions(&form_state.form.supported_extensions) }
                <Card class="tp__config-view__card">
                    <h1>{translate.t(LABEL_METADATA)}</h1>
                    { config_field!(metadata, translate.t(LABEL_METADATA_PATH), path) }
                    { config_field_bool!(metadata, translate.t(LABEL_FALLBACK_TO_FILENAME), fallback_to_filename) }
                    { config_field_child!(translate.t(LABEL_FORMATS), {
                        html! {
                            <div class="tp__config-view__tags">
                                { for metadata.formats.iter().map(|f| html! { <Chip label={format!("{f:?}")} /> }) }
                            </div>
                        }
                    }) }
                </Card>

                <Card class="tp__config-view__card">
                    <h1>{translate.t(LABEL_READ_EXISTING)}</h1>
                    { config_field_bool!(read_existing, translate.t(LABEL_KODI), kodi) }
                    { config_field_bool!(read_existing, translate.t(LABEL_JELLYFIN), jellyfin) }
                    { config_field_bool!(read_existing, translate.t(LABEL_PLEX), plex) }
                </Card>

                <Card class="tp__config-view__card">
                    <h1>{translate.t(LABEL_TMDB)}</h1>
                    { config_field_bool!(tmdb, translate.t(LABEL_ENABLED), enabled) }
                    { config_field_optional!(tmdb, translate.t(LABEL_API_KEY), api_key) }
                    { config_field!(tmdb, translate.t(LABEL_RATE_LIMIT_MS), rate_limit_ms) }
                    { config_field!(tmdb, translate.t(LABEL_CACHE_DURATION_DAYS), cache_duration_days) }
                    { config_field!(tmdb, translate.t(LABEL_LANGUAGE), language) }
                </Card>

                <Card class="tp__config-view__card">
                    <h1>{translate.t(LABEL_PLAYLIST)}</h1>
                     { config_field!(playlist, translate.t(LABEL_MOVIE_CATEGORY), movie_category) }
                     { config_field!(playlist, translate.t(LABEL_SERIES_CATEGORY), series_category) }
                </Card>
            </div>
            </>
        }
    };

    let render_edit_mode = || html! {
        <>
         <div class="tp__library-config-view__header">
            { edit_field_bool!(form_state, translate.t(LABEL_ENABLED), enabled, LibraryConfigFormAction::Enabled) }
            { render_scan_directories_edit() }
         </div>
         <div class="tp__library-config-view__body tp__config-view-page__body">
             <Card class="tp__config-view__card">
                { edit_field_list!(form_state, translate.t(LABEL_SUPPORTED_EXTENSIONS), supported_extensions, LibraryConfigFormAction::SupportedExtensions, translate.t(LABEL_ADD_EXTENSION)) }
            </Card>

             <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_METADATA)}</h1>
                { edit_field_text!(metadata_state, translate.t(LABEL_METADATA_PATH), path, LibraryMetadataConfigFormAction::Path) }
                { edit_field_bool!(metadata_state, translate.t(LABEL_FALLBACK_TO_FILENAME), fallback_to_filename, LibraryMetadataConfigFormAction::FallbackToFilename) }
                { config_field_child!(translate.t(LABEL_FORMATS), {
                    let metadata_state = metadata_state.clone();
                    let formats = metadata_state.form.formats.clone();
                    html! {
                        <div class="tp__config-view__tags">
                             { for formats.iter().map(|f| {
                                 let metadata_state = metadata_state.clone();
                                 let f = *f;
                                 html! { <Chip label={format!("{f:?}")} on_remove={Callback::from(move |_| {
                                     let mut updated = metadata_state.form.formats.clone();
                                     updated.retain(|&x| x != f);
                                     metadata_state.dispatch(LibraryMetadataConfigFormAction::Formats(updated));
                                 })} /> }
                             }) }
                             if !formats.contains(&LibraryMetadataFormat::Nfo) {
                                 <IconButton name="Add" icon="Add" hint={translate.t(LABEL_ADD_FORMAT)} onclick={Callback::from(move |_| {
                                     let mut updated = metadata_state.form.formats.clone();
                                     updated.push(LibraryMetadataFormat::Nfo);
                                     metadata_state.dispatch(LibraryMetadataConfigFormAction::Formats(updated));
                                 })} />
                             }
                        </div>
                    }
                }) }
            </Card>

             <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_READ_EXISTING)}</h1>
                { edit_field_bool!(metadata_read_state, translate.t(LABEL_KODI), kodi, LibraryMetadataReadConfigFormAction::Kodi) }
                { edit_field_bool!(metadata_read_state, translate.t(LABEL_JELLYFIN), jellyfin, LibraryMetadataReadConfigFormAction::Jellyfin) }
                { edit_field_bool!(metadata_read_state, translate.t(LABEL_PLEX), plex, LibraryMetadataReadConfigFormAction::Plex) }
            </Card>

             <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_TMDB)}</h1>
                { edit_field_bool!(tmdb_state, translate.t(LABEL_ENABLED), enabled, LibraryTmdbConfigFormAction::Enabled) }
                { edit_field_text_option!(tmdb_state, translate.t(LABEL_API_KEY), api_key, LibraryTmdbConfigFormAction::ApiKey, true) }
                { edit_field_number_u64!(tmdb_state, translate.t(LABEL_RATE_LIMIT_MS), rate_limit_ms, LibraryTmdbConfigFormAction::RateLimitMs) }
                { edit_field_number!(tmdb_state, translate.t(LABEL_CACHE_DURATION_DAYS), cache_duration_days, LibraryTmdbConfigFormAction::CacheDurationDays) }
                { edit_field_text!(tmdb_state, translate.t(LABEL_LANGUAGE), language, LibraryTmdbConfigFormAction::Language) }
             </Card>

             <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_PLAYLIST)}</h1>
                 { edit_field_text!(playlist_state, translate.t(LABEL_MOVIE_CATEGORY), movie_category, LibraryPlaylistConfigFormAction::MovieCategory) }
                 { edit_field_text!(playlist_state, translate.t(LABEL_SERIES_CATEGORY), series_category, LibraryPlaylistConfigFormAction::SeriesCategory) }
             </Card>
        </div>
        </>
    };

    html! {
        <div class="tp__library-config-view tp__config-view-page">
            <div class="tp__config-view-page__title">{translate.t(LABEL_LIBRARY_CONFIG)}</div>
            { if *config_view_ctx.edit_mode { render_edit_mode() } else { render_view_mode() } }
        </div>
    }
}

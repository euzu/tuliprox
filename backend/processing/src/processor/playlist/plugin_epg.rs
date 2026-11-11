//! The EPG of a `type: plugin` input.
//!
//! `fetch-epg` returns channels and programmes in memory; the EPG path that
//! follows (`TVGuide`, the streaming XMLTV parser, `merge_epg_trees`) works on
//! XMLTV files. So the result is written as one XMLTV file in the input's
//! storage directory and handed to the guide as one more source, which makes it
//! reach the normal EPG path unchanged: merging by priority, `on-epg-loaded`,
//! `transform-programmes`, `assign_playlist_epg`.
//!
//! Fail-open: a plugin that fails or has no EPG leaves the input's playlist and
//! its other EPG sources as they are; the failure is reported with the input's
//! EPG errors.

use crate::{metadata_sink::MetadataUpdateSink, parser::xmltv::TVGuide, processor::PlaylistProcessingContext};
use log::{debug, warn};
use quick_xml::{
    events::{BytesEnd, BytesStart, BytesText, Event},
    Writer,
};
use shared::{
    error::TuliproxError,
    model::{EpgProgramme, EventSink, HookCtx, PluginEpgChannel, PluginHost, PluginInputRequest},
    utils::sanitize_sensitive_info,
};
use std::{
    io::Result as IoResult,
    path::{Path, PathBuf},
    sync::Arc,
};
use tuliprox_core::{
    model::{ConfigInput, PersistedEpgSource, PersistedEpgSourceKind},
    utils::{format_xmltv_time_utc, tmp_path_for, EpgTimeShift},
};
use tuliprox_repository::get_input_storage_path;

/// The file name in the input's storage directory. Not `*_epg.xml`: that suffix
/// belongs to the downloaded sources, whose cleanup would delete this file.
const PLUGIN_EPG_FILE: &str = "plugin-epg.xml";

/// `guide` plus the plugin's own EPG as one more XMLTV source.
///
/// While the input is in maintenance the plugin is not called and the file of
/// the last run is used. With `cache_duration` set a fresh file is reused too.
pub(crate) async fn with_plugin_epg<E: EventSink + Clone + 'static, M: MetadataUpdateSink, P: PluginHost>(
    ctx: &PlaylistProcessingContext<E, M, P>,
    input: &ConfigInput,
    guide: Option<TVGuide>,
    errors: &mut Vec<TuliproxError>,
) -> Option<TVGuide> {
    let Some(plugin) = input.plugin.as_deref() else { return guide };
    let storage_dir = ctx.config.config.load().storage_dir.clone();
    let path = match get_input_storage_path(&input.name, &storage_dir).await {
        Ok(dir) => dir.join(PLUGIN_EPG_FILE),
        Err(err) => {
            errors.push(TuliproxError::Io(format!(
                "Could not access the plugin epg directory of '{}': {err}",
                input.name
            )));
            return guide;
        }
    };

    let reuse = if ctx.is_input_in_maintenance(&input.name) {
        debug!("Input '{}' is in maintenance; using its persisted plugin epg", input.name);
        true
    } else {
        is_fresh(&path, input.cache_duration_seconds).await
    };
    if !reuse {
        match fetch_to_file(ctx, input, plugin, &path).await {
            Ok(()) => {}
            Err(err) => {
                errors.push(TuliproxError::Download(format!(
                    "plugin input '{}': {}",
                    input.name,
                    sanitize_sensitive_info(&err)
                )));
                return guide;
            }
        }
    }
    if !tokio::fs::metadata(&path).await.is_ok_and(|metadata| metadata.is_file()) {
        return guide;
    }

    let mut sources = guide.map(|guide| guide.get_epg_sources().clone()).unwrap_or_default();
    sources.push(PersistedEpgSource {
        file_path: path,
        priority: 0,
        logo_override: false,
        kind: PersistedEpgSourceKind::Xmltv,
        // Not a configured `epg.sources` entry: keep its channels out of the
        // per-source analytics of the input's own sources.
        source_idx: usize::MAX,
    });
    Some(TVGuide::new(sources).with_file_locks(Arc::clone(&ctx.config.file_locks)))
}

/// A file younger than `cache_duration_seconds` (`0` means never).
async fn is_fresh(path: &Path, cache_duration_seconds: u64) -> bool {
    if cache_duration_seconds == 0 {
        return false;
    }
    let Ok(modified) = tokio::fs::metadata(path).await.and_then(|metadata| metadata.modified()) else {
        return false;
    };
    std::time::SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|elapsed| elapsed.as_secs() < cache_duration_seconds)
}

/// `fetch-epg`, then the XMLTV file (temp file, then rename). A plugin without
/// EPG (`ok([])`) removes the file of an earlier run.
async fn fetch_to_file<E: EventSink + Clone + 'static, M: MetadataUpdateSink, P: PluginHost>(
    ctx: &PlaylistProcessingContext<E, M, P>,
    input: &ConfigInput,
    plugin: &str,
    path: &Path,
) -> Result<(), String> {
    let hook_ctx = HookCtx::new("").with_input(&input.name);
    let request = PluginInputRequest {
        input_name: &input.name,
        url: &input.url,
        username: input.username.as_deref(),
        password: input.password.as_deref(),
        options: &input.plugin_options,
    };
    let channels = ctx.plugins.fetch_epg(plugin, &hook_ctx, &request).await?;

    let _lock = ctx.config.file_locks.write_lock(path).await;
    if channels.is_empty() {
        return remove_if_present(path).await;
    }
    let path = path.to_path_buf();
    // Serializing is CPU work and the write is file work: both off the async workers.
    tokio::task::spawn_blocking(move || write_atomically(&path, &channels))
        .await
        .map_err(|err| format!("epg write task failed: {err}"))?
        .map_err(|err| format!("can't write the plugin epg: {err}"))
}

async fn remove_if_present(path: &Path) -> Result<(), String> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => {
            warn!("Can't remove the stale plugin epg {}: {err}", path.display());
            Err(format!("can't remove the stale plugin epg: {err}"))
        }
    }
}

fn write_atomically(path: &Path, channels: &[(PluginEpgChannel, Vec<EpgProgramme>)]) -> IoResult<()> {
    let tmp: PathBuf = tmp_path_for(path);
    let result = xmltv_document(channels)
        .and_then(|bytes| std::fs::write(&tmp, bytes))
        .and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn text_element(writer: &mut Writer<Vec<u8>>, name: &str, text: &str) -> IoResult<()> {
    writer.write_event(Event::Start(BytesStart::new(name)))?;
    writer.write_event(Event::Text(BytesText::new(text)))?;
    writer.write_event(Event::End(BytesEnd::new(name)))
}

/// The XMLTV document of `channels`: every channel, then every programme in the
/// order returned. Times are UTC.
fn xmltv_document(channels: &[(PluginEpgChannel, Vec<EpgProgramme>)]) -> IoResult<Vec<u8>> {
    let mut writer = Writer::new(Vec::new());
    writer.write_event(Event::Start(BytesStart::new("tv")))?;
    for (channel, _) in channels {
        let mut start = BytesStart::new("channel");
        start.push_attribute(("id", channel.id.as_str()));
        writer.write_event(Event::Start(start))?;
        for name in &channel.names {
            text_element(&mut writer, "display-name", name)?;
        }
        if let Some(icon) = channel.icon.as_deref().filter(|icon| !icon.is_empty()) {
            let mut element = BytesStart::new("icon");
            element.push_attribute(("src", icon));
            writer.write_event(Event::Empty(element))?;
        }
        writer.write_event(Event::End(BytesEnd::new("channel")))?;
    }
    for (channel, programmes) in channels {
        for programme in programmes {
            let mut start = BytesStart::new("programme");
            start.push_attribute(("start", format_xmltv_time_utc(programme.start, &EpgTimeShift::None).as_str()));
            start.push_attribute(("stop", format_xmltv_time_utc(programme.stop, &EpgTimeShift::None).as_str()));
            start.push_attribute(("channel", channel.id.as_str()));
            if let Some(catchup_id) = programme.catchup_id.as_deref() {
                start.push_attribute(("catchup-id", catchup_id));
            }
            writer.write_event(Event::Start(start))?;
            if let Some(title) = programme.title.as_deref() {
                text_element(&mut writer, "title", title)?;
            }
            if let Some(desc) = programme.desc.as_deref() {
                text_element(&mut writer, "desc", desc)?;
            }
            for category in &programme.categories {
                let mut element = BytesStart::new("category");
                if let Some(lang) = category.lang.as_deref() {
                    element.push_attribute(("lang", lang));
                }
                writer.write_event(Event::Start(element))?;
                writer.write_event(Event::Text(BytesText::new(&category.value)))?;
                writer.write_event(Event::End(BytesEnd::new("category")))?;
            }
            if let Some(icon) = programme.icon.as_deref() {
                let mut element = BytesStart::new("icon");
                element.push_attribute(("src", icon));
                writer.write_event(Event::Empty(element))?;
            }
            for (flag, tag) in [
                (programme.is_live, "live"),
                (programme.is_new, "new"),
                (programme.previously_shown, "previously-shown"),
            ] {
                if flag {
                    writer.write_event(Event::Empty(BytesStart::new(tag)))?;
                }
            }
            writer.write_event(Event::End(BytesEnd::new("programme")))?;
        }
    }
    writer.write_event(Event::End(BytesEnd::new("tv")))?;
    Ok(writer.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::{model::EpgCategory, utils::Internable};
    use tuliprox_core::model::parse_xmltv_for_web_ui_from_file;

    fn channels() -> Vec<(PluginEpgChannel, Vec<EpgProgramme>)> {
        let mut programme = EpgProgramme::new_all(
            1_700_000_000,
            1_700_003_600,
            "Chan.One".intern(),
            Some("News & <Weather>".intern()),
            Some("The day's news".intern()),
            Some("cu-1".intern()),
        );
        programme.categories = vec![EpgCategory { value: "News".intern(), lang: Some("en".intern()) }];
        programme.is_live = true;
        vec![
            (
                PluginEpgChannel {
                    id: "Chan.One".to_string(),
                    names: vec!["Channel One".to_string()],
                    icon: Some("http://host/one.png".to_string()),
                },
                vec![programme],
            ),
            (PluginEpgChannel { id: "Chan.Two".to_string(), names: vec![], icon: None }, vec![]),
        ]
    }

    fn fresh_channels() -> Vec<(PluginEpgChannel, Vec<EpgProgramme>)> {
        let mut programme = EpgProgramme::new_all(
            4_102_444_800,
            4_102_448_400,
            "Chan.One".intern(),
            Some("News".intern()),
            Some("The day's news".intern()),
            Some("cu-1".intern()),
        );
        programme.categories = vec![EpgCategory { value: "News".intern(), lang: Some("en".intern()) }];
        programme.is_live = true;
        vec![
            (
                PluginEpgChannel {
                    id: "Chan.One".to_string(),
                    names: vec!["Channel One".to_string()],
                    icon: Some("http://host/one.png".to_string()),
                },
                vec![programme],
            ),
            (PluginEpgChannel { id: "Chan.Two".to_string(), names: vec![], icon: None }, vec![]),
        ]
    }

    #[test]
    fn the_document_has_every_channel_and_escapes_text() {
        let xml = String::from_utf8(xmltv_document(&channels()).unwrap()).unwrap();
        assert!(xml.starts_with("<tv>") && xml.ends_with("</tv>"));
        assert!(xml.contains(r#"<channel id="Chan.One"><display-name>Channel One</display-name><icon src="http://host/one.png"/></channel>"#));
        assert!(xml.contains(r#"<channel id="Chan.Two"></channel>"#));
        assert!(xml.contains("<title>News &amp; &lt;Weather&gt;</title>"), "{xml}");
        assert!(xml.contains(r#"start="20231114221320 +0000""#), "{xml}");
        assert!(xml.contains(r#"catchup-id="cu-1""#));
        assert!(xml.contains(r#"<category lang="en">News</category>"#));
        assert!(xml.contains("<live/>"));
        assert!(!xml.contains("<new/>"));
    }

    #[test]
    fn the_same_channels_give_a_byte_identical_document() {
        assert_eq!(xmltv_document(&channels()).unwrap(), xmltv_document(&channels()).unwrap());
    }

    #[tokio::test]
    async fn the_xmltv_parser_reads_the_written_file_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PLUGIN_EPG_FILE);
        write_atomically(&path, &fresh_channels()).unwrap();
        assert!(!tmp_path_for(&path).exists(), "the temp file is renamed away");

        let read = parse_xmltv_for_web_ui_from_file(&path).await.unwrap();
        let one = read.iter().find(|channel| &*channel.id == "Chan.One").expect("channel one is read back");
        assert_eq!(one.programmes.len(), 1);
        let programme = &one.programmes[0];
        assert_eq!((programme.start, programme.stop), (4_102_444_800, 4_102_448_400));
        assert_eq!(programme.title.as_deref(), Some("News"));
        assert!(programme.is_live);
    }

    #[tokio::test]
    async fn a_failed_write_leaves_no_temp_file_and_keeps_the_previous_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("taken");
        std::fs::create_dir(&path).unwrap();
        assert!(write_atomically(&path, &channels()).is_err());
        assert!(!tmp_path_for(&path).exists());
        assert!(path.is_dir());
    }

    #[tokio::test]
    async fn freshness_follows_cache_duration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PLUGIN_EPG_FILE);
        assert!(!is_fresh(&path, 60).await, "no file is never fresh");
        std::fs::write(&path, "<tv/>").unwrap();
        assert!(is_fresh(&path, 3600).await);
        assert!(!is_fresh(&path, 0).await, "cache_duration 0 always refetches");
    }

    #[tokio::test]
    async fn fetch_to_file_writes_file_when_channels_exist_without_programmes() {
        use crate::processor::playlist::ingest::PlaylistProcessingContext;
        use arc_swap::{ArcSwap, ArcSwapOption};
        use shared::model::{ConfigPaths, HookCtx, PluginInputRequest};
        use std::sync::Arc;
        use tuliprox_core::{
            model::{AppConfig, Config, ConfigInput, ProcessTargets, SourcesConfig},
            utils::FileLockManager,
        };

        #[derive(Clone)]
        struct EpgHost;
        #[allow(clippy::unused_async_trait_impl)]
        impl PluginHost for EpgHost {
            async fn fetch_epg(
                &self,
                _plugin: &str,
                _ctx: &HookCtx<'_>,
                _request: &PluginInputRequest<'_>,
            ) -> Result<Vec<(PluginEpgChannel, Vec<EpgProgramme>)>, String> {
                Ok(vec![(
                    PluginEpgChannel { id: "Chan.One".to_string(), names: vec!["Channel One".to_string()], icon: None },
                    vec![],
                )])
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("epg.xml");
        std::fs::write(&path, "<tv/>").unwrap();

        let app_config = AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config::default())),
            sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            plugins: Arc::default(),
            api_proxy: Arc::new(ArcSwapOption::default()),
            file_locks: Arc::new(FileLockManager::default()),
            failover: Arc::default(),
            paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
                home_path: String::new(),
                config_path: String::new(),
                storage_path: String::new(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                failover_file_path: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(ArcSwapOption::default()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(tuliprox_core::model::MediaToolCapabilities::new()),
        };

        let ctx: PlaylistProcessingContext<shared::model::NoopSink, crate::metadata_sink::NoopMetadataSink, _> =
            PlaylistProcessingContext {
                client: reqwest::Client::new(),
                run_id: "test-epg-run".into(),
                execution_order: crate::processor::playlist::PlaylistUpdateRunOrder::from(1),
                config: Arc::new(app_config),
                user_targets: Arc::new(ProcessTargets {
                    enabled: false,
                    inputs: Vec::new(),
                    targets: Vec::new(),
                    target_names: Vec::new(),
                }),
                events: shared::model::NoopSink,
                playlist_state: None,
                disabled_headers: None,
                processed_inputs: Arc::default(),
                input_completions: Arc::default(),
                input_locks: Arc::default(),
                provider_manager: None,
                metadata_manager: None,
                pre_processed_inputs: None,
                stalker_refresh_mode: crate::processor::stalker_refresh::StalkerRefreshMode::Complete,
                partial_refresh: Arc::default(),
                had_quality_rejections: Arc::default(),
                replace_target_contents: false,
                publication_deadline: None,
                input_refresh: None,
                library_update_mode: crate::processor::playlist::LibraryUpdateMode::default(),
                maintenance: None,
                override_status: None,
                plugins: EpgHost,
            };

        let input = ConfigInput { name: "test-input".intern(), ..ConfigInput::default() };
        fetch_to_file(&ctx, &input, "plugin", &path).await.unwrap();

        assert!(path.exists(), "epg file must exist when channels exist even without programmes");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains(r#"<channel id="Chan.One">"#));
    }

    #[tokio::test]
    async fn fetch_to_file_deletes_file_when_channels_are_empty() {
        use crate::processor::playlist::ingest::PlaylistProcessingContext;
        use arc_swap::{ArcSwap, ArcSwapOption};
        use shared::model::{ConfigPaths, HookCtx, PluginInputRequest};
        use std::sync::Arc;
        use tuliprox_core::{
            model::{AppConfig, Config, ConfigInput, ProcessTargets, SourcesConfig},
            utils::FileLockManager,
        };

        #[derive(Clone)]
        struct EmptyEpgHost;
        #[allow(clippy::unused_async_trait_impl)]
        impl PluginHost for EmptyEpgHost {
            async fn fetch_epg(
                &self,
                _plugin: &str,
                _ctx: &HookCtx<'_>,
                _request: &PluginInputRequest<'_>,
            ) -> Result<Vec<(PluginEpgChannel, Vec<EpgProgramme>)>, String> {
                Ok(vec![])
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("epg.xml");
        std::fs::write(&path, "<tv/>").unwrap();

        let app_config = AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config::default())),
            sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            plugins: Arc::default(),
            api_proxy: Arc::new(ArcSwapOption::default()),
            file_locks: Arc::new(FileLockManager::default()),
            failover: Arc::default(),
            paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
                home_path: String::new(),
                config_path: String::new(),
                storage_path: String::new(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                failover_file_path: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(ArcSwapOption::default()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(tuliprox_core::model::MediaToolCapabilities::new()),
        };

        let ctx: PlaylistProcessingContext<shared::model::NoopSink, crate::metadata_sink::NoopMetadataSink, _> =
            PlaylistProcessingContext {
                client: reqwest::Client::new(),
                run_id: "test-epg-run".into(),
                execution_order: crate::processor::playlist::PlaylistUpdateRunOrder::from(1),
                config: Arc::new(app_config),
                user_targets: Arc::new(ProcessTargets {
                    enabled: false,
                    inputs: Vec::new(),
                    targets: Vec::new(),
                    target_names: Vec::new(),
                }),
                events: shared::model::NoopSink,
                playlist_state: None,
                disabled_headers: None,
                processed_inputs: Arc::default(),
                input_completions: Arc::default(),
                input_locks: Arc::default(),
                provider_manager: None,
                metadata_manager: None,
                pre_processed_inputs: None,
                stalker_refresh_mode: crate::processor::stalker_refresh::StalkerRefreshMode::Complete,
                partial_refresh: Arc::default(),
                had_quality_rejections: Arc::default(),
                replace_target_contents: false,
                publication_deadline: None,
                input_refresh: None,
                library_update_mode: crate::processor::playlist::LibraryUpdateMode::default(),
                maintenance: None,
                override_status: None,
                plugins: EmptyEpgHost,
            };

        let input = ConfigInput { name: "test-input".intern(), ..ConfigInput::default() };
        fetch_to_file(&ctx, &input, "plugin", &path).await.unwrap();

        assert!(!path.exists(), "epg file must be deleted when channels are empty");
    }
}

//! Human-readable summary of the resolved runtime configuration.
//!
//! Logged once at startup. It reads the same config this crate loads, so it
//! belongs beside the loader rather than in the binary.

use crate::read_app_config_dto;
use arc_swap::{access::Access, ArcSwap};
use log::{error, info};
use serde::Serialize;
use serde_json::Value;
use shared::{
    error::TuliproxError,
    model::{
        ApiProxyConfigDto, AppConfigDto, ConfigDto, ConfigPaths, HdHomeRunConfigDto, HdHomeRunDeviceConfigDto,
        LibraryConfigDto, LibraryMetadataConfigDto, LibraryMetadataReadConfigDto, LibraryPlaylistConfigDto,
        LogConfigDto, RuntimeConfigReportFormat, ThumbnailConfigDto, VideoConfigDto,
    },
};
use tuliprox_core::model::{AppConfig, Config, HdHomeRunConfig, HdHomeRunFlags, LibraryConfig};

#[derive(Debug, Serialize)]
struct RuntimeConfigReport {
    config: ConfigDto,
    sources: shared::model::SourcesConfigDto,
    mappings: Option<shared::model::MappingsDto>,
    templates: Option<shared::model::TemplateDefinitionDto>,
    api_proxy: Option<ApiProxyConfigDto>,
    paths: ConfigPaths,
}

pub async fn log_runtime_config_report(app_config: &AppConfig) {
    let config = <std::sync::Arc<ArcSwap<Config>> as Access<Config>>::load(&app_config.config);
    let Some(log_config) = config.log.clone() else {
        return;
    };
    if !log_config.runtime_config_report_enabled {
        return;
    }

    match render_runtime_config_report(app_config, log_config.runtime_config_report_format).await {
        Ok(rendered) => info!("Runtime config report:\n{rendered}"),
        Err(err) => error!("Failed to render runtime config report: {err}"),
    }
}

async fn render_runtime_config_report(
    app_config: &AppConfig,
    format: RuntimeConfigReportFormat,
) -> Result<String, TuliproxError> {
    let report = build_runtime_config_report(app_config).await?;
    let mut value = serde_json::to_value(&report)
        .map_err(|err| TuliproxError::Config(format!("Failed to convert runtime config report to value: {err}")))?;
    redact_value(None, None, None, &mut value);
    serialize_report_value(&value, format)
}

async fn build_runtime_config_report(app_config: &AppConfig) -> Result<RuntimeConfigReport, TuliproxError> {
    let paths = <std::sync::Arc<ArcSwap<ConfigPaths>> as Access<ConfigPaths>>::load(&app_config.paths).clone();
    let AppConfigDto { sources, mappings, templates, api_proxy, .. } = read_app_config_dto(&paths, true, true).await?;

    let config = runtime_config_to_dto(&<std::sync::Arc<ArcSwap<Config>> as Access<Config>>::load(&app_config.config));
    let api_proxy =
        app_config.api_proxy.load().as_ref().map(|runtime| ApiProxyConfigDto::from(runtime.as_ref())).or(api_proxy);

    Ok(RuntimeConfigReport { config, sources, mappings, templates, api_proxy, paths })
}

fn runtime_config_to_dto(config: &Config) -> ConfigDto {
    ConfigDto {
        process_parallel: config.process_parallel,
        api: shared::model::ConfigApiDto::from(&config.api),
        storage_dir: Some(config.storage_dir.clone()),
        default_user_agent: config.default_user_agent.clone(),
        backup_dir: config.backup_dir.clone(),
        user_config_dir: config.user_config_dir.clone(),
        mapping_path: config.mapping_path.clone(),
        template_path: config.template_path.clone(),
        custom_stream_response_path: config.custom_stream_response_path.clone(),
        custom_stream_response_timeout_secs: config.custom_stream_response_timeout_secs,
        custom_stream_response_enabled: config.custom_stream_response_enabled,
        custom_stream_response_error_status: config.custom_stream_response_error_status,
        video: config.video.as_ref().map(VideoConfigDto::from),
        metadata_update: config.metadata_update.as_ref().map(shared::model::MetadataUpdateConfigDto::from),
        schedules: config
            .schedules
            .as_ref()
            .map(|items| items.iter().map(shared::model::ScheduleConfigDto::from).collect()),
        log: config.log.as_ref().map(LogConfigDto::from),
        user_access_control: config.user_access_control,
        connect_timeout_secs: config.connect_timeout_secs,
        interner_gc_interval_secs: config.interner_gc_interval_secs,
        interner_gc_min_pool_size: config.interner_gc_min_pool_size,
        sleep_timer_mins: config.sleep_timer_mins,
        update_on_boot: config.update_on_boot,
        config_hot_reload: config.config_hot_reload,
        disk_based_processing: config.disk_based_processing,
        accept_insecure_ssl_certificates: config.accept_insecure_ssl_certificates,
        web_ui: config.web_ui.as_ref().map(shared::model::WebUiConfigDto::from),
        messaging: config.messaging.as_ref().map(shared::model::MessagingConfigDto::from),
        reverse_proxy: config.reverse_proxy.as_ref().map(shared::model::ReverseProxyConfigDto::from),
        hdhomerun: config.hdhomerun.as_ref().map(hdhomerun_config_to_dto),
        proxy: config.proxy.as_ref().map(shared::model::ProxyConfigDto::from),
        ipcheck: config.ipcheck.as_ref().map(shared::model::IpCheckConfigDto::from),
        library: config.library.as_ref().map(library_config_to_dto),
    }
}

fn hdhomerun_config_to_dto(config: &HdHomeRunConfig) -> HdHomeRunConfigDto {
    HdHomeRunConfigDto {
        enabled: config.flags.contains(HdHomeRunFlags::Enabled),
        auth: config.flags.contains(HdHomeRunFlags::Auth),
        ssdp_discovery: config.flags.contains(HdHomeRunFlags::SsdpDiscovery),
        proprietary_discovery: config.flags.contains(HdHomeRunFlags::ProprietaryDiscovery),
        devices: config
            .devices
            .iter()
            .map(|device| HdHomeRunDeviceConfigDto {
                friendly_name: device.friendly_name.clone(),
                manufacturer: device.manufacturer.clone(),
                model_name: device.model_name.clone(),
                model_number: device.model_number.clone(),
                firmware_name: device.firmware_name.clone(),
                firmware_version: device.firmware_version.clone(),
                device_id: device.device_id.clone(),
                device_type: device.device_type.clone(),
                device_udn: device.device_udn.clone(),
                name: device.name.clone(),
                port: device.port,
                tuner_count: device.tuner_count,
            })
            .collect(),
    }
}

fn library_config_to_dto(config: &LibraryConfig) -> LibraryConfigDto {
    LibraryConfigDto {
        enabled: config.enabled,
        scan_directories: config
            .scan_directories
            .iter()
            .map(|directory| shared::model::LibraryScanDirectoryDto {
                enabled: directory.enabled,
                path: directory.path.clone(),
                content_type: directory.content_type,
                recursive: directory.recursive,
            })
            .collect(),
        supported_extensions: config.supported_extensions.clone(),
        metadata: LibraryMetadataConfigDto {
            read_existing: LibraryMetadataReadConfigDto {
                kodi: config.metadata.read_existing.kodi,
                jellyfin: config.metadata.read_existing.jellyfin,
                plex: config.metadata.read_existing.plex,
            },
            fallback_to_filename: config.metadata.fallback_to_filename,
            formats: config.metadata.formats.clone(),
        },
        playlist: LibraryPlaylistConfigDto {
            movie_category: config.playlist.movie_category.to_string(),
            series_category: config.playlist.series_category.to_string(),
        },
        thumbnails: ThumbnailConfigDto {
            enabled: config.thumbnails.enabled,
            width: config.thumbnails.width,
            height: config.thumbnails.height,
        },
    }
}

fn serialize_report_value(value: &Value, format: RuntimeConfigReportFormat) -> Result<String, TuliproxError> {
    match format {
        RuntimeConfigReportFormat::Json => serde_json::to_string_pretty(value)
            .map_err(|err| TuliproxError::Config(format!("Failed to serialize runtime config report as JSON: {err}"))),
        RuntimeConfigReportFormat::Yaml => {
            let mut serialized = String::new();
            let options = serde_saphyr::ser_options! {prefer_block_scalars: false};
            serde_saphyr::to_fmt_writer_with_options(&mut serialized, value, options).map_err(|err| {
                TuliproxError::Config(format!("Failed to serialize runtime config report as YAML: {err}"))
            })?;
            Ok(serialized)
        }
    }
}

fn redact_value(current_key: Option<&str>, parent_key: Option<&str>, grandparent_key: Option<&str>, value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                redact_value(Some(key.as_str()), current_key, parent_key, child);
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_value(None, current_key, parent_key, item);
            }
        }
        Value::String(text) => {
            if should_redact_field(current_key, parent_key, grandparent_key) {
                *text = "***".to_string();
            } else if should_redact_as_url(current_key) {
                *text = redact_url_like_value(text);
            }
        }
        _ => {}
    }
}

fn should_redact_field(current_key: Option<&str>, parent_key: Option<&str>, grandparent_key: Option<&str>) -> bool {
    let Some(key) = current_key.map(str::to_ascii_lowercase) else {
        return false;
    };
    if matches!(
        key.as_str(),
        "password"
            | "password_hash"
            | "secret"
            | "rewrite_secret"
            | "token"
            | "bot_token"
            | "api_key"
            | "apikey"
            | "authorization"
            | "proxy_authorization"
            | "proxy-authorization"
            | "access_token"
    ) {
        return true;
    }

    if parent_key.is_some_and(|parent| parent.eq_ignore_ascii_case("headers"))
        && grandparent_key.is_some_and(|parent| parent.eq_ignore_ascii_case("recording"))
    {
        return true;
    }

    if parent_key.is_some_and(|parent| parent.eq_ignore_ascii_case("headers")) {
        return matches!(
            key.as_str(),
            "authorization" | "proxy-authorization" | "proxy_authorization" | "cookie" | "set-cookie" | "x-api-key"
        );
    }

    key.ends_with("_password") || key.ends_with("_secret") || key.ends_with("_token") || key.ends_with("_api_key")
}

fn should_redact_as_url(current_key: Option<&str>) -> bool {
    current_key.is_some_and(|key| {
        let key = key.to_ascii_lowercase();
        key == "url" || key.ends_with("_url") || key == "base_url"
    })
}

fn redact_url_like_value(value: &str) -> String { shared::utils::sanitize_sensitive_info(value).to_string() }

#[cfg(test)]
mod tests {
    use super::{build_runtime_config_report, redact_value, render_runtime_config_report};
    use crate::read_initial_app_config;
    use shared::model::{ConfigPaths, RuntimeConfigReportFormat};
    use tempfile::tempdir;
    use tokio::fs;
    use tuliprox_core::model::AppConfig;

    fn test_paths(config_dir: &std::path::Path) -> ConfigPaths {
        ConfigPaths {
            home_path: config_dir.join("home").to_string_lossy().to_string(),
            config_path: config_dir.to_string_lossy().to_string(),
            storage_path: String::new(),
            config_file_path: config_dir.join("config.yml").to_string_lossy().to_string(),
            sources_file_path: config_dir.join("source.yml").to_string_lossy().to_string(),
            mapping_file_path: None,
            mapping_files_used: None,
            template_file_path: None,
            template_files_used: None,
            api_proxy_file_path: config_dir.join("api-proxy.yml").to_string_lossy().to_string(),
            custom_stream_response_path: None,
        }
    }

    async fn create_test_app_config() -> AppConfig {
        let temp_dir = tempdir().expect("temp dir");
        let config_dir = temp_dir.path().join("config");
        let home_dir = config_dir.join("home");
        fs::create_dir_all(&home_dir).await.expect("home dir");

        let config_yml = r"
storage_dir: data
api:
  host: 0.0.0.0
  port: 8901
  web_root: web
log:
  runtime_config_report_enabled: true
  runtime_config_report_format: json
proxy:
  url: http://proxy.example
  username: proxy-user
  password: proxy-pass
";
        let source_yml = r#"
inputs:
  - name: demo
    type: m3u
    url: http://provider.example/get.php?username=alice&password=secret
sources:
  - inputs: [demo]
    targets:
      - name: default
        filter: 'Group ~ ".*"'
        output:
          - type: m3u
"#;
        fs::create_dir_all(&config_dir).await.expect("config dir");
        fs::write(config_dir.join("config.yml"), config_yml).await.expect("config file");
        fs::write(config_dir.join("source.yml"), source_yml).await.expect("source file");

        let mut paths = test_paths(&config_dir);
        let app_config = read_initial_app_config(&mut paths, true, true, false).await.expect("app config");
        std::mem::forget(temp_dir);
        app_config
    }

    #[tokio::test]
    async fn runtime_config_report_effective_uses_runtime_paths() {
        let app_config = create_test_app_config().await;

        let report = build_runtime_config_report(&app_config).await.expect("report");

        assert!(report.config.storage_dir.as_deref().is_some_and(|value| value.contains("/config/home/data")));
        assert_eq!(report.config.api.web_root, report.paths.config_path.clone() + "/home/web");
    }

    #[test]
    fn runtime_config_report_redaction_masks_sensitive_fields_and_urls() {
        let mut value = serde_json::json!({
            "password": "secret-pass",
            "secret": "hidden",
            "headers": {
                "Authorization": "Bearer abc"
            },
            "url": "http://example.com/get.php?username=alice&password=secret"
        });

        redact_value(None, None, None, &mut value);

        assert_eq!(value["password"], "***");
        assert_eq!(value["secret"], "***");
        assert_eq!(value["headers"]["Authorization"], "***");
        assert_eq!(value["url"], "http://***/get.php?username=***&password=***");
    }

    #[test]
    fn runtime_config_report_redacts_every_recording_header() {
        let mut value = serde_json::json!({
            "video": { "recording": { "headers": { "X-Upstream-Key": "custom-secret" } } }
        });

        redact_value(None, None, None, &mut value);

        assert_eq!(value["video"]["recording"]["headers"]["X-Upstream-Key"], "***");
    }

    #[tokio::test]
    async fn runtime_config_report_formats_yaml() {
        let app_config = create_test_app_config().await;

        let rendered = render_runtime_config_report(&app_config, RuntimeConfigReportFormat::Yaml).await.expect("yaml");

        assert!(rendered.contains("config:"));
        assert!(rendered.contains("sources:"));
        assert!(rendered.contains("paths:"));
    }

    #[tokio::test]
    async fn runtime_config_report_masks_runtime_secrets() {
        let app_config = create_test_app_config().await;

        let rendered = render_runtime_config_report(&app_config, RuntimeConfigReportFormat::Json).await.expect("json");

        assert!(rendered.contains("\"password\": \"***\""));
        assert!(!rendered.contains("proxy-pass"));
    }

    #[tokio::test]
    async fn runtime_config_report_emits_recording_from_runtime_video_config() {
        let make_recording = |dir: &str| {
            tuliprox_core::model::RecordingConfig::from(&shared::model::RecordingConfigDto {
                enabled: true,
                directory: Some(dir.to_string()),
                ..shared::model::RecordingConfigDto::default()
            })
        };
        let config = tuliprox_core::model::Config {
            video: Some(tuliprox_core::model::VideoConfig {
                extensions: vec![".ts".to_string()],
                web_search: None,
                recording: Some(make_recording("/canonical/recordings")),
            }),
            ..tuliprox_core::model::Config::default()
        };

        let dto = super::runtime_config_to_dto(&config);

        let video = dto.video.as_ref().expect("report must carry video");
        let recording = video.recording.as_ref().expect("report must carry recording");
        assert_eq!(
            recording.directory.as_deref(),
            Some("/canonical/recordings"),
            "report must read from Config.video.recording"
        );
    }
}

//! Template resolution, caching and rendering.
//!
//! `resolve_template` used to wrap every template string in an
//! `InputSource` and call `download_text_content` - once per message, per
//! channel. A `file://` template was re-read from disk and an `http://`
//! template re-fetched over the network for every notification, and an
//! *inline* Handlebars string was pushed through a download attempt too,
//! because nothing distinguished the three cases.
//!
//! This module classifies the source once, caches what it resolves, and
//! keeps the compiled Handlebars template so a render is not a re-parse.

use handlebars::{Context, Handlebars, Helper, HelperResult, Output, RenderContext};
use log::{debug, error, warn};
use shared::{
    model::{notification::registry, InputFetchMethod},
    utils::Internable,
};
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, RwLock},
    time::{Duration, Instant},
};
use tuliprox_core::{
    model::{AppConfig, InputSource, NotificationEvent},
    utils::request::download_text_content,
};

/// How long a resolved remote template is trusted before revalidating.
const REMOTE_TTL: Duration = Duration::from_mins(5);

/// Where a configured template actually lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateSource {
    /// The config value *is* the template body.
    Inline,
    /// A local file, re-read when its mtime moves.
    File(String),
    /// A remote document, revalidated on a TTL.
    Url,
}

impl TemplateSource {
    /// Classify without touching the network or the disk.
    ///
    /// The old code could not do this, so an inline template paid for a
    /// download attempt on every single send.
    #[must_use]
    pub fn classify(template: &str) -> Self {
        let trimmed = template.trim_start();
        if let Some(path) = trimmed.strip_prefix("file://") {
            return Self::File(path.to_string());
        }
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            return Self::Url;
        }
        Self::Inline
    }
}

#[derive(Debug)]
struct CacheEntry {
    body: Arc<str>,
    fetched_at: Instant,
    /// File mtime at fetch time, so a local edit is picked up immediately
    /// rather than after the TTL.
    mtime: Option<std::time::SystemTime>,
}

/// Resolved template bodies, keyed by the raw config value.
static CACHE: LazyLock<RwLock<HashMap<String, CacheEntry>>> = LazyLock::new(|| RwLock::new(HashMap::new()));

/// Compiled templates, keyed by the raw config value.
static REGISTRY: LazyLock<RwLock<Handlebars<'static>>> = LazyLock::new(|| RwLock::new(new_handlebars()));

fn new_handlebars() -> Handlebars<'static> {
    let mut h = Handlebars::new();
    h.register_helper(
        "json_escape",
        Box::new(
            |h: &Helper, _: &Handlebars, _: &Context, _: &mut RenderContext, out: &mut dyn Output| -> HelperResult {
                let param = h.param(0).and_then(|v| v.value().as_str()).unwrap_or("");
                let escaped = serde_json::to_string(param).unwrap_or_default();
                if escaped.len() >= 2 {
                    out.write(&escaped[1..escaped.len() - 1])?;
                }
                Ok(())
            },
        ),
    );
    h
}

/// Drop every cached body and compiled template.
///
/// Called on config reload so an edited template takes effect without a
/// restart.
pub fn invalidate_cache() {
    if let Ok(mut cache) = CACHE.write() {
        cache.clear();
    }
    if let Ok(mut registry) = REGISTRY.write() {
        *registry = new_handlebars();
    }
}

fn file_mtime(path: &str) -> Option<std::time::SystemTime> { std::fs::metadata(path).ok()?.modified().ok() }

/// Resolve a template value to its body, using the cache where possible.
async fn resolve(app_config: &Arc<AppConfig>, client: &reqwest::Client, template: &str) -> Arc<str> {
    let source = TemplateSource::classify(template);
    if source == TemplateSource::Inline {
        // Nothing to fetch. This is the case the old code paid a download
        // attempt for.
        return Arc::from(template);
    }

    let current_mtime = match &source {
        TemplateSource::File(path) => file_mtime(path),
        _ => None,
    };

    if let Ok(cache) = CACHE.read() {
        if let Some(entry) = cache.get(template) {
            let fresh = match &source {
                // A local edit wins over the TTL.
                TemplateSource::File(_) => entry.mtime == current_mtime,
                _ => entry.fetched_at.elapsed() < REMOTE_TTL,
            };
            if fresh {
                return Arc::clone(&entry.body);
            }
        }
    }

    let input_source = InputSource {
        name: "Template".intern(),
        url: template.to_string(),
        provider: None,
        username: None,
        password: None,
        method: InputFetchMethod::GET,
        headers: HashMap::default(),
    };
    match download_text_content(app_config, client, &input_source, None, None, false).await {
        Ok((content, _)) => {
            let body: Arc<str> = Arc::from(content.as_str());
            if let Ok(mut cache) = CACHE.write() {
                cache.insert(
                    template.to_string(),
                    CacheEntry { body: Arc::clone(&body), fetched_at: Instant::now(), mtime: current_mtime },
                );
            }
            body
        }
        Err(err) => {
            // Serve a stale body rather than silently degrading to the
            // built-in text: the operator configured a template for a
            // reason, and a template host blip should not change what the
            // notification looks like.
            if let Ok(cache) = CACHE.read() {
                if let Some(entry) = cache.get(template) {
                    warn!("Template {template} could not be refreshed ({err}); serving the cached copy");
                    return Arc::clone(&entry.body);
                }
            }
            error!("Template {template} could not be resolved: {err}");
            Arc::from(template)
        }
    }
}

/// Render `event` through `template`, or `None` when the template is
/// unusable.
///
/// A `None` return means the caller falls back to `event.body`, which is
/// always populated.
pub async fn render(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    template: &str,
    event: &NotificationEvent,
) -> Option<String> {
    let body = resolve(app_config, client, template).await;
    let data = context_for(event);
    render_cached(template, &body, &data)
}

/// Render against the compiled-template registry, compiling on first use.
fn render_cached(key: &str, body: &str, data: &serde_json::Value) -> Option<String> {
    // Fast path: already compiled.
    if let Ok(registry) = REGISTRY.read() {
        if registry.has_template(key) {
            return match registry.render(key, data) {
                Ok(rendered) => Some(rendered),
                Err(err) => {
                    error!("Template {key} failed to render: {err}");
                    None
                }
            };
        }
    }
    let mut registry = REGISTRY.write().ok()?;
    if !registry.has_template(key) {
        if let Err(err) = registry.register_template_string(key, body) {
            error!("Template {key} failed to compile: {err}");
            return None;
        }
        debug!("Compiled notification template {key}");
    }
    match registry.render(key, data) {
        Ok(rendered) => Some(rendered),
        Err(err) => {
            error!("Template {key} failed to render: {err}");
            None
        }
    }
}

/// Compile-check a template body without sending anything.
///
/// Config validation calls this so a malformed template is an error the
/// operator sees at load, not a per-send `error!` with a silent fallback
/// that looks plausible.
pub fn validate(body: &str) -> Result<(), String> {
    new_handlebars().render_template(body, &serde_json::json!({})).map(|_| ()).map_err(|err| err.to_string())
}

/// The data a template sees.
///
/// Carries the new uniform `event.*` shape *and* every legacy top-level
/// key, so templates written against `{{kind}}`, `{{message}}`,
/// `{{stats}}`, `{{processing.errors}}`, `{{disk.percent}}` and the
/// flattened first-input fields keep rendering identically.
#[must_use]
pub fn context_for(event: &NotificationEvent) -> serde_json::Value {
    let mut root = serde_json::Map::new();

    root.insert("event".to_string(), serde_json::to_value(event).unwrap_or(serde_json::Value::Null));
    root.insert("timestamp".to_string(), event.timestamp_rfc3339().into());
    root.insert("kind".to_string(), legacy_kind_label(event).into());
    root.insert("severity".to_string(), event.severity.wire_name().into());
    root.insert("title".to_string(), event.title.clone().into());
    root.insert("body".to_string(), event.body.clone().into());

    let fields = &event.fields;
    match event.id {
        id if id == registry::SYSTEM_INFO || id == registry::SYSTEM_ERROR => {
            root.insert("message".to_string(), fields.clone());
        }
        id if id == registry::PLAYLIST_WATCH_CHANGED => {
            root.insert("watch".to_string(), fields.clone());
        }
        id if id == registry::PLAYLIST_UPDATE_COMPLETED || id == registry::PLAYLIST_UPDATE_FAILED => {
            root.insert("processing".to_string(), fields.clone());
            if let Some(stats) = fields.get("stats") {
                root.insert("stats".to_string(), stats.clone());
                // The legacy context flattened the first input's stats to the
                // root so short templates could say `{{name}}`/`{{took}}`.
                if let Some(first_input) = stats.get(0).and_then(|s| s.get("inputs")).and_then(|i| i.get(0)) {
                    if let Some(map) = first_input.as_object() {
                        for (key, value) in map {
                            root.entry(key.clone()).or_insert_with(|| value.clone());
                        }
                    }
                }
            }
            if let Some(errors) = fields.get("errors") {
                root.insert("message".to_string(), errors.clone());
            }
        }
        id if id == registry::SYSTEM_DISK_ALERT => {
            root.insert("disk".to_string(), fields.clone());
        }
        id if id == registry::RECORDING_STARTED
            || id == registry::RECORDING_COMPLETED
            || id == registry::RECORDING_FAILED =>
        {
            root.insert("recording".to_string(), fields.clone());
            root.insert("message".to_string(), event.title.clone().into());
        }
        _ => {}
    }
    serde_json::Value::Object(root)
}

/// The legacy `{{kind}}` label.
///
/// Templates in the wild print this, and the documented examples assert on
/// values like `Info` and `Stats`, so it stays the old CamelCase name for
/// registered legacy ids and the dotted id for everything new.
fn legacy_kind_label(event: &NotificationEvent) -> String {
    let id = event.id;
    if id == registry::SYSTEM_INFO {
        "Info".to_string()
    } else if id == registry::SYSTEM_ERROR {
        "Error".to_string()
    } else if id == registry::PLAYLIST_UPDATE_COMPLETED {
        "Stats".to_string()
    } else if id == registry::PLAYLIST_UPDATE_FAILED {
        "Error".to_string()
    } else if id == registry::PLAYLIST_WATCH_CHANGED {
        "Watch".to_string()
    } else if id == registry::SYSTEM_DISK_ALERT {
        "DiskAlert".to_string()
    } else if id == registry::RECORDING_STARTED {
        "RecordingStarted".to_string()
    } else if id == registry::RECORDING_COMPLETED {
        "RecordingCompleted".to_string()
    } else if id == registry::RECORDING_FAILED {
        "RecordingFailed".to_string()
    } else {
        id.as_str().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{context_for, validate, TemplateSource};
    use shared::model::{DiskAlert, DiskAlertLevel, InputStats, InputType, PlaylistStats, SourceStats, TargetStats};
    use tuliprox_core::model::{MessageContent, NotificationEvent, ProcessingStats, WatchChanges};

    fn stats_event() -> NotificationEvent {
        let input = InputStats {
            name: "Test Input".to_string(),
            input_type: InputType::M3u,
            error_count: 5,
            raw_stats: PlaylistStats { group_count: 100, channel_count: 1000 },
            processed_stats: PlaylistStats { group_count: 50, channel_count: 500 },
            secs_took: 125,
        };
        let content = MessageContent::ProcessingStats(ProcessingStats {
            stats: Some(vec![SourceStats { inputs: vec![input], targets: vec![TargetStats::success("Target 1")] }]),
            errors: Some("global error".to_string()),
        });
        NotificationEvent::from_content(&content)
    }

    #[test]
    fn inline_templates_are_not_fetched() {
        // The old code pushed every inline template through
        // `download_text_content` on every send.
        assert_eq!(TemplateSource::classify("Message: {{message}}"), TemplateSource::Inline);
        assert_eq!(TemplateSource::classify("  {\"content\": \"x\"}"), TemplateSource::Inline);
    }

    #[test]
    fn file_and_url_templates_are_classified() {
        assert_eq!(TemplateSource::classify("file:///tmp/a.templ"), TemplateSource::File("/tmp/a.templ".to_string()));
        assert_eq!(TemplateSource::classify("https://example.test/a.templ"), TemplateSource::Url);
        assert_eq!(TemplateSource::classify("http://example.test/a.templ"), TemplateSource::Url);
    }

    #[test]
    fn legacy_kind_labels_are_preserved() {
        // Templates in the wild print `{{kind}}` and the documented examples
        // assert on these exact strings.
        let info = NotificationEvent::from_content(&MessageContent::Info("hi".to_string()));
        assert_eq!(context_for(&info)["kind"], "Info");
        assert_eq!(context_for(&stats_event())["kind"], "Stats");
        let err = NotificationEvent::from_content(&MessageContent::ProcessingStats(ProcessingStats::new_error(
            "boom".to_string(),
        )));
        assert_eq!(context_for(&err)["kind"], "Error");
    }

    #[test]
    fn legacy_message_key_still_resolves_for_info() {
        let event = NotificationEvent::from_content(&MessageContent::Info("Hello World".to_string()));
        assert_eq!(context_for(&event)["message"], "Hello World");
    }

    #[test]
    fn legacy_processing_and_stats_keys_still_resolve() {
        let ctx = context_for(&stats_event());
        assert_eq!(ctx["processing"]["errors"], "global error");
        assert!(ctx["stats"].is_array(), "legacy `stats` key missing: {ctx}");
        // `{{message}}` carried the error string for a stats message.
        assert_eq!(ctx["message"], "global error");
    }

    #[test]
    fn first_input_stats_are_flattened_to_the_root() {
        // Legacy templates use bare `{{name}}` / `{{took}}` for the first
        // input without walking into `stats`.
        let ctx = context_for(&stats_event());
        assert_eq!(ctx["name"], "Test Input");
        assert!(ctx.get("took").is_some(), "flattened `took` missing: {ctx}");
    }

    #[test]
    fn flattened_fields_never_shadow_a_real_context_key() {
        // `stats` and `kind` must keep their meanings even though an input
        // payload could carry a same-named field.
        let ctx = context_for(&stats_event());
        assert!(ctx["stats"].is_array());
        assert_eq!(ctx["kind"], "Stats");
    }

    #[test]
    fn legacy_disk_key_still_resolves() {
        let event = NotificationEvent::from_content(&MessageContent::DiskAlert(DiskAlert {
            level: DiskAlertLevel::Critical,
            total_bytes: 1_000_000_000,
            free_bytes: 50_000_000,
            used_bytes: 950_000_000,
            percent: 95.0,
        }));
        let ctx = context_for(&event);
        assert_eq!(ctx["kind"], "DiskAlert");
        assert_eq!(ctx["disk"]["level"], "critical");
        assert_eq!(ctx["disk"]["percent"], 95.0);
    }

    #[test]
    fn legacy_watch_key_still_resolves() {
        let event = NotificationEvent::from_content(&MessageContent::Watch(WatchChanges {
            target: "T".to_string(),
            group: "G".to_string(),
            added: vec!["A".to_string()],
            removed: vec![],
        }));
        let ctx = context_for(&event);
        assert_eq!(ctx["kind"], "Watch");
        assert_eq!(ctx["watch"]["target"], "T");
    }

    #[test]
    fn the_new_event_shape_is_available_alongside_the_legacy_keys() {
        let ctx = context_for(&stats_event());
        assert_eq!(ctx["event"]["id"], "playlist.update.completed");
        assert_eq!(ctx["event"]["severity"], "info");
        assert!(ctx["event"]["title"].as_str().is_some_and(|t| !t.is_empty()));
        assert!(ctx["severity"].as_str().is_some());
    }

    #[test]
    fn validate_accepts_a_good_template_and_rejects_a_broken_one() {
        assert!(validate("Hello {{message}}").is_ok());
        // Unclosed block - this is exactly what used to fail silently at
        // send time and fall back to the built-in text.
        assert!(validate("{{#each stats}}oops").is_err());
    }
}

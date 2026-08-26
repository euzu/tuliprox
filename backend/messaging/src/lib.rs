//! Outbound notification delivery.
//!
//! Renders message templates and sends them over the configured channels -
//! Telegram and generic REST endpoints. The playlist pipeline, the recording
//! supervisor and the API all notify through here, and none of them are named
//! by this crate.

use chrono::Utc;
use handlebars::{Context, Handlebars, Helper, HelperResult, Output, RenderContext};
use log::{debug, error};
use reqwest::{header, Method};
use serde_json::json;
use shared::{
    model::{DiskAlert, InputFetchMethod, MsgKind},
    utils::{escape_markdown_v2, human_readable_byte_size, json_str_to_markdown, Internable},
};
use std::{
    borrow::Cow,
    collections::HashMap,
    str::FromStr,
    sync::{Arc, LazyLock},
};
use tuliprox_core::{
    model::{AppConfig, InputSource, MessageContent, MessagingConfig, TemplateContext},
    utils::{
        request::download_text_content, telegram_create_instance, telegram_send_message, SendMessageOption,
        SendMessageParseMode,
    },
};

fn is_enabled(kind: MsgKind, cfg: &MessagingConfig) -> bool { cfg.notify_on.contains(&kind) }

/// One configured outbound messaging channel.
///
/// The notification outbox retries per channel, not per message: a
/// message that reached Telegram but not Discord must be re-sent only to
/// Discord, or the retry would deliver a duplicate. Serialized into the
/// outbox file, so the variant names are part of that file's format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessagingChannel {
    Telegram,
    Rest,
    Pushover,
    Discord,
}

/// Result of one channel send. `None` means "nothing to do" — the
/// channel is not configured, or this message kind is filtered out — and
/// must never be retried.
pub type ChannelOutcome = Option<bool>;

/// The channels currently configured for `kind`, in a stable order.
pub fn configured_channels(app_config: &Arc<AppConfig>, kind: MsgKind) -> Vec<MessagingChannel> {
    let cfg = app_config.config.load();
    let Some(messaging) = cfg.messaging.as_ref() else {
        return Vec::new();
    };
    if !is_enabled(kind, messaging) {
        return Vec::new();
    }
    let mut channels = Vec::with_capacity(4);
    if messaging.telegram.is_some() {
        channels.push(MessagingChannel::Telegram);
    }
    if messaging.rest.is_some() {
        channels.push(MessagingChannel::Rest);
    }
    if messaging.pushover.is_some() {
        channels.push(MessagingChannel::Pushover);
    }
    if messaging.discord.is_some() {
        channels.push(MessagingChannel::Discord);
    }
    channels
}

/// Send `content` to exactly one channel and report whether it landed.
///
/// `send_message` fans out to every channel and swallows the outcome,
/// which is fine for fire-and-forget notifications but leaves a
/// recording-lifecycle event unrecoverable after a transient provider
/// error. This is the entry point the outbox worker drives.
pub async fn send_message_to_channel(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    content: &MessageContent,
    channel: MessagingChannel,
) -> ChannelOutcome {
    let cfg = app_config.config.load();
    let messaging = cfg.messaging.as_ref()?;
    if !is_enabled(content.kind(), messaging) {
        return None;
    }
    match channel {
        MessagingChannel::Telegram => send_telegram_message(app_config, client, content, messaging).await,
        MessagingChannel::Rest => send_rest_message(app_config, client, content, messaging).await,
        MessagingChannel::Pushover => send_pushover_message(app_config, client, content, messaging).await,
        MessagingChannel::Discord => send_discord_message(app_config, client, content, messaging).await,
    }
}

/// Default fallback string for a disk alert when no template is configured.
fn default_disk_alert_text(alert: &DiskAlert) -> String {
    format!(
        "Disk usage {}: {:.1}% used ({} of {}), {} free.",
        alert.level,
        alert.percent,
        human_readable_byte_size(alert.used_bytes),
        human_readable_byte_size(alert.total_bytes),
        human_readable_byte_size(alert.free_bytes),
    )
}

static HANDLEBARS: LazyLock<Handlebars> = LazyLock::new(|| {
    let mut h = Handlebars::new();
    h.register_helper(
        "json_escape",
        Box::new(
            |h: &Helper, _: &Handlebars, _: &Context, _: &mut RenderContext, out: &mut dyn Output| -> HelperResult {
                let param = h.param(0).and_then(|v| v.value().as_str()).unwrap_or("");
                let escaped = serde_json::to_string(param).unwrap_or_else(|_| String::new());
                if escaped.len() >= 2 {
                    out.write(&escaped[1..escaped.len() - 1])?;
                }
                Ok(())
            },
        ),
    );
    h
});

async fn render_template(
    app_config: &Arc<AppConfig>,
    http_client: &reqwest::Client,
    template: Option<&str>,
    content: &MessageContent,
) -> String {
    let timestamp = Utc::now().to_rfc3339();
    let kind = content.kind().to_string();

    let mut template_context = TemplateContext {
        kind,
        timestamp,
        message: None,
        stats: None,
        watch: None,
        processing: None,
        disk: None,
        recording: None,
        flat_stats: None,
    };

    match content {
        MessageContent::Info(msg) | MessageContent::Error(msg) => {
            template_context.message = Some(msg);
        }
        MessageContent::Watch(changes) => {
            template_context.watch = Some(changes);
        }
        MessageContent::ProcessingStats(stats) => {
            template_context.processing = Some(stats.clone());
            if let Some(stats) = &stats.stats {
                template_context.stats = Some(stats);
                if let Some(first_source) = stats.first() {
                    if let Some(first_input) = first_source.inputs.first() {
                        template_context.flat_stats = Some(first_input.clone());
                    }
                }
            }
            if let Some(errors) = &stats.errors {
                template_context.message = Some(errors);
            }
        }
        MessageContent::DiskAlert(alert) => {
            template_context.disk = Some(alert);
        }
        MessageContent::RecordingLifecycle(recording) => {
            template_context.recording = Some(recording);
            template_context.message = Some(match recording.event {
                MsgKind::RecordingStarted => "Recording started",
                MsgKind::RecordingCompleted => "Recording completed",
                MsgKind::RecordingFailed => "Recording failed",
                _ => "Recording lifecycle event",
            });
        }
    }

    match template {
        Some(template_content_or_uri) => {
            let t = resolve_template(app_config, http_client, template_content_or_uri).await;

            match HANDLEBARS.render_template(&t, &template_context) {
                Ok(rendered) => rendered,
                Err(e) => {
                    error!("Failed to render template: {e}");
                    default_text_for(content)
                }
            }
        }
        None => default_text_for(content),
    }
}

fn default_text_for(content: &MessageContent) -> String {
    match content {
        MessageContent::Info(s) | MessageContent::Error(s) => s.clone(),
        MessageContent::Watch(w) => serde_json::to_string(w).unwrap_or_default(),
        MessageContent::ProcessingStats(ps) => serde_json::to_string(ps).unwrap_or_default(),
        MessageContent::DiskAlert(alert) => default_disk_alert_text(alert),
        MessageContent::RecordingLifecycle(recording) => default_recording_lifecycle_text(recording),
    }
}

fn default_recording_lifecycle_text(recording: &tuliprox_core::model::RecordingLifecycleMessage) -> String {
    let label = match recording.event {
        MsgKind::RecordingStarted => "Recording started",
        MsgKind::RecordingCompleted => "Recording completed",
        MsgKind::RecordingFailed => "Recording failed",
        _ => "Recording lifecycle event",
    };
    let title = recording.programme_title.as_deref().unwrap_or("Untitled");
    let channel = recording.channel.as_deref().unwrap_or("unknown channel");
    match recording.failure_reason.as_deref() {
        Some(reason) => format!("{label}: {title} on {channel} ({reason})"),
        None => format!("{label}: {title} on {channel}"),
    }
}

async fn send_rest_message(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    content: &MessageContent,
    messaging: &MessagingConfig,
) -> ChannelOutcome {
    if let Some(rest) = &messaging.rest {
        let kind = content.kind();
        let template = rest.templates.get(&kind).map(String::as_str);
        let body = render_template(app_config, client, template, content).await;
        let method = Method::from_str(&rest.method).unwrap_or(Method::POST);

        let mut rb = client.request(method, &rest.url);

        let has_content_type = rest.headers.keys().any(|k| k.eq_ignore_ascii_case("content-type"));
        if !has_content_type {
            rb = rb.header(header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string());
        }

        for (key, value) in &rest.headers {
            rb = rb.header(key, value);
        }

        match rb.body(body).send().await {
            Ok(response) => {
                if response.status().is_success() {
                    debug!("Message sent successfully to rest api");
                    Some(true)
                } else {
                    error!("Failed to send message to rest api, status code {}", response.status());
                    Some(false)
                }
            }
            Err(e) => {
                error!("Message wasn't sent to rest api because of: {e}");
                Some(false)
            }
        }
    } else {
        None
    }
}

async fn send_discord_message(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    content: &MessageContent,
    messaging: &MessagingConfig,
) -> ChannelOutcome {
    if let Some(discord) = &messaging.discord {
        let kind = content.kind();
        let template = discord.templates.get(&kind).map(String::as_str);

        let body = if let Some(templ) = template {
            render_template(app_config, client, Some(templ), content).await
        } else {
            // Default json formatting
            let msg_str = default_text_for(content);
            json!({ "content": msg_str }).to_string()
        };

        match client
            .post(&discord.url)
            .header(header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())
            .body(body)
            .send()
            .await
        {
            Ok(response) => {
                if response.status().is_success() {
                    debug!("Message sent successfully to Discord");
                    Some(true)
                } else {
                    error!("Failed to send message to Discord, status code {}", response.status());
                    Some(false)
                }
            }
            Err(e) => {
                error!("Message wasn't sent to Discord because of: {e}");
                Some(false)
            }
        }
    } else {
        None
    }
}

async fn send_telegram_message(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    content: &MessageContent,
    messaging: &MessagingConfig,
) -> ChannelOutcome {
    if let Some(telegram) = &messaging.telegram {
        let kind = content.kind();
        let template = telegram.templates.get(&kind).map(String::as_str);
        let has_template = template.is_some();

        let msg = if let Some(templ) = template {
            render_template(app_config, client, Some(templ), content).await
        } else {
            let serialized;
            match content {
                MessageContent::Info(s) | MessageContent::Error(s) => s.clone(),
                MessageContent::Watch(s) => {
                    serialized = serde_json::to_string_pretty(s).unwrap_or_default();
                    serialized
                }
                MessageContent::ProcessingStats(ps) => {
                    serialized = serde_json::to_string_pretty(ps).unwrap_or_default();
                    serialized
                }
                MessageContent::DiskAlert(alert) => default_disk_alert_text(alert),
                MessageContent::RecordingLifecycle(recording) => default_recording_lifecycle_text(recording),
            }
        };

        let (message, options) = {
            if telegram.markdown {
                if let Ok(md) = json_str_to_markdown(&msg) {
                    (Cow::Owned(md), Some(SendMessageOption { parse_mode: SendMessageParseMode::MarkdownV2 }))
                } else {
                    // Keep template markdown as-is, but escape plain text to avoid MarkdownV2 parse errors.
                    if has_template {
                        (Cow::Borrowed(&msg), Some(SendMessageOption { parse_mode: SendMessageParseMode::MarkdownV2 }))
                    } else {
                        (
                            Cow::Owned(escape_markdown_v2(&msg)),
                            Some(SendMessageOption { parse_mode: SendMessageParseMode::MarkdownV2 }),
                        )
                    }
                }
            } else {
                (Cow::Borrowed(&msg), None)
            }
        };

        // A single failed chat id fails the channel: the outbox retries
        // the whole channel, which is the coarsest granularity the
        // Telegram config exposes.
        let mut all_delivered = true;
        for chat_id in &telegram.chat_ids {
            let bot = telegram_create_instance(&telegram.bot_token, chat_id);
            let send_result = telegram_send_message(app_config, client, &bot, &message, options.as_ref()).await;
            let mut delivered = send_result.delivered;
            if telegram.markdown && has_template && send_result.parse_error && !delivered {
                // Template output can include dynamic fields that break MarkdownV2. Retry once escaped.
                let escaped = escape_markdown_v2(&msg);
                let escaped_options = SendMessageOption { parse_mode: SendMessageParseMode::MarkdownV2 };
                delivered =
                    telegram_send_message(app_config, client, &bot, &escaped, Some(&escaped_options)).await.delivered;
            }
            all_delivered &= delivered;
        }
        Some(all_delivered)
    } else {
        None
    }
}

async fn send_pushover_message(
    _app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    content: &MessageContent,
    messaging: &MessagingConfig,
) -> ChannelOutcome {
    if let Some(pushover) = &messaging.pushover {
        let msg = default_text_for(content);

        let encoded_message: String = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("token", pushover.token.as_str())
            .append_pair("user", pushover.user.as_str())
            .append_pair("message", &msg)
            .finish();
        match client
            .post(&pushover.url)
            .header(header::CONTENT_TYPE, mime::APPLICATION_WWW_FORM_URLENCODED.to_string())
            .body(encoded_message)
            .send()
            .await
        {
            Ok(response) => {
                if response.status().is_success() {
                    debug!("Text message sent successfully to PUSHOVER, status code {}", response.status());
                    Some(true)
                } else {
                    error!("Failed to send text message to PUSHOVER, status code {}", response.status());
                    Some(false)
                }
            }
            Err(e) => {
                error!("Text message wasn't sent to PUSHOVER api because of: {e}");
                Some(false)
            }
        }
    } else {
        None
    }
}

async fn dispatch_send_message(app_config: &Arc<AppConfig>, client: &reqwest::Client, content: MessageContent) {
    let cfg = app_config.config.load();
    let msg_cfg = cfg.messaging.as_ref();
    if let Some(messaging) = msg_cfg {
        let kind = content.kind();
        if is_enabled(kind, messaging) {
            let _ = tokio::join!(
                send_telegram_message(app_config, client, &content, messaging),
                send_rest_message(app_config, client, &content, messaging),
                send_pushover_message(app_config, client, &content, messaging),
                send_discord_message(app_config, client, &content, messaging)
            );
        }
    }
}

pub async fn send_message(app_config: &Arc<AppConfig>, client: &reqwest::Client, content: MessageContent) {
    dispatch_send_message(app_config, client, content).await;
}

async fn resolve_template<'a>(
    app_config: &'a Arc<AppConfig>,
    http_client: &'a reqwest::Client,
    template: &'a str,
) -> Cow<'a, str> {
    let url = template.to_string();

    let input_source = InputSource {
        name: "Template".intern(),
        url,
        provider: None,
        username: None,
        password: None,
        method: InputFetchMethod::GET,
        headers: HashMap::default(),
    };
    if let Ok((content, _response_url)) =
        download_text_content(app_config, http_client, &input_source, None, None, false).await
    {
        Cow::Owned(content)
    } else {
        Cow::Borrowed(template)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_swap::{ArcSwap, ArcSwapOption};
    use shared::model::ConfigPaths;
    use tuliprox_core::{
        model::{MediaToolCapabilities, ProcessingStats},
        utils::FileLockManager,
    };

    fn create_app_config() -> Arc<AppConfig> {
        Arc::new(AppConfig {
            config: Arc::new(ArcSwap::default()),
            sources: Arc::new(ArcSwap::default()),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            api_proxy: Arc::new(ArcSwapOption::default()),
            file_locks: Arc::new(FileLockManager::default()),
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
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(ArcSwapOption::default()),
            access_token_secret: [
                1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28,
                29, 30, 31, 32,
            ],
            encrypt_secret: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            media_tools: Arc::new(MediaToolCapabilities::new()),
        })
    }

    #[tokio::test]
    async fn test_render_template_simple() {
        let msg = "Hello World".to_string();
        let content = MessageContent::Info(msg);
        let app_cfg = create_app_config();
        let client = reqwest::Client::new();
        let output = render_template(&app_cfg, &client, Some("Message: {{message}}, Kind: {{kind}}"), &content).await;

        assert!(output.contains("Message: Hello World"));
        assert!(output.contains("Kind: Info"));
    }

    #[tokio::test]
    async fn test_render_template_processing_stats() {
        let stats = ProcessingStats { stats: None, errors: Some("test error".to_string()) };
        let content = MessageContent::ProcessingStats(stats);
        let app_cfg = create_app_config();
        let client = reqwest::Client::new();
        let output = render_template(&app_cfg, &client, Some("Error: {{processing.errors}}"), &content).await;
        assert_eq!(output, "Error: test error");
    }

    #[tokio::test]
    async fn test_render_discord_template() {
        use shared::model::{InputStats, InputType, PlaylistStats, SourceStats, TargetStats};

        let input_stats = InputStats {
            name: "Test Input".to_string(),
            input_type: InputType::M3u,
            error_count: 5,
            raw_stats: PlaylistStats { group_count: 100, channel_count: 1000 },
            processed_stats: PlaylistStats { group_count: 50, channel_count: 500 },
            secs_took: 125,
        };

        let source_stats = SourceStats { inputs: vec![input_stats], targets: vec![TargetStats::success("Target 1")] };

        // Add a second source for testing multi-source rendering
        let input_stats2 = InputStats {
            name: "Input 2".to_string(),
            input_type: InputType::Xtream,
            error_count: 0,
            raw_stats: PlaylistStats { group_count: 200, channel_count: 2000 },
            processed_stats: PlaylistStats { group_count: 180, channel_count: 1800 },
            secs_took: 300,
        };
        let source_stats2 = SourceStats { inputs: vec![input_stats2], targets: vec![TargetStats::success("Target 2")] };

        let stats = ProcessingStats {
            stats: Some(vec![source_stats, source_stats2]),
            errors: Some("Some global error message".to_string()),
        };

        let content = MessageContent::ProcessingStats(stats);
        let app_cfg = create_app_config();
        let client = reqwest::Client::new();

        // Use the absolute path for the template
        let template = r#"
            {
              "username": "Tuliprox",
              "avatar_url": "https://raw.githubusercontent.com/euzu/tuliprox/refs/heads/develop/frontend/public/assets/tuliprox-logo.svg",
              "embeds": [
                {
                  "title": "🔄 Playlist Update Report",
                  "color": 3310335,
                  "fields": [
                    {{#each stats}}
                    {
                      "name": "📥 Source Stats",
                      "value": "{{#each inputs}}**{{name}}** (`{{type}}`)\n⏱️ Took: `{{took}}` | ❌ Errors: `{{errors}}` \n📊 `{{raw.groups}}`/`{{raw.channels}}` ➔ **`{{processed.groups}}`**/**`{{processed.channels}}`**\n{{#unless @last}}\n{{/unless}}{{/each}}",
                      "inline": false
                    },
                    {
                      "name": "🚀 Targets",
                      "value": "{{#each targets}}✅ `{{target}}`{{#unless @last}}\n{{/unless}}{{/each}}",
                      "inline": false
                    }{{#unless @last}},{{/unless}}
                    {{/each}}
                    {{#if processing.errors}}
                    {{#if stats}},{{/if}}
                    {
                      "name": "❌ Processing Errors",
                      "value": "```{{processing.errors}}```",
                      "inline": false
                    }
                    {{/if}}
                  ],
                  "footer": {
                    "text": "Tuliprox • Automated Task",
                    "icon_url": "https://raw.githubusercontent.com/euzu/tuliprox/refs/heads/develop/frontend/public/assets/tuliprox-logo.svg"
                  },
                  "timestamp": "{{timestamp}}"
                }
              ]
            }
        "#;

        let output = render_template(&app_cfg, &client, Some(template), &content).await;

        println!("{output}");

        // Verify some expected strings in the output
        assert!(output.contains("\"username\": \"Tuliprox\""));
        assert!(output.contains("Test Input"));
        assert!(output.contains("Input 2"));
        assert!(output.contains("📥 Source Stats"));
        assert!(output.contains("❌ Processing Errors"));
        assert!(output.contains("Some global error message"));
        assert!(output.contains("Target 1"));
        assert!(output.contains("Target 2"));
        assert!(output.contains("2:05 mins")); // 125 secs
        assert!(output.contains("5:00 mins")); // 300 secs
    }

    #[tokio::test]
    async fn test_render_telegram_template() {
        use shared::model::{InputStats, InputType, PlaylistStats, SourceStats, TargetStats};

        let input_stats = InputStats {
            name: "Telegram Input".to_string(),
            input_type: InputType::Xtream,
            error_count: 2,
            raw_stats: PlaylistStats { group_count: 50, channel_count: 500 },
            processed_stats: PlaylistStats { group_count: 45, channel_count: 450 },
            secs_took: 45,
        };

        let source_stats = SourceStats { inputs: vec![input_stats], targets: vec![TargetStats::success("Target T1")] };

        let stats = ProcessingStats {
            stats: Some(vec![source_stats]),
            errors: Some("An error occurred during sync".to_string()),
        };

        let content = MessageContent::ProcessingStats(stats);
        let app_cfg = create_app_config();
        let client = reqwest::Client::new();

        let template = r"
            *🔄 Playlist Update Report*

            {{#each stats}}
            *📥 Source Stats*
            {{#each inputs}}
            • *{{name}}* (`{{type}}`)
              ⏱️ Took: `{{took}}` | ❌ Errors: `{{errors}}`
              📊 `{{raw.groups}}`/`{{raw.channels}}` ➔ *`{{processed.groups}}`*/*`{{processed.channels}}`*
            {{/each}}

            *🚀 Targets*
            {{#each targets}}
            ✅ `{{target}}`
            {{/each}}
            {{/each}}

            {{#if processing.errors}}
            *❌ Processing Errors*
            ```
            {{processing.errors}}
            ```
            {{/if}}

            _Timestamp: {{timestamp}}_
        ";
        let output = render_template(&app_cfg, &client, Some(template), &content).await;

        println!("Telegram Output:\n{output}");

        assert!(output.contains("🔄 Playlist Update Report"));
        assert!(output.contains("Telegram Input"));
        assert!(output.contains("⏱️ Took: `45 secs`"));
        assert!(output.contains("❌ Errors: `2`"));
        assert!(output.contains("Target T1"));
        assert!(output.contains("An error occurred during sync"));
    }

    #[test]
    fn disk_alert_kind_is_msg_kind_disk_alert() {
        let alert = shared::model::DiskAlert {
            level: shared::model::DiskAlertLevel::Critical,
            total_bytes: 1_000,
            free_bytes: 50,
            used_bytes: 950,
            percent: 95.0,
        };
        let content = MessageContent::DiskAlert(alert);
        assert_eq!(content.kind(), shared::model::MsgKind::DiskAlert);
    }

    #[tokio::test]
    async fn test_render_disk_alert_template_exposes_fields() {
        let alert = shared::model::DiskAlert {
            level: shared::model::DiskAlertLevel::Critical,
            total_bytes: 1_000_000_000,
            free_bytes: 50_000_000,
            used_bytes: 950_000_000,
            percent: 95.0,
        };
        let content = MessageContent::DiskAlert(alert);
        let app_cfg = create_app_config();
        let client = reqwest::Client::new();
        let template = r"[{{kind}}] {{disk.level}} - {{disk.percent}}% used";
        let output = render_template(&app_cfg, &client, Some(template), &content).await;
        assert!(output.starts_with("[DiskAlert] critical - "));
        assert!(output.ends_with("% used"));
    }

    #[tokio::test]
    async fn test_render_disk_alert_falls_back_to_default_text() {
        let alert = shared::model::DiskAlert {
            level: shared::model::DiskAlertLevel::Warn,
            total_bytes: 1_000_000_000,
            free_bytes: 150_000_000,
            used_bytes: 850_000_000,
            percent: 85.0,
        };
        let content = MessageContent::DiskAlert(alert);
        let app_cfg = create_app_config();
        let client = reqwest::Client::new();
        let output = render_template(&app_cfg, &client, None, &content).await;
        assert!(output.contains("warning"), "fallback must mention alert level, got {output}");
        assert!(output.contains("85.0%"), "fallback must include percent, got {output}");
    }
}

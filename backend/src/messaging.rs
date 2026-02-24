use crate::model::{AppConfig, InputSource, MessagingConfig, MessageContent, TemplateContext};
use crate::utils::{telegram_create_instance, telegram_send_message, SendMessageOption, SendMessageParseMode};
use chrono::Utc;
use handlebars::{Context, Handlebars, Helper, HelperResult, Output, RenderContext};
use log::{debug, error};
use reqwest::{header, Method};
use serde_json::json;
use shared::model::{InputFetchMethod, MsgKind};
use shared::utils::{escape_markdown_v2, json_str_to_markdown, Internable};
use std::borrow::Cow;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, LazyLock};
use crate::utils::request::download_text_content;

fn is_enabled(kind: MsgKind, cfg: &MessagingConfig) -> bool {
    cfg.notify_on.contains(&kind)
}

static HANDLEBARS: LazyLock<Handlebars> = LazyLock::new(|| {
    let mut h = Handlebars::new();
    h.register_helper("json_escape", Box::new(|h: &Helper, _: &Handlebars, _: &Context, _: &mut RenderContext, out: &mut dyn Output| -> HelperResult {
        let param = h.param(0).and_then(|v| v.value().as_str()).unwrap_or("");
        let escaped = serde_json::to_string(param).unwrap_or_else(|_| String::new());
        if escaped.len() >= 2 {
            out.write(&escaped[1..escaped.len()-1])?;
        }
        Ok(())
    }));
    h
});

async fn render_template(app_config: &Arc<AppConfig>, http_client: &reqwest::Client, template: Option<&str>, content: &MessageContent) -> String {
    let timestamp = Utc::now().to_rfc3339();
    let kind = content.kind().to_string();

    let mut template_context = TemplateContext {
        kind,
        timestamp,
        message: None,
        stats: None,
        watch: None,
        processing: None,
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
    }

    match template {
        Some(template_content_or_uri) => {
            let t = resolve_template(app_config, http_client, template_content_or_uri).await;

            match HANDLEBARS.render_template(&t, &template_context) {
                Ok(rendered) => rendered,
                Err(e) => {
                    error!("Failed to render template: {e}");
                    match content {
                        MessageContent::Info(s) | MessageContent::Error(s) => s.clone(),
                        MessageContent::Watch(w) => serde_json::to_string(w).unwrap_or_default(),
                        MessageContent::ProcessingStats(ps) => serde_json::to_string(ps).unwrap_or_default(),
                    }
                }
            }
        }
        None => {
             match content {
                MessageContent::Info(s) | MessageContent::Error(s) => s.clone(),
                MessageContent::Watch(w) => serde_json::to_string(w).unwrap_or_default(),
                MessageContent::ProcessingStats(ps) => serde_json::to_string(ps).unwrap_or_default(),
            }
        }
    }
}

async fn send_rest_message(app_config: &Arc<AppConfig>, client: &reqwest::Client, content: &MessageContent, messaging: &MessagingConfig) {
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
                } else {
                    error!("Failed to send message to rest api, status code {}", response.status());
                }
            }
            Err(e) => error!("Message wasn't sent to rest api because of: {e}"),
        }
    }
}

async fn send_discord_message(app_config: &Arc<AppConfig>, client: &reqwest::Client, content: &MessageContent, messaging: &MessagingConfig) {
    if let Some(discord) = &messaging.discord {
        let kind = content.kind();
        let template = discord.templates.get(&kind).map(String::as_str);
        
        let body = if let Some(templ) = template {
            render_template(app_config, client, Some(templ), content).await
        } else {
             // Default json formatting
             let msg_str = match content {
                MessageContent::Info(s) | MessageContent::Error(s) => s.clone(),
                MessageContent::Watch(s) => serde_json::to_string(s).unwrap_or_default(),
                MessageContent::ProcessingStats(ps) => serde_json::to_string(ps).unwrap_or_default(),
            };
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
                } else {
                    error!("Failed to send message to Discord, status code {}", response.status());
                }
            }
            Err(e) => error!("Message wasn't sent to Discord because of: {e}"),
        }
    }
}

async fn send_telegram_message(app_config: &Arc<AppConfig>, client: &reqwest::Client, content: &MessageContent, messaging: &MessagingConfig) {
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
                        (Cow::Owned(escape_markdown_v2(&msg)), Some(SendMessageOption { parse_mode: SendMessageParseMode::MarkdownV2 }))
                    }
                }
            } else {
                (Cow::Borrowed(&msg), None)
            }
        };

        for chat_id in &telegram.chat_ids {
            let bot = telegram_create_instance(&telegram.bot_token, chat_id);
            let send_result = telegram_send_message(app_config, client, &bot, &message, options.as_ref()).await;
            if telegram.markdown && has_template && send_result.parse_error && !send_result.delivered {
                // Template output can include dynamic fields that break MarkdownV2. Retry once escaped.
                let escaped = escape_markdown_v2(&msg);
                let escaped_options = SendMessageOption {
                    parse_mode: SendMessageParseMode::MarkdownV2,
                };
                let _ = telegram_send_message(app_config, client, &bot, &escaped, Some(&escaped_options)).await;
            }
        }
    }
}

async fn send_pushover_message(_app_config: &Arc<AppConfig>, client: &reqwest::Client, content: &MessageContent, messaging: &MessagingConfig) {
    if let Some(pushover) = &messaging.pushover {
        let msg = match content {
             MessageContent::Info(s) | MessageContent::Error(s) => s.clone(),
             MessageContent::Watch(s) => serde_json::to_string_pretty(s).unwrap_or_default(),
             MessageContent::ProcessingStats(ps) => serde_json::to_string_pretty(ps).unwrap_or_default(),
        };

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
                } else {
                    error!("Failed to send text message to PUSHOVER, status code {}", response.status());
                }
            }
            Err(e) => error!("Text message wasn't sent to PUSHOVER api because of: {e}"),
        }
    }
}

async fn dispatch_send_message(app_config: &Arc<AppConfig>, client: &reqwest::Client, content: MessageContent) {
    let cfg = app_config.config.load();
    let msg_cfg = cfg.messaging.as_ref();
    if let Some(messaging) = msg_cfg {
        let kind = content.kind();
        if is_enabled(kind, messaging) {
            tokio::join!(
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

async fn resolve_template<'a>(app_config: &'a Arc<AppConfig>, http_client: &'a reqwest::Client, template: &'a str) -> Cow<'a, str> {
    let url = template.to_string();

    let input_source =  InputSource {
        name: "Template".intern(),
        url,
        provider: None,
        username: None,
        password: None,
        method: InputFetchMethod::GET,
        headers: HashMap::default(),
    };
    if let Ok((content, _response_url)) = download_text_content(
        app_config,
        http_client,
        &input_source,
        None,
        None,
        false,
    ).await {
        Cow::Owned(content)
    } else {
        Cow::Borrowed(template)
    }
}

#[cfg(test)]
mod tests {
    use arc_swap::{ArcSwap, ArcSwapOption};
    use crate::model::ProcessingStats;
    use super::*;
    use shared::model::{ConfigPaths};
    use crate::utils::FileLockManager;

    fn create_app_config() -> Arc<AppConfig> {
        Arc::new(AppConfig {
            config: Arc::new(ArcSwap::default()),
            sources: Arc::new(ArcSwap::default()),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            api_proxy: Arc::new(ArcSwapOption::default()),
            file_locks: Arc::new(FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
                config_path: String::new(),
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
            access_token_secret: [1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32],
            encrypt_secret: [1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16],
            ffprobe_available: Arc::default(),
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
        let stats = ProcessingStats {
            stats: None,
            errors: Some("test error".to_string()),
        };
        let content = MessageContent::ProcessingStats(stats);
        let app_cfg = create_app_config();
        let client = reqwest::Client::new();
        let output = render_template(&app_cfg, &client, Some("Error: {{processing.errors}}"), &content).await;
        assert_eq!(output, "Error: test error");
    }

    #[tokio::test]
    async fn test_render_discord_template() {
        use shared::model::{SourceStats, InputStats, InputType, PlaylistStats, TargetStats};

        let input_stats = InputStats {
            name: "Test Input".to_string(),
            input_type: InputType::M3u,
            error_count: 5,
            raw_stats: PlaylistStats { group_count: 100, channel_count: 1000 },
            processed_stats: PlaylistStats { group_count: 50, channel_count: 500 },
            secs_took: 125,
        };

        let source_stats = SourceStats {
            inputs: vec![input_stats],
            targets: vec![TargetStats::success("Target 1")],
        };

        // Add a second source for testing multi-source rendering
        let input_stats2 = InputStats {
            name: "Input 2".to_string(),
            input_type: InputType::Xtream,
            error_count: 0,
            raw_stats: PlaylistStats { group_count: 200, channel_count: 2000 },
            processed_stats: PlaylistStats { group_count: 180, channel_count: 1800 },
            secs_took: 300,
        };
        let source_stats2 = SourceStats {
            inputs: vec![input_stats2],
            targets: vec![TargetStats::success("Target 2")],
        };

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
        use shared::model::{SourceStats, InputStats, InputType, PlaylistStats, TargetStats};

        let input_stats = InputStats {
            name: "Telegram Input".to_string(),
            input_type: InputType::Xtream,
            error_count: 2,
            raw_stats: PlaylistStats { group_count: 50, channel_count: 500 },
            processed_stats: PlaylistStats { group_count: 45, channel_count: 450 },
            secs_took: 45,
        };

        let source_stats = SourceStats {
            inputs: vec![input_stats],
            targets: vec![TargetStats::success("Target T1")],
        };

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
}

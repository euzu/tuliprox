//! Outbound notification delivery.
//!
//! Renders notification events and sends them over the configured channels.
//! The playlist pipeline, the recording supervisor and the API all notify
//! through here, and none of them are named by this crate.
//!
//! Channels are an open set: see [`channel::NotificationChannel`]. Event
//! kinds are an open set too - see [`shared::model::notification`]. Adding
//! either no longer means editing a match in this file.

pub mod channel;
pub mod channels;
pub mod outbox;
pub mod rate_limit;
pub mod render;

use crate::{
    channel::{Delivery, RenderedMessage},
    channels::{Channel, ChannelSet},
};
use log::{debug, error};
use shared::model::{
    notification::{EventId, Severity},
    MsgKind,
};
use std::sync::Arc;
use tuliprox_core::model::{AppConfig, MessageContent, MessagingConfig, NotificationEvent};

/// Is this event subscribed by `notify_on`?
fn is_enabled(id: EventId, cfg: &MessagingConfig) -> bool { cfg.subscription().matches(id) }

/// The channels currently configured for `id`, by stable channel id.
///
/// The outbox persists these strings, so a channel added in a later release
/// no longer makes an older build reject the whole outbox file.
pub fn configured_channels(app_config: &Arc<AppConfig>, id: EventId) -> Vec<String> {
    let cfg = app_config.config.load();
    let Some(messaging) = cfg.messaging.as_ref() else {
        return Vec::new();
    };
    if !is_enabled(id, messaging) {
        return Vec::new();
    }
    channels::channels(app_config, messaging)
        .into_iter()
        .filter(|channel| channel.wants(id, severity_of(id)))
        .map(|channel| channel.id().to_string())
        .collect()
}

fn severity_of(id: EventId) -> Severity { shared::model::notification::registry::default_severity(id) }

/// Render `event` for `channel`, falling back to the built-in body.
async fn render_for<'a>(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    channel: &Channel,
    event: &'a NotificationEvent,
) -> RenderedMessage<'a> {
    let capabilities = channel.capabilities();
    let mut templated = false;
    let mut body = event.body.clone();

    if capabilities.supports_templates {
        if let Some(template) = channel.template_for(event.id) {
            if let Some(rendered) = render::render(app_config, client, template, event).await {
                body = rendered;
                templated = true;
            }
        }
    }

    if let Some(limit) = capabilities.max_body_bytes {
        if body.len() > limit {
            // Truncate on a character boundary, never mid-codepoint.
            let mut end = limit;
            while end > 0 && !body.is_char_boundary(end) {
                end -= 1;
            }
            body.truncate(end);
        }
    }

    RenderedMessage { event, body, templated }
}

/// Send `event` to exactly one channel, by its stable id.
///
/// This is the entry point the outbox worker drives: it retries per
/// channel, so a message that reached Telegram but not Discord is re-sent
/// only to Discord.
pub async fn send_event_to_channel(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    event: &NotificationEvent,
    channel_id: &str,
) -> Delivery {
    let cfg = app_config.config.load();
    let Some(messaging) = cfg.messaging.as_ref() else {
        return Delivery::Skipped;
    };
    if !is_enabled(event.id, messaging) {
        return Delivery::Skipped;
    }
    let Some(channel) = channels::channels(app_config, messaging).into_iter().find(|c| c.id() == channel_id) else {
        // The channel was removed from the config between enqueue and
        // delivery. Nothing to do, and nothing to retry.
        return Delivery::Skipped;
    };
    if !channel.wants(event.id, event.severity) {
        return Delivery::Skipped;
    }
    let routing = channel.routing();
    if let Some(reason) =
        rate_limit::admit(channel.id(), event.dedup_key.as_deref(), routing.dedup_window, routing.max_per_hour)
    {
        return suppression_outcome(&channel, event, reason);
    }
    let msg = render_for(app_config, client, &channel, event).await;
    channel.send(&msg).await
}

/// Turn a suppression decision into a delivery outcome.
///
/// A rate-limit ceiling reports itself once so the resulting silence is
/// distinguishable from a notifier that has died.
fn suppression_outcome(channel: &Channel, event: &NotificationEvent, reason: rate_limit::Suppression) -> Delivery {
    match reason {
        rate_limit::Suppression::Duplicate => {
            debug!("Notification {} suppressed on {} as a duplicate", event.id, channel.id());
            Delivery::Skipped
        }
        rate_limit::Suppression::RateLimited => Delivery::Skipped,
        rate_limit::Suppression::RateLimitReached => {
            error!(
                target: "notification::audit",
                "notification_rate_limit_reached: channel={} event={} - further notifications suppressed this hour",
                channel.id(), event.id
            );
            Delivery::Skipped
        }
    }
}

/// Minutes since local midnight, for quiet-hours evaluation.
fn local_minutes_now() -> u16 {
    use chrono::Timelike;
    let now = chrono::Local::now();
    u16::try_from(now.hour() * 60 + now.minute()).unwrap_or(0)
}

/// How long delivery to `channel` should be deferred for quiet hours.
///
/// Quiet hours defer rather than drop: an overnight outage nobody hears
/// about afterwards is worse than one that arrives late.
#[must_use]
pub fn quiet_hours_defer(channel: &Channel) -> Option<std::time::Duration> {
    let window = channel.routing().quiet_hours?;
    let minutes = local_minutes_now();
    if !window.contains(minutes) {
        return None;
    }
    Some(std::time::Duration::from_secs(u64::from(window.minutes_until_end(minutes)) * 60))
}

/// Notify.
///
/// Hands the event to the durable outbox where one is installed, so a
/// transient provider error is retried instead of being logged and lost -
/// which is what happened to every non-recording notification before the
/// outbox was promoted out of the recording supervisor. Falls back to a
/// direct best-effort send when the outbox is absent (unit tests, early
/// startup) or full.
pub async fn send_event(app_config: &Arc<AppConfig>, client: &reqwest::Client, event: NotificationEvent) {
    let cfg = app_config.config.load();
    let Some(messaging) = cfg.messaging.as_ref() else {
        return;
    };
    if !is_enabled(event.id, messaging) {
        return;
    }
    let event = match outbox::notification_outbox() {
        Some(outbox) => match outbox.enqueue(event) {
            None => return,
            Some(rejected) => rejected,
        },
        None => event,
    };
    let set: ChannelSet = channels::channels(app_config, messaging);
    dispatch(app_config, client, &event, &set).await;
}

/// Deliver to every channel in `set` at once.
///
/// Concurrent rather than sequential: one slow provider must not delay the
/// others.
async fn dispatch(app_config: &Arc<AppConfig>, client: &reqwest::Client, event: &NotificationEvent, set: &ChannelSet) {
    let sends = set.iter().filter(|channel| channel.wants(event.id, event.severity)).map(|channel| async move {
        let routing = channel.routing();
        if let Some(reason) =
            rate_limit::admit(channel.id(), event.dedup_key.as_deref(), routing.dedup_window, routing.max_per_hour)
        {
            return (channel.id(), suppression_outcome(channel, event, reason));
        }
        let msg = render_for(app_config, client, channel, event).await;
        let outcome = channel.send(&msg).await;
        (channel.id(), outcome)
    });
    for (id, outcome) in futures::future::join_all(sends).await {
        match outcome {
            Delivery::Delivered | Delivery::Skipped => debug!("Notification {} handled by {id}", event.id),
            Delivery::Retry { reason, .. } => error!("Notification {} to {id} failed: {reason}", event.id),
            Delivery::Permanent { reason } => {
                error!("Notification {} to {id} permanently rejected: {reason}", event.id);
            }
        }
    }
}

/// Send a legacy [`MessageContent`], lifted into the envelope.
///
/// Kept so the existing emitters need no change.
pub async fn send_message(app_config: &Arc<AppConfig>, client: &reqwest::Client, content: MessageContent) {
    send_event(app_config, client, NotificationEvent::from_content(&content)).await;
}

/// Legacy shim: the channels configured for a [`MsgKind`].
pub fn configured_channels_for_kind(app_config: &Arc<AppConfig>, kind: MsgKind) -> Vec<String> {
    match EventId::from_wire(kind.wire_name()) {
        Some(id) => configured_channels(app_config, id),
        None => Vec::new(),
    }
}
/// One channel's result from a test send.
pub struct TestOutcome {
    pub channel: String,
    /// `delivered`, `skipped`, `retry`, `permanent`, or `preview`.
    pub outcome: String,
    pub reason: Option<String>,
    /// Exactly what the channel was asked to send.
    pub rendered: String,
    pub templated: bool,
}

/// A representative event for `id`, for testing a channel or a template.
///
/// Carries a plausible payload so a template that walks `event.fields`
/// renders something recognisable rather than blank.
#[must_use]
pub fn test_event(id: EventId) -> NotificationEvent {
    let description = shared::model::notification::registry::describe(id)
        .map_or("Test notification", |descriptor| descriptor.description);
    NotificationEvent::new(
        id,
        format!("Test: {id}"),
        format!("{description}\n\nThis is a test notification from tuliprox."),
    )
    .with_fields(&serde_json::json!({ "test": true }))
}

/// Render `event` for each requested channel, and send unless `preview`.
///
/// Bypasses `notify_on` and the suppression window deliberately: the
/// operator asked for this one explicitly, and a test that silently does
/// nothing because of a dedup window would be worse than useless.
pub async fn render_and_send_test(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    event: &NotificationEvent,
    only_channel: Option<&str>,
    preview: bool,
) -> Vec<TestOutcome> {
    let cfg = app_config.config.load();
    let Some(messaging) = cfg.messaging.as_ref() else {
        return Vec::new();
    };
    let set = channels::channels(app_config, messaging);
    let mut results = Vec::with_capacity(set.len());
    for channel in set.iter().filter(|c| only_channel.is_none_or(|want| c.id() == want)) {
        let msg = render_for(app_config, client, channel, event).await;
        if preview {
            results.push(TestOutcome {
                channel: channel.id().to_string(),
                outcome: "preview".to_string(),
                reason: None,
                rendered: msg.body.clone(),
                templated: msg.templated,
            });
            continue;
        }
        let (outcome, reason) = match channel.send(&msg).await {
            Delivery::Delivered => ("delivered".to_string(), None),
            Delivery::Skipped => ("skipped".to_string(), None),
            Delivery::Retry { reason, .. } => ("retry".to_string(), Some(reason)),
            Delivery::Permanent { reason } => ("permanent".to_string(), Some(reason)),
        };
        results.push(TestOutcome {
            channel: channel.id().to_string(),
            outcome,
            reason,
            rendered: msg.body.clone(),
            templated: msg.templated,
        });
    }
    results
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

    /// Adapter onto the new pipeline.
    ///
    /// These tests predate the channel/envelope refactor and assert that the
    /// documented Discord and Telegram templates still render exactly as
    /// before, so they are kept pointed at the real renderer rather than
    /// rewritten.
    async fn render_template(
        app_cfg: &Arc<AppConfig>,
        client: &reqwest::Client,
        template: Option<&str>,
        content: &MessageContent,
    ) -> String {
        let event = NotificationEvent::from_content(content);
        match template {
            Some(t) => render::render(app_cfg, client, t, &event).await.unwrap_or_else(|| event.body.clone()),
            None => event.body.clone(),
        }
    }

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

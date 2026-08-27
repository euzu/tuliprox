use crate::model::macros;
use log::warn;
use shared::model::{
    notification::{registry, EventId, EventSubscription},
    DiscordMessagingConfigDto, DiskAlertConfigDto, MessagingConfigDto, PushoverMessagingConfigDto,
    RestMessagingConfigDto, TelegramMessagingConfigDto,
};
use std::path::{Path, PathBuf};

/// Runtime configuration for the threshold-driven disk-space alert.
#[derive(Debug, Clone)]
pub struct DiskAlertConfig {
    /// Disk usage at or above this percentage triggers the warning state.
    pub warn_percent: f64,
    /// Disk usage at or above this percentage triggers the critical state.
    pub critical_percent: f64,
    /// Re-send the alert after this many seconds if the disk is still in
    /// the same alert state.
    pub repeat_interval_secs: u64,
}

impl Default for DiskAlertConfig {
    fn default() -> Self {
        Self {
            warn_percent: DiskAlertConfigDto::default().warn_percent,
            critical_percent: DiskAlertConfigDto::default().critical_percent,
            repeat_interval_secs: DiskAlertConfigDto::default().repeat_interval_secs,
        }
    }
}

macros::from_impl!(DiskAlertConfig);
impl From<&DiskAlertConfigDto> for DiskAlertConfig {
    fn from(dto: &DiskAlertConfigDto) -> Self {
        Self {
            warn_percent: dto.warn_percent,
            critical_percent: dto.critical_percent,
            repeat_interval_secs: dto.repeat_interval_secs,
        }
    }
}

impl From<&DiskAlertConfig> for DiskAlertConfigDto {
    fn from(instance: &DiskAlertConfig) -> Self {
        Self {
            warn_percent: instance.warn_percent,
            critical_percent: instance.critical_percent,
            repeat_interval_secs: instance.repeat_interval_secs,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TelegramMessagingConfig {
    pub bot_token: String,
    pub chat_ids: Vec<String>,
    pub markdown: bool,
    pub templates: std::collections::HashMap<EventId, String>,
}

impl TelegramMessagingConfig {
    pub fn prepare(&mut self, templates_dir: &Path) {
        discover_templates("telegram", &mut self.templates, templates_dir);
    }
}

macros::from_impl!(TelegramMessagingConfig);
impl From<&TelegramMessagingConfigDto> for TelegramMessagingConfig {
    fn from(dto: &TelegramMessagingConfigDto) -> Self {
        Self {
            bot_token: dto.bot_token.clone(),
            chat_ids: dto.chat_ids.clone(),
            markdown: dto.markdown,
            templates: resolve_template_keys(&dto.templates),
        }
    }
}

impl From<&TelegramMessagingConfig> for TelegramMessagingConfigDto {
    fn from(instance: &TelegramMessagingConfig) -> Self {
        Self {
            bot_token: instance.bot_token.clone(),
            chat_ids: instance.chat_ids.clone(),
            markdown: instance.markdown,
            templates: template_keys_to_wire(&instance.templates),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RestMessagingConfig {
    pub url: String,
    pub method: String,
    pub headers: std::collections::HashMap<String, String>,
    pub templates: std::collections::HashMap<EventId, String>,
}

impl RestMessagingConfig {
    pub fn prepare(&mut self, templates_dir: &Path) { discover_templates("rest", &mut self.templates, templates_dir); }
}

macros::from_impl!(RestMessagingConfig);
impl From<&RestMessagingConfigDto> for RestMessagingConfig {
    fn from(dto: &RestMessagingConfigDto) -> Self {
        let mut headers = std::collections::HashMap::new();
        for h in &dto.headers {
            if let Some((k, v)) = h.split_once(':') {
                headers.insert(k.trim().to_string(), v.trim().to_string());
            } else if !h.trim().is_empty() {
                warn!("Ignoring malformed header (missing ':'): {h}");
            }
        }
        Self {
            url: dto.url.clone(),
            method: dto.method.clone().unwrap_or_else(|| "POST".to_string()),
            headers,
            templates: resolve_template_keys(&dto.templates),
        }
    }
}

impl From<&RestMessagingConfig> for RestMessagingConfigDto {
    fn from(model: &RestMessagingConfig) -> Self {
        let headers = model.headers.iter().map(|(k, v)| format!("{k}: {v}")).collect();
        Self {
            url: model.url.clone(),
            method: Some(model.method.clone()),
            headers,
            templates: template_keys_to_wire(&model.templates),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DiscordMessagingConfig {
    pub url: String,
    pub templates: std::collections::HashMap<EventId, String>,
}

impl DiscordMessagingConfig {
    pub fn prepare(&mut self, templates_dir: &Path) {
        discover_templates("discord", &mut self.templates, templates_dir);
    }
}

macros::from_impl!(DiscordMessagingConfig);
impl From<&DiscordMessagingConfigDto> for DiscordMessagingConfig {
    fn from(dto: &DiscordMessagingConfigDto) -> Self {
        Self { url: dto.url.clone(), templates: resolve_template_keys(&dto.templates) }
    }
}

impl From<&DiscordMessagingConfig> for DiscordMessagingConfigDto {
    fn from(instance: &DiscordMessagingConfig) -> Self {
        Self { url: instance.url.clone(), templates: template_keys_to_wire(&instance.templates) }
    }
}

#[derive(Debug, Clone)]
pub struct PushoverMessagingConfig {
    pub url: String,
    pub token: String,
    pub user: String,
    pub templates: std::collections::HashMap<EventId, String>,
}

impl PushoverMessagingConfig {
    pub fn prepare(&mut self, templates_dir: &Path) {
        discover_templates("pushover", &mut self.templates, templates_dir);
    }
}

macros::from_impl!(PushoverMessagingConfig);
impl From<&PushoverMessagingConfigDto> for PushoverMessagingConfig {
    fn from(dto: &PushoverMessagingConfigDto) -> Self {
        Self {
            url: dto
                .url
                .as_ref()
                .map_or_else(|| String::from("https://api.pushover.net/1/messages.json"), ToString::to_string),
            token: dto.token.clone(),
            user: dto.user.clone(),
            templates: resolve_template_keys(&dto.templates),
        }
    }
}

impl From<&PushoverMessagingConfig> for PushoverMessagingConfigDto {
    fn from(instance: &PushoverMessagingConfig) -> Self {
        Self {
            url: Some(instance.url.clone()),
            token: instance.token.clone(),
            user: instance.user.clone(),
            templates: template_keys_to_wire(&instance.templates),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MessagingConfig {
    /// Raw `notify_on` entries as written in config. Glob patterns and
    /// legacy `MsgKind` names are both accepted.
    pub notify_on: Vec<String>,
    pub telegram: Option<TelegramMessagingConfig>,
    pub rest: Option<RestMessagingConfig>,
    pub pushover: Option<PushoverMessagingConfig>,
    pub discord: Option<DiscordMessagingConfig>,
    /// Optional disk-space alert config. When `None`, no alert is fired.
    pub disk_alert: Option<DiskAlertConfig>,
    /// `notify_on` parsed once at load. Matching runs per notification, so
    /// re-parsing the patterns each time would be wasteful.
    subscription: EventSubscription,
}

impl MessagingConfig {
    /// The parsed `notify_on` subscription.
    #[must_use]
    pub fn subscription(&self) -> &EventSubscription { &self.subscription }

    pub fn prepare(&mut self, config_path: &str) {
        let templates_dir = PathBuf::from(config_path).join("messaging_templates");
        if let Some(t) = &mut self.telegram {
            t.prepare(&templates_dir);
        }
        if let Some(r) = &mut self.rest {
            r.prepare(&templates_dir);
        }
        if let Some(d) = &mut self.discord {
            d.prepare(&templates_dir);
        }
        if let Some(p) = &mut self.pushover {
            p.prepare(&templates_dir);
        }
    }
}

macros::from_impl!(MessagingConfig);
impl From<&MessagingConfigDto> for MessagingConfig {
    fn from(dto: &MessagingConfigDto) -> Self {
        Self {
            notify_on: dto.notify_on.clone(),
            subscription: EventSubscription::parse(dto.notify_on.iter()),
            telegram: dto.telegram.as_ref().map(Into::into),
            rest: dto.rest.as_ref().map(Into::into),
            pushover: dto.pushover.as_ref().map(Into::into),
            discord: dto.discord.as_ref().map(Into::into),
            disk_alert: dto.disk_alert.as_ref().map(Into::into),
        }
    }
}

impl From<&MessagingConfig> for MessagingConfigDto {
    fn from(instance: &MessagingConfig) -> Self {
        Self {
            notify_on: instance.notify_on.clone(),
            telegram: instance.telegram.as_ref().map(Into::into),
            rest: instance.rest.as_ref().map(Into::into),
            pushover: instance.pushover.as_ref().map(Into::into),
            discord: instance.discord.as_ref().map(Into::into),
            disk_alert: instance.disk_alert.as_ref().map(Into::into),
        }
    }
}

/// Resolve config template keys to event ids, warning on anything unknown.
///
/// A typo here used to be indistinguishable from an event that never fires.
fn resolve_template_keys(
    raw: &std::collections::HashMap<String, String>,
) -> std::collections::HashMap<EventId, String> {
    let mut out = std::collections::HashMap::with_capacity(raw.len());
    for (key, value) in raw {
        match EventId::from_wire(key) {
            Some(id) => {
                out.insert(id, value.clone());
            }
            None => warn!("Ignoring messaging template for unknown event `{key}`"),
        }
    }
    out
}

/// Canonical wire names for round-tripping back into the DTO.
fn template_keys_to_wire(
    templates: &std::collections::HashMap<EventId, String>,
) -> std::collections::HashMap<String, String> {
    templates.iter().map(|(id, value)| (id.as_str().to_string(), value.clone())).collect()
}

/// Adopt `{prefix}_{event}.templ` files sitting in the templates directory.
///
/// Iterates the event registry rather than a hardcoded variant list, which
/// is what previously made a newly added kind silently undiscoverable.
fn discover_templates(prefix: &str, templates: &mut std::collections::HashMap<EventId, String>, templates_dir: &Path) {
    for descriptor in registry::ALL {
        if let std::collections::hash_map::Entry::Vacant(e) = templates.entry(descriptor.id) {
            // Canonical name first, then any legacy filename that resolves
            // to the same id, so existing `telegram_disk_alert.templ` files
            // keep being picked up.
            let candidates = std::iter::once(descriptor.id.template_filename(prefix)).chain(
                shared::model::notification::LEGACY_ALIASES
                    .iter()
                    .filter(|(_, id)| *id == descriptor.id)
                    .map(|(legacy, _)| format!("{prefix}_{legacy}.templ")),
            );
            for filename in candidates {
                let file_path = templates_dir.join(filename);
                if file_path.exists() {
                    e.insert(format!("file://{}", file_path.to_string_lossy()));
                    break;
                }
            }
        }
    }
}

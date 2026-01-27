use log::warn;
use crate::model::macros;
use shared::model::{DiscordMessagingConfigDto, MessagingConfigDto, MsgKind, PushoverMessagingConfigDto, RestMessagingConfigDto, TelegramMessagingConfigDto};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct TelegramMessagingConfig {
    pub bot_token: String,
    pub chat_ids: Vec<String>,
    pub markdown: bool,
    pub templates: std::collections::HashMap<MsgKind, String>,
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
            templates: dto.templates.clone(),
        }
    }
}

impl From<&TelegramMessagingConfig> for TelegramMessagingConfigDto {
    fn from(instance: &TelegramMessagingConfig) -> Self {
        Self {
            bot_token: instance.bot_token.clone(),
            chat_ids: instance.chat_ids.clone(),
            markdown: instance.markdown,
            templates: instance.templates.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RestMessagingConfig {
    pub url: String,
    pub method: String,
    pub headers: std::collections::HashMap<String, String>,
    pub templates: std::collections::HashMap<MsgKind, String>,
}

impl RestMessagingConfig {
    pub fn prepare(&mut self, templates_dir: &Path) {
        discover_templates("rest", &mut self.templates, templates_dir);
    }
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
            templates: dto.templates.clone(),
        }
    }
}

impl From<&RestMessagingConfig> for RestMessagingConfigDto {
    fn from(model: &RestMessagingConfig) -> Self {
        let headers = model.headers.iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect();
        Self {
            url: model.url.clone(),
            method: Some(model.method.clone()),
            headers,
            templates: model.templates.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DiscordMessagingConfig {
    pub url: String,
    pub templates: std::collections::HashMap<MsgKind, String>,
}

impl DiscordMessagingConfig {
    pub fn prepare(&mut self, templates_dir: &Path) {
        discover_templates("discord", &mut self.templates, templates_dir);
    }
}

macros::from_impl!(DiscordMessagingConfig);
impl From<&DiscordMessagingConfigDto> for DiscordMessagingConfig {
    fn from(dto: &DiscordMessagingConfigDto) -> Self {
        Self {
            url: dto.url.clone(),
            templates: dto.templates.clone(),
        }
    }
}

impl From<&DiscordMessagingConfig> for DiscordMessagingConfigDto {
    fn from(instance: &DiscordMessagingConfig) -> Self {
        Self {
            url: instance.url.clone(),
            templates: instance.templates.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PushoverMessagingConfig {
    pub url: String,
    pub token: String,
    pub user: String,
}

macros::from_impl!(PushoverMessagingConfig);
impl From<&PushoverMessagingConfigDto> for PushoverMessagingConfig {
    fn from(dto: &PushoverMessagingConfigDto) -> Self {
        Self {
            url: dto.url.as_ref().map_or_else(|| String::from("https://api.pushover.net/1/messages.json"), ToString::to_string),
            token: dto.token.clone(),
            user: dto.user.clone(),
        }
    }
}

impl From<&PushoverMessagingConfig> for PushoverMessagingConfigDto {
    fn from(instance: &PushoverMessagingConfig) -> Self {
        Self {
            url: Some(instance.url.clone()),
            token: instance.token.clone(),
            user: instance.user.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MessagingConfig {
    pub notify_on: Vec<MsgKind>,
    pub telegram: Option<TelegramMessagingConfig>,
    pub rest: Option<RestMessagingConfig>,
    pub pushover: Option<PushoverMessagingConfig>,
    pub discord: Option<DiscordMessagingConfig>,
}

impl MessagingConfig {
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
    }
}

macros::from_impl!(MessagingConfig);
impl From<&MessagingConfigDto> for MessagingConfig {
    fn from(dto: &MessagingConfigDto) -> Self {
        Self {
            notify_on: dto.notify_on.clone(),
            telegram: dto.telegram.as_ref().map(Into::into),
            rest: dto.rest.as_ref().map(Into::into),
            pushover: dto.pushover.as_ref().map(Into::into),
            discord: dto.discord.as_ref().map(Into::into),
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
        }
    }
}

fn discover_templates(prefix: &str, templates: &mut std::collections::HashMap<MsgKind, String>, templates_dir: &Path) {
    let variants = [MsgKind::Info, MsgKind::Stats, MsgKind::Error, MsgKind::Watch];
    for kind in variants {
        if let std::collections::hash_map::Entry::Vacant(e) = templates.entry(kind) {
            let filename = kind.template_filename(prefix);
            let file_path = templates_dir.join(filename);
            if file_path.exists() {
                e.insert(format!("file://{}", file_path.to_string_lossy()));
            }
        }
    }
}
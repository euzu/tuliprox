use super::{MessagingConfigDto, REDACTED_SECRET};

fn redact(value: &mut String) {
    if !value.trim().is_empty() {
        *value = REDACTED_SECRET.to_string();
    }
}

fn restore(incoming: &mut String, stored: &str) {
    if incoming.trim() == REDACTED_SECRET {
        *incoming = stored.to_string();
    }
}

impl MessagingConfigDto {
    /// Replace channel secrets with [`REDACTED_SECRET`].
    ///
    /// The config GET returns `config.yml` in full to any client with
    /// `ConfigRead`, which included the Telegram bot token.
    pub fn redact_secrets(&mut self) {
        if let Some(telegram) = self.telegram.as_mut() {
            redact(&mut telegram.bot_token);
        }
    }

    /// Put back any secret the client returned still redacted.
    ///
    /// Without this a round-trip through the UI would overwrite the stored
    /// token with the mask, silently breaking the channel.
    pub fn restore_redacted_secrets(&mut self, current: &Self) {
        if let (Some(incoming), Some(stored)) = (self.telegram.as_mut(), current.telegram.as_ref()) {
            restore(&mut incoming.bot_token, &stored.bot_token);
        }
    }
}

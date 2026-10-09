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

/// Header names whose value is a credential.
fn is_sensitive_header(name: &str) -> bool {
    const SENSITIVE: &[&str] = &["authorization", "x-api-key", "x-auth-token", "proxy-authorization", "cookie"];
    let name = name.trim();
    SENSITIVE.iter().any(|candidate| name.eq_ignore_ascii_case(candidate))
}

impl MessagingConfigDto {
    /// Replace channel secrets with [`REDACTED_SECRET`].
    ///
    /// The config GET returns `config.yml` in full to any client with
    /// `ConfigRead`, which included the Telegram bot token, the Pushover
    /// token and user key, and any `Authorization` header configured on the
    /// REST channel.
    pub fn redact_secrets(&mut self) {
        if let Some(telegram) = self.telegram.as_mut() {
            redact(&mut telegram.bot_token);
        }
        if let Some(pushover) = self.pushover.as_mut() {
            redact(&mut pushover.token);
            redact(&mut pushover.user);
        }
        if let Some(ntfy) = self.ntfy.as_mut() {
            if let Some(token) = ntfy.token.as_mut() {
                redact(token);
            }
        }
        if let Some(gotify) = self.gotify.as_mut() {
            redact(&mut gotify.token);
        }
        if let Some(rest) = self.rest.as_mut() {
            if let Some(secret) = rest.signing_secret.as_mut() {
                redact(secret);
            }
            for header in &mut rest.headers {
                // `Name: value` - keep the name, mask the value.
                if let Some((name, _)) = header.split_once(':') {
                    if is_sensitive_header(name) {
                        *header = format!("{name}: {REDACTED_SECRET}");
                    }
                }
            }
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
        if let (Some(incoming), Some(stored)) = (self.pushover.as_mut(), current.pushover.as_ref()) {
            restore(&mut incoming.token, &stored.token);
            restore(&mut incoming.user, &stored.user);
        }
        if let (Some(incoming), Some(stored)) = (self.ntfy.as_mut(), current.ntfy.as_ref()) {
            if let (Some(token), Some(original)) = (incoming.token.as_mut(), stored.token.as_ref()) {
                restore(token, original);
            }
        }
        if let (Some(incoming), Some(stored)) = (self.gotify.as_mut(), current.gotify.as_ref()) {
            restore(&mut incoming.token, &stored.token);
        }
        if let (Some(incoming), Some(stored)) = (self.rest.as_mut(), current.rest.as_ref()) {
            if let (Some(secret), Some(original)) = (incoming.signing_secret.as_mut(), stored.signing_secret.as_ref()) {
                restore(secret, original);
            }
            for header in &mut incoming.headers {
                let Some((name, value)) = header.split_once(':') else { continue };
                if value.trim() != REDACTED_SECRET {
                    continue;
                }
                let name = name.to_string();
                if let Some(original) =
                    stored.headers.iter().find(|h| h.split_once(':').is_some_and(|(n, _)| n.trim() == name.trim()))
                {
                    *header = original.clone();
                }
            }
        }
    }
}

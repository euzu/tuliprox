//! Discord Notifier: delivers tuliprox notifications to a Discord webhook.
//!
//! `notifications.send` posts one notification to a Discord webhook
//! ([`discord`]). The host applies routing, quiet hours and suppression and
//! owns the retrying outbox; this plugin formats the message and answers each
//! attempt with a [`DeliveryError`] the outbox understands.
//!
//! The settings form is generated from `config-schema.json`; there is no UI code.

mod discord;

use tuliprox_plugin_sdk::prelude::*;

#[tuliprox_plugin(manifest = "manifest.yml")]
mod plugin {
    use super::*;

    /// The plugin settings (`config-schema.json`). `routing` is read by the
    /// host, not by the plugin, so it is not a field here.
    #[tuliprox_plugin_settings]
    pub struct Settings {
        #[serde(default)]
        pub url: Secret<String>,
        #[serde(default)]
        pub style: crate::discord::Style,
    }

    pub struct DiscordNotifier;

    impl NotificationHooks for DiscordNotifier {
        fn send_notification(msg: Notification) -> Result<(), DeliveryError> {
            // Settings that no longer parse will not parse on a retry either.
            let settings = Settings::current().map_err(DeliveryError::Permanent)?;
            crate::discord::deliver(settings.url.expose(), settings.style, &msg)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuliprox_plugin_sdk::mock;

    /// The one test that goes through the settings static: it is process-wide,
    /// so no other test may call `Settings::current`.
    #[test]
    fn the_hook_reads_the_webhook_url_from_the_host_config_and_never_logs_it() {
        mock::reset();
        mock::set_config(
            r#"{"url":"https://discord.test/api/webhooks/1/SECRET-TOKEN","routing":{"min_severity":"error"}}"#,
        );
        assert!(plugin::Settings::reload().is_ok());
        let settings = plugin::Settings::current().unwrap();
        assert!(!format!("{:?}", settings.url).contains("SECRET-TOKEN"), "a Secret never prints its value");

        let msg = Notification {
            title: "Update".to_string(),
            body: "Update done".to_string(),
            severity: Severity::Info,
            payload: "{}".to_string(),
        };
        mock::http_respond(http::Method::Post, "https://discord.test/api/webhooks/1/SECRET-TOKEN", 204, "");
        assert!(plugin::DiscordNotifier::send_notification(msg.clone()).is_ok());
        assert_eq!(mock::http_requests().len(), 1);
        assert!(mock::logs().iter().all(|line| !line.message.contains("SECRET-TOKEN")));

        mock::http_fail(
            http::Method::Post,
            "https://discord.test/api/webhooks/1/SECRET-TOKEN",
            "dial https://discord.test/api/webhooks/1/SECRET-TOKEN failed",
        );
        let err = plugin::DiscordNotifier::send_notification(msg).unwrap_err();
        assert!(!format!("{err:?}").contains("SECRET-TOKEN"), "a transport error never carries the url");
        assert!(mock::logs().iter().all(|line| !line.message.contains("SECRET-TOKEN")));
    }

    #[test]
    fn the_module_exports_exactly_the_manifest_hooks() {
        assert_eq!(plugin::HOOK_IDS, ["notifications.send"]);
        assert_eq!(plugin::EXPORTS, ["tuliprox:plugin/lifecycle@1.0.0", "tuliprox:plugin/notification-hooks@1.0.0"]);
    }
}

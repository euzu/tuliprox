//! Telegram Notifier: delivers tuliprox notifications to Telegram chats.
//!
//! `notifications.send` sends one notification to every configured chat through a
//! bot ([`telegram`]), splitting a long message into several and remembering
//! which chats already have it ([`state`]), so a retry by the host's outbox never
//! repeats a delivery. The host applies routing, quiet hours and suppression and
//! owns the retrying outbox; this plugin formats the message and answers each
//! attempt with a [`DeliveryError`] the outbox understands.
//!
//! The channel config is a `ui.config_extensions` fragment of the Web UI messaging page, stored under
//! `settings.ext.messaging`; there is no UI code and no standalone settings form.

mod state;
mod telegram;

use tuliprox_plugin_sdk::prelude::*;

#[tuliprox_plugin(manifest = "manifest.yml")]
mod plugin {
    use super::*;

    /// The channel config: the `ext.messaging` object of the plugin settings
    /// (`ui.config_extensions` in `manifest.yml`). `routing` is read by the
    /// host, not by the plugin, so it is not a field here.
    #[derive(serde::Deserialize, Default)]
    pub struct Channel {
        #[serde(default)]
        pub bot_token: Secret<String>,
        #[serde(default)]
        pub chat_ids: Vec<String>,
        #[serde(default)]
        pub style: crate::telegram::Style,
    }

    #[derive(serde::Deserialize, Default)]
    pub struct Ext {
        #[serde(default)]
        pub messaging: Channel,
    }

    /// The plugin settings. Everything lives under `ext.messaging`; the plugin
    /// has no standalone settings form.
    #[tuliprox_plugin_settings]
    pub struct Settings {
        #[serde(default)]
        pub ext: Ext,
    }

    pub struct TelegramNotifier;

    impl NotificationHooks for TelegramNotifier {
        fn send_notification(msg: Notification) -> Result<(), DeliveryError> {
            // Settings that no longer parse will not parse on a retry either.
            let settings = Settings::current().map_err(DeliveryError::Permanent)?;
            let settings = &settings.ext.messaging;
            let target = crate::telegram::Target {
                token: settings.bot_token.expose(),
                chat_ids: &settings.chat_ids,
                style: settings.style,
            };
            crate::telegram::deliver(&target, &msg)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuliprox_plugin_sdk::mock;

    const SEND: &str = "https://api.telegram.org/bot123456:SECRET-TOKEN/sendMessage";

    /// The one test that goes through the settings static: it is process-wide,
    /// so no other test may call `Settings::current`.
    #[test]
    fn the_hook_reads_its_settings_from_the_host_config_and_never_logs_the_token() {
        mock::reset();
        mock::set_config(
            r#"{"ext":{"messaging":{"bot_token":"123456:SECRET-TOKEN","chat_ids":["-100","-200:5"],"routing":{"min_severity":"error"}}}}"#,
        );
        assert!(plugin::Settings::reload().is_ok());
        let settings = plugin::Settings::current().unwrap();
        let settings = &settings.ext.messaging;
        assert_eq!(settings.style, telegram::Style::Html, "html is the default style");
        assert_eq!(settings.chat_ids, ["-100", "-200:5"]);
        assert!(!format!("{:?}", settings.bot_token).contains("SECRET"), "a Secret never prints its value");

        let msg = || Notification {
            title: "Update".to_string(),
            body: "Update done".to_string(),
            severity: Severity::Info,
            payload: r#"{"id":"system.info","timestamp":1}"#.to_string(),
        };
        mock::http_respond(http::Method::Post, SEND, 200, r#"{"ok":true}"#);
        mock::http_respond(http::Method::Post, SEND, 200, r#"{"ok":true}"#);
        assert!(plugin::TelegramNotifier::send_notification(msg()).is_ok());
        assert_eq!(mock::http_requests().len(), 2, "one request per chat");
        assert!(mock::logs().iter().all(|line| !line.message.contains("SECRET")));
        assert!(mock::kv_snapshot().is_empty(), "a finished delivery leaves nothing in the key-value store");

        mock::reset();
        mock::set_config(r#"{"ext":{"messaging":{"bot_token":"123456:SECRET-TOKEN","chat_ids":["-100"]}}}"#);
        assert!(plugin::Settings::reload().is_ok());
        mock::http_fail(http::Method::Post, SEND, "dial https://api.telegram.org/bot123456:SECRET-TOKEN failed");
        let err = plugin::TelegramNotifier::send_notification(msg()).unwrap_err();
        assert!(!format!("{err:?}").contains("SECRET"), "a transport error never carries the token");
        assert!(mock::logs().iter().all(|line| !line.message.contains("SECRET")));
    }

    #[test]
    fn the_module_exports_exactly_the_manifest_hooks() {
        assert_eq!(plugin::HOOK_IDS, ["notifications.send"]);
        assert_eq!(plugin::EXPORTS, ["tuliprox:plugin/lifecycle@1.0.0", "tuliprox:plugin/notification-hooks@1.0.0"]);
    }
}

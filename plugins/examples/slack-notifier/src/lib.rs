//! Slack Notifier: delivers tuliprox notifications to a Slack incoming webhook.
//!
//! `notifications.send` posts one notification to a Slack webhook
//! ([`slack`]). The host applies routing, quiet hours and suppression and
//! owns the retrying outbox; this plugin formats the message and answers each
//! attempt with a [`DeliveryError`] the outbox understands.
//!
//! The channel config is a `ui.config_extensions` fragment of the Web UI messaging page, stored under
//! `settings.ext.messaging`; there is no UI code and no standalone settings form.

mod slack;

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
        pub url: Secret<String>,
        #[serde(default)]
        pub style: crate::slack::Style,
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

    pub struct SlackNotifier;

    impl NotificationHooks for SlackNotifier {
        fn send_notification(msg: Notification) -> Result<(), DeliveryError> {
            // Settings that no longer parse will not parse on a retry either.
            let settings = Settings::current().map_err(DeliveryError::Permanent)?;
            let settings = &settings.ext.messaging;
            crate::slack::deliver(settings.url.expose(), settings.style, &msg)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuliprox_plugin_sdk::mock;

    const HOOK: &str = "https://hooks.slack.test/services/T000/B000/SECRET-TOKEN";

    /// The one test that goes through the settings static: it is process-wide,
    /// so no other test may call `Settings::current`.
    #[test]
    fn the_hook_reads_the_webhook_url_from_the_host_config_and_never_logs_it() {
        mock::reset();
        mock::set_config(&format!(
            r#"{{"ext":{{"messaging":{{"url":"{HOOK}","routing":{{"min_severity":"error"}}}}}}}}"#
        ));
        assert!(plugin::Settings::reload().is_ok());
        let settings = plugin::Settings::current().unwrap();
        let settings = &settings.ext.messaging;
        assert!(!format!("{:?}", settings.url).contains("SECRET-TOKEN"), "a Secret never prints its value");
        assert_eq!(settings.style, slack::Style::Blocks, "blocks is the default style");

        let msg = || Notification {
            title: "Update".to_string(),
            body: "Update done".to_string(),
            severity: Severity::Info,
            payload: "{}".to_string(),
        };
        mock::http_respond(http::Method::Post, HOOK, 200, "ok");
        assert!(plugin::SlackNotifier::send_notification(msg()).is_ok());
        assert_eq!(mock::http_requests().len(), 1);
        assert!(mock::logs().iter().all(|line| !line.message.contains("SECRET-TOKEN")));

        mock::http_fail(http::Method::Post, HOOK, &format!("dial {HOOK} failed"));
        let err = plugin::SlackNotifier::send_notification(msg()).unwrap_err();
        assert!(!format!("{err:?}").contains("SECRET-TOKEN"), "a transport error never carries the url");
        assert!(mock::logs().iter().all(|line| !line.message.contains("SECRET-TOKEN")));
    }

    #[test]
    fn the_module_exports_exactly_the_manifest_hooks() {
        assert_eq!(plugin::HOOK_IDS, ["notifications.send"]);
        assert_eq!(plugin::EXPORTS, ["tuliprox:plugin/lifecycle@1.0.0", "tuliprox:plugin/notification-hooks@1.0.0"]);
    }
}

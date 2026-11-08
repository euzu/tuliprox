//! Pushover Notifier: delivers tuliprox notifications to Pushover.
//!
//! `notifications.send` posts one notification to the Pushover messages API
//! ([`pushover`]). The host applies routing, quiet hours and suppression and
//! owns the retrying outbox; this plugin formats the message and answers each
//! attempt with a [`DeliveryError`] the outbox understands.
//!
//! The channel config is a `ui.config_extensions` fragment of the Web UI messaging page, stored under
//! `settings.ext.messaging`; there is no UI code and no standalone settings form.

mod pushover;

use tuliprox_plugin_sdk::prelude::*;

#[tuliprox_plugin(manifest = "manifest.yml")]
mod plugin {
    use super::*;

    fn default_url() -> String { crate::pushover::DEFAULT_URL.to_string() }

    /// The channel config: the `ext.messaging` object of the plugin settings
    /// (`ui.config_extensions` in `manifest.yml`). `routing` is read by the
    /// host, not by the plugin, so it is not a field here.
    #[derive(serde::Deserialize)]
    pub struct Channel {
        #[serde(default)]
        pub token: Secret<String>,
        #[serde(default)]
        pub user: Secret<String>,
        #[serde(default = "default_url")]
        pub url: String,
        #[serde(default)]
        pub style: crate::pushover::Style,
    }

    impl Default for Channel {
        fn default() -> Self {
            Self { token: Secret::default(), user: Secret::default(), url: default_url(), style: Default::default() }
        }
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

    pub struct PushoverNotifier;

    impl NotificationHooks for PushoverNotifier {
        fn send_notification(msg: Notification) -> Result<(), DeliveryError> {
            // Settings that no longer parse will not parse on a retry either.
            let settings = Settings::current().map_err(DeliveryError::Permanent)?;
            let settings = &settings.ext.messaging;
            let target = crate::pushover::Target {
                url: &settings.url,
                token: settings.token.expose(),
                user: settings.user.expose(),
                style: settings.style,
            };
            crate::pushover::deliver(&target, &msg)
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
    fn the_hook_reads_its_settings_from_the_host_config_and_never_logs_the_credentials() {
        mock::reset();
        mock::set_config(
            r#"{"ext":{"messaging":{"token":"SECRET-APP","user":"SECRET-USER","routing":{"min_severity":"error"}}}}"#,
        );
        assert!(plugin::Settings::reload().is_ok());
        let settings = plugin::Settings::current().unwrap();
        let settings = &settings.ext.messaging;
        assert_eq!(settings.url, pushover::DEFAULT_URL, "the API url defaults to Pushover's");
        let shown = format!("{:?} {:?}", settings.token, settings.user);
        assert!(!shown.contains("SECRET"), "a Secret never prints its value");
        assert_eq!(settings.style, pushover::Style::Text, "text is the default style");

        let msg = || Notification {
            title: "Update".to_string(),
            body: "Update done".to_string(),
            severity: Severity::Info,
            payload: "{}".to_string(),
        };
        mock::http_respond(http::Method::Post, pushover::DEFAULT_URL, 200, r#"{"status":1}"#);
        assert!(plugin::PushoverNotifier::send_notification(msg()).is_ok());
        let sent = mock::http_requests();
        assert_eq!(sent.len(), 1);
        assert!(!sent[0].url().contains("SECRET"), "no credential is ever in the url");
        assert!(mock::logs().iter().all(|line| !line.message.contains("SECRET")));

        mock::http_fail(http::Method::Post, pushover::DEFAULT_URL, "dial failed: token=SECRET-APP user=SECRET-USER");
        let err = plugin::PushoverNotifier::send_notification(msg()).unwrap_err();
        assert!(!format!("{err:?}").contains("SECRET"), "a transport error never carries a credential");
        assert!(mock::logs().iter().all(|line| !line.message.contains("SECRET")));
    }

    #[test]
    fn the_module_exports_exactly_the_manifest_hooks() {
        assert_eq!(plugin::HOOK_IDS, ["notifications.send"]);
        assert_eq!(plugin::EXPORTS, ["tuliprox:plugin/lifecycle@1.0.0", "tuliprox:plugin/notification-hooks@1.0.0"]);
    }
}

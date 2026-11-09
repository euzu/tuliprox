//! Matrix Notifier: an example notification-provider plugin.
//!
//! * `notifications.send` delivers a tuliprox notification to one Matrix room
//!   ([`matrix`]). The host's `Channel::Plugin` supplies routing, quiet hours,
//!   suppression and the retrying outbox; this plugin answers each attempt with
//!   a [`DeliveryError`] the outbox understands.
//! * `events.on-event` counts and logs the bus events it subscribed to
//!   ([`tally`]) and never sends.
//!
//! The channel config is a `ui.config_extensions` fragment of the Web UI messaging page, stored under
//! `settings.ext.messaging`; there is no UI code and no standalone settings form.

mod matrix;
mod tally;

use tuliprox_plugin_sdk::prelude::*;

#[tuliprox_plugin(manifest = "manifest.yml")]
mod plugin {
    use super::*;
    use crate::matrix::{default_homeserver, deliver, MinSeverity, Target};

    /// The channel config: the `ext.messaging` object of the plugin settings
    /// (`ui.config_extensions` in `manifest.yml`). `routing` is read by the
    /// host, not by the plugin, so it is not a field here.
    #[derive(serde::Deserialize)]
    pub struct Channel {
        #[serde(default = "default_homeserver")]
        pub homeserver: String,
        #[serde(default)]
        pub room_id: String,
        #[serde(default)]
        pub access_token: Secret<String>,
        #[serde(default)]
        pub min_severity: MinSeverity,
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

    impl Default for Channel {
        fn default() -> Self {
            Self {
                homeserver: default_homeserver(),
                room_id: String::new(),
                access_token: Secret::default(),
                min_severity: MinSeverity::default(),
            }
        }
    }

    impl Channel {
        pub fn target(&self) -> Target<'_> {
            Target {
                homeserver: &self.homeserver,
                room_id: &self.room_id,
                access_token: self.access_token.expose(),
                min_severity: self.min_severity,
            }
        }
    }

    pub struct MatrixNotifier;

    impl NotificationHooks for MatrixNotifier {
        fn send_notification(msg: Notification) -> Result<(), DeliveryError> {
            // Settings that no longer parse will not parse on a retry either.
            let settings = Settings::current().map_err(DeliveryError::Permanent)?;
            let settings = &settings.ext.messaging;
            deliver(&settings.target(), &msg)
        }
    }

    impl EventHooks for MatrixNotifier {
        fn on_event(ev: Event) -> Result<(), String> { crate::tally::record(&ev) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuliprox_plugin_sdk::mock;

    /// The one test that goes through the settings static: it is process-wide,
    /// so no other test may call `Settings::current`.
    #[test]
    fn the_hooks_read_their_settings_from_the_host_config_and_never_log_the_token() {
        mock::reset();
        mock::set_now(500);
        mock::set_config(
            r#"{"ext":{"messaging":{"room_id":"!demo:localhost","access_token":"tok-123","min_severity":"warn","routing":{"min_severity":"error"}}}}"#,
        );
        assert!(plugin::Settings::reload().is_ok());
        let settings = plugin::Settings::current().unwrap();
        let settings = &settings.ext.messaging;
        assert_eq!(settings.homeserver, "http://127.0.0.1:18972", "the homeserver defaults to the demo mock");
        assert!(!format!("{:?}", settings.access_token).contains("tok-123"), "a Secret never prints its value");

        let info = Notification {
            title: "Update".to_string(),
            body: "Update".to_string(),
            severity: Severity::Info,
            payload: "{}".to_string(),
        };
        assert!(plugin::MatrixNotifier::send_notification(info).is_ok(), "below min_severity: acknowledged");
        assert!(mock::http_requests().is_empty());

        let error = Notification {
            title: "Boom".to_string(),
            body: "Boom".to_string(),
            severity: Severity::Error,
            payload: "{}".to_string(),
        };
        let url = format!(
            "http://127.0.0.1:18972/_matrix/client/v3/rooms/%21demo%3Alocalhost/send/m.room.message/{}",
            matrix::txn_id(&matrix::fingerprint(&error), 500)
        );
        mock::http_respond(http::Method::Put, &url, 200, "{}");
        assert!(plugin::MatrixNotifier::send_notification(error).is_ok());
        let sent = mock::http_requests();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].headers().contains(&("authorization".to_string(), "Bearer tok-123".to_string())));
        assert!(mock::logs().iter().all(|line| !line.message.contains("tok-123")));
    }

    #[test]
    fn the_module_exports_exactly_the_manifest_hooks() {
        assert_eq!(plugin::HOOK_IDS, ["notifications.send", "events.on-event"]);
        assert_eq!(
            plugin::EXPORTS,
            [
                "tuliprox:plugin/lifecycle@1.0.0",
                "tuliprox:plugin/event-hooks@1.0.0",
                "tuliprox:plugin/notification-hooks@1.0.0",
            ]
        );
    }
}

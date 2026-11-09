//! REST Notifier: delivers tuliprox notifications to any HTTP endpoint.
//!
//! `notifications.send` sends one notification as an HTTP request
//! ([`rest`]): the event as JSON (or its plain-text body), your own headers, and an
//! optional HMAC signature the receiver can verify. The host applies routing,
//! quiet hours and suppression and owns the retrying outbox; this plugin builds
//! the request and answers each attempt with a [`DeliveryError`] the outbox
//! understands.
//!
//! The channel config is a `ui.config_extensions` fragment of the Web UI messaging page, stored under
//! `settings.ext.messaging`; there is no UI code and no standalone settings form.

mod rest;

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
        pub url: String,
        #[serde(default)]
        pub method: crate::rest::Method,
        #[serde(default)]
        pub body: crate::rest::Body,
        #[serde(default)]
        pub headers: Vec<String>,
        #[serde(default)]
        pub secret_headers: Vec<Secret<String>>,
        #[serde(default)]
        pub signing_secret: Secret<String>,
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

    pub struct RestNotifier;

    impl NotificationHooks for RestNotifier {
        fn send_notification(msg: Notification) -> Result<(), DeliveryError> {
            // Settings that no longer parse will not parse on a retry either.
            let settings = Settings::current().map_err(DeliveryError::Permanent)?;
            let settings = &settings.ext.messaging;
            let secret_headers: Vec<&str> =
                settings.secret_headers.iter().map(Secret::expose).map(String::as_str).collect();
            let target = crate::rest::Target {
                url: &settings.url,
                method: settings.method,
                body: settings.body,
                headers: &settings.headers,
                secret_headers: &secret_headers,
                signing_secret: settings.signing_secret.expose(),
            };
            crate::rest::deliver(&target, &msg)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuliprox_plugin_sdk::mock;

    const URL: &str = "https://hooks.test/in";

    /// The one test that goes through the settings static: it is process-wide,
    /// so no other test may call `Settings::current`.
    #[test]
    fn the_hook_reads_its_settings_from_the_host_config_and_never_logs_a_secret() {
        mock::reset();
        mock::set_config(
            r#"{"ext":{"messaging":{"url":"https://hooks.test/in","headers":["X-Env: prod"],"secret_headers":["Authorization: Bearer SECRET-BEARER"],"signing_secret":"SECRET-SIGN","routing":{"min_severity":"error"}}}}"#,
        );
        assert!(plugin::Settings::reload().is_ok());
        let settings = plugin::Settings::current().unwrap();
        let settings = &settings.ext.messaging;
        assert_eq!(settings.method, rest::Method::Post, "POST is the default method");
        assert_eq!(settings.body, rest::Body::Event, "the event is the default body");
        let shown = format!("{:?} {:?}", settings.secret_headers, settings.signing_secret);
        assert!(!shown.contains("SECRET"), "a Secret never prints its value: {shown}");

        let msg = || Notification {
            title: "Update".to_string(),
            body: "Update done".to_string(),
            severity: Severity::Info,
            payload: r#"{"id":"system.info","title":"Update"}"#.to_string(),
        };
        mock::http_respond(http::Method::Post, URL, 200, "{}");
        assert!(plugin::RestNotifier::send_notification(msg()).is_ok());
        let sent = mock::http_requests();
        assert_eq!(sent.len(), 1);
        let has = |name: &str, value: Option<&str>| {
            sent[0]
                .headers()
                .iter()
                .any(|(n, v)| n.eq_ignore_ascii_case(name) && value.is_none_or(|expected| v == expected))
        };
        assert!(has("authorization", Some("Bearer SECRET-BEARER")));
        assert!(has("x-env", Some("prod")));
        assert!(has("x-tuliprox-signature", None));
        assert!(mock::logs().iter().all(|line| !line.message.contains("SECRET")));

        mock::http_fail(http::Method::Post, URL, "dial failed (Authorization: Bearer SECRET-BEARER, SECRET-SIGN)");
        let err = plugin::RestNotifier::send_notification(msg()).unwrap_err();
        assert!(!format!("{err:?}").contains("SECRET"), "a transport error never carries a secret");
        assert!(mock::logs().iter().all(|line| !line.message.contains("SECRET")));
    }

    #[test]
    fn the_module_exports_exactly_the_manifest_hooks() {
        assert_eq!(plugin::HOOK_IDS, ["notifications.send"]);
        assert_eq!(plugin::EXPORTS, ["tuliprox:plugin/lifecycle@1.0.0", "tuliprox:plugin/notification-hooks@1.0.0"]);
    }
}

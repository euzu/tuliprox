//! Command Notifier: runs a local program for each tuliprox notification.
//!
//! `notifications.send` starts the configured program ([`command`]) with the event as JSON
//! on its standard input. The host applies routing, quiet hours and suppression and owns the
//! retrying outbox; this plugin runs the program and answers each attempt with a
//! [`DeliveryError`] the outbox understands.
//!
//! The settings form is generated from `config-schema.json`; there is no UI code.

mod command;

use tuliprox_plugin_sdk::prelude::*;

#[tuliprox_plugin(manifest = "manifest.yml")]
mod plugin {
    use super::*;

    /// The plugin settings (`config-schema.json`). `routing` is read by the
    /// host, not by the plugin, so it is not a field here.
    #[tuliprox_plugin_settings]
    pub struct Settings {
        #[serde(default)]
        pub program: String,
        #[serde(default)]
        pub args: Vec<String>,
        #[serde(default)]
        pub timeout_secs: Option<u32>,
    }

    pub struct CommandNotifier;

    impl NotificationHooks for CommandNotifier {
        fn send_notification(msg: Notification) -> Result<(), DeliveryError> {
            // Settings that no longer parse will not parse on a retry either.
            let settings = Settings::current().map_err(DeliveryError::Permanent)?;
            crate::command::deliver(&settings.program, &settings.args, settings.timeout_secs, &msg)
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
    fn the_hook_reads_its_settings_from_the_host_config_and_runs_the_program() {
        mock::reset();
        mock::set_config(
            r#"{"program":"/config/notify.sh","args":["--from-tuliprox"],"timeout_secs":5,"routing":{"min_severity":"error"}}"#,
        );
        assert!(plugin::Settings::reload().is_ok());
        let settings = plugin::Settings::current().unwrap();
        assert_eq!(settings.program, "/config/notify.sh");
        assert_eq!(settings.args, ["--from-tuliprox"]);
        assert_eq!(settings.timeout_secs, Some(5));

        mock::process_respond(
            "/config/notify.sh",
            Ok(process::Output { status: Some(0), stdout: Vec::new(), stderr: Vec::new() }),
        );
        let msg = Notification {
            title: "Update".to_string(),
            body: "Update done".to_string(),
            severity: Severity::Info,
            payload: r#"{"id":"system.info","severity":"info"}"#.to_string(),
        };
        assert!(plugin::CommandNotifier::send_notification(msg).is_ok());
        let runs = mock::process_runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].args, ["--from-tuliprox"]);
        assert_eq!(runs[0].timeout_secs, 5);
    }

    #[test]
    fn the_module_exports_exactly_the_manifest_hooks() {
        assert_eq!(plugin::HOOK_IDS, ["notifications.send"]);
        assert_eq!(plugin::EXPORTS, ["tuliprox:plugin/lifecycle@1.0.0", "tuliprox:plugin/notification-hooks@1.0.0"]);
    }
}

//! Typed settings and secrets.
//!
//! ```ignore
//! #[tuliprox_plugin_settings]
//! struct Settings {
//!     api_key: Secret<String>,
//!     #[serde(default = "default_hours")]
//!     refresh_hours: u32,
//! }
//!
//! let settings = Settings::current()?;          // parsed from get-config() once
//! client.header("x-api-key", settings.api_key.expose());
//! ```

use crate::config;
use serde::de::{Deserialize, DeserializeOwned, Deserializer};
use std::{
    fmt,
    sync::{Arc, Mutex, PoisonError},
};

/// A value that must never reach a log line, an error message or a panic.
///
/// `Debug` and `Display` print `********`; the value is only reachable through
/// [`Secret::expose`]. It deserializes transparently from the settings JSON
/// (the host resolves secrets before the plugin sees them) and has no
/// `Serialize` impl, so it cannot be written back out by accident.
#[derive(Clone, Default)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    pub const fn new(value: T) -> Self { Self(value) }

    /// The secret value. Do not log it, put it in an error, or store it in the KV.
    pub const fn expose(&self) -> &T { &self.0 }

    pub fn into_inner(self) -> T { self.0 }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str("Secret(********)") }
}

impl<T> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str("********") }
}

impl<T> From<T> for Secret<T> {
    fn from(value: T) -> Self { Self(value) }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Secret<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(deserializer).map(Self)
    }
}

/// The cell `#[tuliprox_plugin_settings]` puts in a `static` per settings type.
#[doc(hidden)]
pub struct SettingsSlot<T>(Mutex<Option<Arc<T>>>);

impl<T> SettingsSlot<T> {
    #[must_use]
    pub const fn new() -> Self { Self(Mutex::new(None)) }

    fn get(&self) -> Option<Arc<T>> { self.0.lock().unwrap_or_else(PoisonError::into_inner).clone() }

    fn put(&self, value: Arc<T>) { *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(value); }
}

impl<T> Default for SettingsSlot<T> {
    fn default() -> Self { Self::new() }
}

/// Implemented by `#[tuliprox_plugin_settings]`.
pub trait PluginSettings: DeserializeOwned + Send + Sync + Sized + 'static {
    #[doc(hidden)]
    fn slot() -> &'static SettingsSlot<Self>;

    /// Parses a settings JSON document. An empty string is `{}`.
    fn parse(raw: &str) -> Result<Self, String> {
        let raw = if raw.trim().is_empty() { "{}" } else { raw };
        serde_json::from_str(raw).map_err(|err| format!("invalid plugin settings: {err}"))
    }

    /// Parses and stores the settings. This is what the default `lifecycle`
    /// calls with the `init` string, so a bad configuration disables the
    /// instance with the parse error.
    fn init(raw: &str) -> Result<(), String> {
        let parsed = Self::parse(raw)?;
        Self::slot().put(Arc::new(parsed));
        Ok(())
    }

    /// The settings stored by [`init`](Self::init), or, when `init` did not run
    /// (a unit test, a custom `lifecycle`), the ones parsed from `get-config()`.
    fn current() -> Result<Arc<Self>, String> { Self::slot().get().map_or_else(Self::reload, Ok) }

    /// Parses `get-config()` afresh and stores it.
    fn reload() -> Result<Arc<Self>, String> {
        let parsed = Arc::new(Self::parse(&config::raw())?);
        Self::slot().put(Arc::clone(&parsed));
        Ok(parsed)
    }
}

/// A chain of settings migrations, one step per old schema version.
///
/// The settings schema carries a top-level `"version"` (default 1). When a
/// plugin update raises it, the host calls the plugin's
/// [`SettingsMigration`](crate::SettingsMigration) export with the stored
/// version and JSON. `Migrations` is the usual body of that export: each
/// [`step`](Self::step) turns the settings of version `n` into those of `n + 1`,
/// and [`run`](Self::run) applies the steps from the stored version up to the
/// newest one.
///
/// ```ignore
/// impl SettingsMigration for Plugin {
///     fn migrate(from_version: u32, settings: &str) -> Result<String, String> {
///         Migrations::new()
///             .step(1, |mut v| { rename(&mut v, "host", "endpoint"); Ok(v) })
///             .run(from_version, settings)
///     }
/// }
/// ```
#[derive(Default)]
pub struct Migrations {
    steps: Vec<(u32, Step)>,
}

type Step = Box<dyn Fn(serde_json::Value) -> Result<serde_json::Value, String>>;

impl Migrations {
    #[must_use]
    pub fn new() -> Self { Self::default() }

    /// The step that migrates settings of version `from` to version `from + 1`.
    #[must_use]
    pub fn step(
        mut self,
        from: u32,
        migrate: impl Fn(serde_json::Value) -> Result<serde_json::Value, String> + 'static,
    ) -> Self {
        self.steps.push((from, Box::new(migrate)));
        self
    }

    /// The version the steps lead to: one past the highest step.
    #[must_use]
    pub fn latest(&self) -> u32 { self.steps.iter().map(|(from, _)| *from + 1).max().unwrap_or(1) }

    /// Migrate `settings` (JSON, an empty string is `{}`) from `from_version`
    /// to [`latest`](Self::latest) and return the JSON.
    ///
    /// # Errors
    /// The settings are not a JSON object, a step in the chain is missing, or a
    /// step failed. A `from_version` that is already the latest returns the
    /// settings unchanged; a newer one is an error (nothing can migrate down).
    pub fn run(&self, from_version: u32, settings: &str) -> Result<String, String> {
        let raw = if settings.trim().is_empty() { "{}" } else { settings };
        let mut value: serde_json::Value =
            serde_json::from_str(raw).map_err(|err| format!("the stored settings are not JSON: {err}"))?;
        if !value.is_object() {
            return Err("the stored settings are not a JSON object".to_string());
        }
        let latest = self.latest();
        if from_version > latest {
            return Err(format!("the settings are at version {from_version}, newer than the latest known, {latest}"));
        }
        for version in from_version..latest {
            let (_, step) = self
                .steps
                .iter()
                .find(|(from, _)| *from == version)
                .ok_or_else(|| format!("no migration from settings version {version} to {}", version + 1))?;
            value = step(value).map_err(|err| format!("migration from version {version}: {err}"))?;
        }
        Ok(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock;

    #[derive(serde::Deserialize)]
    struct Fields {
        api_key: Secret<String>,
        #[serde(default)]
        hours: u32,
    }

    impl PluginSettings for Fields {
        fn slot() -> &'static SettingsSlot<Self> {
            static SLOT: SettingsSlot<Fields> = SettingsSlot::new();
            &SLOT
        }
    }

    #[test]
    fn a_secret_never_prints_its_value() {
        let secret = Secret::new("hunter2".to_string());
        assert_eq!(format!("{secret}"), "********");
        assert_eq!(format!("{secret:?}"), "Secret(********)");
        assert_eq!(format!("{secret:#?}"), "Secret(********)");
        assert_eq!(secret.expose(), "hunter2");

        let parsed = Fields::parse(r#"{"api_key":"hunter2","hours":3}"#).unwrap();
        assert!(!format!("{:?}", parsed.api_key).contains("hunter2"));
        assert_eq!(parsed.api_key.expose(), "hunter2");
        assert_eq!(parsed.hours, 3);
    }

    #[test]
    fn settings_come_from_init_or_get_config() {
        mock::reset();
        mock::set_config(r#"{"api_key":"from-config"}"#);
        let loaded = Fields::reload().unwrap();
        assert_eq!(loaded.api_key.expose(), "from-config");
        assert_eq!(loaded.hours, 0, "serde defaults apply");

        Fields::init(r#"{"api_key":"from-init","hours":9}"#).unwrap();
        assert_eq!(Fields::current().unwrap().api_key.expose(), "from-init");
        assert!(Fields::init("{}").unwrap_err().contains("api_key"), "a missing field names itself");
    }

    fn rename(value: &mut serde_json::Value, from: &str, to: &str) {
        if let Some(object) = value.as_object_mut() {
            if let Some(moved) = object.remove(from) {
                object.insert(to.to_string(), moved);
            }
        }
    }

    fn chain() -> Migrations {
        Migrations::new()
            .step(1, |mut value| {
                rename(&mut value, "host", "endpoint");
                Ok(value)
            })
            .step(2, |mut value| {
                let object = value.as_object_mut().ok_or("not an object")?;
                object.entry("retries").or_insert(serde_json::json!(3));
                Ok(value)
            })
    }

    fn parsed(text: &str) -> serde_json::Value { serde_json::from_str(text).unwrap() }

    #[test]
    fn migrations_chain_the_steps_from_the_stored_version() {
        let migrations = chain();
        assert_eq!(migrations.latest(), 3);
        let from_one = migrations.run(1, r#"{"host":"a","keep":true}"#).unwrap();
        assert_eq!(parsed(&from_one), serde_json::json!({"endpoint": "a", "keep": true, "retries": 3}));
        let from_two = migrations.run(2, r#"{"endpoint":"b"}"#).unwrap();
        assert_eq!(parsed(&from_two), serde_json::json!({"endpoint": "b", "retries": 3}));
        assert_eq!(parsed(&migrations.run(1, "").unwrap()), serde_json::json!({"retries": 3}));
    }

    #[test]
    fn the_latest_version_is_returned_unchanged_and_a_newer_one_is_refused() {
        let migrations = chain();
        assert_eq!(parsed(&migrations.run(3, r#"{"endpoint":"c"}"#).unwrap()), serde_json::json!({"endpoint": "c"}));
        assert!(migrations.run(4, "{}").unwrap_err().contains("newer"));
        assert_eq!(Migrations::new().latest(), 1);
        assert!(Migrations::new().run(1, "{}").is_ok());
    }

    #[test]
    fn a_gap_a_failing_step_and_bad_input_are_errors() {
        let gap = Migrations::new().step(2, Ok);
        assert!(gap.run(1, "{}").unwrap_err().contains("no migration from settings version 1"));
        let failing = Migrations::new().step(1, |_| Err("boom".to_string()));
        assert_eq!(failing.run(1, "{}").unwrap_err(), "migration from version 1: boom");
        assert!(chain().run(1, "[1]").unwrap_err().contains("not a JSON object"));
        assert!(chain().run(1, "{oops").unwrap_err().contains("not JSON"));
    }
}

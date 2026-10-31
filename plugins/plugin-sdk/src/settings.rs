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
}

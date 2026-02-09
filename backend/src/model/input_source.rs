use std::collections::HashMap;
use std::sync::Arc;
use shared::model::InputFetchMethod;
use crate::model::{ConfigInput, ConfigProvider, StagedInput};

/// Represents an input source for fetching content.
/// 
/// When created from a `ConfigInput` that uses the `provider://` scheme,
/// the provider context is preserved to enable URL failover on errors.
#[derive(Clone, Debug)]
pub struct InputSource {
    pub name: Arc<str>,
    pub url: String,
    /// The provider associated with this input, if the URL uses `provider://` scheme.
    /// This enables failover to alternative URLs when the current URL fails.
    pub provider: Option<Arc<ConfigProvider>>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub method: InputFetchMethod,
    pub headers: HashMap<String, String>,
}

impl InputSource {
    /// Creates a new `InputSource` with a different URL while preserving provider context.
    /// 
    /// The provider is preserved so that failover can occur even when the URL
    /// is derived from the original (e.g., adding query parameters, changing paths).
    pub fn with_url(&self, url: String) -> Self {
        Self {
            name: self.name.clone(),
            url,
            provider: self.provider.clone(),
            username: self.username.clone(),
            password: self.password.clone(),
            method: self.method,
            headers: self.headers.clone(),
        }
    }

    /// Returns the provider for this input source, if available.
    /// 
    /// This can be used to pass to `send_with_retry_and_provider` for failover support.
    #[inline]
    pub fn get_provider(&self) -> Option<&Arc<ConfigProvider>> {
        self.provider.as_ref()
    }
}

impl From<&ConfigInput> for InputSource {
    fn from(input: &ConfigInput) -> Self {
        Self {
            name: input.name.clone(),
            url: input.url.clone(),
            provider: input.get_resolve_provider(&input.url),
            username: input.username.clone(),
            password: input.password.clone(),
            method: input.method,
            headers: input.headers.clone(),
        }
    }
}

impl From<&StagedInput> for InputSource {
    fn from(input: &StagedInput) -> Self {
        Self {
            name: input.name.clone(),
            url: input.url.clone(),
            provider: input.provider_config.clone(),
            username: input.username.clone(),
            password: input.password.clone(),
            method: input.method,
            headers: input.headers.clone(),
        }
    }
}
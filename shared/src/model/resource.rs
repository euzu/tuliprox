use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Deserializer, Serialize};
use std::{fmt, sync::Arc};
use url::Url;

pub const RESOURCE_LOCATOR_PREFIX: &str = "resource://v1/";
const RESOURCE_SCHEME_PREFIX: &str = "resource://";
const MAX_RESOURCE_LOCATOR_ENCODED_BYTES: usize = 16 * 1024;
const MAX_RESOURCE_LOCATOR_PAYLOAD_BYTES: usize = 12 * 1024;

/// Internal representation of a HTTP(S) resource and the canonical input that supplied it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResourceLocator {
    #[serde(with = "crate::utils::arc_str_serde")]
    pub input_name: Arc<str>,
    #[serde(with = "crate::utils::arc_str_serde")]
    pub url: Arc<str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceLocatorError {
    MissingPrefix,
    TooLarge,
    InvalidEncoding,
    InvalidPayload,
    EmptyInput,
    UnsupportedScheme,
}

impl fmt::Display for ResourceLocatorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingPrefix => f.write_str("resource locator has no supported prefix"),
            Self::TooLarge => f.write_str("resource locator exceeds the size limit"),
            Self::InvalidEncoding => f.write_str("resource locator encoding is invalid"),
            Self::InvalidPayload => f.write_str("resource locator payload is invalid"),
            Self::EmptyInput => f.write_str("resource locator input is empty"),
            Self::UnsupportedScheme => f.write_str("resource locator URL scheme is unsupported"),
        }
    }
}

impl std::error::Error for ResourceLocatorError {}

impl ResourceLocator {
    pub fn new(input_name: Arc<str>, url: Arc<str>) -> Result<Self, ResourceLocatorError> {
        if input_name.is_empty() {
            return Err(ResourceLocatorError::EmptyInput);
        }
        if !is_http_resource_url(&url) {
            return Err(ResourceLocatorError::UnsupportedScheme);
        }
        Ok(Self { input_name, url })
    }

    pub fn encode(&self) -> Result<Arc<str>, ResourceLocatorError> {
        let payload = rmp_serde::to_vec(self).map_err(|_| ResourceLocatorError::InvalidPayload)?;
        if payload.len() > MAX_RESOURCE_LOCATOR_PAYLOAD_BYTES {
            return Err(ResourceLocatorError::TooLarge);
        }
        let encoded = URL_SAFE_NO_PAD.encode(payload);
        if encoded.len() > MAX_RESOURCE_LOCATOR_ENCODED_BYTES {
            return Err(ResourceLocatorError::TooLarge);
        }
        Ok(format!("{RESOURCE_LOCATOR_PREFIX}{encoded}").into())
    }

    pub fn decode(value: &str) -> Result<Self, ResourceLocatorError> {
        let encoded = value.strip_prefix(RESOURCE_LOCATOR_PREFIX).ok_or(ResourceLocatorError::MissingPrefix)?;
        if encoded.is_empty() {
            return Err(ResourceLocatorError::InvalidEncoding);
        }
        if encoded.len() > MAX_RESOURCE_LOCATOR_ENCODED_BYTES {
            return Err(ResourceLocatorError::TooLarge);
        }
        let payload = URL_SAFE_NO_PAD.decode(encoded).map_err(|_| ResourceLocatorError::InvalidEncoding)?;
        if payload.len() > MAX_RESOURCE_LOCATOR_PAYLOAD_BYTES {
            return Err(ResourceLocatorError::TooLarge);
        }
        let locator = rmp_serde::from_slice::<Self>(&payload).map_err(|_| ResourceLocatorError::InvalidPayload)?;
        Self::new(locator.input_name, locator.url)
    }
}

pub fn has_resource_scheme(value: &str) -> bool { value.starts_with(RESOURCE_SCHEME_PREFIX) }

/// Provider-facing serde formats are human-readable (JSON/XML adapters). Reserved locators are
/// accepted only from the binary repository representation and are also checked by the explicit
/// ingestion traversal used by manual parsers.
pub fn deserialize_untrusted_resource_arc<'de, D>(deserializer: D) -> Result<Arc<str>, D::Error>
where
    D: Deserializer<'de>,
{
    let human_readable = deserializer.is_human_readable();
    let mut value = crate::utils::arc_str_none_default_on_null(deserializer)?;
    if human_readable && has_resource_scheme(&value) {
        value = Arc::from("");
    }
    Ok(value)
}

pub fn deserialize_untrusted_resource_option<'de, D>(deserializer: D) -> Result<Option<Arc<str>>, D::Error>
where
    D: Deserializer<'de>,
{
    let human_readable = deserializer.is_human_readable();
    let mut value = crate::utils::deserialize_as_option_arc_str(deserializer)?;
    if human_readable && value.as_deref().is_some_and(has_resource_scheme) {
        value = None;
    }
    Ok(value)
}

pub fn deserialize_untrusted_resource_array<'de, D>(deserializer: D) -> Result<Option<Vec<Arc<str>>>, D::Error>
where
    D: Deserializer<'de>,
{
    let human_readable = deserializer.is_human_readable();
    let mut values = crate::utils::deserialize_as_string_array(deserializer)?;
    if human_readable {
        if let Some(values) = &mut values {
            values.retain(|value| !has_resource_scheme(value));
        }
    }
    Ok(values)
}

pub fn is_http_resource_url(value: &str) -> bool {
    Url::parse(value).is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some())
}

/// Converts one untrusted provider value into the trusted internal representation.
/// Reserved locators are cleared so provider data can never make an authorization claim.
pub fn ingest_resource_value(value: &mut Arc<str>, input_name: &Arc<str>) -> Result<(), ResourceLocatorError> {
    if value.is_empty() {
        return Ok(());
    }
    if has_resource_scheme(value) {
        *value = Arc::from("");
        return Err(ResourceLocatorError::UnsupportedScheme);
    }
    if is_http_resource_url(value) {
        *value = ResourceLocator::new(Arc::clone(input_name), Arc::clone(value))?.encode()?;
    }
    Ok(())
}

pub fn ingest_optional_resource_value(
    value: &mut Option<Arc<str>>,
    input_name: &Arc<str>,
) -> Result<(), ResourceLocatorError> {
    if let Some(resource) = value {
        if let Err(error) = ingest_resource_value(resource, input_name) {
            *value = None;
            return Err(error);
        }
    }
    Ok(())
}

/// Normalizes values after trusted internal transformations. Existing valid locators are
/// preserved; newly produced raw HTTP(S) values become owned by `input_name`.
pub fn normalize_internal_resource_value(
    value: &mut Arc<str>,
    input_name: &Arc<str>,
) -> Result<(), ResourceLocatorError> {
    if value.starts_with(RESOURCE_LOCATOR_PREFIX) {
        ResourceLocator::decode(value)?;
        return Ok(());
    }
    ingest_resource_value(value, input_name)
}

pub fn resolve_resource_value(value: &str) -> Result<Option<ResourceLocator>, ResourceLocatorError> {
    if value.starts_with(RESOURCE_LOCATOR_PREFIX) {
        return ResourceLocator::decode(value).map(Some);
    }
    if has_resource_scheme(value) {
        return Err(ResourceLocatorError::MissingPrefix);
    }
    Ok(None)
}

/// Converts the internal representation to the value safe for external output. Malformed
/// reserved values fail closed instead of leaking the internal scheme.
pub fn external_resource_value(value: &str) -> Arc<str> {
    match resolve_resource_value(value) {
        Ok(Some(locator)) => locator.url,
        Ok(None) => Arc::from(value),
        Err(_) => Arc::from(""),
    }
}

/// Authenticated payload of a rewritten resource link.
///
/// Routes whose path holds only an encoded URL have no stored record to read an origin from, so
/// the origin travels inside the link. The payload is authenticated, which also makes it
/// distinguishable from the two legacy encodings that carry a bare URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceToken {
    pub resource: String,
}

#[cfg(test)]
mod tests {
    use super::{
        external_resource_value, has_resource_scheme, ingest_resource_value, ResourceLocator, ResourceLocatorError,
        MAX_RESOURCE_LOCATOR_ENCODED_BYTES, RESOURCE_LOCATOR_PREFIX,
    };
    use crate::model::{LiveStreamProperties, StalkerPlaylistItem, VideoStreamProperties};
    use std::sync::Arc;

    #[test]
    fn locator_round_trips_http_urls_losslessly() {
        for url in [
            "http://example.com/image.png",
            "https://user:password@example.com:8443/a@b?q=a:b/c#fragment",
            "https://[2001:db8::1]:8443/image.png",
            "https://example.com/%E2%98%83?q=%C3%A4",
        ] {
            let locator = ResourceLocator::new(Arc::from("input:@/ä"), Arc::from(url)).expect("valid locator");
            let encoded = locator.encode().expect("encode locator");
            assert!(encoded.starts_with(RESOURCE_LOCATOR_PREFIX), "encoded starts with prefix");
            assert_eq!(ResourceLocator::decode(&encoded).expect("decode locator"), locator);
        }
    }

    #[test]
    fn locator_rejects_malformed_and_unsupported_values() {
        assert_eq!(ResourceLocator::decode("resource://v2/AAAA"), Err(ResourceLocatorError::MissingPrefix));
        assert_eq!(ResourceLocator::decode("resource://v1/***"), Err(ResourceLocatorError::InvalidEncoding));
        assert_eq!(
            ResourceLocator::new(Arc::from(""), Arc::from("https://example.com/a")),
            Err(ResourceLocatorError::EmptyInput)
        );
        assert_eq!(
            ResourceLocator::new(Arc::from("input"), Arc::from("")),
            Err(ResourceLocatorError::UnsupportedScheme)
        );
        for url in ["resource://v1/AAAA", "file:///tmp/a", "provider://a", "batch://a", "media-server://image/a"] {
            assert_eq!(
                ResourceLocator::new(Arc::from("input"), Arc::from(url)),
                Err(ResourceLocatorError::UnsupportedScheme)
            );
        }
        let oversized = format!("resource://v1/{}", "A".repeat(MAX_RESOURCE_LOCATOR_ENCODED_BYTES + 1));
        assert_eq!(ResourceLocator::decode(&oversized), Err(ResourceLocatorError::TooLarge));
    }

    #[test]
    fn provider_values_cannot_supply_internal_locators() {
        let locator = ResourceLocator::new(Arc::from("other-input"), Arc::from("https://private.example/a"))
            .expect("locator")
            .encode()
            .expect("encode");
        let mut forged = locator;
        assert!(ingest_resource_value(&mut forged, &Arc::from("provider-input")).is_err());
        assert!(forged.is_empty());
        assert!(has_resource_scheme("resource://v99/value"));
    }

    #[test]
    fn ingest_wraps_http_and_preserves_non_http_values() {
        let input = Arc::from("test-input");

        // HTTP URL
        let mut http_val = Arc::from("https://cdn.example.com/logo.png");
        assert!(ingest_resource_value(&mut http_val, &input).is_ok());
        assert!(http_val.starts_with(RESOURCE_LOCATOR_PREFIX));
        let decoded = ResourceLocator::decode(&http_val).expect("decode http");
        assert_eq!(decoded.url.as_ref(), "https://cdn.example.com/logo.png");
        assert_eq!(decoded.input_name.as_ref(), "test-input");

        // Dedicated non-HTTP schemes stay on their existing serving paths.
        let mut ms_val = Arc::from("media-server://image/plex/logo");
        assert!(ingest_resource_value(&mut ms_val, &input).is_ok());
        assert_eq!(ms_val.as_ref(), "media-server://image/plex/logo");

        // Local/internal paths are not proxied HTTP resources.
        let mut path_val = Arc::from("/path/to/logo.png");
        assert!(ingest_resource_value(&mut path_val, &input).is_ok());
        assert_eq!(path_val.as_ref(), "/path/to/logo.png");

        // Empty stays empty
        let mut empty_val = Arc::from("");
        assert!(ingest_resource_value(&mut empty_val, &input).is_ok());
        assert!(empty_val.is_empty());
    }

    #[test]
    fn external_resource_value_unwraps_locator() {
        let input = Arc::from("test-input");
        let url = "https://cdn.example.com/logo.png";
        let locator = ResourceLocator::new(input, Arc::from(url)).and_then(|l| l.encode()).expect("encode");
        assert_eq!(external_resource_value(&locator).as_ref(), url);
    }

    #[test]
    fn human_readable_provider_models_reject_reserved_locators() {
        let locator = ResourceLocator::new(Arc::from("other-input"), Arc::from("https://private.example/a"))
            .and_then(|locator| locator.encode())
            .expect("locator");

        let live: LiveStreamProperties =
            serde_json::from_value(serde_json::json!({ "stream_icon": locator })).expect("live JSON");
        assert!(live.stream_icon.is_empty());

        let video: VideoStreamProperties = serde_json::from_value(serde_json::json!({
            "stream_icon": locator,
            "details": {
                "cover_big": locator,
                "movie_image": locator,
                "backdrop_path": [locator]
            }
        }))
        .expect("video JSON");
        assert!(video.stream_icon.is_empty());
        let details = video.details.expect("details");
        assert!(details.cover_big.is_none());
        assert!(details.movie_image.is_none());
        assert!(details.backdrop_path.is_some_and(|values| values.is_empty()));

        let stalker: StalkerPlaylistItem = serde_json::from_value(serde_json::json!({
            "stream_id": 1,
            "name": "name",
            "category_id": 1,
            "category_name": "group",
            "number": 1,
            "logo_url": locator,
            "stream_url": "https://example.com/live",
            "stream_kind": "live",
            "cmd": ""
        }))
        .expect("Stalker JSON");
        assert!(stalker.logo_url.is_none());
    }

    #[test]
    fn binary_repository_values_preserve_internal_locators() {
        let locator = ResourceLocator::new(Arc::from("input"), Arc::from("https://example.com/logo.png"))
            .and_then(|locator| locator.encode())
            .expect("locator");
        let properties = LiveStreamProperties { stream_icon: Arc::clone(&locator), ..LiveStreamProperties::default() };

        let encoded = rmp_serde::to_vec(&properties).expect("serialize repository value");
        let decoded: LiveStreamProperties = rmp_serde::from_slice(&encoded).expect("deserialize repository value");

        assert_eq!(decoded.stream_icon, locator);
    }
}

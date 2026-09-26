use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::Deserialize;
use std::{borrow::Cow, sync::Arc};
use url::Url;

const LEGACY_PREFIX: &str = "resource://v1/";
const MAX_ENCODED_BYTES: usize = 16 * 1024;
const MAX_PAYLOAD_BYTES: usize = 12 * 1024;

#[derive(Deserialize)]
struct LegacyResourceLocator {
    input_name: String,
    url: String,
}

/// Decodes stored resource locators without accepting an invalid locator as a URL.
pub fn persisted_resource_url(value: &str) -> Option<Cow<'_, str>> {
    if !value.starts_with("resource://") {
        return Some(Cow::Borrowed(value));
    }
    let encoded = value.strip_prefix(LEGACY_PREFIX)?;
    if encoded.is_empty() || encoded.len() > MAX_ENCODED_BYTES {
        return None;
    }
    let payload = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    if payload.len() > MAX_PAYLOAD_BYTES {
        return None;
    }
    let locator = rmp_serde::from_slice::<LegacyResourceLocator>(&payload).ok()?;
    if locator.input_name.is_empty()
        || !Url::parse(&locator.url).is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some())
    {
        return None;
    }
    Some(Cow::Owned(locator.url))
}

pub fn persisted_resource_arc(value: &Arc<str>) -> Arc<str> {
    match persisted_resource_url(value) {
        Some(Cow::Borrowed(_)) => Arc::clone(value),
        Some(Cow::Owned(decoded)) => Arc::from(decoded),
        None => Arc::from(""),
    }
}

#[cfg(test)]
mod tests {
    use super::{persisted_resource_url, LEGACY_PREFIX};
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use serde::Serialize;

    #[derive(Serialize)]
    struct OldLocator<'a> {
        input_name: &'a str,
        url: &'a str,
    }

    #[test]
    fn decodes_persisted_resource_without_exposing_malformed_locators() {
        let payload =
            rmp_serde::to_vec(&OldLocator { input_name: "m3u", url: "http://10.0.0.5/logo.png" }).expect("old locator");
        let locator = format!("{LEGACY_PREFIX}{}", URL_SAFE_NO_PAD.encode(payload));
        assert_eq!(persisted_resource_url(&locator).as_deref(), Some("http://10.0.0.5/logo.png"));
        assert_eq!(persisted_resource_url("resource://v1/invalid"), None);
        assert_eq!(persisted_resource_url("resource://v2/unknown"), None);
    }
}

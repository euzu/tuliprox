use axum::http::HeaderValue;

// Content types used in responses
pub(crate) static CT_JSON: HeaderValue = HeaderValue::from_static("application/json");
pub(crate) static CT_XML: HeaderValue = HeaderValue::from_static("text/xml");
pub(crate) static CT_M3U: HeaderValue = HeaderValue::from_static("application/vnd.apple.mpegurl");
pub(crate) static CT_OCTET: HeaderValue = HeaderValue::from_static("application/octet-stream");

// Cache control
pub(crate) static CC_NO_STORE: HeaderValue = HeaderValue::from_static("no-store, no-cache, must-revalidate");

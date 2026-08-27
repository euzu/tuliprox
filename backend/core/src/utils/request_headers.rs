//! Copying request headers out of an incoming request.
//!
//! Pure `HeaderMap` work with an optional name filter. Both the streaming layer
//! and the HLS proxy forward headers upstream, so it lives here rather than with
//! either.

use axum::http::HeaderMap;
use std::collections::HashMap;

/// Optional predicate over a header name; `None` keeps every header.
pub type HeaderFilter = Option<Box<dyn Fn(&str) -> bool + Send>>;
pub fn get_headers_from_request(req_headers: &HeaderMap, filter: &HeaderFilter) -> HashMap<String, Vec<u8>> {
    req_headers
        .iter()
        .filter(|(k, _)| match &filter {
            None => true,
            Some(predicate) => predicate(k.as_str()),
        })
        .map(|(k, v)| (k.as_str().to_string(), v.as_bytes().to_vec()))
        .collect()
}

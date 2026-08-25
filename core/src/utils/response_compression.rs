//! Opting a single response out of the compression layer.
//!
//! The compression middleware runs over every response; a handler that has
//! already produced exact bytes - a byte-range slice, a transport-stream
//! segment - marks its response here so the layer leaves it alone. The marker
//! and the predicate that reads it live together so the two cannot drift.

use axum::http::{Extensions, Response};

/// Marker extension: this response must reach the client uncompressed.
#[derive(Clone, Copy, Debug)]
pub struct DisableResponseCompression;

/// Mark `response` so the compression layer skips it.
pub fn mark_response_as_uncompressed<B>(response: &mut Response<B>) {
    response.extensions_mut().insert(DisableResponseCompression);
}

/// `true` when the compression layer may compress this response.
pub fn should_compress_response<B>(response: &Response<B>) -> bool {
    should_compress_response_extensions(response.extensions())
}

/// `true` when the compression layer may compress a response with these
/// extensions.
pub fn should_compress_response_extensions(extensions: &Extensions) -> bool {
    extensions.get::<DisableResponseCompression>().is_none()
}

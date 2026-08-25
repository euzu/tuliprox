//! Small response-building macros shared by every crate that builds HTTP
//! responses.

/// Turn a `Result<Response, _>` into a response, mapping the error case to a
/// 500. The expansion names only `axum`, so it works in any crate.
#[macro_export]
macro_rules! try_unwrap_body {
    ($body:expr) => {
        $body
            .map_or_else(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response(), |resp| resp.into_response())
    };
}

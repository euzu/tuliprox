use shared::error::{notify_err_res, TuliproxError};
use reqwest::StatusCode;

/// Handle Trakt API response status and convert to appropriate error
pub fn handle_trakt_api_error(status: StatusCode, user: &str, list_slug: &str) -> Result<(), TuliproxError> {
    match status.as_u16() {
        404 => notify_err_res!("Trakt list not found: {user}:{list_slug}"),
        401 => notify_err_res!("Trakt API key is invalid or expired"),
        429 => notify_err_res!("Trakt API rate limit exceeded"),
        _ => notify_err_res!( "Trakt API error {status}: {}", status.canonical_reason().unwrap_or("Unknown"))
    }
}

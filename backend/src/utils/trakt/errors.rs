use shared::error::TuliproxError;
use reqwest::StatusCode;

/// Handle Trakt API response status and convert to appropriate error
pub fn handle_trakt_api_error(status: StatusCode, user: &str, list_slug: &str) -> Result<(), TuliproxError> {
    match status.as_u16() {
        404 => Err(TuliproxError::RepositoryTrakt(format!("Trakt list not found: {user}:{list_slug}"))),
        401 => Err(TuliproxError::RepositoryTrakt("Trakt API key is invalid or expired".to_string())),
        429 => Err(TuliproxError::RepositoryTrakt("Trakt API rate limit exceeded".to_string())),
        _ => Err(TuliproxError::RepositoryTrakt(format!( "Trakt API error {status}: {}", status.canonical_reason().unwrap_or("Unknown"))))
    }
}

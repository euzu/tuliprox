use reqwest::StatusCode;
use shared::error::TuliproxError;

/// Handle Trakt API response status and convert to appropriate error
pub fn handle_trakt_api_error(status: StatusCode, resource_kind: &str, resource_id: &str) -> Result<(), TuliproxError> {
    match status.as_u16() {
        401 => Err(TuliproxError::RepositoryTrakt(
            "Trakt rejected the configured Client ID (HTTP 401 Unauthorized); check trakt.api.api_key",
        )),
        403 => Err(TuliproxError::RepositoryTrakt(
            "Trakt denied the request (HTTP 403 Forbidden); check the configured Client ID and resource access; creating Trakt API applications currently requires active VIP membership",
        )),
        404 => Err(TuliproxError::RepositoryTrakt(format!(
            "Trakt {resource_kind} not found: {resource_id}"
        ))),
        429 => Err(TuliproxError::RepositoryTrakt(
            "Trakt API rate limit exceeded (HTTP 429 Too Many Requests); retry later",
        )),
        _ => Err(TuliproxError::RepositoryTrakt(format!(
            "Trakt API error {status}: {}",
            status.canonical_reason().unwrap_or("Unknown")
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translated_error(status: StatusCode, resource_kind: &str, resource_id: &str) -> TuliproxError {
        handle_trakt_api_error(status, resource_kind, resource_id)
            .expect_err("unsuccessful status should be translated")
    }

    #[test]
    fn trakt_api_errors_translate_unauthorized_client_id() {
        let message = translated_error(StatusCode::UNAUTHORIZED, "list", "alice:watchlist").message().to_string();

        assert!(message.contains("401"));
        assert!(message.contains("Client ID"));
        assert!(message.contains("trakt.api.api_key"));
        assert!(!message.contains("alice:watchlist"));
    }

    #[test]
    fn trakt_api_errors_translate_forbidden_without_overstating_vip_cause() {
        let message = translated_error(StatusCode::FORBIDDEN, "chart", "movies:trending").message().to_string();

        assert!(message.contains("403"));
        assert!(message.contains("Trakt denied the request"));
        assert!(message.contains("configured Client ID"));
        assert!(message.contains("resource access"));
        assert!(message.contains("creating Trakt API applications currently requires active VIP"));
    }

    #[test]
    fn trakt_api_errors_distinguish_list_and_chart_not_found() {
        let list_message = translated_error(StatusCode::NOT_FOUND, "list", "alice:watchlist").message().to_string();
        let chart_message = translated_error(StatusCode::NOT_FOUND, "chart", "movies:trending").message().to_string();

        assert_eq!(list_message, "Trakt list not found: alice:watchlist");
        assert_eq!(chart_message, "Trakt chart not found: movies:trending");
    }

    #[test]
    fn trakt_api_errors_translate_rate_limit() {
        let message = translated_error(StatusCode::TOO_MANY_REQUESTS, "chart", "shows:popular").message().to_string();

        assert!(message.contains("429"));
        assert!(message.contains("rate limit"));
        assert!(message.contains("retry later"));
    }
}

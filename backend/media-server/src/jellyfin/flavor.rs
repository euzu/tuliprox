use crate::{
    emby::flavor::{
        impl_emby_family_flavor, sensitive_header_value, validate_token, EmbyClientIdentity, EmbyEndpoint, EmbyPath,
        UserScope,
    },
    MediaServerError,
};
use reqwest::header::{HeaderMap, AUTHORIZATION};

#[derive(Debug, Clone, Copy)]
pub struct Jellyfin;

// Jellyfin 10.11 deprecated and 12.0 removes the `X-Emby-*` / `X-MediaBrowser-*`
// headers and `api_key`, so the `Authorization` header is the only credential carrier.
impl_emby_family_flavor!(Jellyfin, "jellyfin", {
    fn auth_headers(identity: &EmbyClientIdentity, token: Option<&str>) -> Result<HeaderMap, MediaServerError> {
        let token = token.map(|token| validate_token(token, "jellyfin")).transpose()?;
        let mut headers = HeaderMap::with_capacity(1);
        headers.insert(AUTHORIZATION, sensitive_header_value(&identity.media_browser_value(token), "jellyfin")?);
        Ok(headers)
    }

    fn items_path(user_id: Option<&str>) -> EmbyEndpoint<'_> {
        let user_scope = if user_id.is_some() { UserScope::Query } else { UserScope::None };
        EmbyEndpoint { path: EmbyPath::one("Items"), user_scope }
    }

    fn user_views_path(_user_id: &str) -> EmbyEndpoint<'_> {
        EmbyEndpoint { path: EmbyPath::one("UserViews"), user_scope: UserScope::Query }
    }
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{emby::flavor::EmbyFamilyFlavor, MediaServerErrorKind};
    use shared::utils::short_hash;

    fn identity() -> EmbyClientIdentity { EmbyClientIdentity::new("input", Some("alice")) }

    fn expected_identity() -> String {
        format!(
            "MediaBrowser Client=\"Tuliprox\", Device=\"Tuliprox\", DeviceId=\"tuliprox-{}\", Version=\"{}\"",
            short_hash("input\u{1f}alice"),
            env!("CARGO_PKG_VERSION")
        )
    }

    #[test]
    fn auth_headers_use_only_the_authorization_header() {
        let headers = Jellyfin::auth_headers(&identity(), Some("secret")).expect("headers build");

        assert_eq!(headers.len(), 1);
        let value = headers.get(AUTHORIZATION).expect("authorization header").to_str().expect("ascii");
        assert_eq!(value, format!("{}, Token=\"secret\"", expected_identity()));
        assert!(headers.get(AUTHORIZATION).expect("authorization header").is_sensitive());
    }

    #[test]
    fn identity_only_headers_carry_no_token() {
        let headers = Jellyfin::auth_headers(&identity(), None).expect("headers build");

        assert_eq!(headers.len(), 1);
        assert_eq!(
            headers.get(AUTHORIZATION).expect("authorization header").to_str().expect("ascii"),
            expected_identity()
        );
    }

    #[test]
    fn tokens_that_cannot_be_quoted_are_auth_denied() {
        for token in ["a\"b", "a\\b", "a b", "a\nb", "tök\u{e9}n", ""] {
            let error = Jellyfin::auth_headers(&identity(), Some(token)).expect_err("token is rejected");

            assert_eq!(error.kind, MediaServerErrorKind::MediaServerAuthDenied, "token {token:?}");
        }
    }

    #[test]
    fn paths_scope_to_the_user_with_a_query_parameter() {
        assert_eq!(
            Jellyfin::items_path(None),
            EmbyEndpoint { path: EmbyPath::one("Items"), user_scope: UserScope::None }
        );
        assert_eq!(
            Jellyfin::items_path(Some("u1")),
            EmbyEndpoint { path: EmbyPath::one("Items"), user_scope: UserScope::Query }
        );
        assert_eq!(
            Jellyfin::user_views_path("u1"),
            EmbyEndpoint { path: EmbyPath::one("UserViews"), user_scope: UserScope::Query }
        );
    }
}

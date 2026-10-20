use crate::{MediaServerError, MediaServerErrorKind, MediaServerImageRef, MediaServerKind, MediaServerStreamRef};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use shared::utils::short_hash;
use std::sync::Arc;

const CLIENT_NAME: &str = "Tuliprox";

/// How this client introduces itself to an Emby-family server. The `DeviceId`
/// is derived from the input so the server replaces the previous session on a
/// restart instead of accumulating devices.
#[derive(Debug, Clone)]
pub struct EmbyClientIdentity {
    device_id: Arc<str>,
}

impl EmbyClientIdentity {
    pub(crate) fn new(input_name: &str, username: Option<&str>) -> Self {
        let device_id =
            format!("tuliprox-{}", short_hash(&format!("{input_name}\u{1f}{}", username.unwrap_or_default())));
        Self { device_id: Arc::from(device_id) }
    }

    /// `MediaBrowser Client=..., Device=..., DeviceId=..., Version=...[, Token=...]`.
    /// `token` must already have passed [`validate_token`].
    pub(crate) fn media_browser_value(&self, token: Option<&str>) -> String {
        let mut value = format!(
            "MediaBrowser Client=\"{CLIENT_NAME}\", Device=\"{CLIENT_NAME}\", DeviceId=\"{}\", Version=\"{}\"",
            self.device_id,
            env!("CARGO_PKG_VERSION")
        );
        if let Some(token) = token {
            value.push_str(", Token=\"");
            value.push_str(token);
            value.push('"');
        }
        value
    }
}

/// A token ends up inside a quoted header parameter, so anything that is not
/// visible ASCII, or could terminate or escape the quoting, is refused.
pub(crate) fn validate_token<'a>(token: &'a str, provider: &'static str) -> Result<&'a str, MediaServerError> {
    if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_graphic() && byte != b'"' && byte != b'\\') {
        return Err(MediaServerError::new(MediaServerErrorKind::MediaServerAuthDenied)
            .provider(provider)
            .detail("token contains characters that are not allowed in a request header"));
    }
    Ok(token)
}

pub(crate) fn sensitive_header_value(text: &str, provider: &'static str) -> Result<HeaderValue, MediaServerError> {
    let mut value = HeaderValue::from_str(text).map_err(|_| {
        MediaServerError::new(MediaServerErrorKind::MediaServerAuthDenied)
            .provider(provider)
            .detail("credentials contain invalid header characters")
    })?;
    value.set_sensitive(true);
    Ok(value)
}

/// Request path as at most three segments, appended to the configured base URL
/// (percent-encoded, base path prefix preserved) by the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbyPath<'a> {
    segments: [&'a str; 3],
    len: usize,
}

impl<'a> EmbyPath<'a> {
    pub const fn one(first: &'a str) -> Self { Self { segments: [first, "", ""], len: 1 } }

    pub const fn three(first: &'a str, second: &'a str, third: &'a str) -> Self {
        Self { segments: [first, second, third], len: 3 }
    }

    pub fn segments(&self) -> &[&'a str] { &self.segments[..self.len] }
}

/// How an endpoint is scoped to the authenticated user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserScope {
    /// The endpoint is not user scoped.
    None,
    /// The user id is a segment of the endpoint path.
    InPath,
    /// The user id is sent as the `UserId` query parameter.
    Query,
}

/// An endpoint path together with the way it is scoped to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbyEndpoint<'a> {
    pub path: EmbyPath<'a>,
    pub user_scope: UserScope,
}

/// Borrowed view of an Emby-family stream reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbyItemRefParts<'a> {
    pub input_name: &'a str,
    pub server_id: &'a str,
    pub item_id: &'a str,
    pub media_source_id: Option<&'a str>,
}

/// Borrowed view of an Emby-family image reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbyImageRefParts<'a> {
    pub input_name: &'a str,
    pub server_id: &'a str,
    pub item_id: &'a str,
    pub image_kind: &'a str,
    pub tag: Option<&'a str>,
}

/// Wire differences between Emby and Jellyfin, selected at compile time so the
/// mapper and client stay generic without dynamic dispatch.
pub trait EmbyFamilyFlavor: sealed::Sealed + Send + Sync + 'static {
    const KIND: MediaServerKind;
    const PROVIDER: &'static str;

    fn stream_ref(
        input_name: &Arc<str>,
        server_id: &Arc<str>,
        item_id: &Arc<str>,
        media_source_id: Option<Arc<str>>,
    ) -> MediaServerStreamRef;

    fn image_ref(
        input_name: &Arc<str>,
        server_id: &Arc<str>,
        item_id: &Arc<str>,
        image_kind: Arc<str>,
        tag: Option<Arc<str>>,
    ) -> MediaServerImageRef;

    /// `None` when the reference belongs to a different media-server kind.
    fn stream_ref_parts(stream_ref: &MediaServerStreamRef) -> Option<EmbyItemRefParts<'_>>;

    /// `None` when the reference belongs to a different media-server kind.
    fn image_ref_parts(image_ref: &MediaServerImageRef) -> Option<EmbyImageRefParts<'_>>;

    /// Identity and credential headers for one request. `token: None` yields the
    /// identity-only form used for discovery and login.
    fn auth_headers(identity: &EmbyClientIdentity, token: Option<&str>) -> Result<HeaderMap, MediaServerError>;

    /// Item listing endpoint and how it is scoped to the user.
    fn items_path(user_id: Option<&str>) -> EmbyEndpoint<'_>;

    /// Endpoint listing the libraries visible to a user and how it is scoped to the user.
    fn user_views_path(user_id: &str) -> EmbyEndpoint<'_>;
}

pub(crate) mod sealed {
    /// Keeps [`EmbyFamilyFlavor`](super::EmbyFamilyFlavor) implementable only inside this crate.
    pub trait Sealed {}
}

/// Implements [`EmbyFamilyFlavor`] for a unit struct whose name equals both the
/// `MediaServerKind` variant and the `MediaServerStreamRef`/`MediaServerImageRef` variant.
macro_rules! impl_emby_family_flavor {
    ($flavor:ident, $provider:literal, { $($flavor_items:item)* }) => {
        impl $crate::emby::flavor::sealed::Sealed for $flavor {}

        impl $crate::emby::flavor::EmbyFamilyFlavor for $flavor {
            const KIND: $crate::MediaServerKind = $crate::MediaServerKind::$flavor;
            const PROVIDER: &'static str = $provider;

            fn stream_ref(
                input_name: &::std::sync::Arc<str>,
                server_id: &::std::sync::Arc<str>,
                item_id: &::std::sync::Arc<str>,
                media_source_id: Option<::std::sync::Arc<str>>,
            ) -> $crate::MediaServerStreamRef {
                $crate::MediaServerStreamRef::$flavor {
                    input_name: ::std::sync::Arc::clone(input_name),
                    server_id: ::std::sync::Arc::clone(server_id),
                    item_id: ::std::sync::Arc::clone(item_id),
                    media_source_id,
                }
            }

            fn image_ref(
                input_name: &::std::sync::Arc<str>,
                server_id: &::std::sync::Arc<str>,
                item_id: &::std::sync::Arc<str>,
                image_kind: ::std::sync::Arc<str>,
                tag: Option<::std::sync::Arc<str>>,
            ) -> $crate::MediaServerImageRef {
                $crate::MediaServerImageRef::$flavor {
                    input_name: ::std::sync::Arc::clone(input_name),
                    server_id: ::std::sync::Arc::clone(server_id),
                    item_id: ::std::sync::Arc::clone(item_id),
                    image_kind,
                    tag,
                }
            }

            fn stream_ref_parts(
                stream_ref: &$crate::MediaServerStreamRef,
            ) -> Option<$crate::emby::flavor::EmbyItemRefParts<'_>> {
                match stream_ref {
                    $crate::MediaServerStreamRef::$flavor { input_name, server_id, item_id, media_source_id } => {
                        Some($crate::emby::flavor::EmbyItemRefParts {
                            input_name,
                            server_id,
                            item_id,
                            media_source_id: media_source_id.as_deref(),
                        })
                    }
                    _ => None,
                }
            }

            fn image_ref_parts(
                image_ref: &$crate::MediaServerImageRef,
            ) -> Option<$crate::emby::flavor::EmbyImageRefParts<'_>> {
                match image_ref {
                    $crate::MediaServerImageRef::$flavor { input_name, server_id, item_id, image_kind, tag } => {
                        Some($crate::emby::flavor::EmbyImageRefParts {
                            input_name,
                            server_id,
                            item_id,
                            image_kind,
                            tag: tag.as_deref(),
                        })
                    }
                    _ => None,
                }
            }

            $($flavor_items)*
        }
    };
}

pub(crate) use impl_emby_family_flavor;

const X_EMBY_TOKEN: HeaderName = HeaderName::from_static("x-emby-token");
const X_EMBY_AUTHORIZATION: HeaderName = HeaderName::from_static("x-emby-authorization");

#[derive(Debug, Clone, Copy)]
pub struct Emby;

impl_emby_family_flavor!(Emby, "emby", {
    fn auth_headers(identity: &EmbyClientIdentity, token: Option<&str>) -> Result<HeaderMap, MediaServerError> {
        let mut headers = HeaderMap::with_capacity(2);
        headers.insert(X_EMBY_AUTHORIZATION, sensitive_header_value(&identity.media_browser_value(None), "emby")?);
        if let Some(token) = token {
            headers.insert(X_EMBY_TOKEN, sensitive_header_value(validate_token(token, "emby")?, "emby")?);
        }
        Ok(headers)
    }

    fn items_path(user_id: Option<&str>) -> EmbyEndpoint<'_> {
        match user_id {
            Some(user_id) => {
                EmbyEndpoint { path: EmbyPath::three("Users", user_id, "Items"), user_scope: UserScope::InPath }
            }
            None => EmbyEndpoint { path: EmbyPath::one("Items"), user_scope: UserScope::None },
        }
    }

    fn user_views_path(user_id: &str) -> EmbyEndpoint<'_> {
        EmbyEndpoint { path: EmbyPath::three("Users", user_id, "Views"), user_scope: UserScope::InPath }
    }
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MediaServerErrorKind;

    fn identity() -> EmbyClientIdentity { EmbyClientIdentity::new("input", Some("alice")) }

    fn expected_identity() -> String {
        format!(
            "MediaBrowser Client=\"Tuliprox\", Device=\"Tuliprox\", DeviceId=\"tuliprox-{}\", Version=\"{}\"",
            short_hash("input\u{1f}alice"),
            env!("CARGO_PKG_VERSION")
        )
    }

    #[test]
    fn auth_headers_send_token_and_identity_in_emby_headers() {
        let headers = Emby::auth_headers(&identity(), Some("secret")).expect("headers build");

        assert_eq!(headers.len(), 2);
        assert_eq!(headers.get("x-emby-token").expect("token header").to_str().expect("ascii"), "secret");
        assert_eq!(
            headers.get("x-emby-authorization").expect("identity header").to_str().expect("ascii"),
            expected_identity()
        );
        assert!(headers.get("x-emby-token").expect("token header").is_sensitive());
        assert!(headers.get(reqwest::header::AUTHORIZATION).is_none());
    }

    #[test]
    fn identity_only_headers_carry_no_token() {
        let headers = Emby::auth_headers(&identity(), None).expect("headers build");

        assert_eq!(headers.len(), 1);
        assert!(headers.get("x-emby-token").is_none());
    }

    #[test]
    fn tokens_that_cannot_be_quoted_are_auth_denied() {
        for token in ["a\"b", "a b", "a\r\nb", "tök\u{e9}n"] {
            let error = Emby::auth_headers(&identity(), Some(token)).expect_err("token is rejected");

            assert_eq!(error.kind, MediaServerErrorKind::MediaServerAuthDenied, "token {token:?}");
        }
    }

    #[test]
    fn paths_scope_to_the_user_with_a_path_prefix() {
        assert_eq!(Emby::items_path(None), EmbyEndpoint { path: EmbyPath::one("Items"), user_scope: UserScope::None });
        assert_eq!(
            Emby::items_path(Some("u1")),
            EmbyEndpoint { path: EmbyPath::three("Users", "u1", "Items"), user_scope: UserScope::InPath }
        );
        assert_eq!(
            Emby::user_views_path("u1"),
            EmbyEndpoint { path: EmbyPath::three("Users", "u1", "Views"), user_scope: UserScope::InPath }
        );
    }

    #[test]
    fn device_id_is_stable_per_input_and_user() {
        let first = EmbyClientIdentity::new("input", Some("alice")).media_browser_value(None);

        assert_eq!(first, EmbyClientIdentity::new("input", Some("alice")).media_browser_value(None));
        assert_ne!(first, EmbyClientIdentity::new("input", Some("bob")).media_browser_value(None));
        assert_ne!(first, EmbyClientIdentity::new("other", Some("alice")).media_browser_value(None));
    }
}

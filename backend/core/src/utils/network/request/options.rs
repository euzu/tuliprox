use super::{RequestFetchOptions, TextContentBodyOptions};
use crate::utils::content_coding::{apply_outbound_content_coding_policy, ContentCodingDetection};
use std::io::{Error, ErrorKind};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum ResourceRetryExecution {
    #[default]
    Configured,
    ProviderFailoverOnly,
}

pub(super) fn apply_request_fetch_options(request: &mut reqwest::Request, options: RequestFetchOptions) {
    if let Some(timeout) = options.attempt_idle_timeout {
        *request.timeout_mut() = Some(timeout);
    }
    apply_outbound_content_coding_policy(request.headers_mut(), options.content_coding);
}

impl RequestFetchOptions {
    /// Refuses a manual redirect from an `https` URL to an `http` URL.
    ///
    /// Off by default, so existing callers keep following such redirects. A caller that sends
    /// credentials or private payloads turns it on, since a downgraded hop exposes the request in clear.
    pub const fn with_reject_https_downgrade(mut self, enabled: bool) -> Self {
        self.reject_https_downgrade = enabled;
        self
    }

    /// Rejects a request whose body is larger than `max_bytes`, or whose size cannot be known up front
    /// (a streaming body), before anything is sent.
    pub const fn with_max_request_body_bytes(mut self, max_bytes: u64) -> Self {
        self.max_request_body_bytes = Some(max_bytes);
        self
    }

    /// Rejects a request that carries more than `max_headers` header values, counting repeated names
    /// once per value.
    pub const fn with_max_request_headers(mut self, max_headers: usize) -> Self {
        self.max_request_headers = Some(max_headers);
        self
    }

    /// Rejects a request whose header names and values add up to more than `max_bytes`.
    pub const fn with_max_request_header_bytes(mut self, max_bytes: usize) -> Self {
        self.max_request_header_bytes = Some(max_bytes);
        self
    }

    pub(super) const fn rejects_https_downgrade(self) -> bool { self.reject_https_downgrade }
}

/// Checks a built request against the transport limits of `options`.
///
/// Runs on the request as it will be sent, after the attempt target and the fetch options have been
/// applied, so headers added on the way count. Headers that the HTTP client adds itself at send time
/// (client-level defaults) are not visible here. Messages carry sizes and limits only, never header
/// names, header values or body content.
pub fn enforce_request_transport_limits(request: &reqwest::Request, options: RequestFetchOptions) -> Result<(), Error> {
    let headers = request.headers();
    if let Some(max_headers) = options.max_request_headers {
        if headers.len() > max_headers {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("Request has {} headers, more than the allowed {max_headers}", headers.len()),
            ));
        }
    }
    if let Some(max_bytes) = options.max_request_header_bytes {
        let total = headers
            .iter()
            .fold(0usize, |total, (name, value)| total.saturating_add(name.as_str().len()).saturating_add(value.len()));
        if total > max_bytes {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("Request headers take {total} bytes, more than the allowed {max_bytes}"),
            ));
        }
    }
    if let Some(max_bytes) = options.max_request_body_bytes {
        if let Some(body) = request.body() {
            match body.as_bytes() {
                Some(bytes) if u64::try_from(bytes.len()).is_ok_and(|len| len <= max_bytes) => {}
                Some(bytes) => {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        format!("Request body of {} bytes is larger than the allowed {max_bytes}", bytes.len()),
                    ));
                }
                None => {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        format!("Request body has no known size, so the {max_bytes} byte limit cannot be enforced"),
                    ));
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum TextContentRetryOwner {
    #[default]
    RequestStack,
    DecodedBodyConsumer,
}

impl Default for TextContentBodyOptions {
    fn default() -> Self {
        Self {
            detection: ContentCodingDetection::DeclaredOrLegacyTextMagic,
            max_decoded_bytes: None,
            deadline: None,
            retry_owner: TextContentRetryOwner::RequestStack,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{enforce_request_transport_limits, RequestFetchOptions};
    use std::io::ErrorKind;

    fn build(body: Option<&'static [u8]>, headers: &[(&str, &str)]) -> reqwest::Request {
        let client = reqwest::Client::new();
        let mut builder = match body {
            Some(bytes) => client.post("http://127.0.0.1:1/").body(bytes),
            None => client.get("http://127.0.0.1:1/"),
        };
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.build().expect("request builds")
    }

    #[test]
    fn default_options_enforce_nothing() {
        let request = build(Some(&[0u8; 4096]), &[("x-a", "1"), ("x-b", "2")]);
        assert!(enforce_request_transport_limits(&request, RequestFetchOptions::default()).is_ok());
    }

    #[test]
    fn a_body_over_the_limit_is_rejected_and_one_at_the_limit_is_not() {
        let request = build(Some(b"12345"), &[]);
        let at_limit = RequestFetchOptions::default().with_max_request_body_bytes(5);
        assert!(enforce_request_transport_limits(&request, at_limit).is_ok());
        let over = RequestFetchOptions::default().with_max_request_body_bytes(4);
        let error = enforce_request_transport_limits(&request, over).expect_err("five bytes exceed a four byte limit");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert!(error.to_string().contains("5 bytes"));
    }

    #[test]
    fn a_request_without_a_body_passes_a_zero_body_limit() {
        let request = build(None, &[]);
        let options = RequestFetchOptions::default().with_max_request_body_bytes(0);
        assert!(enforce_request_transport_limits(&request, options).is_ok());
    }

    #[test]
    fn too_many_headers_are_rejected() {
        let request = build(None, &[("x-a", "1"), ("x-b", "2"), ("x-c", "3")]);
        let at_limit = RequestFetchOptions::default().with_max_request_headers(3);
        assert!(enforce_request_transport_limits(&request, at_limit).is_ok());
        let over = RequestFetchOptions::default().with_max_request_headers(2);
        let error = enforce_request_transport_limits(&request, over).expect_err("three headers exceed two");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn header_bytes_count_names_and_values_and_the_message_hides_both() {
        let request = build(None, &[("x-secret-name", "secret-value")]);
        let exact = "x-secret-name".len() + "secret-value".len();
        let at_limit = RequestFetchOptions::default().with_max_request_header_bytes(exact);
        assert!(enforce_request_transport_limits(&request, at_limit).is_ok());
        let over = RequestFetchOptions::default().with_max_request_header_bytes(exact - 1);
        let error = enforce_request_transport_limits(&request, over).expect_err("one byte over");
        let message = error.to_string();
        assert!(!message.contains("secret"), "limit errors must not echo header content: {message}");
    }
}

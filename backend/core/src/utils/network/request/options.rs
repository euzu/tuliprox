use super::{RequestFetchOptions, TextContentBodyOptions};
use crate::utils::content_coding::{apply_outbound_content_coding_policy, ContentCodingDetection};

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

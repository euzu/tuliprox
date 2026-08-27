//! The outbound channel abstraction.
//!
//! Adding a channel used to mean editing ten sites across three crates: a
//! `MessagingChannel` variant, the `is_some()` chain in
//! `configured_channels`, the match in `send_message_to_channel`, a new
//! `send_*_message` function, the hardcoded `tokio::join!` in
//! `dispatch_send_message`, a config field, a DTO field, both `From` impls,
//! `MessagingConfig::prepare`, and the template-discovery prefix.
//!
//! Now it is one [`NotificationChannel`] impl plus one config field. The
//! dispatcher never learns the channel's name.
//!
//! Dispatch is dynamic on purpose. The channel set is an open world
//! resolved once at config load, and a notification send is bounded by a
//! network round trip - the vtable hop is not measurable here, and static
//! dispatch would put the closed-world match back.

use shared::model::notification::{EventId, Severity};
use std::{future::Future, pin::Pin, sync::Arc};
use tuliprox_core::model::NotificationEvent;

/// A boxed channel send. Matches the `SinkFuture` convention in
/// `tuliprox-processing` rather than pulling in `async-trait`.
pub type SendFuture<'a> = Pin<Box<dyn Future<Output = Delivery> + Send + 'a>>;

/// What happened to one delivery attempt.
///
/// The old `Option<bool>` could not tell "retry me" from "this URL is
/// malformed, retrying is pointless", so a typo'd webhook burned every
/// attempt before dead-lettering, and a `429` was retried straight back
/// into the rate limit it had just hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// Landed. Never retry.
    Delivered,
    /// Transient. Retry after `after` if the provider named a delay,
    /// otherwise on the outbox's own backoff schedule.
    Retry { reason: String, after: Option<std::time::Duration> },
    /// Permanently undeliverable - malformed request, rejected credentials.
    /// Retrying cannot help, so dead-letter immediately.
    Permanent { reason: String },
    /// Nothing to do: the channel does not want this event. Not a failure,
    /// and must never be retried.
    Skipped,
}

impl Delivery {
    /// Transient failure with no provider-supplied delay.
    pub fn retry(reason: impl Into<String>) -> Self { Self::Retry { reason: reason.into(), after: None } }

    /// Transient failure honouring a provider `Retry-After`.
    pub fn retry_after(reason: impl Into<String>, after: std::time::Duration) -> Self {
        Self::Retry { reason: reason.into(), after: Some(after) }
    }

    pub fn permanent(reason: impl Into<String>) -> Self { Self::Permanent { reason: reason.into() } }

    #[must_use]
    pub fn is_delivered(&self) -> bool { matches!(self, Self::Delivered) }

    /// Should the outbox keep this channel pending?
    #[must_use]
    pub fn should_retry(&self) -> bool { matches!(self, Self::Retry { .. }) }
}

/// Classify an HTTP status into a delivery outcome.
///
/// Shared by every HTTP-backed channel so they agree on what is worth
/// retrying. `408`/`429` and all of `5xx` are transient; other `4xx` are
/// the caller's fault and will fail identically forever.
#[must_use]
pub fn delivery_for_status(status: reqwest::StatusCode, retry_after: Option<std::time::Duration>) -> Delivery {
    if status.is_success() {
        return Delivery::Delivered;
    }
    let reason = format!("status {status}");
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status == reqwest::StatusCode::REQUEST_TIMEOUT {
        return match retry_after {
            Some(after) => Delivery::retry_after(reason, after),
            None => Delivery::retry(reason),
        };
    }
    if status.is_server_error() {
        return Delivery::retry(reason);
    }
    Delivery::permanent(reason)
}

/// No provider is allowed to park a retry for longer than this.
const MAX_RETRY_AFTER: std::time::Duration = std::time::Duration::from_hours(24);

/// Read a `Retry-After` header in either of its two legal forms.
#[must_use]
pub fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<std::time::Duration> {
    let raw = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    // Delta-seconds is by far the common form.
    if let Ok(secs) = raw.trim().parse::<u64>() {
        return Some(std::time::Duration::from_secs(secs).min(MAX_RETRY_AFTER));
    }
    // HTTP-date form. A date already in the past means the wait has elapsed,
    // so it clamps to "retry now" rather than falling back to the default
    // backoff - the server told us the delay is over.
    let when = chrono::DateTime::parse_from_rfc2822(raw.trim()).ok()?;
    let delta = when.timestamp().saturating_sub(chrono::Utc::now().timestamp()).max(0);
    Some(std::time::Duration::from_secs(u64::try_from(delta).unwrap_or(0)).min(MAX_RETRY_AFTER))
}

/// What a channel can accept, so the dispatcher can prepare the body once.
#[derive(Debug, Clone, Copy)]
pub struct ChannelCapabilities {
    /// The channel renders operator-supplied templates. When `false` the
    /// dispatcher hands over [`NotificationEvent::body`] and does not look
    /// for a template - which is what Pushover needed and never had.
    pub supports_templates: bool,
    /// Hard body limit in bytes; `None` for unlimited. The dispatcher
    /// truncates on a character boundary before calling `send`.
    pub max_body_bytes: Option<usize>,
}

impl Default for ChannelCapabilities {
    fn default() -> Self { Self { supports_templates: true, max_body_bytes: None } }
}

/// An event with its body already rendered for one channel.
#[derive(Debug, Clone)]
pub struct RenderedMessage<'a> {
    /// The event itself, for channels that build structured payloads.
    pub event: &'a NotificationEvent,
    /// The body to send: the channel's template output, or
    /// [`NotificationEvent::body`] when no template applies.
    pub body: String,
    /// `true` when `body` came from an operator template rather than the
    /// built-in rendering. Telegram needs this to decide whether escaping
    /// its own output would corrupt deliberate markdown.
    pub templated: bool,
}

/// One configured outbound channel.
pub trait NotificationChannel: Send + Sync {
    /// Stable wire id: config key, outbox key, metric label, template
    /// filename prefix. Must not change once released - the outbox
    /// persists it.
    fn id(&self) -> &'static str;

    fn capabilities(&self) -> ChannelCapabilities { ChannelCapabilities::default() }

    /// The operator's template for this event, if any.
    fn template_for(&self, event: EventId) -> Option<&str>;

    /// Does this channel want this event at this severity?
    ///
    /// Default accepts everything; per-channel routing overrides it.
    fn wants(&self, _event: EventId, _severity: Severity) -> bool { true }

    /// Deliver. Must not panic and must not block - a slow provider has to
    /// surface as [`Delivery::Retry`], not a stalled worker.
    fn send<'a>(&'a self, msg: &'a RenderedMessage<'a>) -> SendFuture<'a>;
}

/// The channels configured right now, in a stable order.
pub type ChannelSet = Vec<Arc<dyn NotificationChannel>>;

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::{header::HeaderMap, StatusCode};

    #[test]
    fn success_is_delivered() {
        assert_eq!(delivery_for_status(StatusCode::OK, None), Delivery::Delivered);
        assert_eq!(delivery_for_status(StatusCode::NO_CONTENT, None), Delivery::Delivered);
    }

    #[test]
    fn server_errors_are_retried() {
        assert!(delivery_for_status(StatusCode::INTERNAL_SERVER_ERROR, None).should_retry());
        assert!(delivery_for_status(StatusCode::BAD_GATEWAY, None).should_retry());
        assert!(delivery_for_status(StatusCode::SERVICE_UNAVAILABLE, None).should_retry());
    }

    #[test]
    fn client_errors_are_permanent_so_a_typo_does_not_burn_every_attempt() {
        // A malformed webhook URL used to consume all `max_attempts` with
        // exponential backoff before being dead-lettered.
        assert!(matches!(delivery_for_status(StatusCode::NOT_FOUND, None), Delivery::Permanent { .. }));
        assert!(matches!(delivery_for_status(StatusCode::UNAUTHORIZED, None), Delivery::Permanent { .. }));
        assert!(matches!(delivery_for_status(StatusCode::BAD_REQUEST, None), Delivery::Permanent { .. }));
    }

    #[test]
    fn rate_limit_is_retried_and_carries_the_provider_delay() {
        let delay = std::time::Duration::from_secs(30);
        let outcome = delivery_for_status(StatusCode::TOO_MANY_REQUESTS, Some(delay));
        assert_eq!(outcome, Delivery::Retry { reason: "status 429 Too Many Requests".to_string(), after: Some(delay) });
    }

    #[test]
    fn rate_limit_without_a_header_still_retries() {
        assert!(delivery_for_status(StatusCode::TOO_MANY_REQUESTS, None).should_retry());
    }

    #[test]
    fn retry_after_reads_delta_seconds() {
        let mut headers = HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "42".parse().expect("header"));
        assert_eq!(parse_retry_after(&headers), Some(std::time::Duration::from_secs(42)));
    }

    #[test]
    fn retry_after_is_clamped_to_a_day() {
        let mut headers = HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "999999999".parse().expect("header"));
        assert_eq!(parse_retry_after(&headers), Some(std::time::Duration::from_hours(24)));
    }

    #[test]
    fn retry_after_absent_or_garbage_is_none() {
        assert_eq!(parse_retry_after(&HeaderMap::new()), None);
        let mut headers = HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "not-a-date".parse().expect("header"));
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[test]
    fn a_past_http_date_yields_no_delay_rather_than_underflowing() {
        let mut headers = HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "Wed, 21 Oct 2015 07:28:00 GMT".parse().expect("header"));
        assert_eq!(parse_retry_after(&headers), Some(std::time::Duration::ZERO));
    }

    #[test]
    fn skipped_and_permanent_are_never_retried() {
        assert!(!Delivery::Skipped.should_retry());
        assert!(!Delivery::permanent("nope").should_retry());
        assert!(!Delivery::Delivered.should_retry());
    }
}

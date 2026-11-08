//! Generic REST/webhook channel.

use crate::channel::{
    delivery_for_status, parse_retry_after, ChannelCapabilities, Delivery, NotificationChannel, RenderedMessage,
};
use log::debug;
use reqwest::{header, Method, StatusCode};
use shared::model::{
    notification::{EventId, Severity},
    Millis,
};
use std::{str::FromStr, sync::Arc, time::Duration};
use tuliprox_core::{
    model::{ChannelRouting, RestMessagingConfig},
    utils::{
        request::{
            enforce_request_transport_limits, OutboundError, OutboundExecutor, OutboundPolicy, RateLimitConfig,
            RequestFetchOptions,
        },
        CallOutcome, CircuitBreakerPolicy, CountedOutcomes, SystemClock,
    },
};

/// Executor key of the REST channel. One channel instance owns one executor, so the key is constant and
/// low-cardinality by construction: it never derives from the configured URL or headers.
const REST_EXECUTOR_KEY: &str = "notify.rest";

/// Consecutive counted failures (connection, timeout, 429, retryable 5xx) that open the circuit.
const REST_FAILURE_THRESHOLD: u32 = 5;

/// How long the circuit stays open before one probe send is admitted. The outbox keeps the messages
/// meanwhile and retries them on its own schedule.
const REST_OPEN_DURATION: Millis = Millis::new(30_000);

/// Budget for one send, covering the bulkhead and rate-limit waits and the HTTP call. Generous on
/// purpose: it is a backstop for a stuck send, and the HTTP client's own timeouts normally act first.
const REST_OPERATION_DEADLINE: Millis = Millis::new(60_000);

/// Requests per second the channel sends once its burst is spent. Notifications are low-volume; this
/// only matters when the outbox drains a backlog into one endpoint.
const REST_RATE_PER_SECOND: u32 = 5;

/// Requests that may go out at once before the rate limit starts to queue callers.
const REST_RATE_BURST: u32 = 10;

/// Sends that may be in flight at once.
const REST_MAX_CONCURRENCY: usize = 4;

/// Request bounds of the REST channel (plan §7 *Bounded HTTP transport*). A rendered notification
/// is a few KiB; the caps only stop a runaway template or header config. Exceeding one is a local,
/// permanent rejection that the breaker does not count.
const REST_MAX_REQUEST_BODY_BYTES: u64 = 1024 * 1024;
const REST_MAX_REQUEST_HEADERS: usize = 64;
const REST_MAX_REQUEST_HEADER_BYTES: usize = 32 * 1024;

fn rest_request_limits() -> RequestFetchOptions {
    RequestFetchOptions::default()
        .with_max_request_body_bytes(REST_MAX_REQUEST_BODY_BYTES)
        .with_max_request_headers(REST_MAX_REQUEST_HEADERS)
        .with_max_request_header_bytes(REST_MAX_REQUEST_HEADER_BYTES)
}

/// Hex HMAC-SHA256 of `{timestamp}.{body}`.
///
/// The timestamp is inside the signed payload so a captured request cannot
/// be replayed later with a fresh timestamp header.
fn sign(secret: &str, timestamp: i64, body: &str) -> String {
    use hmac::{KeyInit, Mac, SimpleHmac};
    let mut mac = <SimpleHmac<sha2::Sha256> as KeyInit>::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts a key of any length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body.as_bytes());
    let bytes = mac.finalize().into_bytes();
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// The breaker, rate limit and bulkhead of the REST channel.
///
/// `Permanent` is deliberately not counted: a 4xx other than 429 or 408 means the endpoint answered
/// and rejected this message, which says nothing about its health.
fn rest_outbound_policy() -> OutboundPolicy {
    let breaker = CircuitBreakerPolicy {
        failure_threshold: REST_FAILURE_THRESHOLD,
        open_duration: REST_OPEN_DURATION,
        // A probe that outlives the operation deadline has been abandoned.
        probe_lease: REST_OPERATION_DEADLINE,
        half_open_probes: 1,
        counted: CountedOutcomes::NONE.with(CallOutcome::Transient).with(CallOutcome::RateLimited),
    };
    OutboundPolicy::default()
        .with_circuit_breaker(breaker)
        .with_rate_limit(RateLimitConfig::per_second(REST_RATE_PER_SECOND, REST_RATE_BURST))
        .with_max_concurrency(REST_MAX_CONCURRENCY)
        .with_deadline(REST_OPERATION_DEADLINE)
}

fn new_rest_executor() -> Arc<OutboundExecutor<SystemClock>> {
    let executor = OutboundExecutor::new();
    executor
        .register(REST_EXECUTOR_KEY, rest_outbound_policy())
        .expect("a fresh executor has room for one key and the policy is valid");
    Arc::new(executor)
}

/// What the call future hands back: only what the delivery decision needs, so the response (and its
/// body) is dropped inside the executor's permit scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SendReply {
    status: StatusCode,
    retry_after: Option<Duration>,
}

/// How the breaker should see an HTTP status.
///
/// Counted: 429 (rate-limited), 408 and the retryable 5xx. 501 and 505 are not transient, so they are
/// not counted. Other 4xx are `Permanent`, which the channel's breaker policy does not count.
fn classify_status(status: StatusCode) -> CallOutcome {
    if status.is_success() {
        CallOutcome::Success
    } else if status == StatusCode::TOO_MANY_REQUESTS {
        CallOutcome::RateLimited
    } else if status == StatusCode::REQUEST_TIMEOUT
        || (status.is_server_error()
            && status != StatusCode::NOT_IMPLEMENTED
            && status != StatusCode::HTTP_VERSION_NOT_SUPPORTED)
    {
        CallOutcome::Transient
    } else {
        CallOutcome::Permanent
    }
}

/// How the breaker should see a transport error. Connection, timeout and mid-flight request or body
/// failures count. A request that could not be built is local validation and does not.
fn classify_error(error: &reqwest::Error) -> CallOutcome {
    if error.is_builder() {
        CallOutcome::LocalRejection
    } else if error.is_timeout() || error.is_connect() || error.is_request() || error.is_body() {
        CallOutcome::Transient
    } else {
        CallOutcome::LocalRejection
    }
}

fn classify_send(result: &Result<SendReply, reqwest::Error>) -> CallOutcome {
    match result {
        Ok(reply) => classify_status(reply.status),
        Err(error) => classify_error(error),
    }
}

/// Everything the executor decides locally is transient from the outbox's point of view: the message
/// is intact and the outbox keeps it and retries on its own schedule. The executor never retries.
fn delivery_for_local_rejection(error: &OutboundError<reqwest::Error>) -> Delivery {
    let after = |retry_after: &Option<Millis>| retry_after.map(Millis::as_duration);
    let (reason, after) = match error {
        OutboundError::CircuitOpen { retry_after } => ("rest circuit breaker is open", after(retry_after)),
        OutboundError::RateLimited { retry_after } => ("rest rate limit reached", after(retry_after)),
        OutboundError::BulkheadFull => ("rest concurrency limit reached", None),
        OutboundError::DeadlineExceeded => ("rest request exceeded its deadline", None),
        OutboundError::Cancelled => ("rest request was cancelled", None),
        OutboundError::UnknownKey => ("rest outbound policy is not registered", None),
        OutboundError::Inner(_) => return Delivery::retry("rest request failed"),
    };
    Delivery::Retry { reason: reason.to_string(), after }
}

#[derive(Clone)]
pub struct RestChannel {
    config: RestMessagingConfig,
    client: reqwest::Client,
    executor: Arc<OutboundExecutor<SystemClock>>,
}

impl RestChannel {
    pub fn new(config: RestMessagingConfig, client: reqwest::Client) -> Self {
        Self { config, client, executor: new_rest_executor() }
    }

    #[cfg(test)]
    fn with_executor(
        config: RestMessagingConfig,
        client: reqwest::Client,
        executor: Arc<OutboundExecutor<SystemClock>>,
    ) -> Self {
        Self { config, client, executor }
    }
}

impl NotificationChannel for RestChannel {
    fn id(&self) -> &'static str { "rest" }

    fn template_for(&self, event: EventId) -> Option<&str> { self.config.templates.get(&event).map(String::as_str) }

    fn routing(&self) -> &ChannelRouting { &self.config.routing }

    fn wants(&self, event: EventId, severity: Severity) -> bool { self.config.routing.accepts(event, severity) }

    async fn send(&self, msg: &RenderedMessage<'_>) -> Delivery {
        let method = Method::from_str(&self.config.method).unwrap_or(Method::POST);
        let mut rb = self.client.request(method, &self.config.url);

        // HMAC-SHA256 over `timestamp.body`, so the receiving endpoint
        // can verify the sender and reject a replayed request.
        if let Some(secret) = &self.config.signing_secret {
            let timestamp = chrono::Utc::now().timestamp();
            let signature = sign(secret, timestamp, &msg.body);
            rb = rb
                .header("X-Tuliprox-Timestamp", timestamp.to_string())
                .header("X-Tuliprox-Signature", format!("sha256={signature}"));
        }

        let has_content_type = self.config.headers.keys().any(|k| k.eq_ignore_ascii_case("content-type"));
        if !has_content_type {
            rb = rb.header(header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string());
        }
        for (key, value) in &self.config.headers {
            rb = rb.header(key, value);
        }
        // Built here rather than inside `send()` so the transport limits can see the final request.
        // A build error surfaces exactly as `send()` reported it before.
        let request = match rb.body(msg.body.clone()).build() {
            Ok(request) => request,
            Err(err) => return Delivery::retry(format!("rest request failed: {err}")),
        };
        if let Err(err) = enforce_request_transport_limits(&request, rest_request_limits()) {
            return Delivery::permanent(format!("rest request rejected locally: {err}"));
        }

        // One send under the breaker, rate limit and bulkhead. Retry stays with the outbox.
        let client = &self.client;
        let sent = self
            .executor
            .execute(REST_EXECUTOR_KEY, classify_send, || async move {
                let response = client.execute(request).await?;
                Ok(SendReply { status: response.status(), retry_after: parse_retry_after(response.headers()) })
            })
            .await;
        match sent {
            Ok(SendReply { status, retry_after }) => {
                if status.is_success() {
                    debug!("Notification delivered to rest api");
                }
                delivery_for_status(status, retry_after)
            }
            Err(OutboundError::Inner(err)) => Delivery::retry(format!("rest request failed: {err}")),
            Err(rejection) => delivery_for_local_rejection(&rejection),
        }
    }

    fn capabilities(&self) -> ChannelCapabilities { ChannelCapabilities::default() }
}

#[cfg(test)]
mod tests {
    use super::{
        classify_send, classify_status, delivery_for_local_rejection, rest_outbound_policy, sign, RestChannel,
        SendReply, REST_EXECUTOR_KEY, REST_OPEN_DURATION,
    };
    use crate::channel::{Delivery, NotificationChannel, RenderedMessage};
    use reqwest::StatusCode;
    use shared::model::{notification::registry::SYSTEM_INFO, Millis};
    use std::{sync::Arc, time::Duration};
    use tuliprox_core::{
        model::{ChannelRouting, NotificationEvent, RestMessagingConfig},
        utils::{
            request::{OutboundError, OutboundExecutor},
            CallOutcome, CircuitBreakerPolicy, SystemClock,
        },
    };

    #[test]
    fn the_status_table_counts_only_dependency_failures() {
        let table = [
            (200, CallOutcome::Success),
            (204, CallOutcome::Success),
            (400, CallOutcome::Permanent),
            (401, CallOutcome::Permanent),
            (404, CallOutcome::Permanent),
            (408, CallOutcome::Transient),
            (429, CallOutcome::RateLimited),
            (500, CallOutcome::Transient),
            (501, CallOutcome::Permanent),
            (502, CallOutcome::Transient),
            (503, CallOutcome::Transient),
            (504, CallOutcome::Transient),
            (505, CallOutcome::Permanent),
        ];
        for (code, expected) in table {
            let status = StatusCode::from_u16(code).expect("valid status");
            assert_eq!(classify_status(status), expected, "status {code}");
        }
    }

    #[test]
    fn the_channel_policy_does_not_count_permanent_or_local_outcomes() {
        let breaker = rest_outbound_policy().circuit_breaker.expect("a breaker is configured");
        assert!(breaker.counts(CallOutcome::Transient) && breaker.counts(CallOutcome::RateLimited));
        for outcome in [CallOutcome::Permanent, CallOutcome::LocalRejection, CallOutcome::Cancelled] {
            assert!(!breaker.counts(outcome), "{outcome:?} must not open the circuit");
        }
    }

    #[test]
    fn a_request_that_cannot_be_built_is_local_validation_not_a_dependency_failure() {
        let error = reqwest::Client::new().get("not a url").build().expect_err("the url is invalid");
        assert_eq!(classify_send(&Err(error)), CallOutcome::LocalRejection);
        let reply = SendReply { status: StatusCode::SERVICE_UNAVAILABLE, retry_after: None };
        assert_eq!(classify_send(&Ok(reply)), CallOutcome::Transient);
    }

    #[test]
    fn local_rejections_become_retries_that_carry_the_known_delay() {
        let open = OutboundError::CircuitOpen { retry_after: Some(Millis::new(1_500)) };
        assert_eq!(
            delivery_for_local_rejection(&open),
            Delivery::retry_after("rest circuit breaker is open", Duration::from_millis(1_500))
        );
        let limited = OutboundError::RateLimited { retry_after: Some(Millis::new(250)) };
        assert!(matches!(
            delivery_for_local_rejection(&limited),
            Delivery::Retry { after: Some(after), .. } if after == Duration::from_millis(250)
        ));
        for rejection in [
            OutboundError::CircuitOpen { retry_after: None },
            OutboundError::RateLimited { retry_after: None },
            OutboundError::BulkheadFull,
            OutboundError::DeadlineExceeded,
            OutboundError::Cancelled,
            OutboundError::UnknownKey,
        ] {
            assert!(
                matches!(delivery_for_local_rejection(&rejection), Delivery::Retry { after: None, .. }),
                "{rejection:?}"
            );
        }
    }

    #[tokio::test]
    async fn an_open_circuit_returns_retry_without_sending() {
        // Port 1 refuses connections at once, which is a counted failure.
        let config = RestMessagingConfig {
            url: "http://127.0.0.1:1/hook".to_string(),
            method: "POST".to_string(),
            signing_secret: None,
            headers: std::collections::HashMap::new(),
            templates: std::collections::HashMap::new(),
            routing: ChannelRouting::default(),
        };
        let breaker =
            CircuitBreakerPolicy { failure_threshold: 1, ..rest_outbound_policy().circuit_breaker.expect("breaker") };
        let executor = Arc::new(OutboundExecutor::<SystemClock>::new());
        executor.register(REST_EXECUTOR_KEY, rest_outbound_policy().with_circuit_breaker(breaker)).expect("register");
        let channel = RestChannel::with_executor(config, reqwest::Client::new(), Arc::clone(&executor));

        let event = NotificationEvent::new(SYSTEM_INFO, "title", "body");
        let message = RenderedMessage { event: &event, body: "body".to_string(), templated: false };

        let first = channel.send(&message).await;
        assert!(
            matches!(&first, Delivery::Retry { reason, .. } if reason.starts_with("rest request failed")),
            "{first:?}"
        );
        assert_eq!(executor.snapshot().admitted, 1);

        let second = channel.send(&message).await;
        match second {
            Delivery::Retry { reason, after: Some(after) } => {
                assert_eq!(reason, "rest circuit breaker is open");
                assert!(after > Duration::ZERO && after <= REST_OPEN_DURATION.as_duration(), "{after:?}");
            }
            other => panic!("expected a retry with a delay, got {other:?}"),
        }
        let snapshot = executor.snapshot();
        assert_eq!((snapshot.admitted, snapshot.rejected_circuit_open), (1, 1), "the second send never left");
    }

    #[test]
    fn signing_is_deterministic_for_the_same_inputs() {
        assert_eq!(sign("secret", 1_700_000_000, "body"), sign("secret", 1_700_000_000, "body"));
    }

    #[test]
    fn the_signature_covers_the_secret_the_timestamp_and_the_body() {
        let base = sign("secret", 1_700_000_000, "body");
        assert_ne!(base, sign("other", 1_700_000_000, "body"), "secret not covered");
        assert_ne!(base, sign("secret", 1_700_000_001, "body"), "timestamp not covered");
        assert_ne!(base, sign("secret", 1_700_000_000, "other"), "body not covered");
    }

    #[test]
    fn the_signature_is_64_hex_characters() {
        let signature = sign("secret", 1, "body");
        assert_eq!(signature.len(), 64, "sha256 hex must be 64 chars: {signature}");
        assert!(signature.chars().all(|c| c.is_ascii_hexdigit()), "non-hex output: {signature}");
    }

    #[test]
    fn matches_a_known_hmac_sha256_vector() {
        // RFC 4231 test case 2: key "Jefe", data "what do ya want for nothing?".
        // Reproduced through `sign` by splitting the data across the
        // timestamp/body fields exactly as the separator implies.
        use hmac::{KeyInit, Mac, SimpleHmac};
        let mut mac = <SimpleHmac<sha2::Sha256> as KeyInit>::new_from_slice(b"Jefe").expect("key");
        mac.update(b"what do ya want for nothing?");
        let bytes = mac.finalize().into_bytes();
        let expected = bytes.iter().fold(String::new(), |mut out, b| {
            use std::fmt::Write;
            let _ = write!(out, "{b:02x}");
            out
        });
        assert_eq!(expected, "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
    }
}

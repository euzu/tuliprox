//! Outbound HTTP for guests: the `send_request` hook of `wasmtime-wasi-http`,
//! implemented on the dedicated plugin `reqwest::Client` under an
//! [`OutboundExecutor`].
//!
//! `wasmtime-wasi-http` is built without `default-send-request`, so the embedder
//! must supply this hook; the default would open raw sockets and build its own
//! TLS stack. The guest never touches a socket: it hands over a request, and
//! the host client (with the proxy and TLS settings of every other outbound
//! call, but deliberately without the resource destination resolver)
//! performs it.
//!
//! Every plugin gets its own executor key (its id): a circuit breaker, a token
//! bucket and a bulkhead, so one misbehaving plugin or upstream cannot exhaust
//! the others. There is no retry here: a guest's HTTP library decides that.
//! The client never follows redirects; a 3xx reaches the guest, so each hop is
//! a separate request under the same limits.
//!
//! Transport limits come from the core request layer
//! ([`enforce_request_transport_limits`]): header count, header bytes and request
//! body size are checked on the request as it will be sent. The https-downgrade
//! rejection of that layer guards redirects this client never follows, so it has
//! nothing to do here. Guest `request-options` timeouts can only tighten ours.
//!
//! The limits are fixed consts.

use bytes::{Bytes, BytesMut};
use http_body_util::{BodyExt, Full};
use shared::{error::TuliproxError, model::Millis};
use std::{convert::Infallible, future::Future, io, sync::Arc, time::Duration};
use tuliprox_core::utils::{
    request::{
        enforce_request_transport_limits, OutboundError, OutboundExecutor, OutboundPolicy, RateLimitConfig,
        RequestFetchOptions,
    },
    CallOutcome, CircuitBreakerPolicy, CountedOutcomes, SystemClock,
};
use wasmtime_wasi_http::{Error, RequestOptions, WasiBody, WasiHttpHooks};

/// One attempt, connect through last body byte.
pub(crate) const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest response body a guest receives; larger fails the request.
pub(crate) const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
/// Largest request body a guest may send.
pub(crate) const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;
/// Most header values one guest request may carry.
const MAX_REQUEST_HEADERS: usize = 64;
/// Most bytes the header names and values of one guest request may add up to.
const MAX_REQUEST_HEADER_BYTES: usize = 16 * 1024;

/// Consecutive counted failures (connect, timeout, 429, 5xx) that open a plugin's circuit.
const FAILURE_THRESHOLD: u32 = 5;
/// How long an open circuit rejects a plugin's requests before one probe goes out.
const OPEN_DURATION: Millis = Millis::new(30_000);
/// Requests per second one plugin may send once its burst is spent.
const RATE_PER_SECOND: u32 = 20;
/// Requests one plugin may send at once before the rate limit queues it.
const RATE_BURST: u32 = 40;
/// Requests one plugin may have in flight at once.
const MAX_CONCURRENCY: usize = 8;
/// One request including its queue waits: the attempt timeout plus slack for the limits.
const OPERATION_DEADLINE: Millis = Millis::new(35_000);

type IoFuture = Box<dyn Future<Output = Result<(), Error>> + Send>;
type SendFuture = Box<dyn Future<Output = Result<(http::Response<WasiBody>, IoFuture), Error>> + Send>;

fn plugin_policy() -> OutboundPolicy {
    OutboundPolicy::default()
        .with_circuit_breaker(CircuitBreakerPolicy {
            failure_threshold: FAILURE_THRESHOLD,
            open_duration: OPEN_DURATION,
            probe_lease: OPERATION_DEADLINE,
            half_open_probes: 1,
            counted: CountedOutcomes::NONE.with(CallOutcome::Transient).with(CallOutcome::RateLimited),
        })
        .with_rate_limit(RateLimitConfig::per_second(RATE_PER_SECOND, RATE_BURST))
        .with_max_concurrency(MAX_CONCURRENCY)
        .with_deadline(OPERATION_DEADLINE)
}

/// The plugin client and the executor every plugin's requests run under.
#[derive(Clone)]
pub(crate) struct Outbound {
    client: reqwest::Client,
    executor: Arc<OutboundExecutor<SystemClock>>,
}

impl Outbound {
    pub(crate) fn new(client: reqwest::Client) -> Self { Self { client, executor: Arc::new(OutboundExecutor::new()) } }

    /// Give `plugin` its own breaker, rate limit and bulkhead.
    ///
    /// Idempotent: the policy is a constant, so a key that is already registered
    /// is kept as it is. That matters on a registry reload, because replacing a
    /// key would close an open circuit, refill the token bucket and release the
    /// callers queued on it. Only a new key can fail (executor key capacity).
    pub(crate) fn register(&self, plugin: &Arc<str>) -> Result<(), String> {
        if self.executor.circuit_snapshot(plugin).is_some() {
            return Ok(());
        }
        self.executor.register(Arc::clone(plugin), plugin_policy()).map_err(|err| err.to_string())
    }
}

/// One plugin's outbound side: its executor key and whether the `network`
/// capability was granted. Part of [`crate::state::PluginEnv`].
#[derive(Clone)]
pub(crate) struct PluginHttp {
    outbound: Outbound,
    plugin: Arc<str>,
    granted: bool,
}

impl PluginHttp {
    /// Registers `plugin`'s executor key (idempotent across reloads).
    pub(crate) fn new(outbound: &Outbound, plugin: &Arc<str>, granted: bool) -> Result<Self, TuliproxError> {
        outbound.register(plugin).map_err(TuliproxError::Plugin)?;
        Ok(Self { outbound: outbound.clone(), plugin: Arc::clone(plugin), granted })
    }

    /// `GET url` through the same client, executor key and capability check as
    /// guest requests. Returns the body (at most `cap` bytes, further capped by
    /// [`MAX_RESPONSE_BYTES`]) and the `Content-Type` header. Redirects are not
    /// followed and a non-2xx status is an `Err`. The message never carries the
    /// URL, which may hold credentials.
    pub(crate) async fn fetch(&self, url: &str, cap: usize) -> Result<(Bytes, Option<String>), String> {
        if !self.granted {
            return Err(Error::HttpRequestDenied.to_string());
        }
        let PluginHttp { outbound: Outbound { client, executor }, plugin, .. } = self;
        let request = client.get(url).build().map_err(|err| map_error(err).to_string())?;
        let cap = cap.min(MAX_RESPONSE_BYTES);
        let fetched = executor
            .execute(plugin, classify, || perform(client, request, Timeouts::default(), cap))
            .await
            .map_err(|err| map_rejection(plugin, err).to_string())?;
        if !fetched.status.is_success() {
            return Err(format!("upstream answered HTTP {}", fetched.status.as_u16()));
        }
        let content_type =
            fetched.headers.get(http::header::CONTENT_TYPE).and_then(|value| value.to_str().ok()).map(str::to_string);
        Ok((fetched.body, content_type))
    }
}

/// The `send_request` hook of one `Store`.
pub(crate) struct GuestHttpHooks {
    http: PluginHttp,
}

impl GuestHttpHooks {
    pub(crate) fn new(http: &PluginHttp) -> Self { Self { http: http.clone() } }
}

impl WasiHttpHooks for GuestHttpHooks {
    // reqwest derives `Host` from the URL; a guest-supplied copy would only be
    // a second opinion.
    fn set_host_header(&mut self) -> bool { false }

    fn send_request(
        &mut self,
        request: http::Request<WasiBody>,
        options: Option<RequestOptions>,
        _fut: IoFuture,
    ) -> SendFuture {
        let PluginHttp { outbound: Outbound { client, executor }, plugin, granted } = self.http.clone();
        let timeouts = Timeouts::from_options(options);
        Box::new(async move {
            if !granted {
                return Err(Error::HttpRequestDenied);
            }
            let fetched = executor
                .execute(&plugin, classify, || async {
                    let request = guest_request(&client, request).await?;
                    perform(&client, request, timeouts, MAX_RESPONSE_BYTES).await
                })
                .await
                .map_err(|err| map_rejection(&plugin, err))?;
            let done: IoFuture = Box::new(async { Ok(()) });
            Ok((fetched.into_response(), done))
        })
    }
}

/// The timeouts of one attempt. `ATTEMPT_TIMEOUT` always bounds the whole
/// attempt (set on the request itself); these tighten phases of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Timeouts {
    /// Until the response head has arrived (connect plus first byte).
    head: Duration,
    /// Between two chunks of the response body.
    between_bytes: Option<Duration>,
}

impl Default for Timeouts {
    fn default() -> Self { Self { head: ATTEMPT_TIMEOUT, between_bytes: None } }
}

impl Timeouts {
    /// min(guest, ours). The client cannot time connect and first byte apart,
    /// so the head budget only tightens when the guest set both (their sum is
    /// the faithful bound); one alone could cut a slow but healthy answer short.
    fn from_options(options: Option<RequestOptions>) -> Self {
        let Some(options) = options else { return Self::default() };
        let head = match (options.connect_timeout, options.first_byte_timeout) {
            (Some(connect), Some(first_byte)) => ATTEMPT_TIMEOUT.min(connect.saturating_add(first_byte)),
            _ => ATTEMPT_TIMEOUT,
        };
        Self { head, between_bytes: options.between_bytes_timeout.map(|timeout| timeout.min(ATTEMPT_TIMEOUT)) }
    }
}

/// A complete response, body decoded and capped.
struct Fetched {
    status: http::StatusCode,
    headers: http::HeaderMap,
    body: Bytes,
}

impl Fetched {
    fn into_response(self) -> http::Response<WasiBody> {
        let body: WasiBody = Full::new(self.body).map_err(|never: Infallible| match never {}).boxed_unsync();
        let mut out = http::Response::new(body);
        *out.status_mut() = self.status;
        *out.headers_mut() = self.headers;
        out
    }
}

/// The guest's request as a `reqwest::Request`: body read under
/// [`MAX_REQUEST_BYTES`], URL and method taken as they are.
async fn guest_request(client: &reqwest::Client, request: http::Request<WasiBody>) -> Result<reqwest::Request, Error> {
    let (parts, body) = request.into_parts();
    let body = read_capped(body, MAX_REQUEST_BYTES).await?;
    let url = parts.uri.to_string();
    let mut builder = client.request(parts.method, url.as_str()).headers(parts.headers);
    if !body.is_empty() {
        builder = builder.body(body);
    }
    builder.build().map_err(map_error)
}

/// Send `request` and collect the response under every local limit.
async fn perform(
    client: &reqwest::Client,
    mut request: reqwest::Request,
    timeouts: Timeouts,
    cap: usize,
) -> Result<Fetched, Error> {
    if !matches!(request.url().scheme(), "http" | "https") {
        return Err(Error::HttpRequestUriInvalid);
    }
    *request.timeout_mut() = Some(ATTEMPT_TIMEOUT);
    check_request_limits(&request)?;

    let mut response = tokio::time::timeout(timeouts.head, client.execute(request))
        .await
        .map_err(|_| Error::HttpResponseTimeout)?
        .map_err(map_error)?;

    if let Some(length) = response.content_length().filter(|length| *length > cap as u64) {
        return Err(Error::HttpResponseBodySize(Some(length)));
    }
    let status = response.status();
    let headers = response.headers().clone();
    let mut collected = BytesMut::new();
    loop {
        let next = match timeouts.between_bytes {
            Some(limit) => {
                tokio::time::timeout(limit, response.chunk()).await.map_err(|_| Error::HttpResponseTimeout)?
            }
            None => response.chunk().await,
        };
        let Some(chunk) = next.map_err(map_error)? else { break };
        if collected.len() + chunk.len() > cap {
            return Err(Error::HttpResponseBodySize(None));
        }
        collected.extend_from_slice(&chunk);
    }
    Ok(Fetched { status, headers, body: collected.freeze() })
}

/// The core transport limits, one at a time so a hit maps to its own error code.
fn check_request_limits(request: &reqwest::Request) -> Result<(), Error> {
    let limits = RequestFetchOptions::default();
    let headers_ok = enforce_request_transport_limits(request, limits.with_max_request_headers(MAX_REQUEST_HEADERS))
        .is_ok()
        && enforce_request_transport_limits(request, limits.with_max_request_header_bytes(MAX_REQUEST_HEADER_BYTES))
            .is_ok();
    if !headers_ok {
        let total = request
            .headers()
            .iter()
            .fold(0usize, |total, (name, value)| total.saturating_add(name.as_str().len()).saturating_add(value.len()));
        return Err(Error::HttpRequestHeaderSectionSize(u32::try_from(total).ok()));
    }
    if enforce_request_transport_limits(request, limits.with_max_request_body_bytes(MAX_REQUEST_BYTES as u64)).is_err()
    {
        let size = request.body().and_then(reqwest::Body::as_bytes).map(|bytes| bytes.len() as u64);
        return Err(Error::HttpRequestBodySize(size));
    }
    Ok(())
}

/// How the breaker sees one request. A 4xx is the upstream answering, not
/// failing; size and URL errors are the guest's own request.
fn classify(result: &Result<Fetched, Error>) -> CallOutcome {
    match result {
        Ok(fetched) if fetched.status == http::StatusCode::TOO_MANY_REQUESTS => CallOutcome::RateLimited,
        Ok(fetched) if fetched.status.is_server_error() => CallOutcome::Transient,
        Ok(_) => CallOutcome::Success,
        Err(
            Error::HttpRequestBodySize(_)
            | Error::HttpResponseBodySize(_)
            | Error::HttpRequestUriInvalid
            | Error::HttpRequestHeaderSectionSize(_),
        ) => CallOutcome::LocalRejection,
        Err(_) => CallOutcome::Transient,
    }
}

/// A local rejection becomes a `wasi:http` error code the guest can act on:
/// rate limit and bulkhead are `ConnectionLimitReached`, the operation deadline
/// is `HttpResponseTimeout`, an open circuit (and a cancelled or unregistered
/// key) is an `InternalError` whose text says why.
fn map_rejection(plugin: &str, err: OutboundError<Error>) -> Error {
    match err {
        OutboundError::Inner(err) => err,
        OutboundError::RateLimited { .. } | OutboundError::BulkheadFull => Error::ConnectionLimitReached,
        OutboundError::DeadlineExceeded => Error::HttpResponseTimeout,
        OutboundError::CircuitOpen { retry_after } => {
            log::debug!("[plugin:{plugin}] outbound request refused: circuit open");
            Error::InternalError(Some(match retry_after {
                Some(after) => format!("outbound circuit open, retry in {after}"),
                None => "outbound circuit open".to_string(),
            }))
        }
        OutboundError::Cancelled => Error::InternalError(Some("outbound request cancelled".to_string())),
        OutboundError::UnknownKey => Error::InternalError(Some("outbound key is not registered".to_string())),
    }
}

/// Collect `body`, failing as soon as it exceeds `cap` bytes.
pub(crate) async fn read_capped(mut body: WasiBody, cap: usize) -> Result<Bytes, Error> {
    let mut collected = BytesMut::new();
    while let Some(frame) = body.frame().await {
        if let Ok(data) = frame?.into_data() {
            if collected.len() + data.len() > cap {
                return Err(Error::HttpRequestBodySize(None));
            }
            collected.extend_from_slice(&data);
        }
    }
    Ok(collected.freeze())
}

/// `reqwest` errors carry the request URL, which may hold credentials; strip it
/// before the message crosses into the guest.
fn map_error(err: reqwest::Error) -> Error {
    if err.is_timeout() {
        return if err.is_connect() { Error::ConnectionTimeout } else { Error::HttpResponseTimeout };
    }
    if err.is_connect() {
        return Error::Connect(io::Error::other(err.without_url().to_string()));
    }
    if err.is_builder() {
        return Error::HttpRequestUriInvalid;
    }
    if err.is_decode() {
        return Error::HttpResponseContentCoding(Some(err.without_url().to_string()));
    }
    if err.is_body() {
        return Error::HttpResponseIncomplete;
    }
    if err.is_request() {
        return Error::ConnectionTerminated;
    }
    Error::InternalError(Some(err.without_url().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(chunks: &[&'static [u8]]) -> WasiBody {
        let data: Vec<u8> = chunks.concat();
        Full::new(Bytes::from(data)).map_err(|never: Infallible| match never {}).boxed_unsync()
    }

    fn outbound() -> Outbound { Outbound::new(reqwest::Client::new()) }

    fn fetched(status: u16) -> Fetched {
        Fetched {
            status: http::StatusCode::from_u16(status).unwrap(),
            headers: http::HeaderMap::new(),
            body: Bytes::new(),
        }
    }

    fn request(headers: &[(&str, &str)], body: &'static [u8]) -> reqwest::Request {
        let mut builder = reqwest::Client::new().post("http://127.0.0.1:1/");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        if !body.is_empty() {
            builder = builder.body(body);
        }
        builder.build().unwrap()
    }

    #[tokio::test]
    async fn read_capped_returns_a_body_within_the_cap() {
        let bytes = read_capped(body(&[b"abc", b"def"]), 6).await.unwrap();
        assert_eq!(&bytes[..], b"abcdef");
    }

    #[tokio::test]
    async fn read_capped_rejects_a_body_over_the_cap() {
        assert!(matches!(read_capped(body(&[b"abcdef"]), 5).await, Err(Error::HttpRequestBodySize(None))));
    }

    #[test]
    fn classify_counts_upstream_failures_and_spares_the_guest_and_4xx() {
        assert_eq!(classify(&Ok(fetched(200))), CallOutcome::Success);
        assert_eq!(classify(&Ok(fetched(302))), CallOutcome::Success);
        assert_eq!(classify(&Ok(fetched(404))), CallOutcome::Success);
        assert_eq!(classify(&Ok(fetched(429))), CallOutcome::RateLimited);
        assert_eq!(classify(&Ok(fetched(503))), CallOutcome::Transient);
        for local in [
            Error::HttpRequestBodySize(None),
            Error::HttpResponseBodySize(Some(1)),
            Error::HttpRequestUriInvalid,
            Error::HttpRequestHeaderSectionSize(None),
        ] {
            assert_eq!(classify(&Err(local)), CallOutcome::LocalRejection);
        }
        assert_eq!(classify(&Err(Error::ConnectionTimeout)), CallOutcome::Transient);
        assert_eq!(classify(&Err(Error::HttpResponseTimeout)), CallOutcome::Transient);
    }

    #[test]
    fn local_rejections_map_to_wasi_error_codes() {
        let map = |err| map_rejection("p", err);
        assert!(matches!(map(OutboundError::RateLimited { retry_after: None }), Error::ConnectionLimitReached));
        assert!(matches!(map(OutboundError::BulkheadFull), Error::ConnectionLimitReached));
        assert!(matches!(map(OutboundError::DeadlineExceeded), Error::HttpResponseTimeout));
        assert!(matches!(map(OutboundError::Inner(Error::HttpRequestDenied)), Error::HttpRequestDenied));
        match map(OutboundError::CircuitOpen { retry_after: Some(Millis::new(1500)) }) {
            Error::InternalError(Some(message)) => {
                assert!(message.contains("circuit open"), "{message}");
                assert!(message.contains("1500ms"), "{message}");
            }
            other => panic!("circuit open must be an InternalError, got {other:?}"),
        }
        assert!(matches!(map(OutboundError::Cancelled), Error::InternalError(Some(_))));
        assert!(matches!(map(OutboundError::UnknownKey), Error::InternalError(Some(_))));
    }

    #[test]
    fn request_limits_map_to_size_codes() {
        assert!(check_request_limits(&request(&[("x-a", "1")], b"body")).is_ok());

        let names: Vec<String> = (0..=MAX_REQUEST_HEADERS).map(|index| format!("x-h{index}")).collect();
        let many: Vec<(&str, &str)> = names.iter().map(|name| (name.as_str(), "1")).collect();
        assert!(matches!(check_request_limits(&request(&many, b"")), Err(Error::HttpRequestHeaderSectionSize(_))));

        let long = "v".repeat(MAX_REQUEST_HEADER_BYTES);
        assert!(matches!(
            check_request_limits(&request(&[("x-long", long.as_str())], b"")),
            Err(Error::HttpRequestHeaderSectionSize(Some(_)))
        ));
    }

    #[test]
    fn guest_timeouts_only_tighten_ours() {
        assert_eq!(Timeouts::from_options(None), Timeouts::default());
        let secs = Duration::from_secs;
        let tight = Timeouts::from_options(Some(RequestOptions {
            connect_timeout: Some(secs(2)),
            first_byte_timeout: Some(secs(3)),
            between_bytes_timeout: Some(secs(4)),
        }));
        assert_eq!(tight, Timeouts { head: secs(5), between_bytes: Some(secs(4)) });
        let loose = Timeouts::from_options(Some(RequestOptions {
            connect_timeout: Some(secs(600)),
            first_byte_timeout: Some(secs(600)),
            between_bytes_timeout: Some(secs(600)),
        }));
        assert_eq!(loose, Timeouts { head: ATTEMPT_TIMEOUT, between_bytes: Some(ATTEMPT_TIMEOUT) });
        // One head timeout alone cannot bound connect plus first byte.
        let lone = Timeouts::from_options(Some(RequestOptions {
            connect_timeout: Some(secs(1)),
            ..RequestOptions::default()
        }));
        assert_eq!(lone, Timeouts::default());
    }

    #[test]
    fn a_builder_error_is_an_invalid_uri() {
        let err = reqwest::Client::new().get("not a url").build().unwrap_err();
        assert!(matches!(map_error(err), Error::HttpRequestUriInvalid));
    }

    #[tokio::test]
    async fn a_scheme_other_than_http_is_an_invalid_uri() {
        let client = reqwest::Client::new();
        // `build()` rejects the scheme itself now, so assemble the request directly:
        // `perform` must still refuse it on its own.
        let url = reqwest::Url::parse("file:///etc/passwd").unwrap();
        let request = reqwest::Request::new(reqwest::Method::GET, url);
        assert!(matches!(
            perform(&client, request, Timeouts::default(), 1024).await,
            Err(Error::HttpRequestUriInvalid)
        ));
    }

    #[test]
    fn register_is_idempotent_and_keeps_the_existing_key() {
        let outbound = outbound();
        let plugin: Arc<str> = Arc::from("sports");
        outbound.register(&plugin).unwrap();
        outbound.register(&plugin).unwrap();
        assert!(PluginHttp::new(&outbound, &plugin, true).is_ok());
        assert_eq!(outbound.executor.len(), 1);
        let other: Arc<str> = Arc::from("matcher");
        outbound.register(&other).unwrap();
        assert_eq!(outbound.executor.len(), 2);
    }

    #[tokio::test]
    async fn fetch_without_the_network_capability_is_denied_before_any_request() {
        let outbound = outbound();
        let plugin: Arc<str> = Arc::from("denied");
        let http = PluginHttp::new(&outbound, &plugin, false).unwrap();
        let err = http.fetch("http://127.0.0.1:1/x", 1024).await.unwrap_err();
        assert_eq!(err, Error::HttpRequestDenied.to_string());
        assert_eq!(outbound.executor.snapshot().started, 0);
    }

    #[tokio::test]
    async fn guest_requests_without_the_network_capability_are_denied() {
        let outbound = outbound();
        let plugin: Arc<str> = Arc::from("denied");
        let http = PluginHttp::new(&outbound, &plugin, false).unwrap();
        let mut hooks = GuestHttpHooks::new(&http);
        let request = http::Request::builder().uri("http://127.0.0.1:1/").body(body(&[])).unwrap();
        let done: IoFuture = Box::new(async { Ok(()) });
        let result = Box::into_pin(hooks.send_request(request, None, done)).await;
        assert!(matches!(result, Err(Error::HttpRequestDenied)));
        assert_eq!(outbound.executor.snapshot().started, 0);
    }
}

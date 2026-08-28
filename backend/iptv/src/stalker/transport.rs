//! Where a Stalker request actually goes.
//!
//! [`StalkerApiClient`](super::client::StalkerApiClient) used to own a `reqwest::Client`
//! outright, which put every interesting decision it makes — the recipe fallback chain,
//! pagination termination, the 401-triggered re-handshake, the portal's habit of reporting
//! `{"code": 44}` inside a `200 OK` — behind a live network connection. The module docs
//! conceded as much: *no HTTP requests are issued from unit tests*.
//!
//! Splitting *send this* out from *decide what to send* is all it takes to reach that
//! logic. Requests are still built with `reqwest`'s builder, because the fake needs to
//! build them too and there is nothing to gain from re-modelling a query string; only the
//! send is abstracted.
//!
//! The trait is held as a generic parameter defaulted to [`ReqwestTransport`], so
//! production construction sites are unchanged and there is no vtable on the hot path.

use crate::stalker::error::{StalkerError, StalkerResult};
use reqwest::{Client, Request, RequestBuilder, Response};
use std::future::Future;

pub trait StalkerTransport: Send + Sync + 'static {
    /// Start a request. Stalker portals answer `GET` for every action, including the ones
    /// that mutate session state.
    fn get(&self, url: &str) -> RequestBuilder;

    /// Send a built request.
    ///
    /// Errors are already domain errors rather than `reqwest::Error`, both because callers
    /// only ever converted them anyway and because `reqwest::Error` cannot be constructed
    /// outside `reqwest` — a fake could not report a transport failure otherwise.
    fn execute(&self, request: Request) -> impl Future<Output = StalkerResult<Response>> + Send;
}

/// The production transport: a real `reqwest::Client`, so connection pooling, timeouts and
/// proxy settings stay under the caller's control exactly as before.
#[derive(Debug, Clone)]
pub struct ReqwestTransport {
    client: Client,
}

impl ReqwestTransport {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    #[must_use]
    pub fn client(&self) -> &Client {
        &self.client
    }
}

impl StalkerTransport for ReqwestTransport {
    fn get(&self, url: &str) -> RequestBuilder {
        self.client.get(url)
    }

    fn execute(&self, request: Request) -> impl Future<Output = StalkerResult<Response>> + Send {
        let client = self.client.clone();
        async move { client.execute(request).await.map_err(StalkerError::from) }
    }
}

/// Sharing one transport across several clients — several inputs against the same portal
/// host, say — is just `Arc`.
impl<T: StalkerTransport> StalkerTransport for std::sync::Arc<T> {
    fn get(&self, url: &str) -> RequestBuilder {
        (**self).get(url)
    }

    fn execute(&self, request: Request) -> impl Future<Output = StalkerResult<Response>> + Send {
        (**self).execute(request)
    }
}

/// Test doubles for the transport seam.
///
/// Kept in the crate rather than a test file because the logic worth reaching from a fake
/// — recipe fallback, pagination, the 4xx re-handshake — lives in sibling modules.
#[cfg(test)]
pub mod testing {
    use super::{StalkerTransport, StalkerResult};
    use crate::stalker::error::StalkerError;
    use parking_lot::Mutex;
    use reqwest::{Client, Request, RequestBuilder, Response};
    use std::{collections::VecDeque, future::Future};

    /// What the fake portal answers with, one per request, in order.
    pub enum Reply {
        /// An HTTP response with this status and body.
        Http(u16, String),
        /// A transport-level failure — a connection reset, a timeout.
        Transport(StalkerError),
    }

    impl Reply {
        /// `200 OK` carrying `body`.
        pub fn ok(body: &str) -> Self {
            Self::Http(200, body.to_string())
        }
    }

    /// A portal that answers from a script instead of the network, and records what it
    /// was asked for.
    pub struct FakeTransport {
        /// Only ever used to *build* requests; it never sends one.
        client: Client,
        replies: Mutex<VecDeque<Reply>>,
        requested: Mutex<Vec<String>>,
    }

    impl FakeTransport {
        pub fn new(replies: impl IntoIterator<Item = Reply>) -> Self {
            Self {
                client: Client::new(),
                replies: Mutex::new(replies.into_iter().collect()),
                requested: Mutex::new(Vec::new()),
            }
        }

        /// Every URL the client asked for, in order, including query strings.
        pub fn requested(&self) -> Vec<String> {
            self.requested.lock().clone()
        }

        /// The paths asked for, without scheme, host or query — enough to assert which
        /// endpoint candidate was tried without pinning the whole URL.
        pub fn requested_paths(&self) -> Vec<String> {
            self.requested
                .lock()
                .iter()
                .filter_map(|url| url::Url::parse(url).ok())
                .map(|url| url.path().to_string())
                .collect()
        }
    }

    impl StalkerTransport for FakeTransport {
        fn get(&self, url: &str) -> RequestBuilder {
            self.client.get(url)
        }

        fn execute(&self, request: Request) -> impl Future<Output = StalkerResult<Response>> + Send {
            self.requested.lock().push(request.url().to_string());
            let reply = self.replies.lock().pop_front();
            async move {
                match reply {
                    Some(Reply::Http(status, body)) => {
                        let response = http::Response::builder()
                            .status(status)
                            .body(body)
                            .map_err(|err| StalkerError::BodyDecode { message: err.to_string() })?;
                        Ok(Response::from(response))
                    }
                    Some(Reply::Transport(err)) => Err(err),
                    None => Err(StalkerError::EmptyBody { action: "fake transport ran out of replies".to_string() }),
                }
            }
        }
    }
}

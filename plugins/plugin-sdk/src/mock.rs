//! The mock host: in-memory fakes for everything the plugin imports, used
//! automatically on every target but `wasm32`.
//!
//! State is thread-local, so parallel tests do not see each other. Start each
//! test with [`reset`].
//!
//! ```
//! use tuliprox_plugin_sdk::{clock, kv, mock};
//!
//! mock::reset();
//! mock::set_now(100);
//! kv::set_str("greeting", "hi").unwrap();
//! assert_eq!(clock::now_unix(), 100);
//! assert_eq!(kv::get_str("greeting").unwrap().as_deref(), Some("hi"));
//! ```

use crate::{
    bindings::tuliprox::plugin::{host::KvError, types::LogLevel},
    http::{Error as HttpError, Method, Request, Response},
};
use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
};

/// The host's KV limits (`backend/plugins` `kv.rs`).
pub const MAX_KEY_BYTES: usize = 1024;
pub const MAX_VALUE_BYTES: usize = 1 << 20;
pub const MAX_TOTAL_BYTES: usize = 50 << 20;

/// One `host::log` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub level: LogLevel,
    pub message: String,
}

struct Host {
    logs: Vec<LogLine>,
    kv: BTreeMap<String, Vec<u8>>,
    kv_unavailable: Option<String>,
    config: String,
    now: i64,
    images: BTreeMap<String, Result<String, String>>,
    cached: Vec<String>,
    http_queue: BTreeMap<(String, String), VecDeque<Result<Response, HttpError>>>,
    http_log: Vec<Request>,
}

impl Host {
    fn new() -> Self {
        Self {
            logs: Vec::new(),
            kv: BTreeMap::new(),
            kv_unavailable: None,
            config: "{}".to_string(),
            now: 0,
            images: BTreeMap::new(),
            cached: Vec::new(),
            http_queue: BTreeMap::new(),
            http_log: Vec::new(),
        }
    }
}

thread_local! {
    static HOST: RefCell<Host> = RefCell::new(Host::new());
}

fn with<T>(f: impl FnOnce(&mut Host) -> T) -> T { HOST.with(|host| f(&mut host.borrow_mut())) }

/// Forgets everything: logs, KV, config (`{}`), clock (0), images, HTTP.
pub fn reset() { with(|host| *host = Host::new()); }

/// The settings JSON `host::get-config` returns.
pub fn set_config(json: &str) { with(|host| host.config = json.to_string()); }

/// Freezes the host clock at `unix` seconds.
pub fn set_now(unix: i64) { with(|host| host.now = unix); }

/// Moves the host clock forward.
pub fn advance(secs: i64) { with(|host| host.now += secs); }

/// Makes every KV call fail with `unavailable(msg)`.
pub fn set_kv_unavailable(msg: &str) { with(|host| host.kv_unavailable = Some(msg.to_string())); }

/// Every KV entry, by key.
#[must_use]
pub fn kv_snapshot() -> BTreeMap<String, Vec<u8>> { with(|host| host.kv.clone()) }

/// Every `host::log` call so far.
#[must_use]
pub fn logs() -> Vec<LogLine> { with(|host| host.logs.clone()) }

/// What `host::cache-image(url)` returns for `url`. The default is
/// `Ok("/mock-cache/<url>")`.
pub fn set_cached_image(url: &str, result: Result<&str, &str>) {
    with(|host| {
        host.images.insert(url.to_string(), result.map(str::to_string).map_err(str::to_string));
    });
}

/// The URLs passed to `host::cache-image`, in call order.
#[must_use]
pub fn cached_images() -> Vec<String> { with(|host| host.cached.clone()) }

/// Queues the next response for `method url` (first in, first out).
pub fn http_respond(method: Method, url: &str, status: u16, body: impl Into<Vec<u8>>) {
    queue_http(method, url, Ok(Response::new(status, Vec::new(), body.into())));
}

/// Queues a response with headers.
pub fn http_respond_with(method: Method, url: &str, response: Response) { queue_http(method, url, Ok(response)); }

/// Queues a transport failure (connection refused, timeout) for `method url`.
pub fn http_fail(method: Method, url: &str, message: &str) {
    queue_http(method, url, Err(HttpError::Transport(message.to_string())));
}

fn queue_http(method: Method, url: &str, outcome: Result<Response, HttpError>) {
    with(|host| host.http_queue.entry((method.as_str().to_string(), url.to_string())).or_default().push_back(outcome));
}

/// Every request sent through [`crate::http`], in call order.
#[must_use]
pub fn http_requests() -> Vec<Request> { with(|host| host.http_log.clone()) }

pub(crate) fn record_log(level: LogLevel, message: &str) {
    with(|host| host.logs.push(LogLine { level, message: message.to_string() }));
}

fn kv_available(host: &Host) -> Result<(), KvError> {
    host.kv_unavailable.as_ref().map_or(Ok(()), |msg| Err(KvError::Unavailable(msg.clone())))
}

pub(crate) fn kv_get(key: &str) -> Result<Option<Vec<u8>>, KvError> {
    with(|host| {
        kv_available(host)?;
        Ok(host.kv.get(key).cloned())
    })
}

pub(crate) fn kv_set(key: &str, value: &[u8]) -> Result<(), KvError> {
    with(|host| {
        kv_available(host)?;
        if key.len() > MAX_KEY_BYTES {
            return Err(KvError::KeyTooLarge);
        }
        if value.len() > MAX_VALUE_BYTES {
            return Err(KvError::ValueTooLarge);
        }
        let others: usize = host.kv.iter().filter(|(k, _)| k.as_str() != key).map(|(k, v)| k.len() + v.len()).sum();
        if others + key.len() + value.len() > MAX_TOTAL_BYTES {
            return Err(KvError::QuotaExceeded);
        }
        host.kv.insert(key.to_string(), value.to_vec());
        Ok(())
    })
}

pub(crate) fn kv_delete(key: &str) -> Result<(), KvError> {
    with(|host| {
        kv_available(host)?;
        host.kv.remove(key);
        Ok(())
    })
}

pub(crate) fn config() -> String { with(|host| host.config.clone()) }

pub(crate) fn now_unix() -> i64 { with(|host| host.now) }

pub(crate) fn cache_image(url: &str) -> Result<String, String> {
    with(|host| {
        host.cached.push(url.to_string());
        host.images.get(url).cloned().unwrap_or_else(|| Ok(format!("/mock-cache/{url}")))
    })
}

pub(crate) fn http_send(request: &Request) -> Result<Response, HttpError> {
    with(|host| {
        host.http_log.push(request.clone());
        let key = (request.method().as_str().to_string(), request.url().to_string());
        host.http_queue
            .get_mut(&key)
            .and_then(VecDeque::pop_front)
            .unwrap_or_else(|| Err(HttpError::Transport(format!("mock: no response queued for {} {}", key.0, key.1))))
    })
}

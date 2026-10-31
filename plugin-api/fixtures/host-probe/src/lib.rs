//! Phase 1 fixture: probes the host functions, `wasi:http` and the traps.
//!
//! One component serves every id under `manifests/`; each manifest grants a
//! different capability set and resource budget. `on-stage` reads `mode` from
//! the settings JSON and writes `<mode>:<result>` into the first channel's
//! `title` (a sparse `modified` change), so the integration tests observe the
//! results through the items. Modes that must trap never return.

wit_bindgen::generate!({
    path: "../../wit",
    world: "playlist-plugin",
    generate_all,
});

use exports::tuliprox::plugin::{
    lifecycle,
    playlist_hooks::{self, Stage},
};
use serde_json::Value;
use tuliprox::plugin::{
    host,
    types::{Channel, ChannelChange, LogLevel},
};
use wasi::http::{
    outgoing_handler,
    types::{Fields, IncomingBody, Method, OutgoingRequest, Scheme},
};
use wasi::io::streams::StreamError;

const ONE_MIB: usize = 1 << 20;
const LOG_BURST: usize = 500;
const CHUNK: u64 = 64 * 1024;

struct Probe;

/// The kebab-case name of a WIT variant, from its Rust `Debug` form:
/// `HttpRequestDenied(None)` becomes `http-request-denied`.
fn kebab(debug: &str) -> String {
    let name = debug.split('(').next().unwrap_or_default();
    let mut out = String::with_capacity(name.len() + 4);
    for (index, ch) in name.chars().enumerate() {
        if ch.is_uppercase() {
            if index > 0 {
                out.push('-');
            }
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

fn load_settings() -> Value { serde_json::from_str(&host::get_config()).unwrap_or(Value::Null) }

fn text<'a>(settings: &'a Value, key: &str) -> &'a str { settings.get(key).and_then(Value::as_str).unwrap_or_default() }

/// Never returns: the epoch deadline has to end it.
fn spin() -> ! {
    let mut ticks: u64 = 0;
    loop {
        ticks = core::hint::black_box(ticks.wrapping_add(1));
    }
}

/// Never returns: keeps touched 1 MiB blocks alive until the memory limit traps.
fn exhaust_memory() -> ! {
    let mut held: Vec<Vec<u8>> = Vec::new();
    loop {
        let mut block = vec![0_u8; ONE_MIB];
        block[0] = 1;
        held.push(core::hint::black_box(block));
    }
}

/// Increments the `count` key; a new instance sees what the last one stored.
fn kv_counter() -> String {
    let current = match host::kv_get("count") {
        Ok(Some(bytes)) => String::from_utf8_lossy(&bytes).parse::<u64>().unwrap_or(0),
        Ok(None) => 0,
        Err(err) => return kebab(&format!("{err:?}")),
    };
    let next = current + 1;
    match host::kv_set("count", next.to_string().as_bytes()) {
        Ok(()) => next.to_string(),
        Err(err) => kebab(&format!("{err:?}")),
    }
}

fn scheme_and_authority(url: &str) -> (Scheme, &str, &str) {
    let (scheme, rest) = match url.strip_prefix("https://") {
        Some(rest) => (Scheme::Https, rest),
        None => (Scheme::Http, url.strip_prefix("http://").unwrap_or(url)),
    };
    match rest.find('/') {
        Some(split) => (scheme, &rest[..split], &rest[split..]),
        None => (scheme, rest, "/"),
    }
}

/// Drains the body, then reads the final error from the trailers future, which
/// is where `wasi:http` reports a body that broke part way through.
fn drain(body: IncomingBody) -> Result<(), String> {
    let stream = body.stream().map_err(|()| "body-stream".to_string())?;
    loop {
        match stream.blocking_read(CHUNK) {
            Ok(_) => {}
            Err(StreamError::Closed | StreamError::LastOperationFailed(_)) => break,
        }
    }
    drop(stream);
    let trailers = IncomingBody::finish(body);
    trailers.subscribe().block();
    match trailers.get() {
        Some(Ok(Err(code))) => Err(kebab(&format!("{code:?}"))),
        _ => Ok(()),
    }
}

/// `GET url` through `wasi:http`: the status, or the kebab-case error code.
fn fetch(url: &str) -> Result<u16, String> {
    let (scheme, authority, path) = scheme_and_authority(url);
    let request = OutgoingRequest::new(Fields::new());
    request.set_method(&Method::Get).map_err(|()| "set-method".to_string())?;
    request.set_scheme(Some(&scheme)).map_err(|()| "set-scheme".to_string())?;
    request.set_authority(Some(authority)).map_err(|()| "set-authority".to_string())?;
    request.set_path_with_query(Some(path)).map_err(|()| "set-path".to_string())?;
    let future = outgoing_handler::handle(request, None).map_err(|code| kebab(&format!("{code:?}")))?;
    future.subscribe().block();
    let response = match future.get() {
        Some(Ok(Ok(response))) => response,
        Some(Ok(Err(code))) => return Err(kebab(&format!("{code:?}"))),
        _ => return Err("no-response".to_string()),
    };
    let status = response.status();
    let body = response.consume().map_err(|()| "consume".to_string())?;
    drain(body)?;
    Ok(status)
}

fn run_mode(mode: &str, settings: &Value) -> String {
    match mode {
        "loop" => spin(),
        "oom" => exhaust_memory(),
        "trap" => panic!("trap mode requested"),
        "kv" => kv_counter(),
        "kv-big" => match host::kv_set("big", &vec![0_u8; ONE_MIB + 1]) {
            Ok(()) => "stored".to_string(),
            Err(err) => kebab(&format!("{err:?}")),
        },
        "kv-denied" => match host::kv_get("count") {
            Ok(_) => "allowed".to_string(),
            Err(err) => kebab(&format!("{err:?}")),
        },
        "config" => text(settings, "value").to_string(),
        "now" => host::now_unix().to_string(),
        "http" => match fetch(text(settings, "url")) {
            Ok(status) => status.to_string(),
            Err(code) => format!("err:{code}"),
        },
        "image" => match host::cache_image(text(settings, "url")) {
            Ok(_) => "ok".to_string(),
            Err(_) => "err".to_string(),
        },
        "log" => {
            for index in 0..LOG_BURST {
                host::log(LogLevel::Info, &format!("probe line {index}"));
            }
            "ok".to_string()
        }
        other => format!("unknown-mode:{other}"),
    }
}

impl lifecycle::Guest for Probe {
    fn init(settings: String) -> Result<(), String> {
        let value: Value = serde_json::from_str(&settings).unwrap_or(Value::Null);
        if text(&value, "mode") == "fail-init" {
            return Err("fail-init requested".to_string());
        }
        Ok(())
    }
}

impl playlist_hooks::Guest for Probe {
    fn on_stage(stage: Stage, _target: String, channels: Vec<Channel>) -> Result<Vec<ChannelChange>, String> {
        if !matches!(stage, Stage::AfterFilter) {
            return Ok(Vec::new());
        }
        let Some(mut first) = channels.into_iter().next() else {
            return Ok(Vec::new());
        };
        let settings = load_settings();
        let mode = text(&settings, "mode").to_string();
        first.title = format!("{mode}:{}", run_mode(&mode, &settings));
        Ok(vec![ChannelChange::Modified(first)])
    }
}

export!(Probe);

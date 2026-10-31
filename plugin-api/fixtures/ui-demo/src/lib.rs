//! W6b demo fixture: the rpc methods behind the `ui-demo` frames and MCP tools.
//!
//! `echo` and `counter` are reads, `bump` is a write. The counter lives in the
//! plugin's KV store under `count`, so it survives across calls (each call gets
//! a fresh instance).
//!
//! `spin` never returns. The `ui-demo` manifest does not export it (so the host
//! answers 404); only the `rpc-spin` manifest does, for the isolation tests.

wit_bindgen::generate!({
    path: "../../wit",
    world: "rpc-plugin",
    generate_all,
});

use exports::tuliprox::plugin::{lifecycle, rpc};
use serde_json::{json, Value};
use tuliprox::plugin::{host, types::LogLevel};

const COUNTER_KEY: &str = "count";

struct UiDemo;

fn greeting() -> Value {
    serde_json::from_str::<Value>(&host::get_config())
        .ok()
        .and_then(|settings| settings.get("greeting").cloned())
        .unwrap_or(Value::Null)
}

fn read_counter() -> Result<u64, String> {
    match host::kv_get(COUNTER_KEY) {
        Ok(Some(bytes)) => Ok(String::from_utf8_lossy(&bytes).parse::<u64>().unwrap_or(0)),
        Ok(None) => Ok(0),
        Err(err) => Err(format!("kv-get failed: {err:?}")),
    }
}

fn bump(payload: &Value) -> Result<u64, String> {
    let by = payload
        .get("by")
        .and_then(Value::as_u64)
        .filter(|by| (1..=100).contains(by))
        .ok_or_else(|| "`by` must be an integer from 1 to 100".to_string())?;
    let next = read_counter()?.saturating_add(by);
    host::kv_set(COUNTER_KEY, next.to_string().as_bytes()).map_err(|err| format!("kv-set failed: {err:?}"))?;
    Ok(next)
}

/// Never returns: the call deadline has to end it.
fn spin() -> ! {
    let mut ticks: u64 = 0;
    loop {
        ticks = core::hint::black_box(ticks.wrapping_add(1));
    }
}

impl lifecycle::Guest for UiDemo {
    fn init(_settings: String) -> Result<(), String> {
        host::log(LogLevel::Debug, "ui-demo initialised");
        Ok(())
    }
}

impl rpc::Guest for UiDemo {
    fn call(method: String, payload: String) -> Result<String, String> {
        let payload: Value = serde_json::from_str(&payload).map_err(|err| format!("payload is not JSON: {err}"))?;
        let result = match method.as_str() {
            "echo" => json!({ "echo": payload, "greeting": greeting() }),
            "counter" => json!(read_counter()?),
            "bump" => json!(bump(&payload)?),
            "spin" => spin(),
            other => return Err(format!("unknown method `{other}`")),
        };
        Ok(result.to_string())
    }
}

export!(UiDemo);

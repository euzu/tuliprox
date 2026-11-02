//! Fixture: scheduled tasks.
//!
//! * `ok`: increments the KV key `count` and returns `ok <n>`, so a test sees
//!   that KV survives between runs (each run is a new instance) and that the
//!   host bumps the plugin's `kv_generation`. When the host passes a
//!   `last-success-at`, it is appended: `ok <n> last=<unix seconds>`.
//! * `fail`: returns `err("boom")`.
//! * `slow`: busy for about three seconds of wall clock, then returns
//!   `slow done`. A second fire during that time is dropped
//!   (`concurrency: skip`).
//! * `spin-*`: never returns; only the `task-probe-spin` manifest declares them,
//!   for the isolation tests.

wit_bindgen::generate!({
    path: ["../../wit", "wit"],
    world: "tuliprox:task-probe/task-probe",
    generate_all,
});

use exports::tuliprox::plugin::{
    lifecycle,
    tasks::{self, TaskContext},
};
use tuliprox::plugin::{host, types::LogLevel};

const SLOW_SECS: i64 = 3;

struct TaskProbe;

impl lifecycle::Guest for TaskProbe {
    fn init(_config: String) -> Result<(), String> {
        host::log(LogLevel::Debug, "task-probe initialised");
        Ok(())
    }
}

/// Increments `count`; a new instance sees what the last one stored.
fn counter(last_success_at: Option<i64>) -> Result<String, String> {
    let current = match host::kv_get("count") {
        Ok(Some(bytes)) => String::from_utf8_lossy(&bytes).parse::<u64>().unwrap_or(0),
        Ok(None) => 0,
        Err(err) => return Err(format!("kv-get failed: {err:?}")),
    };
    let next = current + 1;
    host::kv_set("count", next.to_string().as_bytes()).map_err(|err| format!("kv-set failed: {err:?}"))?;
    Ok(match last_success_at {
        Some(at) => format!("ok {next} last={at}"),
        None => format!("ok {next}"),
    })
}

/// Spin on the host clock for `seconds` of wall time.
fn busy(seconds: i64) {
    let until = host::now_unix() + seconds;
    while host::now_unix() < until {
        core::hint::black_box(0_u64);
    }
}

/// Never returns: the task deadline has to end it.
fn spin() -> ! {
    let mut ticks: u64 = 0;
    loop {
        ticks = core::hint::black_box(ticks.wrapping_add(1));
    }
}

impl tasks::Guest for TaskProbe {
    fn run(ctx: TaskContext) -> Result<String, String> {
        match ctx.task_id.as_str() {
            "ok" => counter(ctx.last_success_at),
            "fail" => Err("boom".to_string()),
            "slow" => {
                busy(SLOW_SECS);
                Ok("slow done".to_string())
            }
            id if id.starts_with("spin") => spin(),
            other => Err(format!("unknown task `{other}`")),
        }
    }
}

export!(TaskProbe);

//! W6a fixture: the `on-event` hook. It logs one host line per event it is
//! handed (`event <kind>`, info), so a test can see what the bridge delivered.
//!
//! With the setting `mode: loop` it never returns instead (the isolation tests).
//!
//! One component serves every id under `manifests/`; each manifest subscribes
//! to a different set of `events:` kinds, and the host only calls a plugin for
//! the kinds its manifest lists.

wit_bindgen::generate!({
    path: ["../../wit", "wit"],
    world: "tuliprox:event-probe/event-probe",
    generate_all,
});

use exports::tuliprox::plugin::{event_hooks, lifecycle};
use std::sync::Mutex;
use tuliprox::plugin::{host, types::LogLevel};

static SPIN: Mutex<bool> = Mutex::new(false);

struct EventProbe;

impl lifecycle::Guest for EventProbe {
    fn init(settings: String) -> Result<(), String> {
        // The host sends compact JSON; this fixture has no JSON dependency.
        if let Ok(mut spin) = SPIN.lock() {
            *spin = settings.replace(' ', "").contains("\"mode\":\"loop\"");
        }
        Ok(())
    }
}

impl event_hooks::Guest for EventProbe {
    fn on_event(ev: event_hooks::Event) -> Result<(), String> {
        if SPIN.lock().is_ok_and(|spin| *spin) {
            let mut ticks: u64 = 0;
            loop {
                ticks = core::hint::black_box(ticks.wrapping_add(1));
            }
        }
        host::log(LogLevel::Info, &format!("event {}", ev.kind));
        Ok(())
    }
}

export!(EventProbe);

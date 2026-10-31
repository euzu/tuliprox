//! W6a fixture: the `on-event` hook. It logs one host line per event it is
//! handed (`event <kind>`, info), so a test can see what the bridge delivered.
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
use tuliprox::plugin::{host, types::LogLevel};

struct EventProbe;

impl lifecycle::Guest for EventProbe {
    fn init(_settings: String) -> Result<(), String> { Ok(()) }
}

impl event_hooks::Guest for EventProbe {
    fn on_event(ev: event_hooks::Event) -> Result<(), String> {
        host::log(LogLevel::Info, &format!("event {}", ev.kind));
        Ok(())
    }
}

export!(EventProbe);

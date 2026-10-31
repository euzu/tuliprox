//! `events.on-event`: what the plugin knows about the bus events it subscribed
//! to (`playlist.update`, `user.connection.changed`, `stream.probe.failed`).
//!
//! It keeps a per-kind counter and the last event's summary in the KV and logs
//! one line per event. It never talks to Matrix: delivery goes only through the
//! host's notification channel and outbox, which is what applies routing,
//! suppression, quiet hours and retries. An `on-event` that posted to Matrix
//! would bypass all of that and double every message.

use crate::matrix::clean;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tuliprox_plugin_sdk::{
    events::{PlaylistUpdate, ScheduledTaskFailed, StreamProbeFailed, UserConnectionChanged},
    prelude::*,
};

/// KV key of the [`Tally`].
pub const KEY: &str = "event-tally";

/// The summary of the most recent event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Last {
    pub kind: String,
    /// The event's own timestamp (unix seconds).
    pub timestamp: i64,
    pub summary: String,
}

/// Everything kept in the KV. A `BTreeMap`, so the bytes are deterministic.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tally {
    pub counters: BTreeMap<String, u64>,
    pub last: Option<Last>,
}

/// One event: log it, count it, remember it.
pub fn record(ev: &Event) -> Result<(), String> {
    let summary = match ev.typed() {
        Ok(typed) => summarize(&typed),
        Err(err) => {
            warn!("event {} undecodable: {}", clean(&ev.kind, 60), clean(&err, 160));
            "payload undecodable".to_string()
        }
    };
    let kind = clean(&ev.kind, 60);
    info!("event {kind} {summary}");

    let mut tally = kv::get_json::<Tally>(KEY).unwrap_or_default().unwrap_or_default();
    *tally.counters.entry(kind.clone()).or_insert(0) += 1;
    tally.last = Some(Last { kind, timestamp: ev.timestamp, summary });
    kv::set_json(KEY, &tally)?;
    Ok(())
}

/// A single-line, secret-free description of a typed event.
#[must_use]
pub fn summarize(event: &TypedEvent) -> String {
    match event {
        TypedEvent::PlaylistUpdate(PlaylistUpdate { run_id, state, stats, error, .. }) => format!(
            "state={state:?} run_id={} stats={} error={}",
            run_id.as_deref().map_or_else(|| "-".to_string(), |id| clean(id, 40)),
            stats.len(),
            error.is_some()
        ),
        TypedEvent::UserConnectionChanged(change) => match change {
            UserConnectionChanged::Updated(info) => {
                format!("type=updated user={} uid={}", clean(&info.username, 40), info.uid)
            }
            UserConnectionChanged::Disconnected(_) => "type=disconnected".to_string(),
            UserConnectionChanged::DisconnectedStream { uid, .. } => format!("type=disconnected_stream uid={uid}"),
            UserConnectionChanged::Connections(users, connections) => {
                format!("type=connections users={users} connections={connections}")
            }
            UserConnectionChanged::Unknown => "type=unknown".to_string(),
        },
        TypedEvent::StreamProbeFailed(StreamProbeFailed { input, unique_id, reason, .. }) => {
            format!("input={} stream={} reason={reason:?}", clean(input, 40), clean(unique_id, 40))
        }
        TypedEvent::ScheduledTaskFailed(ScheduledTaskFailed { task, plugin_task, .. }) => {
            format!("task={} plugin_task={}", clean(task, 40), plugin_task.as_deref().map_or("-", |name| name))
        }
        TypedEvent::RecordingChanged => "changed".to_string(),
        TypedEvent::Other { .. } => "(no typed payload)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuliprox_plugin_sdk::mock;

    fn event(kind: &str, payload: &str, timestamp: i64) -> Event {
        Event { kind: kind.to_string(), payload: payload.to_string(), timestamp }
    }

    fn tally() -> Tally { kv::get_json::<Tally>(KEY).unwrap().unwrap() }

    fn lines() -> Vec<String> { mock::logs().into_iter().map(|line| line.message).collect() }

    #[test]
    fn each_kind_gets_one_log_line_a_counter_and_a_last_summary() {
        mock::reset();
        record(&event("playlist.update", r#"{"run_id":"r1","state":"Success","stats":[{},{}]}"#, 10)).unwrap();
        record(&event("playlist.update", r#"{"state":"Failure","error":"boom"}"#, 11)).unwrap();
        record(&event("user.connection.changed", r#"{"type":"connections","payload":[3,5]}"#, 12)).unwrap();
        record(&event("user.connection.changed", r#"{"type":"updated","payload":{"uid":7,"username":"ann"}}"#, 13))
            .unwrap();
        record(&event(
            "stream.probe.failed",
            r#"{"input":"p1","unique_id":"12","url":"http://x/***","reason":"not_found"}"#,
            14,
        ))
        .unwrap();

        assert_eq!(
            lines(),
            vec![
                "event playlist.update state=Success run_id=r1 stats=2 error=false",
                "event playlist.update state=Failure run_id=- stats=0 error=true",
                "event user.connection.changed type=connections users=3 connections=5",
                "event user.connection.changed type=updated user=ann uid=7",
                "event stream.probe.failed input=p1 stream=12 reason=NotFound",
            ]
        );
        let tally = tally();
        assert_eq!(tally.counters.get("playlist.update"), Some(&2));
        assert_eq!(tally.counters.get("user.connection.changed"), Some(&2));
        assert_eq!(tally.counters.get("stream.probe.failed"), Some(&1));
        assert_eq!(
            tally.last,
            Some(Last {
                kind: "stream.probe.failed".to_string(),
                timestamp: 14,
                summary: "input=p1 stream=12 reason=NotFound".to_string()
            })
        );
    }

    #[test]
    fn on_event_never_talks_to_matrix() {
        mock::reset();
        record(&event("playlist.update", r#"{"state":"Failure","error":"boom"}"#, 1)).unwrap();
        record(&event("stream.probe.failed", r#"{"input":"p","reason":"unreachable"}"#, 2)).unwrap();
        assert!(mock::http_requests().is_empty());
    }

    #[test]
    fn an_undecodable_payload_is_still_counted_and_never_an_error() {
        mock::reset();
        record(&event("playlist.update", "{", 5)).unwrap();
        assert_eq!(tally().counters.get("playlist.update"), Some(&1));
        let logged = lines();
        assert_eq!(logged.len(), 2);
        assert!(logged[0].starts_with("event playlist.update undecodable:"));
        assert_eq!(logged[1], "event playlist.update payload undecodable");
    }

    #[test]
    fn an_unknown_kind_is_counted_without_a_typed_summary() {
        mock::reset();
        record(&event("system.disk.alert", r#"{"free":1}"#, 9)).unwrap();
        assert_eq!(lines(), vec!["event system.disk.alert (no typed payload)"]);
        assert_eq!(tally().counters.get("system.disk.alert"), Some(&1));
    }

    #[test]
    fn the_stored_bytes_are_a_function_of_the_events_alone() {
        let run = || {
            mock::reset();
            mock::set_now(1_000);
            record(&event("stream.probe.failed", r#"{"input":"b","reason":"not_found"}"#, 3)).unwrap();
            record(&event("playlist.update", r#"{"state":"Success"}"#, 4)).unwrap();
            mock::kv_snapshot()
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn a_kv_failure_is_reported_but_the_line_is_still_logged() {
        mock::reset();
        mock::set_kv_unavailable("disabled");
        let result = record(&event("playlist.update", r#"{"state":"Success"}"#, 1));
        assert_eq!(result, Err("kv unavailable: disabled".to_string()));
        assert_eq!(lines().len(), 1);
    }

    #[test]
    fn control_characters_in_names_do_not_break_the_log_line() {
        mock::reset();
        record(&event("stream.probe.failed", "{\"input\":\"a\\nb\",\"reason\":\"not_found\"}", 1)).unwrap();
        assert_eq!(lines(), vec!["event stream.probe.failed input=a b stream= reason=NotFound"]);
    }
}

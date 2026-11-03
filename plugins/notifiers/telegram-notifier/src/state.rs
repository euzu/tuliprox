//! What a delivery remembers between attempts.
//!
//! A message goes to several chats and may be split into several parts. If one of
//! them fails for a while, the host's outbox retries the whole notification, and
//! without a memory the chats that already have it would get it again. A *unit* is
//! one part for one chat (`-1001234:5#0`); this module keeps, per notification,
//! which units are delivered and which can never be (a chat the bot was removed
//! from). It is cleared as soon as nothing is left to do, kept for three days at
//! most, and never holds more than 100 notifications.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use tuliprox_plugin_sdk::prelude::*;

const INDEX_KEY: &str = "pending";
/// A notification the outbox has not finished in this time is forgotten.
const WINDOW_SECS: u32 = 3 * 24 * 60 * 60;
const MAX_PENDING: usize = 100;

const FNV_PRIME: u64 = 0x0100_0000_01b3;

/// FNV-1a over the parts, each followed by a 0xff separator.
fn fnv1a(seed: u64, parts: &[&[u8]]) -> u64 {
    let mut hash = seed;
    for part in parts {
        for byte in *part {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

const fn severity_name(severity: Severity) -> &'static str {
    match severity {
        Severity::Info => "info",
        Severity::Warn => "warn",
        Severity::Error => "error",
    }
}

/// 128 bits of FNV-1a (two seeds) over everything that makes a notification this
/// notification. The same notification always has the same value, on every attempt.
pub fn fingerprint(msg: &Notification) -> String {
    let severity = severity_name(msg.severity);
    let parts: [&[u8]; 4] = [msg.title.as_bytes(), msg.body.as_bytes(), severity.as_bytes(), msg.payload.as_bytes()];
    let high = fnv1a(0xcbf2_9ce4_8422_2325, &parts);
    let low = fnv1a(0x8422_2325_cbf2_9ce4, &parts);
    format!("{high:016x}{low:016x}")
}

fn record_key(fingerprint: &str) -> String { format!("msg-{fingerprint}") }

/// The units of one notification that are settled.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    done: BTreeSet<String>,
    dead: BTreeSet<String>,
}

impl State {
    /// The remembered state of `fingerprint`, or an empty one. An unreadable store is an empty one: the worst
    /// case is a chat that gets the message twice.
    pub fn load(fingerprint: &str) -> Self {
        match kv::get_json_fresh::<Self>(&record_key(fingerprint)) {
            Ok(state) => state.unwrap_or_default(),
            Err(err) => {
                warn!("the key-value store is unreadable ({err}); a retry may repeat a delivery");
                Self::default()
            }
        }
    }

    /// Whether `unit` needs nothing more: delivered, or impossible.
    pub fn settled(&self, unit: &str) -> bool { self.done.contains(unit) || self.dead.contains(unit) }

    pub fn mark_done(&mut self, unit: &str) { self.done.insert(unit.to_string()); }

    pub fn mark_dead(&mut self, unit: &str) { self.dead.insert(unit.to_string()); }

    /// Whether any unit reached its chat, now or on an earlier attempt.
    pub fn any_done(&self) -> bool { !self.done.is_empty() }

    /// Remember this state for the next attempt.
    pub fn save(&self, fingerprint: &str) {
        let now = clock::now_unix();
        let mut index = load_index();
        index.retain(|key, first| {
            let fresh = now.saturating_sub(*first) <= i64::from(WINDOW_SECS);
            if !fresh {
                let _ = kv::delete(&record_key(key));
            }
            fresh
        });
        index.entry(fingerprint.to_string()).or_insert(now);
        while index.len() > MAX_PENDING {
            let Some(oldest) =
                index.iter().min_by_key(|(key, first)| (**first, (*key).clone())).map(|(key, _)| key.clone())
            else {
                break;
            };
            index.remove(&oldest);
            let _ = kv::delete(&record_key(&oldest));
        }
        if let Err(err) = kv::set_json_ttl(&record_key(fingerprint), self, WINDOW_SECS) {
            warn!("the key-value store is not writable ({err}); a retry may repeat a delivery");
            return;
        }
        if let Err(err) = kv::set_json(INDEX_KEY, &index) {
            warn!("the key-value store is not writable ({err}); a retry may repeat a delivery");
        }
    }

    /// Forget `fingerprint`: everything for it is settled.
    pub fn release(fingerprint: &str) {
        let _ = kv::delete(&record_key(fingerprint));
        let mut index = load_index();
        if index.remove(fingerprint).is_some() {
            let _ = if index.is_empty() { kv::delete(INDEX_KEY) } else { kv::set_json(INDEX_KEY, &index) };
        }
    }
}

type Index = BTreeMap<String, i64>;

fn load_index() -> Index { kv::get_json::<Index>(INDEX_KEY).unwrap_or_default().unwrap_or_default() }

#[cfg(test)]
mod tests {
    use super::*;
    use tuliprox_plugin_sdk::mock;

    fn note(body: &str) -> Notification {
        Notification {
            title: "T".to_string(),
            body: body.to_string(),
            severity: Severity::Info,
            payload: r#"{"timestamp":1}"#.to_string(),
        }
    }

    #[test]
    fn the_fingerprint_is_stable_and_changes_with_anything_that_makes_a_notification_another() {
        let base = fingerprint(&note("a"));
        assert_eq!(base, fingerprint(&note("a")));
        assert_eq!(base.len(), 32);
        assert_ne!(base, fingerprint(&note("b")));
        let mut other = note("a");
        other.title = "U".to_string();
        assert_ne!(base, fingerprint(&other));
        other = note("a");
        other.severity = Severity::Error;
        assert_ne!(base, fingerprint(&other));
        other = note("a");
        other.payload = r#"{"timestamp":2}"#.to_string();
        assert_ne!(base, fingerprint(&other));
    }

    #[test]
    fn a_state_survives_between_attempts_and_is_forgotten_when_released() {
        mock::reset();
        mock::set_now(1_000);
        let fp = fingerprint(&note("a"));
        assert_eq!(State::load(&fp), State::default());
        let mut state = State::default();
        state.mark_done("-100#0");
        state.mark_dead("-200#0");
        state.save(&fp);
        let again = State::load(&fp);
        assert!(again.settled("-100#0") && again.settled("-200#0") && !again.settled("-300#0"));
        assert!(again.any_done());
        State::release(&fp);
        assert_eq!(State::load(&fp), State::default());
        assert!(mock::kv_snapshot().is_empty(), "the record and the index are both gone");
    }

    #[test]
    fn a_state_that_only_has_dead_units_has_delivered_nothing() {
        let mut state = State::default();
        state.mark_dead("-1#0");
        assert!(!state.any_done());
    }

    #[test]
    fn old_and_surplus_notifications_are_pruned() {
        mock::reset();
        mock::set_now(1_000);
        let old = fingerprint(&note("old"));
        let mut state = State::default();
        state.mark_done("x#0");
        state.save(&old);
        // Three days and a second later the old record is gone when another one is saved.
        mock::set_now(1_000 + i64::from(WINDOW_SECS) + 1);
        let fresh = fingerprint(&note("fresh"));
        state.save(&fresh);
        assert!(!mock::kv_snapshot().contains_key(&record_key(&old)));
        assert!(mock::kv_snapshot().contains_key(&record_key(&fresh)));
        // Never more than MAX_PENDING at once.
        for n in 0..(MAX_PENDING + 20) {
            mock::advance(1);
            state.save(&fingerprint(&note(&format!("many-{n}"))));
        }
        let kept = mock::kv_snapshot().keys().filter(|key| key.starts_with("msg-")).count();
        assert!(kept <= MAX_PENDING, "{kept} records");
    }
}

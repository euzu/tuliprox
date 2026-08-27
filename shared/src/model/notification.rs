//! Open-world notification event identity.
//!
//! [`MsgKind`](crate::model::MsgKind) is a closed enum: adding one event
//! kind meant editing eight sites across three crates, and two of those
//! sites failed silently rather than at compile time. This module is the
//! replacement extension point.
//!
//! An event is identified by a dotted `domain.event` string. Subscriptions
//! are glob patterns. Adding an event is one [`EventId`] const plus one
//! emit call - no match arms, no template-context field, no discovery
//! array to keep in sync.
//!
//! # Backward compatibility
//!
//! The eight legacy `MsgKind` wire names (`info`, `stats`, `disk_alert`,
//! ...) stay valid wherever they were accepted before: as `notify_on`
//! entries and as template filenames. [`EventId::from_wire`] resolves them
//! through [`LEGACY_ALIASES`], so an existing `config.yml` keeps working
//! untouched.

use std::fmt;

/// Stable dotted identity of a notification event.
///
/// The inner string is the wire form: it appears in `notify_on` patterns,
/// in the outbox file, in template filenames and in metric labels, so it
/// must stay stable across releases once published.
///
/// Backed by `&'static str` because every event is declared as a const in
/// [`registry`]. Plugin-registered events would need an owned variant;
/// that is deliberately deferred until the plugin host exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EventId(&'static str);

impl EventId {
    /// Declare an event id. `const` so the registry is built at compile time.
    #[must_use]
    pub const fn new(id: &'static str) -> Self { Self(id) }

    #[must_use]
    pub const fn as_str(&self) -> &'static str { self.0 }

    /// The `domain` part - everything before the first dot.
    ///
    /// Used to group events in the UI and to answer "does this subscription
    /// cover the whole domain".
    #[must_use]
    pub fn domain(&self) -> &'static str {
        match self.0.split_once('.') {
            Some((domain, _)) => domain,
            None => self.0,
        }
    }

    /// Filename-safe form: dots become underscores.
    ///
    /// `recording.completed` yields `recording_completed`, which is exactly
    /// the legacy template filename for that kind - so recording templates
    /// are discovered by the canonical id with no alias lookup needed.
    #[must_use]
    pub fn file_stem(&self) -> String { self.0.replace('.', "_") }

    /// `{prefix}_{file_stem}.templ`, the on-disk template name.
    #[must_use]
    pub fn template_filename(&self, prefix: &str) -> String { format!("{prefix}_{}.templ", self.file_stem()) }

    /// Resolve a wire string to a known event id.
    ///
    /// Accepts the canonical dotted id and every legacy `MsgKind` wire
    /// name. Returns `None` for an id that is not in the registry, which is
    /// what lets config validation reject a typo instead of silently
    /// subscribing to an event that will never fire.
    #[must_use]
    pub fn from_wire(s: &str) -> Option<Self> {
        if let Some((_, id)) = LEGACY_ALIASES.iter().find(|(legacy, _)| legacy.eq_ignore_ascii_case(s)) {
            return Some(*id);
        }
        registry::ALL.iter().find(|d| d.id.0.eq_ignore_ascii_case(s)).map(|d| d.id)
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(self.0) }
}

impl serde::Serialize for EventId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> { s.serialize_str(self.0) }
}

impl<'de> serde::Deserialize<'de> for EventId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        // An unknown id in a persisted outbox entry must not fail the whole
        // file - see `UNKNOWN`. Config validation rejects typos separately,
        // where an error message can actually reach the user.
        Ok(Self::from_wire(&raw).unwrap_or(registry::UNKNOWN))
    }
}

/// How much the operator is expected to care.
///
/// Ordered, so a channel can subscribe with `min_severity` and get
/// everything at or above it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Something finished normally. Safe to route nowhere.
    #[default]
    Info,
    /// Degraded but self-correcting, or a threshold approached.
    Warn,
    /// An operation failed. Someone should look, eventually.
    Error,
    /// Service is impaired now. Worth waking someone.
    Critical,
}

impl Severity {
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
            Self::Critical => "critical",
        }
    }

    #[must_use]
    pub fn from_wire(s: &str) -> Option<Self> {
        [Self::Info, Self::Warn, Self::Error, Self::Critical]
            .into_iter()
            .find(|c| c.wire_name().eq_ignore_ascii_case(s))
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(self.wire_name()) }
}

/// One entry in the event registry.
#[derive(Debug, Clone, Copy)]
pub struct EventDescriptor {
    pub id: EventId,
    /// Severity the emitter uses unless it overrides per-occurrence.
    pub severity: Severity,
    /// One line, rendered into the docs table and the UI picker.
    pub description: &'static str,
}

/// Legacy `MsgKind` wire name -> canonical id.
///
/// Every entry here is load-bearing for an existing `config.yml`. Removing
/// one silently stops honouring a `notify_on` line that used to work, so
/// entries are append-only.
pub const LEGACY_ALIASES: &[(&str, EventId)] = &[
    ("info", registry::SYSTEM_INFO),
    ("stats", registry::PLAYLIST_UPDATE_COMPLETED),
    ("error", registry::SYSTEM_ERROR),
    ("watch", registry::PLAYLIST_WATCH_CHANGED),
    ("disk_alert", registry::SYSTEM_DISK_ALERT),
    ("diskalert", registry::SYSTEM_DISK_ALERT),
    ("recording_started", registry::RECORDING_STARTED),
    ("recordingstarted", registry::RECORDING_STARTED),
    ("recording_completed", registry::RECORDING_COMPLETED),
    ("recordingcompleted", registry::RECORDING_COMPLETED),
    ("recording_failed", registry::RECORDING_FAILED),
    ("recordingfailed", registry::RECORDING_FAILED),
];

/// The known event ids.
///
/// Adding an event is a const here plus an entry in [`ALL`] - and then one
/// emit call at the site that knows the event happened. Nothing else in
/// the notification path needs to change.
pub mod registry {
    use super::{EventDescriptor, EventId, Severity};

    /// Fallback for an id read back from a persisted outbox that this build
    /// does not know. Lets an outbox written by a newer version round-trip
    /// through an older one instead of failing the whole file.
    pub const UNKNOWN: EventId = EventId::new("unknown");

    // ---- system ---------------------------------------------------------
    pub const SYSTEM_INFO: EventId = EventId::new("system.info");
    pub const SYSTEM_ERROR: EventId = EventId::new("system.error");
    pub const SYSTEM_DISK_ALERT: EventId = EventId::new("system.disk.alert");
    pub const SYSTEM_STARTED: EventId = EventId::new("system.started");
    pub const SYSTEM_SHUTDOWN: EventId = EventId::new("system.shutdown");

    // ---- playlist -------------------------------------------------------
    pub const PLAYLIST_UPDATE_COMPLETED: EventId = EventId::new("playlist.update.completed");
    pub const PLAYLIST_UPDATE_FAILED: EventId = EventId::new("playlist.update.failed");
    pub const PLAYLIST_WATCH_CHANGED: EventId = EventId::new("playlist.watch.changed");

    // ---- recording ------------------------------------------------------
    pub const RECORDING_STARTED: EventId = EventId::new("recording.started");
    pub const RECORDING_COMPLETED: EventId = EventId::new("recording.completed");
    pub const RECORDING_FAILED: EventId = EventId::new("recording.failed");

    // ---- provider -------------------------------------------------------
    pub const PROVIDER_ACCOUNT_STATUS: EventId = EventId::new("provider.account.status_changed");
    pub const PROVIDER_ACCOUNT_EXPIRING: EventId = EventId::new("provider.account.expiring");
    pub const PROVIDER_ACCOUNT_EXPIRED: EventId = EventId::new("provider.account.expired");

    // ---- config ---------------------------------------------------------
    pub const CONFIG_CHANGED: EventId = EventId::new("config.changed");
    pub const CONFIG_RELOAD_FAILED: EventId = EventId::new("config.reload_failed");

    // ---- library --------------------------------------------------------
    pub const LIBRARY_SCAN_COMPLETED: EventId = EventId::new("library.scan.completed");

    // ---- metadata -------------------------------------------------------
    pub const METADATA_UPDATE_STARTED: EventId = EventId::new("metadata.update.started");
    pub const METADATA_UPDATE_COMPLETED: EventId = EventId::new("metadata.update.completed");

    // ---- users and connections ------------------------------------------
    /// High frequency. Subscribe deliberately.
    pub const USER_CONNECTION_CHANGED: EventId = EventId::new("user.connection.changed");
    /// High frequency. Subscribe deliberately.
    pub const PROVIDER_CONNECTIONS_CHANGED: EventId = EventId::new("provider.connections.changed");

    // ---- dvr ------------------------------------------------------------
    pub const RECORDING_QUEUE_CHANGED: EventId = EventId::new("recording.queue.changed");
    pub const RECORDING_RULES_CHANGED: EventId = EventId::new("recording.rules.changed");

    // ---- notification self-reporting ------------------------------------
    /// A notification was permanently lost. Must never route back through
    /// the channel that dropped it.
    pub const NOTIFICATION_DEAD_LETTERED: EventId = EventId::new("notification.dead_lettered");

    /// Every registered event, in display order.
    pub const ALL: &[EventDescriptor] = &[
        EventDescriptor { id: SYSTEM_INFO, severity: Severity::Info, description: "A general informational message." },
        EventDescriptor { id: SYSTEM_ERROR, severity: Severity::Error, description: "A general error message." },
        EventDescriptor {
            id: SYSTEM_DISK_ALERT,
            severity: Severity::Warn,
            description: "Disk usage crossed the warn or critical threshold.",
        },
        EventDescriptor {
            id: SYSTEM_STARTED,
            severity: Severity::Info,
            description: "The server finished starting up.",
        },
        EventDescriptor {
            id: SYSTEM_SHUTDOWN,
            severity: Severity::Info,
            description: "The server is shutting down cleanly.",
        },
        EventDescriptor {
            id: PLAYLIST_UPDATE_COMPLETED,
            severity: Severity::Info,
            description: "A playlist update finished; carries per-source statistics.",
        },
        EventDescriptor {
            id: PLAYLIST_UPDATE_FAILED,
            severity: Severity::Error,
            description: "A playlist update failed.",
        },
        EventDescriptor {
            id: PLAYLIST_WATCH_CHANGED,
            severity: Severity::Info,
            description: "Channels were added to or removed from a watched group.",
        },
        EventDescriptor { id: RECORDING_STARTED, severity: Severity::Info, description: "A recording started." },
        EventDescriptor { id: RECORDING_COMPLETED, severity: Severity::Info, description: "A recording completed." },
        EventDescriptor { id: RECORDING_FAILED, severity: Severity::Error, description: "A recording failed." },
        EventDescriptor {
            id: PROVIDER_ACCOUNT_STATUS,
            severity: Severity::Warn,
            description: "A provider reported a changed account status.",
        },
        EventDescriptor {
            id: PROVIDER_ACCOUNT_EXPIRING,
            severity: Severity::Warn,
            description: "A provider account is approaching its expiry date.",
        },
        EventDescriptor {
            id: PROVIDER_ACCOUNT_EXPIRED,
            severity: Severity::Error,
            description: "A provider account has expired.",
        },
        EventDescriptor {
            id: CONFIG_CHANGED,
            severity: Severity::Info,
            description: "A configuration file was changed and reloaded.",
        },
        EventDescriptor {
            id: CONFIG_RELOAD_FAILED,
            severity: Severity::Error,
            description: "A configuration file changed but could not be reloaded.",
        },
        EventDescriptor {
            id: LIBRARY_SCAN_COMPLETED,
            severity: Severity::Info,
            description: "A local library scan finished.",
        },
        EventDescriptor {
            id: METADATA_UPDATE_STARTED,
            severity: Severity::Info,
            description: "A metadata update started for an input.",
        },
        EventDescriptor {
            id: METADATA_UPDATE_COMPLETED,
            severity: Severity::Info,
            description: "A metadata update finished for an input.",
        },
        EventDescriptor {
            id: USER_CONNECTION_CHANGED,
            severity: Severity::Info,
            description: "A user connected or disconnected. High frequency - subscribe deliberately.",
        },
        EventDescriptor {
            id: PROVIDER_CONNECTIONS_CHANGED,
            severity: Severity::Info,
            description: "A provider's active connection count changed. High frequency - subscribe deliberately.",
        },
        EventDescriptor {
            id: RECORDING_QUEUE_CHANGED,
            severity: Severity::Info,
            description: "The recording queue changed.",
        },
        EventDescriptor {
            id: RECORDING_RULES_CHANGED,
            severity: Severity::Info,
            description: "The recording rule set changed.",
        },
        EventDescriptor {
            id: NOTIFICATION_DEAD_LETTERED,
            severity: Severity::Error,
            description: "A notification was permanently undeliverable and has been dropped.",
        },
    ];

    /// Look up the descriptor for an id.
    #[must_use]
    pub fn describe(id: EventId) -> Option<&'static EventDescriptor> { ALL.iter().find(|d| d.id == id) }

    /// Default severity for an id; `Info` for anything unregistered.
    #[must_use]
    pub fn default_severity(id: EventId) -> Severity { describe(id).map_or(Severity::Info, |d| d.severity) }
}

// ---------------------------------------------------------------------------
// Subscriptions
// ---------------------------------------------------------------------------

/// One `notify_on` entry.
///
/// Grammar, deliberately small enough to explain in a config comment:
///
/// | pattern                  | matches                                     |
/// |--------------------------|---------------------------------------------|
/// | `*`                      | every event                                 |
/// | `recording.*`            | every event under `recording`                |
/// | `recording.completed`    | that event only                             |
/// | `provider.*.expired`     | one wildcard segment                        |
/// | `!recording.started`     | excludes, whatever else matched             |
///
/// A leading `!` negates. A subscription matches when at least one positive
/// pattern matches and no negative pattern does, so `["*", "!system.info"]`
/// reads the way it looks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventPattern {
    segments: Vec<Segment>,
    /// `true` when the pattern is a `!` exclusion.
    pub negated: bool,
    raw: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    /// A literal segment.
    Literal(String),
    /// `*` in an interior position - matches exactly one segment.
    One,
    /// A trailing `.*` - matches one or more remaining segments.
    Rest,
}

impl fmt::Display for EventPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.raw) }
}

impl EventPattern {
    /// Parse a pattern. Never fails: an empty pattern simply matches nothing,
    /// which is the safe reading of a blank config line.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        let trimmed = raw.trim();
        let (negated, body) = match trimmed.strip_prefix('!') {
            Some(rest) => (true, rest.trim()),
            None => (false, trimmed),
        };
        let parts: Vec<&str> = if body.is_empty() { Vec::new() } else { body.split('.').collect() };
        let last = parts.len().saturating_sub(1);
        let segments = parts
            .iter()
            .enumerate()
            .map(|(i, part)| match *part {
                "*" if i == last => Segment::Rest,
                "*" => Segment::One,
                literal => Segment::Literal(literal.to_string()),
            })
            .collect();
        Self { segments, negated, raw: trimmed.to_string() }
    }

    /// The pattern as written, for round-tripping back into config.
    #[must_use]
    pub fn as_str(&self) -> &str { &self.raw }

    /// Does this pattern cover `id`? Ignores negation - the caller combines.
    #[must_use]
    pub fn matches(&self, id: EventId) -> bool { Self::match_segments(&self.segments, id.as_str()) }

    fn match_segments(pattern: &[Segment], id: &str) -> bool {
        let mut parts = id.split('.');
        for (i, segment) in pattern.iter().enumerate() {
            match segment {
                // A trailing `*` swallows every remaining segment, and
                // requires at least one so `recording.*` does not match a
                // bare `recording`.
                Segment::Rest => {
                    // A lone `*` pattern matches everything, including a
                    // single-segment id.
                    return if i == 0 { true } else { parts.next().is_some() };
                }
                Segment::One => {
                    if parts.next().is_none() {
                        return false;
                    }
                }
                Segment::Literal(want) => match parts.next() {
                    Some(got) if got.eq_ignore_ascii_case(want) => {}
                    _ => return false,
                },
            }
        }
        // Every pattern segment consumed; the id must be exhausted too.
        parts.next().is_none()
    }
}

/// A parsed `notify_on` list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventSubscription {
    patterns: Vec<EventPattern>,
}

impl EventSubscription {
    #[must_use]
    pub fn parse<I: IntoIterator<Item = S>, S: AsRef<str>>(raw: I) -> Self {
        Self { patterns: raw.into_iter().map(|s| EventPattern::parse(s.as_ref())).collect() }
    }

    /// The parsed patterns, for round-tripping back into config.
    #[must_use]
    pub fn patterns(&self) -> &[EventPattern] { &self.patterns }

    /// `true` when nothing is subscribed - the caller can skip all work.
    #[must_use]
    pub fn is_empty(&self) -> bool { self.patterns.is_empty() }

    /// At least one positive pattern matches and no negative pattern does.
    #[must_use]
    pub fn matches(&self, id: EventId) -> bool {
        let mut included = false;
        for pattern in &self.patterns {
            if pattern.matches(id) {
                if pattern.negated {
                    return false;
                }
                included = true;
            }
        }
        included
    }

    /// Patterns that match no registered event.
    ///
    /// Config validation surfaces these as a warning: a typo in `notify_on`
    /// is otherwise indistinguishable from an event that simply never fires.
    #[must_use]
    pub fn unmatched_patterns(&self) -> Vec<&str> {
        self.patterns
            .iter()
            .filter(|p| !registry::ALL.iter().any(|d| p.matches(d.id)))
            .map(EventPattern::as_str)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{registry, EventId, EventPattern, EventSubscription, Severity, LEGACY_ALIASES};

    #[test]
    fn every_legacy_msgkind_wire_name_still_resolves() {
        // Load-bearing for existing config.yml files: each of these was a
        // valid `notify_on` entry before the open-world refactor.
        for legacy in [
            "info",
            "stats",
            "error",
            "watch",
            "disk_alert",
            "recording_started",
            "recording_completed",
            "recording_failed",
        ] {
            assert!(EventId::from_wire(legacy).is_some(), "legacy wire name {legacy} no longer resolves");
        }
    }

    #[test]
    fn legacy_alias_targets_are_all_registered() {
        for (legacy, id) in LEGACY_ALIASES {
            assert!(registry::describe(*id).is_some(), "alias {legacy} points at unregistered id {id}");
        }
    }

    #[test]
    fn canonical_ids_resolve_and_round_trip() {
        for descriptor in registry::ALL {
            let resolved = EventId::from_wire(descriptor.id.as_str());
            assert_eq!(resolved, Some(descriptor.id), "id {} did not round-trip", descriptor.id);
        }
    }

    #[test]
    fn unknown_wire_name_does_not_resolve() {
        assert_eq!(EventId::from_wire("recording.definitely_not_a_real_event"), None);
    }

    #[test]
    fn recording_template_filenames_match_the_legacy_names() {
        // The legacy on-disk names were `telegram_recording_completed.templ`
        // and friends. The canonical dotted ids produce the same stems, so
        // existing template files keep being discovered.
        assert_eq!(registry::RECORDING_COMPLETED.template_filename("telegram"), "telegram_recording_completed.templ");
        assert_eq!(registry::RECORDING_STARTED.template_filename("discord"), "discord_recording_started.templ");
        assert_eq!(registry::RECORDING_FAILED.template_filename("rest"), "rest_recording_failed.templ");
    }

    #[test]
    fn domain_is_the_part_before_the_first_dot() {
        assert_eq!(registry::RECORDING_COMPLETED.domain(), "recording");
        assert_eq!(registry::SYSTEM_DISK_ALERT.domain(), "system");
        assert_eq!(registry::UNKNOWN.domain(), "unknown");
    }

    #[test]
    fn star_matches_every_registered_event() {
        let sub = EventSubscription::parse(["*"]);
        for descriptor in registry::ALL {
            assert!(sub.matches(descriptor.id), "`*` failed to match {}", descriptor.id);
        }
    }

    #[test]
    fn prefix_pattern_matches_the_domain_and_nothing_else() {
        let sub = EventSubscription::parse(["recording.*"]);
        assert!(sub.matches(registry::RECORDING_COMPLETED));
        assert!(sub.matches(registry::RECORDING_FAILED));
        assert!(!sub.matches(registry::SYSTEM_INFO));
        assert!(!sub.matches(registry::PLAYLIST_UPDATE_COMPLETED));
    }

    #[test]
    fn prefix_pattern_requires_at_least_one_further_segment() {
        // `recording.*` must not match a hypothetical bare `recording`.
        assert!(!EventPattern::parse("recording.*").matches(EventId::new("recording")));
    }

    #[test]
    fn exact_pattern_matches_only_itself() {
        let sub = EventSubscription::parse(["recording.completed"]);
        assert!(sub.matches(registry::RECORDING_COMPLETED));
        assert!(!sub.matches(registry::RECORDING_STARTED));
    }

    #[test]
    fn interior_wildcard_matches_exactly_one_segment() {
        let pattern = EventPattern::parse("provider.*.expired");
        assert!(pattern.matches(registry::PROVIDER_ACCOUNT_EXPIRED));
        // One segment, not several.
        assert!(!pattern.matches(EventId::new("provider.a.b.expired")));
        // And not zero.
        assert!(!pattern.matches(EventId::new("provider.expired")));
    }

    #[test]
    fn negation_removes_from_a_wider_match() {
        let sub = EventSubscription::parse(["*", "!system.info"]);
        assert!(sub.matches(registry::SYSTEM_ERROR));
        assert!(!sub.matches(registry::SYSTEM_INFO), "negated pattern did not exclude");
    }

    #[test]
    fn negation_wins_regardless_of_order() {
        let before = EventSubscription::parse(["!recording.started", "recording.*"]);
        let after = EventSubscription::parse(["recording.*", "!recording.started"]);
        assert!(!before.matches(registry::RECORDING_STARTED));
        assert!(!after.matches(registry::RECORDING_STARTED));
        assert!(before.matches(registry::RECORDING_COMPLETED));
        assert!(after.matches(registry::RECORDING_COMPLETED));
    }

    #[test]
    fn a_subscription_of_only_negations_matches_nothing() {
        // No positive pattern means nothing was opted in to.
        let sub = EventSubscription::parse(["!system.info"]);
        assert!(!sub.matches(registry::SYSTEM_ERROR));
        assert!(!sub.matches(registry::SYSTEM_INFO));
    }

    #[test]
    fn empty_subscription_matches_nothing() {
        let sub = EventSubscription::parse(Vec::<String>::new());
        assert!(sub.is_empty());
        assert!(!sub.matches(registry::SYSTEM_INFO));
    }

    #[test]
    fn blank_pattern_matches_nothing_rather_than_everything() {
        // A stray empty line in config must not silently subscribe to all.
        let sub = EventSubscription::parse(["  "]);
        assert!(!sub.matches(registry::SYSTEM_INFO));
    }

    #[test]
    fn unmatched_patterns_flags_typos_only() {
        let sub = EventSubscription::parse(["recording.*", "recroding.completed", "*"]);
        assert_eq!(sub.unmatched_patterns(), vec!["recroding.completed"]);
    }

    #[test]
    fn severity_orders_from_info_up_to_critical() {
        assert!(Severity::Info < Severity::Warn);
        assert!(Severity::Warn < Severity::Error);
        assert!(Severity::Error < Severity::Critical);
    }

    #[test]
    fn severity_wire_names_round_trip() {
        for severity in [Severity::Info, Severity::Warn, Severity::Error, Severity::Critical] {
            assert_eq!(Severity::from_wire(severity.wire_name()), Some(severity));
        }
    }

    #[test]
    fn unknown_event_id_deserializes_to_unknown_instead_of_failing() {
        // A newer build's outbox entry must not poison the whole file when
        // read back by an older one.
        let id: EventId = serde_json::from_str("\"some.future.event\"").expect("must not fail");
        assert_eq!(id, registry::UNKNOWN);
    }

    #[test]
    fn event_id_serializes_as_its_wire_string() {
        let json = serde_json::to_string(&registry::RECORDING_COMPLETED).expect("serialize");
        assert_eq!(json, "\"recording.completed\"");
    }

    /// The docs carry a table of every event id. A registry entry that never
    /// reaches the table is undiscoverable, and a table row for an id that no
    /// longer exists is a lie - so the two are checked against each other
    /// rather than maintained by hand.
    #[test]
    fn every_registered_event_appears_in_the_docs_table() {
        const DOCS: &str = include_str!("../../../docs/src/configuration/config.md");
        let table = DOCS
            .split_once("<!-- BEGIN GENERATED EVENT TABLE -->")
            .and_then(|(_, rest)| rest.split_once("<!-- END GENERATED EVENT TABLE -->"))
            .map(|(table, _)| table)
            .expect("docs must contain the generated event table markers");

        for descriptor in registry::ALL {
            let row = format!("| `{}` | {} | {} |", descriptor.id, descriptor.severity, descriptor.description);
            assert!(
                table.contains(&row),
                "docs/src/configuration/config.md is missing this row - add it inside the generated table:\n{row}"
            );
        }

        // And nothing in the table that the registry no longer knows about.
        for line in table.lines().filter(|line| line.trim_start().starts_with("| `")) {
            let id = line.trim_start().trim_start_matches("| `").split('`').next().unwrap_or_default();
            assert!(
                EventId::from_wire(id).is_some(),
                "docs list `{id}`, which is not a registered event - remove the row or register the event"
            );
        }
    }

    #[test]
    fn registry_has_no_duplicate_ids() {
        let mut seen = std::collections::HashSet::new();
        for descriptor in registry::ALL {
            assert!(seen.insert(descriptor.id), "duplicate registry entry for {}", descriptor.id);
        }
    }
}

// ---------------------------------------------------------------------------
// Quiet hours
// ---------------------------------------------------------------------------

/// A local-time window during which a channel stays silent.
///
/// Notifications landing inside the window are *deferred* by the outbox,
/// never dropped: an overnight outage that nobody is told about afterwards
/// is worse than one that arrives late.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuietHours {
    /// Minutes since local midnight.
    start_min: u16,
    end_min: u16,
}

impl QuietHours {
    /// Parse `HH:MM-HH:MM`. Returns `None` for anything malformed, which
    /// config validation turns into an error the operator sees.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let (start, end) = raw.trim().split_once('-')?;
        Some(Self { start_min: parse_hhmm(start)?, end_min: parse_hhmm(end)? })
    }

    /// Is `minutes_since_midnight` inside the window?
    ///
    /// Handles the wrapping case (`23:00-07:00`), which is the one people
    /// actually configure.
    #[must_use]
    pub fn contains(&self, minutes_since_midnight: u16) -> bool {
        if self.start_min == self.end_min {
            // A zero-width window silences nothing. Treating it as "always"
            // would mute a channel completely on a typo.
            return false;
        }
        if self.start_min < self.end_min {
            (self.start_min..self.end_min).contains(&minutes_since_midnight)
        } else {
            minutes_since_midnight >= self.start_min || minutes_since_midnight < self.end_min
        }
    }

    /// Minutes from `minutes_since_midnight` until the window ends.
    #[must_use]
    pub fn minutes_until_end(&self, minutes_since_midnight: u16) -> u16 {
        if !self.contains(minutes_since_midnight) {
            return 0;
        }
        if self.end_min > minutes_since_midnight {
            self.end_min - minutes_since_midnight
        } else {
            (24 * 60 - minutes_since_midnight) + self.end_min
        }
    }
}

fn parse_hhmm(raw: &str) -> Option<u16> {
    let (h, m) = raw.trim().split_once(':')?;
    let h: u16 = h.trim().parse().ok()?;
    let m: u16 = m.trim().parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(h * 60 + m)
}

#[cfg(test)]
mod quiet_hours_tests {
    use super::QuietHours;

    #[test]
    fn a_same_day_window_contains_only_its_own_range() {
        let window = QuietHours::parse("09:00-17:00").expect("parse");
        assert!(!window.contains(8 * 60 + 59));
        assert!(window.contains(9 * 60));
        assert!(window.contains(16 * 60 + 59));
        // End is exclusive, so 17:00 sharp is out.
        assert!(!window.contains(17 * 60));
    }

    #[test]
    fn an_overnight_window_wraps_past_midnight() {
        // The case people actually configure.
        let window = QuietHours::parse("23:00-07:00").expect("parse");
        assert!(window.contains(23 * 60));
        assert!(window.contains(0));
        assert!(window.contains(6 * 60 + 59));
        assert!(!window.contains(7 * 60));
        assert!(!window.contains(12 * 60));
    }

    #[test]
    fn a_zero_width_window_silences_nothing() {
        // Treating it as "always" would mute the channel entirely on a typo.
        let window = QuietHours::parse("08:00-08:00").expect("parse");
        for minute in [0, 8 * 60, 12 * 60, 23 * 60 + 59] {
            assert!(!window.contains(minute), "zero-width window muted {minute}");
        }
    }

    #[test]
    fn minutes_until_end_handles_both_directions() {
        let same_day = QuietHours::parse("09:00-17:00").expect("parse");
        assert_eq!(same_day.minutes_until_end(10 * 60), 7 * 60);
        assert_eq!(same_day.minutes_until_end(18 * 60), 0, "outside the window there is nothing to wait for");

        let overnight = QuietHours::parse("23:00-07:00").expect("parse");
        assert_eq!(overnight.minutes_until_end(23 * 60), 8 * 60);
        assert_eq!(overnight.minutes_until_end(60), 6 * 60);
    }

    #[test]
    fn malformed_windows_are_rejected_rather_than_guessed() {
        for bad in ["", "23:00", "25:00-07:00", "23:60-07:00", "23-07", "abc-def", "23:00_07:00"] {
            assert!(QuietHours::parse(bad).is_none(), "accepted malformed window `{bad}`");
        }
    }

    #[test]
    fn whitespace_is_tolerated() {
        assert_eq!(QuietHours::parse(" 23:00 - 07:00 "), QuietHours::parse("23:00-07:00"));
    }
}

use super::{registry, EventId, EventPattern, EventSubscription};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Segment {
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

    pub(super) fn match_segments(pattern: &[Segment], id: &str) -> bool {
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

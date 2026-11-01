//! Sport detection from channel names, and the group names built from it.
//!
//! Everything here is a pure function of its input: no clock, no KV, no host
//! calls, so it is cheap to unit-test and trivially deterministic.

use tuliprox_plugin_sdk::prelude::{Channel, Cluster};

/// A sport the plugin recognises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Sport {
    AmericanFootball,
    Basketball,
    IceHockey,
    Baseball,
    Tennis,
    Rugby,
    Cricket,
    Golf,
    Motorsport,
    Combat,
    Football,
}

/// Detection order. The generic `football` keywords come last so that
/// `NFL` or `American Football` are not claimed by soccer.
const ORDER: [Sport; 11] = [
    Sport::AmericanFootball,
    Sport::Basketball,
    Sport::IceHockey,
    Sport::Baseball,
    Sport::Tennis,
    Sport::Rugby,
    Sport::Cricket,
    Sport::Golf,
    Sport::Motorsport,
    Sport::Combat,
    Sport::Football,
];

impl Sport {
    /// The id the mock API uses in its `sport` field.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::AmericanFootball => "american-football",
            Self::Basketball => "basketball",
            Self::IceHockey => "ice-hockey",
            Self::Baseball => "baseball",
            Self::Tennis => "tennis",
            Self::Rugby => "rugby",
            Self::Cricket => "cricket",
            Self::Golf => "golf",
            Self::Motorsport => "motorsport",
            Self::Combat => "combat",
            Self::Football => "football",
        }
    }

    /// The display name used in group names.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::AmericanFootball => "American Football",
            Self::Basketball => "Basketball",
            Self::IceHockey => "Ice Hockey",
            Self::Baseball => "Baseball",
            Self::Tennis => "Tennis",
            Self::Rugby => "Rugby",
            Self::Cricket => "Cricket",
            Self::Golf => "Golf",
            Self::Motorsport => "Motorsport",
            Self::Combat => "Combat Sports",
            Self::Football => "Football",
        }
    }

    #[must_use]
    pub const fn emoji(self) -> &'static str {
        match self {
            Self::AmericanFootball => "\u{1F3C8}",
            Self::Basketball => "\u{1F3C0}",
            Self::IceHockey => "\u{1F3D2}",
            Self::Baseball => "\u{26BE}",
            Self::Tennis => "\u{1F3BE}",
            Self::Rugby => "\u{1F3C9}",
            Self::Cricket => "\u{1F3CF}",
            Self::Golf => "\u{26F3}",
            Self::Motorsport => "\u{1F3CE}\u{FE0F}",
            Self::Combat => "\u{1F94A}",
            Self::Football => "\u{26BD}",
        }
    }

    const fn keywords(self) -> &'static [&'static str] {
        match self {
            Self::AmericanFootball => &["nfl", "american football", "ncaa football", "super bowl"],
            Self::Basketball => &["nba", "wnba", "basketball", "euroleague"],
            Self::IceHockey => &["nhl", "ice hockey", "hockey"],
            Self::Baseball => &["mlb", "baseball"],
            Self::Tennis => &["tennis", "atp", "wta", "wimbledon"],
            Self::Rugby => &["rugby", "six nations"],
            Self::Cricket => &["cricket", "ipl"],
            Self::Golf => &["golf", "pga"],
            Self::Motorsport => &["f1", "formula 1", "formula one", "motogp", "nascar", "motorsport"],
            Self::Combat => &["ufc", "boxing", "wrestling", "wwe", "mma"],
            Self::Football => &[
                "football",
                "soccer",
                "premier league",
                "epl",
                "la liga",
                "laliga",
                "serie a",
                "bundesliga",
                "ligue 1",
                "champions league",
                "europa league",
                "mls",
                "fifa",
            ],
        }
    }

    /// The sport whose `id` is `id`.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> { ORDER.into_iter().find(|sport| sport.id().eq_ignore_ascii_case(id)) }
}

/// `true` when `phrase` occurs in `haystack` with a non-alphanumeric character
/// (or the end of the text) on both sides. Both are expected in lowercase.
fn contains_phrase(haystack: &str, phrase: &str) -> bool {
    haystack.match_indices(phrase).any(|(start, found)| {
        let before = haystack[..start].chars().next_back();
        let after = haystack[start + found.len()..].chars().next();
        before.is_none_or(|c| !c.is_alphanumeric()) && after.is_none_or(|c| !c.is_alphanumeric())
    })
}

/// The sport named by `text`, if any.
#[must_use]
pub fn classify_text(text: &str) -> Option<Sport> {
    let lowered = text.to_lowercase();
    ORDER.into_iter().find(|sport| sport.keywords().iter().any(|keyword| contains_phrase(&lowered, keyword)))
}

/// `true` for a live channel (the only kind the plugin regroups).
#[must_use]
pub const fn is_live(channel: &Channel) -> bool { matches!(channel.cluster, Cluster::Live) }

/// The sport of a live channel: its name and title first, its group as the fallback.
#[must_use]
pub fn classify_channel(channel: &Channel) -> Option<Sport> {
    if !is_live(channel) {
        return None;
    }
    classify_text(&format!("{} {}", channel.name, channel.title)).or_else(|| classify_text(&channel.group))
}

/// `<emoji> <Sport>`, or just `<Sport>` when the emoji toggle is off.
#[must_use]
pub fn group_name(sport: Sport, emoji: bool) -> String {
    if emoji {
        format!("{} {}", sport.emoji(), sport.label())
    } else {
        sport.label().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_map_to_sports_with_word_boundaries() {
        let cases = [
            ("Sky Sports Premier League HD", Some(Sport::Football)),
            ("ESPN NBA 1", Some(Sport::Basketball)),
            ("NFL RedZone", Some(Sport::AmericanFootball)),
            ("American Football Network", Some(Sport::AmericanFootball)),
            ("NHL Network", Some(Sport::IceHockey)),
            ("F1 Live", Some(Sport::Motorsport)),
            ("UFC Fight Pass", Some(Sport::Combat)),
            ("Planet Football", Some(Sport::Football)),
            ("News 24", None),
            ("Hockeyz", None),
            ("Anbaa", None),
        ];
        for (name, expected) in cases {
            assert_eq!(classify_text(name), expected, "{name}");
        }
    }

    #[test]
    fn group_names_follow_the_emoji_toggle() {
        assert_eq!(group_name(Sport::Football, true), "\u{26BD} Football");
        assert_eq!(group_name(Sport::Basketball, false), "Basketball");
        assert_eq!(Sport::from_id("Ice-Hockey"), Some(Sport::IceHockey));
        assert_eq!(Sport::from_id("curling"), None);
    }
}

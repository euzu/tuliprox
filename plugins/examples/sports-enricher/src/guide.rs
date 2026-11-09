//! `epg.enrich-programmes`: a synthetic `Home vs Away` programme for every fixture a channel broadcasts.
//!
//! # When the host calls this
//!
//! The host calls `enrich-programmes` once per *guide* channel: a channel of the
//! XMLTV `epg` that a playlist channel was matched to (by its `tvg-id`). A
//! guide channel with no `<programme>` elements at all is still a child of the
//! guide and is enriched, so the plugin can fill an empty channel. A playlist
//! channel with no guide entry at all is never offered, so the demo's XMLTV
//! lists a `<channel>` for every broadcaster id, some of them without programmes.
//!
//! The fixture times are the API's, so the programmes are a pure function of
//! (fixtures in the KV store, existing programmes, `now`). The programme list
//! returned replaces the channel's list, so the existing programmes are returned
//! with the new ones; the host sorts them.

use crate::{
    fixtures::{self, Fixture, League},
    sports::Sport,
    Settings,
};
use tuliprox_plugin_sdk::prelude::*;

/// How far back a fixture is still worth a programme (a match in progress).
const LOOK_BACK_SECS: i64 = 6 * 3600;
/// How far ahead fixtures are added.
const LOOK_AHEAD_SECS: i64 = 7 * 86_400;

#[must_use]
pub fn enrich(settings: &Settings, channel_id: &str, programmes: Vec<EpgProgramme>) -> Vec<EpgProgramme> {
    let library = fixtures::shared(settings, true);
    let now = clock::now_unix();
    let mut matches: Vec<(&League, &Fixture)> = library
        .fixtures()
        .filter(|(_, fixture)| fixture.on_channel(channel_id))
        .filter(|(_, fixture)| fixture.kickoff >= now - LOOK_BACK_SECS && fixture.kickoff <= now + LOOK_AHEAD_SECS)
        .collect();
    matches.sort_by(|(_, a), (_, b)| (a.kickoff, &a.id).cmp(&(b.kickoff, &b.id)));

    let added: Vec<EpgProgramme> = matches
        .into_iter()
        .filter(|(_, fixture)| !already_listed(&programmes, fixture))
        .map(|(league, fixture)| programme(channel_id, league, fixture))
        .collect();
    if added.is_empty() {
        return programmes;
    }
    let mut all = programmes;
    all.extend(added);
    all
}

/// `true` when the guide already has the match: a programme starting at kickoff, or one naming both teams.
fn already_listed(programmes: &[EpgProgramme], fixture: &Fixture) -> bool {
    let (home, away) = (fixture.home.to_lowercase(), fixture.away.to_lowercase());
    programmes.iter().any(|programme| {
        programme.start == fixture.kickoff
            || programme.title.as_deref().is_some_and(|title| {
                let title = title.to_lowercase();
                title.contains(&home) && title.contains(&away)
            })
    })
}

fn programme(channel_id: &str, league: &League, fixture: &Fixture) -> EpgProgramme {
    let sport = if fixture.sport.is_empty() { &league.sport } else { &fixture.sport };
    let sport_label = Sport::from_id(sport).map_or_else(|| sport.clone(), |sport| sport.label().to_string());
    let mut categories = vec![EpgCategory { value: "Sports".to_string(), lang: Some("en".to_string()) }];
    if !sport_label.is_empty() {
        categories.push(EpgCategory { value: sport_label, lang: Some("en".to_string()) });
    }
    EpgProgramme {
        channel: channel_id.to_string(),
        start: fixture.kickoff,
        stop: fixture.end(),
        title: Some(fixture.title()),
        description: Some(describe(league, fixture)),
        categories,
        catchup_id: None,
        icon: None,
        is_live: true,
        is_new: false,
        previously_shown: false,
        extra: Vec::new(),
    }
}

fn describe(league: &League, fixture: &Fixture) -> String {
    let mut text = league.name.clone();
    if !fixture.round.is_empty() {
        text.push_str(&format!(", round {}", fixture.round));
    }
    if !fixture.venue.is_empty() {
        text.push_str(&format!(" at {}", fixture.venue));
    }
    text.push('.');
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        fixtures::testing::{cache, fixture, league},
        testing::settings,
    };
    use tuliprox_plugin_sdk::mock;

    const SETTINGS: &str = r#"{"api_key":"tok","api_base_url":"http://api:1","leagues":["epl"]}"#;

    fn existing(start: i64, title: &str) -> EpgProgramme {
        EpgProgramme {
            channel: "sky.pl".to_string(),
            start,
            stop: start + 1800,
            title: Some(title.to_string()),
            description: None,
            categories: Vec::new(),
            catchup_id: None,
            icon: None,
            is_live: false,
            is_new: false,
            previously_shown: false,
            extra: Vec::new(),
        }
    }

    fn seed() {
        mock::set_now(1_000);
        cache(&[league(
            "epl",
            "football",
            &[],
            vec![
                fixture("e2", "Chelsea", "Everton", 90_000, &["sky.pl"]),
                fixture("e1", "Liverpool", "Arsenal", 5_000, &["SKY.PL"]),
                fixture("e3", "Fulham", "Wolves", 6_000, &["other"]),
                fixture("far", "Leeds", "Hull", 5_000_000, &["sky.pl"]),
            ],
        )]);
    }

    #[test]
    fn an_empty_guide_channel_gets_one_programme_per_fixture_in_window() {
        let settings = settings(SETTINGS);
        seed();
        let out = enrich(&settings, "sky.pl", Vec::new());
        let shown: Vec<_> =
            out.iter().map(|p| (p.title.as_deref().unwrap(), p.start, p.stop, p.channel.as_str())).collect();
        assert_eq!(
            shown,
            [("Liverpool vs Arsenal", 5_000, 12_200, "sky.pl"), ("Chelsea vs Everton", 90_000, 97_200, "sky.pl")]
        );
        assert_eq!(out[0].description.as_deref(), Some("EPL, round 12 at Anfield."));
        assert_eq!(out[0].categories[1].value, "Football");
        assert!(out[0].is_live);
    }

    #[test]
    fn existing_programmes_are_kept_and_a_listed_match_is_not_duplicated() {
        let settings = settings(SETTINGS);
        seed();
        let guide = vec![existing(5_000, "Magazine"), existing(40_000, "Late talk")];
        let out = enrich(&settings, "sky.pl", guide);
        let titles: Vec<_> = out.iter().map(|p| p.title.as_deref().unwrap()).collect();
        assert_eq!(titles, ["Magazine", "Late talk", "Chelsea vs Everton"]);

        let named = vec![existing(7, "Live: chelsea vs EVERTON")];
        let out = enrich(&settings, "sky.pl", named);
        assert_eq!(out.len(), 2, "the Chelsea match is named in the guide already");
    }

    #[test]
    fn a_channel_without_fixtures_and_a_cold_dead_api_change_nothing() {
        let settings = settings(SETTINGS);
        seed();
        assert!(enrich(&settings, "unknown", Vec::new()).is_empty());

        let cold = crate::testing::settings(SETTINGS);
        assert!(enrich(&cold, "sky.pl", Vec::new()).is_empty(), "no cache and the API is unreachable: fail open");
    }
}

//! `playlist.after-filter` (regroup + logos) and `playlist.after-sort` (league, then match time).
//!
//! # Scope
//!
//! The host applies `scope.groups` to both hooks, and it matches the group the
//! channel has *at that moment*. `after-filter` therefore sees the provider's
//! sport groups (`Sports`, `Sport HD`), but `after-sort` sees the groups this
//! plugin created (`Football`, `Basketball`, ...). The manifest regex names the
//! sport groups as well as `sport`, so the second hook still gets its channels.
//!
//! `after-sort` returns an order; the host applies it *within each group* and
//! orders groups by their first channel. A plugin cannot move a channel to
//! another group, which is why the regrouping happens in `after-filter`.

use crate::{
    fixtures::{self, Library},
    sports::{self, Sport},
    Settings,
};
use std::collections::BTreeMap;
use tuliprox_plugin_sdk::prelude::*;

/// Distinct logo URLs one hook call may hand to the host image cache.
const MAX_LOGOS_PER_CALL: usize = 64;

/// The change list: only channels whose group or logo actually changes.
///
/// A live channel is a sport channel when its name (or, failing that, its group) names a sport, or
/// when it broadcasts a cached fixture (matched by EPG id), whatever it is called.
#[must_use]
pub fn after_filter(settings: &Settings, channels: Vec<Channel>) -> Vec<ChannelChange> {
    let live: Vec<Channel> = channels.into_iter().filter(sports::is_live).collect();
    let named: Vec<Option<Sport>> = live.iter().map(sports::classify_channel).collect();
    // Fixtures are needed for a channel whose name says nothing (but has an EPG id) and for a logo.
    // Reading them may cost a bounded API call on a cold cache, so only ask when it matters.
    let needs_library = live.iter().zip(&named).any(|(channel, sport)| match sport {
        None => channel.epg_id.is_some(),
        Some(_) => channel.logo.trim().is_empty(),
    });
    if !needs_library && named.iter().all(Option::is_none) {
        return Vec::new();
    }
    let library = if needs_library { fixtures::shared(settings, true) } else { std::rc::Rc::new(Library::default()) };
    let now = clock::now_unix();

    let mut cached: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut changes = Vec::new();
    for (channel, named) in live.into_iter().zip(named) {
        let from_fixture =
            || channel.epg_id.as_deref().and_then(|epg_id| library.sport_on(epg_id)).and_then(Sport::from_id);
        let Some(sport) = named.or_else(from_fixture) else { continue };
        let mut changed = channel.clone();
        changed.group = sports::group_name(sport, settings.group_emoji);
        if channel.logo.trim().is_empty() {
            let source = logo_source(settings, &library, &channel, sport, now);
            if let Some(local) = local_logo(&mut cached, &source) {
                changed.logo = local;
            }
        }
        if changed.group != channel.group || changed.logo != channel.logo {
            changes.push(ChannelChange::Modified(changed));
        }
    }
    changes
}

/// The remote logo for a channel without one: a team named in the channel name, the league of the
/// match it broadcasts, a league its name mentions, or the generic logo of its sport.
fn logo_source(settings: &Settings, library: &Library, channel: &Channel, sport: Sport, now: i64) -> String {
    let text = format!("{} {}", channel.name, channel.title);
    if let Some(team) = library.team_logo(&text) {
        return team.to_string();
    }
    if let Some(epg_id) = channel.epg_id.as_deref() {
        if let Some((league, _)) = library.upcoming_on(epg_id, now).first() {
            if !league.logo.is_empty() {
                return league.logo.clone();
            }
        }
    }
    if let Some(league) = library.league_named(&text) {
        if !league.logo.is_empty() {
            return league.logo.clone();
        }
    }
    format!("{}/logos/sport-{}.png", settings.base_url(), sport.id())
}

/// The locally served URL of `source`, from the host image cache (once per URL and call).
fn local_logo(cached: &mut BTreeMap<String, Option<String>>, source: &str) -> Option<String> {
    if let Some(known) = cached.get(source) {
        return known.clone();
    }
    if cached.len() >= MAX_LOGOS_PER_CALL {
        return None;
    }
    let local = match images::cache(source) {
        Ok(local) => Some(local),
        Err(err) => {
            debug!("logo `{source}` not cached: {err}");
            None
        }
    };
    cached.insert(source.to_string(), local.clone());
    local
}

/// Handles in league order, then match time, then handle. An empty list leaves the order alone.
#[must_use]
pub fn after_sort(settings: &Settings, channels: &[Channel]) -> Vec<u32> {
    let library = fixtures::shared(settings, true);
    if library.is_empty() {
        return Vec::new();
    }
    let now = clock::now_unix();
    let mut keyed: Vec<((usize, i64, u32), u32)> = channels
        .iter()
        .map(|channel| {
            let (rank, kickoff) = slot(&library, channel, now);
            ((rank, kickoff, channel.handle), channel.handle)
        })
        .collect();
    keyed.sort_by_key(|(key, _)| *key);
    keyed.into_iter().map(|(_, handle)| handle).collect()
}

/// `(league rank, kickoff of the next match)`. A channel whose name only mentions a league sorts
/// after the league's channels with a match, and a channel with no league sorts last.
fn slot(library: &Library, channel: &Channel, now: i64) -> (usize, i64) {
    if let Some(epg_id) = channel.epg_id.as_deref() {
        if let Some((league, fixture)) = library.upcoming_on(epg_id, now).first() {
            if let Some(rank) = library.rank(&league.id) {
                return (rank, fixture.kickoff);
            }
        }
    }
    let text = format!("{} {}", channel.name, channel.title);
    if let Some(rank) = library.league_named(&text).and_then(|league| library.rank(&league.id)) {
        return (rank, i64::MAX - 1);
    }
    (usize::MAX, i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        fixtures::testing::{cache, fixture, league},
        testing::{channel, settings},
    };
    use tuliprox_plugin_sdk::mock;

    const SETTINGS: &str = r#"{"api_key":"tok","api_base_url":"http://api:1","leagues":["epl","nba"]}"#;

    fn seed() {
        mock::set_now(1_000);
        cache(&[
            league(
                "epl",
                "football",
                &["premier league"],
                vec![
                    fixture("e2", "Chelsea", "Everton", 9_000, &["tnt.one"]),
                    fixture("e1", "Liverpool", "Arsenal", 5_000, &["sky.pl"]),
                ],
            ),
            league("nba", "basketball", &["nba"], vec![fixture("n1", "Lakers", "Celtics", 2_000, &["espn.nba"])]),
        ]);
    }

    fn modified(change: &ChannelChange) -> &Channel {
        match change {
            ChannelChange::Modified(channel) => channel,
            ChannelChange::Drop(handle) => panic!("unexpected drop of {handle}"),
        }
    }

    #[test]
    fn sport_channels_are_regrouped_and_only_changed_ones_are_returned() {
        let settings = settings(SETTINGS);
        seed();
        let channels = vec![
            channel(0, "Sky Sports Premier League", "UK Sports", Some("sky.pl"), "http://logo/x.png"),
            channel(1, "ESPN NBA", "UK Sports", Some("espn.nba"), "http://logo/y.png"),
            channel(2, "BBC News", "UK Sports", None, "http://logo/z.png"),
            channel(3, "Football Live", "\u{26BD} Football", None, "http://logo/w.png"),
        ];
        let changes = after_filter(&settings, channels);
        let regrouped: Vec<_> = changes.iter().map(|c| (modified(c).handle, modified(c).group.as_str())).collect();
        assert_eq!(regrouped, [(0, "\u{26BD} Football"), (1, "\u{1F3C0} Basketball")]);
        assert!(mock::cached_images().is_empty(), "channels with a logo never touch the image cache");
    }

    #[test]
    fn the_emoji_toggle_drops_the_emoji() {
        let settings = settings(r#"{"group_emoji":false}"#);
        let changes = after_filter(&settings, vec![channel(0, "NFL Network", "Sports", None, "http://l/x.png")]);
        assert_eq!(modified(&changes[0]).group, "American Football");
    }

    #[test]
    fn a_channel_without_a_logo_gets_a_cached_team_league_or_sport_logo() {
        let settings = settings(SETTINGS);
        seed();
        mock::set_cached_image("http://127.0.0.1:18970/logos/liverpool.png", Ok("/cache/team"));
        mock::set_cached_image("http://127.0.0.1:18970/logos/epl.png", Ok("/cache/league"));
        mock::set_cached_image("http://api:1/logos/sport-ice-hockey.png", Ok("/cache/sport"));
        let channels = vec![
            channel(0, "Liverpool Football TV", "Sports", None, ""),
            channel(1, "Premier League HD", "Sports", Some("sky.pl"), ""),
            channel(2, "NHL Network", "Sports", None, ""),
            channel(3, "Premier League 2", "Sports", Some("sky.pl"), ""),
        ];
        let changes = after_filter(&settings, channels);
        let logos: Vec<_> = changes.iter().map(|c| (modified(c).handle, modified(c).logo.as_str())).collect();
        assert_eq!(logos, [(0, "/cache/team"), (1, "/cache/league"), (2, "/cache/sport"), (3, "/cache/league")]);
        assert_eq!(mock::cached_images().len(), 3, "one image-cache call per distinct logo");
    }

    #[test]
    fn a_failed_image_download_keeps_the_channel_logo_empty_but_still_regroups() {
        let settings = settings(SETTINGS);
        mock::set_cached_image("http://api:1/logos/sport-basketball.png", Err("quota"));
        let changes = after_filter(&settings, vec![channel(0, "NBA TV", "Sports", None, "")]);
        let changed = modified(&changes[0]);
        assert_eq!((changed.group.as_str(), changed.logo.as_str()), ("\u{1F3C0} Basketball", ""));
    }

    #[test]
    fn after_sort_orders_by_league_then_kickoff_and_is_stable_for_ties() {
        let settings = settings(SETTINGS);
        seed();
        let channels = vec![
            channel(0, "Documentary Channel", "Football", None, ""),
            channel(1, "Sky Sports", "Football", Some("TNT.ONE"), ""),
            channel(2, "Arena", "Football", Some("sky.pl"), ""),
            channel(3, "ESPN NBA", "Football", Some("espn.nba"), ""),
            channel(4, "Premier League Extra", "Football", None, ""),
            channel(5, "Other", "Football", None, ""),
        ];
        // epl matches: sky.pl (5000) before tnt.one (9000); a name-only league follows them; nba is the second league;
        // channels without a league keep their order at the end.
        assert_eq!(after_sort(&settings, &channels), [2, 1, 4, 3, 0, 5]);
        assert_eq!(after_sort(&settings, &channels), after_sort(&settings, &channels), "deterministic");
    }

    #[test]
    fn after_sort_without_fixtures_leaves_the_order_alone() {
        let settings = settings(r#"{"leagues":["epl"]}"#);
        assert!(after_sort(&settings, &[channel(0, "Sky", "Football", None, "")]).is_empty());
    }

    #[test]
    fn a_finished_match_no_longer_sorts_a_channel() {
        let settings = settings(SETTINGS);
        mock::set_now(10_000);
        cache(&[league("epl", "football", &["premier league"], vec![fixture("old", "A", "B", 100, &["sky.pl"])])]);
        let channels = vec![
            channel(0, "Arena", "Football", Some("sky.pl"), ""),
            channel(1, "Premier League Extra", "Football", None, ""),
        ];
        assert_eq!(after_sort(&settings, &channels), [1, 0]);
    }

    #[test]
    fn a_channel_that_broadcasts_a_fixture_is_a_sport_channel_whatever_its_name() {
        let settings = settings(SETTINGS);
        seed();
        let channels = vec![
            channel(0, "Arena 1", "Sports", Some("espn.nba"), "http://l/a.png"),
            channel(1, "Arena 2", "Sports", Some("nobody"), "http://l/b.png"),
            channel(2, "Arena 3", "Sports", None, "http://l/c.png"),
        ];
        let changes = after_filter(&settings, channels);
        assert_eq!(changes.len(), 1);
        assert_eq!((modified(&changes[0]).handle, modified(&changes[0]).group.as_str()), (0, "\u{1F3C0} Basketball"));
    }
}

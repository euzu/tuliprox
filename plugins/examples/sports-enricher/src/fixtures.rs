//! Fixtures: the mock sports API, the KV cache in front of it, and lookups.
//!
//! Layout of the plugin KV store:
//!
//! | key | value | written by |
//! | --- | --- | --- |
//! | `fx:<league>` | `Block` as a TTL value (1 h) | refresh task, or a hook on a cold cache |
//! | `fx-fail:<league>` | TTL marker (2 min) so a dead API is not retried by every hook | a failed cold fetch |
//! | `meta:refreshed` | unix seconds of the last successful refresh | refresh |
//! | `calls:<day>` | API calls made on that UTC day | every API call |
//!
//! Hooks only read these keys. They fetch (bounded: one call per cold league, a
//! 2 s timeout, and no further call in the same invocation after one failed)
//! when a league is *cold*, i.e. its key is absent or expired, and they fail
//! open: no key, an API error or a decode error leaves the league out of the
//! [`Library`] and never fails the hook.

use crate::Settings;
use serde::{Deserialize, Serialize};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc, time::Duration};
use tuliprox_plugin_sdk::prelude::*;

/// How long a league stays fresh in the KV store.
pub const CACHE_TTL_SECS: u32 = 3600;
/// After a failed cold fetch a league is not fetched again for this long.
pub const FAIL_BACKOFF_SECS: u32 = 120;
/// Network timeout of one API call made by the refresh task.
const TASK_TIMEOUT: Duration = Duration::from_secs(5);
/// Network timeout of one API call made by a hook: a hook call has a 5 s deadline in all, and at most
/// one call per hook invocation may time out (see [`load`]).
const HOOK_TIMEOUT: Duration = Duration::from_secs(2);
/// Fixtures kept per league (the KV value limit is 1 MiB).
const MAX_FIXTURES: usize = 200;
const DEFAULT_MINUTES: u32 = 120;

const fn default_minutes() -> u32 { DEFAULT_MINUTES }

pub const KEY_REFRESHED: &str = "meta:refreshed";

/// One match, as the API returns it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fixture {
    pub id: String,
    #[serde(default)]
    pub league: String,
    #[serde(default)]
    pub sport: String,
    pub home: String,
    pub away: String,
    /// Unix seconds.
    pub kickoff: i64,
    #[serde(default = "default_minutes")]
    pub duration_min: u32,
    #[serde(default)]
    pub venue: String,
    #[serde(default)]
    pub round: String,
    /// EPG channel ids that broadcast the match.
    #[serde(default)]
    pub broadcasters: Vec<String>,
    #[serde(default)]
    pub home_logo: String,
    #[serde(default)]
    pub away_logo: String,
    /// Match facts for the EPG overlay (form, head to head, ...). A `BTreeMap`: the order is part of the output.
    #[serde(default)]
    pub stats: BTreeMap<String, String>,
}

impl Fixture {
    /// `Home vs Away`.
    #[must_use]
    pub fn title(&self) -> String { format!("{} vs {}", self.home, self.away) }

    /// Unix seconds the match ends.
    #[must_use]
    pub fn end(&self) -> i64 { self.kickoff.saturating_add(i64::from(self.duration_min) * 60) }

    /// `true` when `epg_id` (compared case-insensitively) broadcasts the match.
    #[must_use]
    pub fn on_channel(&self, epg_id: &str) -> bool {
        self.broadcasters.iter().any(|broadcaster| broadcaster.eq_ignore_ascii_case(epg_id))
    }
}

/// One league with its fixtures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct League {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub sport: String,
    /// Logo URL on the API host.
    #[serde(default)]
    pub logo: String,
    /// Lowercase phrases that name the league in a channel name.
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub fixtures: Vec<Fixture>,
}

/// What `fx:<league>` holds.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Block {
    fetched_at: i64,
    league: League,
}

/// The cached leagues of one run, in the configured order.
#[derive(Debug, Default)]
pub struct Library {
    pub leagues: Vec<League>,
}

impl Library {
    #[must_use]
    pub fn is_empty(&self) -> bool { self.leagues.is_empty() }

    /// Every fixture with its league, league order then kickoff then id.
    pub fn fixtures(&self) -> impl Iterator<Item = (&League, &Fixture)> {
        self.leagues.iter().flat_map(|league| league.fixtures.iter().map(move |fixture| (league, fixture)))
    }

    #[must_use]
    pub fn fixture_count(&self) -> usize { self.leagues.iter().map(|league| league.fixtures.len()).sum() }

    /// Position of `league_id` in the configured order.
    #[must_use]
    pub fn rank(&self, league_id: &str) -> Option<usize> {
        self.leagues.iter().position(|league| league.id == league_id)
    }

    /// The fixtures `epg_id` broadcasts that have not ended at `now`, earliest first.
    #[must_use]
    pub fn upcoming_on(&self, epg_id: &str, now: i64) -> Vec<(&League, &Fixture)> {
        let mut found: Vec<_> = self.fixtures().filter(|(_, f)| f.on_channel(epg_id) && f.end() >= now).collect();
        found.sort_by(|(_, a), (_, b)| (a.kickoff, &a.id).cmp(&(b.kickoff, &b.id)));
        found
    }

    /// The sport id of any fixture `epg_id` broadcasts.
    #[must_use]
    pub fn sport_on(&self, epg_id: &str) -> Option<&str> {
        self.fixtures()
            .find(|(_, fixture)| fixture.on_channel(epg_id))
            .map(
                |(league, fixture)| {
                    if fixture.sport.is_empty() {
                        league.sport.as_str()
                    } else {
                        fixture.sport.as_str()
                    }
                },
            )
            .filter(|sport| !sport.is_empty())
    }

    /// The first league (in configured order) whose keywords occur in `text`.
    #[must_use]
    pub fn league_named(&self, text: &str) -> Option<&League> {
        let lowered = text.to_lowercase();
        self.leagues.iter().find(|league| league.keywords.iter().any(|keyword| word_in(&lowered, keyword)))
    }

    /// The logo URL of the team whose name occurs in `text`.
    #[must_use]
    pub fn team_logo(&self, text: &str) -> Option<&str> {
        let lowered = text.to_lowercase();
        self.fixtures().find_map(|(_, fixture)| {
            [(&fixture.home, &fixture.home_logo), (&fixture.away, &fixture.away_logo)]
                .into_iter()
                .find(|(team, logo)| {
                    team.chars().count() >= 4 && !logo.is_empty() && word_in(&lowered, &team.to_lowercase())
                })
                .map(|(_, logo)| logo.as_str())
        })
    }
}

/// `needle` in `haystack` on word boundaries (both lowercase).
fn word_in(haystack: &str, needle: &str) -> bool {
    !needle.is_empty()
        && haystack.match_indices(needle).any(|(start, found)| {
            let before = haystack[..start].chars().next_back();
            let after = haystack[start + found.len()..].chars().next();
            before.is_none_or(|c| !c.is_alphanumeric()) && after.is_none_or(|c| !c.is_alphanumeric())
        })
}

fn block_key(league: &str) -> String { format!("fx:{league}") }

fn fail_key(league: &str) -> String { format!("fx-fail:{league}") }

fn calls_key(day: i64) -> String { format!("calls:{day}") }

fn day_of(now: i64) -> i64 { now.div_euclid(86_400) }

/// API calls made today (host clock, UTC day).
#[must_use]
pub fn calls_today() -> u32 { kv::get_json::<u32>(&calls_key(day_of(clock::now_unix()))).ok().flatten().unwrap_or(0) }

fn count_call() {
    let day = day_of(clock::now_unix());
    let key = calls_key(day);
    let calls = kv::get_json::<u32>(&key).ok().flatten().unwrap_or(0);
    if let Err(err) = kv::set_json(&key, &calls.saturating_add(1)) {
        warn!("could not count an API call: {err}");
    }
    // Yesterday's counter is of no further use.
    let _ = kv::delete(&calls_key(day - 1));
}

/// Unix seconds of the last successful refresh.
#[must_use]
pub fn refreshed_at() -> Option<i64> { kv::get_json::<i64>(KEY_REFRESHED).ok().flatten() }

/// One API call: the fixtures of `league`. Counted in `calls:<day>`.
pub fn fetch_league(settings: &Settings, league: &str, timeout: Duration) -> Result<League, String> {
    let key = settings.api_key_value().ok_or("no api_key is configured")?;
    let url = format!("{}/v1/fixtures?league={league}", settings.base_url());
    count_call();
    let mut fetched: League = http::Client::new()
        .timeout(timeout)
        .get(url)
        .bearer(key)
        .header("accept", "application/json")
        .send()?
        .error_for_status()?
        .json()?;
    fetched.id = league.to_string();
    for fixture in &mut fetched.fixtures {
        fixture.league.clone_from(&fetched.id);
        if fixture.sport.is_empty() {
            fixture.sport.clone_from(&fetched.sport);
        }
    }
    fetched.fixtures.sort_by(|a, b| (a.kickoff, &a.id).cmp(&(b.kickoff, &b.id)));
    fetched.fixtures.truncate(MAX_FIXTURES);
    Ok(fetched)
}

/// Fetch `league` and cache it. The marker of an earlier failure is cleared.
pub fn refresh_league(settings: &Settings, league: &str, timeout: Duration) -> Result<League, String> {
    let fetched = fetch_league(settings, league, timeout)?;
    let block = Block { fetched_at: clock::now_unix(), league: fetched.clone() };
    kv::set_json_ttl(&block_key(league), &block, CACHE_TTL_SECS)?;
    let _ = kv::delete(&fail_key(league));
    Ok(fetched)
}

/// What a full refresh did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Refresh {
    pub leagues: usize,
    pub fixtures: usize,
    pub failed: Vec<String>,
}

/// Refresh every configured league. Records `meta:refreshed` when at least one succeeded.
pub fn refresh_all(settings: &Settings) -> Refresh {
    let mut summary = Refresh::default();
    for id in settings.league_ids() {
        match refresh_league(settings, &id, TASK_TIMEOUT) {
            Ok(league) => {
                summary.leagues += 1;
                summary.fixtures += league.fixtures.len();
            }
            Err(err) => {
                warn!("refresh of league `{id}` failed: {err}");
                summary.failed.push(id);
            }
        }
    }
    if summary.leagues > 0 {
        if let Err(err) = kv::set_json(KEY_REFRESHED, &clock::now_unix()) {
            warn!("could not record the refresh time: {err}");
        }
    }
    summary
}

/// The cached leagues, fetching a cold one when `fetch_on_cold` is set and a key is configured.
#[must_use]
pub fn load(settings: &Settings, fetch_on_cold: bool) -> Library {
    let mut library = Library::default();
    // After one failed fetch this invocation makes no more calls: a dead API would otherwise cost one
    // timeout per league, more than a hook call's deadline.
    let mut failed = false;
    for id in settings.league_ids() {
        match kv::get_json_fresh::<Block>(&block_key(&id)) {
            Ok(Some(block)) => library.leagues.push(block.league),
            Ok(None) if fetch_on_cold && !failed && settings.api_key_value().is_some() => {
                if matches!(kv::get_fresh(&fail_key(&id)), Ok(Some(_))) {
                    continue;
                }
                match refresh_league(settings, &id, HOOK_TIMEOUT) {
                    Ok(league) => library.leagues.push(league),
                    Err(err) => {
                        failed = true;
                        warn!("cold fetch of league `{id}` failed, continuing without it: {err}");
                        if let Err(err) = kv::set_ttl(&fail_key(&id), b"1", FAIL_BACKOFF_SECS) {
                            warn!("could not record the failure: {err}");
                        }
                    }
                }
            }
            Ok(None) => {}
            Err(err) => warn!("cached league `{id}` is unreadable: {err}"),
        }
    }
    library
}

thread_local! {
    /// The library of this instance, so a hook called once per channel reads the KV store once.
    static MEMO: RefCell<Option<(bool, Rc<Library>)>> = const { RefCell::new(None) };
}

/// [`load`], memoized for the life of the instance (hooks run in a fresh instance per call).
#[must_use]
pub fn shared(settings: &Settings, fetch_on_cold: bool) -> Rc<Library> {
    MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        if let Some((fetched, library)) = memo.as_ref() {
            if *fetched || !fetch_on_cold {
                return Rc::clone(library);
            }
        }
        let library = Rc::new(load(settings, fetch_on_cold));
        *memo = Some((fetch_on_cold, Rc::clone(&library)));
        library
    })
}

/// Drops the memoized library (tests, after a refresh).
pub fn forget() { MEMO.with(|memo| *memo.borrow_mut() = None); }

#[cfg(test)]
pub mod testing {
    //! Fixture builders shared by the unit tests of every module.

    use super::*;

    pub fn fixture(id: &str, home: &str, away: &str, kickoff: i64, broadcasters: &[&str]) -> Fixture {
        Fixture {
            id: id.to_string(),
            league: String::new(),
            sport: String::new(),
            home: home.to_string(),
            away: away.to_string(),
            kickoff,
            duration_min: 120,
            venue: "Anfield".to_string(),
            round: "12".to_string(),
            broadcasters: broadcasters.iter().map(|b| (*b).to_string()).collect(),
            home_logo: format!("http://127.0.0.1:18970/logos/{}.png", home.to_lowercase().replace(' ', "-")),
            away_logo: format!("http://127.0.0.1:18970/logos/{}.png", away.to_lowercase().replace(' ', "-")),
            stats: BTreeMap::new(),
        }
    }

    pub fn league(id: &str, sport: &str, keywords: &[&str], fixtures: Vec<Fixture>) -> League {
        League {
            id: id.to_string(),
            name: id.to_uppercase(),
            sport: sport.to_string(),
            logo: format!("http://127.0.0.1:18970/logos/{id}.png"),
            keywords: keywords.iter().map(|k| (*k).to_string()).collect(),
            fixtures,
        }
    }

    /// Stores `leagues` in the mock KV store the way a refresh does.
    pub fn cache(leagues: &[League]) {
        for league in leagues {
            let block = Block { fetched_at: clock::now_unix(), league: league.clone() };
            kv::set_json_ttl(&block_key(&league.id), &block, CACHE_TTL_SECS).unwrap();
        }
        forget();
    }
}

#[cfg(test)]
mod tests {
    use super::{testing::*, *};
    use tuliprox_plugin_sdk::mock;

    fn settings(json: &str) -> Settings {
        mock::reset();
        forget();
        Settings::parse(json).unwrap()
    }

    fn api_league_json() -> String {
        serde_json::json!({
            "id": "epl", "name": "Premier League", "sport": "football", "logo": "http://api/logos/epl.png",
            "keywords": ["premier league"],
            "fixtures": [
                {"id": "b", "home": "Chelsea", "away": "Spurs", "kickoff": 2000, "broadcasters": ["tnt"]},
                {"id": "a", "home": "Liverpool", "away": "Arsenal", "kickoff": 1000, "broadcasters": ["sky"]},
            ],
        })
        .to_string()
    }

    #[test]
    fn a_refresh_calls_the_api_with_the_bearer_token_and_caches_sorted_fixtures() {
        let settings = settings(r#"{"api_key":"tok","api_base_url":"http://api:1","leagues":["epl"]}"#);
        mock::set_now(1_000_000);
        mock::http_respond(http::Method::Get, "http://api:1/v1/fixtures?league=epl", 200, api_league_json());

        let summary = refresh_all(&settings);
        assert_eq!(summary, Refresh { leagues: 1, fixtures: 2, failed: vec![] });
        let sent = mock::http_requests();
        assert!(sent[0].headers().contains(&("authorization".to_string(), "Bearer tok".to_string())));
        assert_eq!(calls_today(), 1);
        assert_eq!(refreshed_at(), Some(1_000_000));

        let library = load(&settings, false);
        let ids: Vec<_> =
            library.fixtures().map(|(league, f)| (league.id.as_str(), f.id.as_str(), f.league.as_str())).collect();
        assert_eq!(ids, [("epl", "a", "epl"), ("epl", "b", "epl")]);
        assert_eq!(library.fixtures().next().map(|(_, f)| f.sport.as_str()), Some("football"));
    }

    #[test]
    fn a_cold_cache_fetches_once_and_a_failure_backs_off() {
        let settings = settings(r#"{"api_key":"tok","api_base_url":"http://api:1","leagues":["epl"]}"#);
        mock::set_now(5_000);
        mock::http_fail(http::Method::Get, "http://api:1/v1/fixtures?league=epl", "connection refused");

        assert!(load(&settings, true).is_empty(), "fail open");
        assert!(load(&settings, true).is_empty(), "backed off: no second request");
        assert_eq!(mock::http_requests().len(), 1);

        mock::advance(i64::from(FAIL_BACKOFF_SECS));
        mock::http_respond(http::Method::Get, "http://api:1/v1/fixtures?league=epl", 200, api_league_json());
        assert_eq!(load(&settings, true).fixture_count(), 2);
        assert_eq!(mock::http_requests().len(), 2);
        assert_eq!(load(&settings, true).fixture_count(), 2, "warm now");
        assert_eq!(mock::http_requests().len(), 2);
    }

    #[test]
    fn one_failed_cold_fetch_stops_further_calls_in_the_same_invocation() {
        let settings = settings(r#"{"api_key":"tok","api_base_url":"http://api:1","leagues":["epl","nba"]}"#);
        mock::set_now(5_000);
        mock::http_fail(http::Method::Get, "http://api:1/v1/fixtures?league=epl", "timed out");
        assert!(load(&settings, true).is_empty());
        assert_eq!(mock::http_requests().len(), 1, "nba is not tried after epl failed");
    }

    #[test]
    fn the_cache_expires_and_no_key_means_no_fetch() {
        let settings = settings(r#"{"api_key":"tok","api_base_url":"http://api:1","leagues":["epl"]}"#);
        mock::set_now(100);
        cache(&[league("epl", "football", &[], vec![fixture("a", "A", "B", 1, &[])])]);
        assert_eq!(load(&settings, false).fixture_count(), 1);
        mock::advance(i64::from(CACHE_TTL_SECS));
        assert_eq!(load(&settings, false).fixture_count(), 0);

        let keyless = Settings::parse(r#"{"leagues":["epl"]}"#).unwrap();
        assert!(load(&keyless, true).is_empty());
        assert!(mock::http_requests().is_empty());
        assert!(fetch_league(&keyless, "epl", HOOK_TIMEOUT).unwrap_err().contains("api_key"));
    }

    #[test]
    fn lookups_by_channel_keyword_and_team() {
        mock::reset();
        let library = Library {
            leagues: vec![league(
                "epl",
                "football",
                &["premier league"],
                vec![
                    fixture("late", "Chelsea", "Everton", 5_000, &["Sky.One"]),
                    fixture("soon", "Liverpool", "Arsenal", 3_000, &["sky.one"]),
                    fixture("gone", "Fulham", "Wolves", 10, &["sky.one"]),
                ],
            )],
        };
        let upcoming = library.upcoming_on("SKY.ONE", 10_000);
        assert_eq!(upcoming.iter().map(|(_, f)| f.id.as_str()).collect::<Vec<_>>(), ["soon", "late"]);
        assert_eq!(library.rank("epl"), Some(0));
        assert_eq!(library.league_named("Sky Sports Premier League 2").map(|l| l.id.as_str()), Some("epl"));
        assert!(library.league_named("Sky Sports Main Event").is_none());
        assert_eq!(library.team_logo("Liverpool TV").map(|l| l.ends_with("liverpool.png")), Some(true));
        assert!(library.team_logo("Everton").is_some());
        assert!(library.team_logo("Evert").is_none());
    }
}

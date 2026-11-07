//! The plugin's own API: the `get-fixtures` rpc and the `refresh-fixtures` task.

use crate::{
    activity::{self, LastUpdate},
    fixtures::{self, Fixture},
    Settings,
};
use serde::{Deserialize, Serialize};
use tuliprox_plugin_sdk::prelude::*;

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;

/// Payload of `get-fixtures` (see `rpc/get-fixtures.schema.json`). Every field is optional.
#[derive(Debug, Default, Deserialize)]
pub struct GetFixtures {
    /// League ids to include; empty means every tracked league.
    #[serde(default)]
    pub leagues: Vec<String>,
    /// Only the fixtures this EPG channel id broadcasts.
    #[serde(default)]
    pub channel: Option<String>,
    /// At most this many fixtures (default 50, at most 200).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct LeagueView {
    pub id: String,
    pub name: String,
    pub sport: String,
    pub fixtures: usize,
}

#[derive(Debug, Serialize)]
pub struct FixtureView<'a> {
    #[serde(flatten)]
    pub fixture: &'a Fixture,
    pub title: String,
    pub league_name: &'a str,
}

/// The answer of `get-fixtures`.
#[derive(Debug, Serialize)]
pub struct FixturesView<'a> {
    /// The host clock, so a frame never compares fixture times with its own clock.
    pub now: i64,
    /// `false` when no API key is set: the lists stay empty.
    pub configured: bool,
    pub refreshed_at: Option<i64>,
    pub calls_today: u32,
    pub leagues: Vec<LeagueView>,
    pub fixtures: Vec<FixtureView<'a>>,
    pub last_update: Option<LastUpdate>,
}

/// Fixtures that have not ended, earliest first, for the requested leagues and channel.
pub fn get_fixtures<'a>(
    settings: &Settings,
    request: &GetFixtures,
    library: &'a fixtures::Library,
) -> FixturesView<'a> {
    let now = clock::now_unix();
    let wanted: Vec<String> =
        request.leagues.iter().map(|id| id.trim().to_lowercase()).filter(|id| !id.is_empty()).collect();
    let limit = request.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    let leagues = library
        .leagues
        .iter()
        .filter(|league| wanted.is_empty() || wanted.contains(&league.id))
        .map(|league| LeagueView {
            id: league.id.clone(),
            name: league.name.clone(),
            sport: league.sport.clone(),
            fixtures: league.fixtures.len(),
        })
        .collect();

    let mut shown: Vec<_> = library
        .fixtures()
        .filter(|(league, _)| wanted.is_empty() || wanted.contains(&league.id))
        .filter(|(_, fixture)| fixture.end() >= now)
        .filter(|(_, fixture)| request.channel.as_deref().is_none_or(|channel| fixture.on_channel(channel)))
        .collect();
    shown.sort_by(|(_, a), (_, b)| (a.kickoff, &a.id).cmp(&(b.kickoff, &b.id)));
    shown.truncate(limit);

    FixturesView {
        now,
        configured: settings.api_key_value().is_some(),
        refreshed_at: fixtures::refreshed_at(),
        calls_today: fixtures::calls_today(),
        leagues,
        fixtures: shown
            .into_iter()
            .map(|(league, fixture)| FixtureView { fixture, title: fixture.title(), league_name: &league.name })
            .collect(),
        last_update: activity::last_update(),
    }
}

/// `rpc.call`: dispatch by method name.
pub fn call(method: &str, payload: &str) -> Result<String, String> {
    let settings = Settings::current()?;
    rpc::Router::new()
        .route("get-fixtures", move |request: Option<GetFixtures>| {
            let library = fixtures::shared(&settings, true);
            serde_json::to_value(get_fixtures(&settings, &request.unwrap_or_default(), &library))
        })
        .dispatch(method, payload)
}

/// The `refresh-fixtures` task: refetch every tracked league into the KV store.
pub fn refresh_fixtures(_ctx: &TaskContext) -> Result<String, String> {
    let settings = Settings::current()?;
    if settings.api_key_value().is_none() {
        return Err("no api_key is configured".to_string());
    }
    let summary = fixtures::refresh_all(&settings);
    fixtures::forget();
    if summary.leagues == 0 {
        return Err(format!("no league could be refreshed (failed: {})", summary.failed.join(", ")));
    }
    let mut report = format!("refreshed {} leagues, {} fixtures", summary.leagues, summary.fixtures);
    if !summary.failed.is_empty() {
        report.push_str(&format!("; failed: {}", summary.failed.join(", ")));
    }
    Ok(report)
}

/// `tasks.run`: dispatch by task id.
pub fn run_task(ctx: &TaskContext) -> Result<String, String> {
    tasks::Router::new().task("refresh-fixtures", refresh_fixtures).dispatch(ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        fixtures::testing::{cache, fixture, league},
        testing::settings,
    };
    use tuliprox_plugin_sdk::mock;

    const SETTINGS: &str = r#"{"api_key":"tok","api_base_url":"http://api:1","leagues":["epl","nba"]}"#;

    fn seed() {
        mock::set_now(1_000);
        cache(&[
            league(
                "epl",
                "football",
                &[],
                vec![
                    fixture("e1", "Liverpool", "Arsenal", 5_000, &["sky.pl"]),
                    fixture("e0", "Old", "Game", -50_000, &["sky.pl"]),
                ],
            ),
            league("nba", "basketball", &[], vec![fixture("n1", "Lakers", "Celtics", 2_000, &["espn.nba"])]),
        ]);
    }

    fn payload(json: &str) -> serde_json::Value { serde_json::from_str(&call("get-fixtures", json).unwrap()).unwrap() }

    #[test]
    fn get_fixtures_lists_upcoming_matches_filtered_by_league_channel_and_limit() {
        let _ = settings(SETTINGS);
        seed();
        crate::Settings::init(SETTINGS).unwrap();

        let all = payload("");
        assert_eq!(all["configured"], true);
        assert_eq!(all["now"], 1_000);
        let ids: Vec<_> =
            all["fixtures"].as_array().unwrap().iter().map(|f| f["id"].as_str().unwrap().to_string()).collect();
        assert_eq!(ids, ["n1", "e1"], "kickoff order, the finished match is gone");
        assert_eq!(all["fixtures"][0]["title"], "Lakers vs Celtics");
        assert_eq!(all["fixtures"][0]["league_name"], "NBA");
        assert_eq!(all["leagues"].as_array().unwrap().len(), 2);

        let epl = payload(r#"{"leagues":["EPL"]}"#);
        assert_eq!(epl["fixtures"].as_array().unwrap().len(), 1);
        assert_eq!(epl["leagues"].as_array().unwrap().len(), 1);
        let by_channel = payload(r#"{"channel":"ESPN.NBA"}"#);
        assert_eq!(by_channel["fixtures"][0]["id"], "n1");
        assert_eq!(payload(r#"{"limit":1}"#)["fixtures"].as_array().unwrap().len(), 1);
        assert!(call("nope", "").unwrap_err().contains("get-fixtures"));
        assert!(call("get-fixtures", r#"{"limit":"x"}"#).is_err());
    }

    #[test]
    fn the_refresh_task_reports_what_it_did_and_fails_when_nothing_refreshed() {
        let _ = settings(SETTINGS);
        mock::set_now(9_000);
        crate::Settings::init(SETTINGS).unwrap();
        let ctx = TaskContext { task_id: "refresh-fixtures".to_string(), scheduled_at: 9_000, last_success_at: None };

        mock::http_respond(
            http::Method::Get,
            "http://api:1/v1/fixtures?league=epl",
            200,
            r#"{"id":"epl","name":"EPL","sport":"football","fixtures":[{"id":"a","home":"A","away":"B","kickoff":9500}]}"#,
        );
        mock::http_fail(http::Method::Get, "http://api:1/v1/fixtures?league=nba", "refused");
        assert_eq!(run_task(&ctx), Ok("refreshed 1 leagues, 1 fixtures; failed: nba".to_string()));
        assert_eq!(fixtures::refreshed_at(), Some(9_000));

        mock::http_fail(http::Method::Get, "http://api:1/v1/fixtures?league=epl", "refused");
        mock::http_fail(http::Method::Get, "http://api:1/v1/fixtures?league=nba", "refused");
        assert!(run_task(&ctx).unwrap_err().contains("epl, nba"));
        let unknown = TaskContext { task_id: "nope".to_string(), ..ctx };
        assert!(run_task(&unknown).unwrap_err().contains("refresh-fixtures"));
    }
}

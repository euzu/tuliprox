//! `playlist.enrich-metadata`: extra text on sports documentaries.
//!
//! The host calls the hook with the resolved VOD record (`VideoStreamProperties`
//! as JSON) after the provider fetch and before it is stored. What reaches a
//! player is the stored record's `details` block, which `get_vod_info` serves as
//! `info`: `plot`, `description` and `genre` are all visible there. The top-level
//! fields only feed `get_vod_streams` (`rating`, `trailer`, `tmdb`, ...), and
//! `plot` is not among them, so the plugin writes into `details`, not the top level.
//! A record with no `details` (a provider that returned no info) is left alone.
//!
//! The hook reads fixtures from the KV store only. It never calls the API:
//! it runs once per resolved title, often from the background metadata worker.

use crate::{
    fixtures::{self, Library},
    sports::{self, Sport},
    timefmt, Settings,
};
use serde_json::{Map, Value};
use tuliprox_plugin_sdk::prelude::*;

/// Marks text this plugin added, so a second pass over the same record changes nothing.
pub const MARKER: &str = "[Sports Enricher]";

/// The amended record, or `None` when this is not a sports title or is already enriched.
pub fn enrich(settings: &Settings, item: &Channel, metadata_json: &str) -> Result<Option<String>, String> {
    if !matches!(item.cluster, Cluster::Video) {
        return Ok(None);
    }
    let mut record: Value =
        serde_json::from_str(metadata_json).map_err(|err| format!("metadata is not JSON: {err}"))?;
    let Some(top) = record.as_object_mut() else { return Ok(None) };
    let Some(details) = top.get_mut("details").and_then(Value::as_object_mut) else { return Ok(None) };

    let plot = text(details, "plot");
    let description = text(details, "description");
    if plot.contains(MARKER) || description.contains(MARKER) {
        return Ok(None);
    }
    let genre = text(details, "genre");
    let haystack = [
        item.name.as_str(),
        item.title.as_str(),
        item.group.as_str(),
        genre.as_str(),
        plot.as_str(),
        description.as_str(),
    ]
    .join(" ");
    let Some(sport) = sports::classify_text(&haystack) else { return Ok(None) };

    let library = fixtures::shared(settings, false);
    let note = note(sport, &library, clock::now_unix());

    if !genre.to_lowercase().contains("sport") {
        let genre = if genre.trim().is_empty() { "Sports".to_string() } else { format!("{genre}, Sports") };
        details.insert("genre".to_string(), Value::String(genre));
    }
    details.insert("plot".to_string(), Value::String(append(&plot, &note)));
    if description.trim().is_empty() {
        details.insert("description".to_string(), Value::String(note));
    }
    serde_json::to_string(&record).map(Some).map_err(|err| err.to_string())
}

fn text(object: &Map<String, Value>, key: &str) -> String {
    object.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn append(base: &str, note: &str) -> String {
    if base.trim().is_empty() {
        note.to_string()
    } else {
        format!("{base}\n\n{note}")
    }
}

/// `[Sports Enricher] <emoji> <Sport> documentary.` plus the next match of that sport, if one is cached.
fn note(sport: Sport, library: &Library, now: i64) -> String {
    let mut note = format!("{MARKER} {} {} documentary.", sport.emoji(), sport.label());
    let next = library
        .fixtures()
        .filter(|(league, fixture)| {
            let id = if fixture.sport.is_empty() { &league.sport } else { &fixture.sport };
            id == sport.id() && fixture.end() >= now
        })
        .min_by(|(_, a), (_, b)| (a.kickoff, &a.id).cmp(&(b.kickoff, &b.id)));
    if let Some((league, fixture)) = next {
        note.push_str(&format!(
            " Next in {}: {}, {}.",
            league.name,
            fixture.title(),
            timefmt::format_utc(fixture.kickoff)
        ));
    }
    note
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        fixtures::testing::{cache, fixture, league},
        testing::{movie, settings},
    };
    use tuliprox_plugin_sdk::mock;

    fn record(genre: &str, plot: &str) -> String {
        serde_json::json!({
            "name": "Anfield Stories",
            "stream_id": 7,
            "rating": 7.5,
            "details": {"genre": genre, "plot": plot, "description": null, "cast": "x"},
        })
        .to_string()
    }

    fn amended(json: &str) -> Value { serde_json::from_str(json).unwrap() }

    #[test]
    fn a_sports_documentary_gets_genre_plot_and_description() {
        let settings = settings("{}");
        mock::set_now(1_793_491_200);
        cache(&[league(
            "epl",
            "football",
            &[],
            vec![fixture("e1", "Liverpool", "Arsenal", 1_793_491_200 + 75_900, &[])],
        )]);
        let item = movie("Anfield Stories: a football documentary", "Documentaries");
        let out = enrich(&settings, &item, &record("Documentary", "A season at the club.")).unwrap().unwrap();
        let value = amended(&out);
        let details = &value["details"];
        assert_eq!(details["genre"], "Documentary, Sports");
        assert_eq!(
            details["plot"],
            "A season at the club.\n\n[Sports Enricher] \u{26BD} Football documentary. Next in EPL: Liverpool vs Arsenal, 2026-11-01 21:05 UTC."
        );
        assert_eq!(details["description"], details["plot"].as_str().unwrap().split("\n\n").nth(1).unwrap());
        assert_eq!(details["cast"], "x", "other fields are untouched");
        assert_eq!(value["stream_id"], 7);
    }

    #[test]
    fn a_second_pass_a_non_sport_title_and_a_missing_details_block_change_nothing() {
        let settings = settings("{}");
        let item = movie("Anfield Stories: a football documentary", "Documentaries");
        let first = enrich(&settings, &item, &record("Sports", "Plot.")).unwrap().unwrap();
        assert_eq!(amended(&first)["details"]["genre"], "Sports", "already a sports genre");
        assert_eq!(enrich(&settings, &item, &first).unwrap(), None, "the marker stops a second pass");

        let cooking = movie("Pasta Masters", "Documentaries");
        assert_eq!(enrich(&settings, &cooking, &record("Documentary", "Noodles.")).unwrap(), None);

        let bare = serde_json::json!({"name": "Anfield Stories", "details": null}).to_string();
        assert_eq!(enrich(&settings, &item, &bare).unwrap(), None);
        assert!(enrich(&settings, &item, "not json").is_err());
    }

    #[test]
    fn only_vod_is_enriched_and_the_genre_may_be_missing() {
        let settings = settings("{}");
        let mut live = movie("Football Highlights", "Sports");
        live.cluster = Cluster::Live;
        assert_eq!(enrich(&settings, &live, &record("", "")).unwrap(), None);

        let item = movie("NBA Legends", "Documentaries");
        let out = enrich(&settings, &item, &record("", "")).unwrap().unwrap();
        let value = amended(&out);
        assert_eq!(value["details"]["genre"], "Sports");
        assert_eq!(value["details"]["plot"], "[Sports Enricher] \u{1F3C0} Basketball documentary.");
    }
}

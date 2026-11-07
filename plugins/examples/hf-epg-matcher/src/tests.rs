//! Unit tests on the SDK mock host (native target; the mock replaces the host).

use crate::{
    engine::{cache_key, fnv1a, match_requests, normalize, Options},
    plugin::Matcher,
};
use tuliprox_plugin_sdk::{http::Method, mock, prelude::*};

const ENDPOINT: &str = "http://mock.test/models/test/model";

fn options() -> Options {
    Options {
        model_id: "test/model".into(),
        token: "hf_secret".into(),
        threshold: 0.6,
        batch_size: 16,
        api_base_url: "http://mock.test/".into(),
    }
}

fn guide(id: &str, names: &[&str]) -> EpgChannel {
    EpgChannel { id: id.into(), names: names.iter().map(ToString::to_string).collect(), icon: None }
}

fn request(name: &str, candidates: Vec<EpgChannel>) -> MatchRequest {
    MatchRequest {
        chan: Channel {
            handle: 0,
            name: name.into(),
            title: name.into(),
            group: String::new(),
            url: String::new(),
            epg_id: None,
            logo: String::new(),
            chno: None,
            item_type: ItemType::Live,
            cluster: Cluster::Live,
            input_name: "in".into(),
            input_stream_id: "1".into(),
            extra: Vec::new(),
        },
        candidates,
    }
}

fn bbc() -> Vec<EpgChannel> { vec![guide("bbc.one", &["BBC One"]), guide("bbc.two", &["BBC Two"])] }

fn respond(body: &str) { mock::http_respond(Method::Post, ENDPOINT, 200, body); }

#[test]
fn the_best_candidate_above_the_threshold_wins_and_answers_stay_index_aligned() {
    mock::reset();
    respond("[0.2, 0.9]");
    respond("[0.1, 0.3]");
    let requests = vec![
        request("BBC Too", bbc()),
        request("No shortlist", Vec::new()),
        request("Unrelated", vec![guide("a", &["A"]), guide("b", &["B"])]),
    ];

    let answers = match_requests(&options(), &requests);

    assert_eq!(answers, vec![Some("bbc.two".to_string()), None, None]);
    assert_eq!(mock::http_requests().len(), 2, "a request without candidates costs no call");
}

#[test]
fn the_request_follows_the_sentence_similarity_shape_and_carries_the_bearer() {
    mock::reset();
    respond("[0.2, 0.9]");

    match_requests(&options(), &[request("BBC Too", bbc())]);

    let sent = mock::http_requests();
    assert_eq!(sent[0].method(), Method::Post);
    assert_eq!(sent[0].url(), ENDPOINT, "the trailing slash of the base url is dropped");
    assert!(sent[0].headers().contains(&("authorization".to_string(), "Bearer hf_secret".to_string())));
    let body: serde_json::Value = serde_json::from_slice(sent[0].body_bytes()).unwrap();
    assert_eq!(
        body,
        serde_json::json!({"inputs": {"source_sentence": "BBC Too", "sentences": ["BBC One", "BBC Two"]}})
    );
    assert!(mock::logs().iter().all(|line| !line.message.contains("hf_secret")));
}

#[test]
fn without_a_token_no_authorization_header_is_sent() {
    mock::reset();
    respond("[0.9]");
    let options = Options { token: String::new(), ..options() };

    match_requests(&options, &[request("BBC Too", vec![guide("bbc.two", &["BBC Two"])])]);

    assert!(mock::http_requests()[0].headers().iter().all(|(name, _)| name != "authorization"));
}

#[test]
fn a_cached_decision_skips_the_http_call() {
    mock::reset();
    respond("[0.2, 0.9]");
    let requests = [request("BBC Too", bbc())];

    let first = match_requests(&options(), &requests);
    assert_eq!(mock::http_requests().len(), 1);
    assert!(mock::kv_snapshot().contains_key(&cache_key("test/model", "bbc too")));

    // Nothing is queued any more, so a second call would fail: the answer must come from the KV.
    let second = match_requests(&options(), &[request("  bbc   TOO ", bbc())]);
    assert_eq!(first, vec![Some("bbc.two".to_string())]);
    assert_eq!(second, first);
    assert_eq!(mock::http_requests().len(), 1, "the normalized name hit the cache");
}

#[test]
fn a_different_shortlist_is_decided_again() {
    mock::reset();
    respond("[0.2, 0.9]");
    respond("[0.95]");
    match_requests(&options(), &[request("BBC Too", bbc())]);

    let answers = match_requests(&options(), &[request("BBC Too", vec![guide("bbc.too", &["BBC Too"])])]);

    assert_eq!(answers, vec![Some("bbc.too".to_string())]);
    assert_eq!(mock::http_requests().len(), 2);
}

#[test]
fn the_threshold_is_applied_when_a_decision_is_read() {
    mock::reset();
    respond("[0.1, 0.55]");
    let requests = [request("BBC Too", bbc())];

    assert_eq!(match_requests(&options(), &requests), vec![None], "0.55 is below 0.6");
    let lenient = Options { threshold: 0.5, ..options() };
    assert_eq!(match_requests(&lenient, &requests), vec![Some("bbc.two".to_string())]);
    let strict = Options { threshold: 0.9, ..options() };
    assert_eq!(match_requests(&strict, &requests), vec![None]);
    assert_eq!(mock::http_requests().len(), 1, "the best score was cached, not the verdict");
}

#[test]
fn a_score_exactly_at_the_threshold_is_accepted() {
    mock::reset();
    respond("[0.6]");

    let answers = match_requests(&options(), &[request("BBC Too", vec![guide("bbc.two", &["BBC Two"])])]);

    assert_eq!(answers, vec![Some("bbc.two".to_string())]);
}

#[test]
fn ties_go_to_the_earlier_candidate_and_a_candidate_scores_by_its_best_name() {
    mock::reset();
    respond("[0.8, 0.8]");
    respond("[0.3, 0.7, 0.7]");
    let tied = request("Tie", vec![guide("first", &["One"]), guide("second", &["Two"])]);
    let by_best_name = request("Names", vec![guide("a", &["Alpha", "Beta"]), guide("b", &["Gamma"])]);

    let answers = match_requests(&options(), &[tied, by_best_name]);

    assert_eq!(answers, vec![Some("first".to_string()), Some("a".to_string())]);
}

#[test]
fn a_candidate_without_names_is_scored_by_its_id() {
    mock::reset();
    respond("[0.9]");

    let answers = match_requests(&options(), &[request("Fox News", vec![guide("fox.news", &[])])]);

    assert_eq!(answers, vec![Some("fox.news".to_string())]);
    let body: serde_json::Value = serde_json::from_slice(mock::http_requests()[0].body_bytes()).unwrap();
    assert_eq!(body["inputs"]["sentences"], serde_json::json!(["fox.news"]));
}

#[test]
fn a_long_shortlist_is_split_into_batches() {
    mock::reset();
    respond("[0.1, 0.2]");
    respond("[0.9]");
    let options = Options { batch_size: 2, ..options() };
    let candidates = vec![guide("a", &["A1", "A2"]), guide("b", &["B1"])];

    let answers = match_requests(&options, &[request("B", candidates)]);

    assert_eq!(answers, vec![Some("b".to_string())]);
    let sent = mock::http_requests();
    assert_eq!(sent.len(), 2);
    let sizes: Vec<usize> = sent
        .iter()
        .map(|request| {
            let body: serde_json::Value = serde_json::from_slice(request.body_bytes()).unwrap();
            body["inputs"]["sentences"].as_array().unwrap().len()
        })
        .collect();
    assert_eq!(sizes, vec![2, 1]);
}

#[test]
fn an_api_that_is_down_fails_open_and_caches_nothing() {
    mock::reset();
    mock::http_fail(Method::Post, ENDPOINT, "connection refused");
    let requests = [request("BBC Too", bbc()), request("Other", bbc())];

    let answers = match_requests(&options(), &requests);

    assert_eq!(answers, vec![None, None]);
    assert_eq!(mock::http_requests().len(), 1, "the first failure stops the API use of the call");
    assert!(mock::kv_snapshot().is_empty(), "a failure is never cached");

    // The API is back: the same names are asked again.
    respond("[0.2, 0.9]");
    respond("[0.2, 0.9]");
    let again = match_requests(&options(), &requests);
    assert_eq!(again, vec![Some("bbc.two".to_string()), Some("bbc.two".to_string())]);
}

#[test]
fn an_error_status_or_a_malformed_answer_also_fails_open() {
    mock::reset();
    mock::http_respond(Method::Post, ENDPOINT, 503, r#"{"error":"Model is loading"}"#);
    assert_eq!(match_requests(&options(), &[request("BBC Too", bbc())]), vec![None]);

    mock::http_respond(Method::Post, ENDPOINT, 200, "[0.9]");
    assert_eq!(match_requests(&options(), &[request("BBC Too", bbc())]), vec![None], "one score for two sentences");

    mock::http_respond(Method::Post, ENDPOINT, 200, r#"{"unexpected":true}"#);
    assert_eq!(match_requests(&options(), &[request("BBC Too", bbc())]), vec![None]);
    assert!(mock::kv_snapshot().is_empty());
}

#[test]
fn an_unavailable_kv_is_survivable() {
    mock::reset();
    mock::set_kv_unavailable("disk full");
    respond("[0.2, 0.9]");

    let answers = match_requests(&options(), &[request("BBC Too", bbc())]);

    assert_eq!(answers, vec![Some("bbc.two".to_string())]);
}

#[test]
fn a_name_that_normalizes_to_nothing_is_not_asked() {
    mock::reset();

    let answers = match_requests(&options(), &[request("  ---  ", bbc())]);

    assert_eq!(answers, vec![None]);
    assert!(mock::http_requests().is_empty());
}

#[test]
fn names_normalize_and_hash_deterministically() {
    assert_eq!(normalize("  BBC   Too! "), "bbc too");
    assert_eq!(normalize("TF1+1 HD"), "tf1 1 hd");
    assert_eq!(normalize("Zürich TV"), "zürich tv");
    assert_eq!(fnv1a(b"bbc too"), 0xd8df_27a5_8fd7_980c);
    assert_eq!(cache_key("m", "bbc too"), "match:m:d8df27a58fd7980c");
}

#[test]
fn the_hook_reads_its_settings_from_the_host_and_applies_the_defaults() {
    mock::reset();
    mock::set_config(r#"{"hf_token":"tok-1","api_base_url":"http://mock.test"}"#);
    mock::http_respond(
        Method::Post,
        "http://mock.test/models/sentence-transformers/all-MiniLM-L6-v2",
        200,
        "[0.2, 0.9]",
    );

    let answers = Matcher::match_channels(vec![request("BBC Too", bbc())]).unwrap();

    assert_eq!(answers, vec![Some("bbc.two".to_string())]);
    let sent = mock::http_requests();
    assert!(sent[0].headers().contains(&("authorization".to_string(), "Bearer tok-1".to_string())));
}

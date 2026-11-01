//! The matching logic, free of the plugin glue so the tests can drive it.

use serde::{Deserialize, Serialize};
use std::time::Duration;
use tuliprox_plugin_sdk::prelude::*;

pub(crate) const DEFAULT_MODEL_ID: &str = "sentence-transformers/all-MiniLM-L6-v2";
pub(crate) const DEFAULT_THRESHOLD: f64 = 0.6;
pub(crate) const DEFAULT_BATCH_SIZE: u32 = 16;
pub(crate) const DEFAULT_API_BASE_URL: &str = "http://127.0.0.1:18971";

/// One request must finish well inside the host's per-call deadline (5 s by default).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(4);

/// The settings the engine works with.
#[derive(Debug, Clone)]
pub(crate) struct Options {
    pub(crate) model_id: String,
    pub(crate) token: String,
    pub(crate) threshold: f64,
    pub(crate) batch_size: u32,
    pub(crate) api_base_url: String,
}

impl Options {
    /// The threshold, kept inside 0..=1 whatever the settings say.
    fn threshold(&self) -> f64 {
        if self.threshold.is_finite() {
            self.threshold.clamp(0.0, 1.0)
        } else {
            DEFAULT_THRESHOLD
        }
    }

    /// Candidate names per HTTP request, kept inside 1..=64.
    fn batch_size(&self) -> usize { usize::try_from(self.batch_size.clamp(1, 64)).unwrap_or(1) }

    fn endpoint(&self) -> String {
        format!("{}/models/{}", self.api_base_url.trim_end_matches('/'), self.model_id.trim_matches('/'))
    }
}

/// What is stored per channel name: the best candidate the model found and how
/// well it scored, whatever the threshold was. The threshold is applied when the
/// entry is read, so raising or lowering it needs no new model call. `id` is
/// `None` (the stored "none") when no candidate had a name to score.
///
/// `shortlist` fingerprints the candidates the decision was made over; an entry
/// made over another shortlist is not trusted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Decision {
    id: Option<String>,
    score: f64,
    shortlist: String,
}

impl Decision {
    /// The candidate id this decision answers at `threshold`, if any.
    fn answer(&self, threshold: f64) -> Option<String> {
        let score = if self.score.is_finite() { self.score } else { 0.0 };
        self.id.as_ref().filter(|_| score >= threshold).cloned()
    }
}

#[derive(Default)]
struct Tally {
    hits: usize,
    calls: usize,
    matched: usize,
    undecided: usize,
}

/// Answers every request, index-aligned: the guide channel id the model picked
/// from `candidates`, or `None`.
pub(crate) fn match_requests(options: &Options, requests: &[MatchRequest]) -> Vec<Option<String>> {
    let threshold = options.threshold();
    let mut answers: Vec<Option<String>> = Vec::with_capacity(requests.len());
    let mut tally = Tally::default();
    let mut api_failed = false;

    for request in requests {
        let name = request.chan.name.trim();
        let normalized = normalize(name);
        if normalized.is_empty() || request.candidates.is_empty() {
            answers.push(None);
            continue;
        }
        let key = cache_key(&options.model_id, &normalized);
        let shortlist = shortlist_fingerprint(&request.candidates);

        if let Some(decision) = cached(&key, &shortlist) {
            tally.hits += 1;
            let answer = decision.answer(threshold);
            tally.matched += usize::from(answer.is_some());
            answers.push(answer);
            continue;
        }
        if api_failed {
            // One failure ends the API use of this call: the rest would fail the same
            // way, slowly. Nothing is cached, so the next run asks again.
            tally.undecided += 1;
            answers.push(None);
            continue;
        }

        tally.calls += 1;
        match best_candidate(options, name, &request.candidates) {
            Ok((id, score)) => {
                let decision = Decision { id, score, shortlist };
                if let Err(err) = kv::set_json(&key, &decision) {
                    warn!("match-channels: cannot cache the decision for `{name}`: {err}");
                }
                let answer = decision.answer(threshold);
                tally.matched += usize::from(answer.is_some());
                answers.push(answer);
            }
            Err(err) => {
                warn!("match-channels: the model API failed ({err}); the remaining uncached channels stay unmatched");
                api_failed = true;
                tally.undecided += 1;
                answers.push(None);
            }
        }
    }

    info!(
        "match-channels: {} requests, {} cache hits, {} model calls, {} matched, {} undecided",
        requests.len(),
        tally.hits,
        tally.calls,
        tally.matched,
        tally.undecided
    );
    answers
}

/// A readable cached decision for `key` made over the same shortlist, or `None`
/// (an absent, unreadable, foreign or unavailable entry is a miss).
fn cached(key: &str, shortlist: &str) -> Option<Decision> {
    match kv::get_json::<Decision>(key) {
        Ok(Some(decision)) if decision.shortlist == shortlist => Some(decision),
        Ok(_) => None,
        Err(err) => {
            warn!("match-channels: cannot read the decision cache ({err}); asking the model");
            None
        }
    }
}

/// The best candidate id and its score. Candidates are scored by their best
/// name; ties keep the earlier candidate. Errors when any request fails.
fn best_candidate(options: &Options, name: &str, candidates: &[EpgChannel]) -> Result<(Option<String>, f64), String> {
    // Every sentence sent to the model, with the candidate it belongs to.
    let mut sentences: Vec<(usize, &str)> = Vec::new();
    for (index, candidate) in candidates.iter().enumerate() {
        let before = sentences.len();
        sentences.extend(
            candidate.names.iter().map(|name| name.trim()).filter(|name| !name.is_empty()).map(|name| (index, name)),
        );
        if sentences.len() == before && !candidate.id.trim().is_empty() {
            sentences.push((index, candidate.id.trim()));
        }
    }
    if sentences.is_empty() {
        return Ok((None, 0.0));
    }

    let mut per_candidate: Vec<Option<f64>> = vec![None; candidates.len()];
    for chunk in sentences.chunks(options.batch_size()) {
        let texts: Vec<&str> = chunk.iter().map(|(_, text)| *text).collect();
        let scores = similarity(options, name, &texts)?;
        for ((index, _), score) in chunk.iter().zip(scores) {
            let slot = &mut per_candidate[*index];
            if slot.is_none_or(|best| score > best) {
                *slot = Some(score);
            }
        }
    }

    let mut best: Option<(usize, f64)> = None;
    for (index, score) in per_candidate.iter().enumerate() {
        if let Some(score) = *score {
            if best.is_none_or(|(_, top)| score > top) {
                best = Some((index, score));
            }
        }
    }
    Ok(best.map_or((None, 0.0), |(index, score)| (Some(candidates[index].id.clone()), score)))
}

/// One sentence-similarity call: a score per sentence, in order.
fn similarity(options: &Options, source: &str, sentences: &[&str]) -> Result<Vec<f64>, String> {
    let body = serde_json::json!({ "inputs": { "source_sentence": source, "sentences": sentences } });
    let mut request =
        http::Request::post(options.endpoint()).header("accept", "application/json").timeout(REQUEST_TIMEOUT);
    if !options.token.is_empty() {
        request = request.bearer(&options.token);
    }
    let scores: Vec<f64> = request.json(&body)?.send()?.error_for_status()?.json()?;
    if scores.len() != sentences.len() {
        return Err(format!("the model returned {} scores for {} sentences", scores.len(), sentences.len()));
    }
    Ok(scores.into_iter().map(|score| if score.is_finite() { score } else { 0.0 }).collect())
}

/// Lowercase, every run of non-alphanumeric characters as one space, trimmed.
pub(crate) fn normalize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut pending_space = false;
    for ch in name.chars() {
        if ch.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.extend(ch.to_lowercase());
        } else {
            pending_space = true;
        }
    }
    out
}

/// FNV-1a, 64 bit: stable across runs, builds and targets (unlike `RandomState`).
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3))
}

/// `match:<model>:<hash of the normalized channel name>`.
pub(crate) fn cache_key(model_id: &str, normalized_name: &str) -> String {
    format!("match:{model_id}:{:016x}", fnv1a(normalized_name.as_bytes()))
}

/// The candidate ids in shortlist order (the order is part of the decision: it
/// breaks ties).
fn shortlist_fingerprint(candidates: &[EpgChannel]) -> String {
    let mut bytes = Vec::new();
    for candidate in candidates {
        bytes.extend(candidate.id.to_ascii_lowercase().bytes());
        bytes.push(0);
    }
    format!("{:016x}", fnv1a(&bytes))
}

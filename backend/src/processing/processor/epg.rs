use crate::{
    model::{Epg, EpgConfig, EpgSmartMatchConfig},
    repository::FetchedPlaylist,
    processing::parser::xmltv::normalize_channel_name,
    utils::with_folded_epg_id,
};
use log::{debug, trace, warn};
use rphonetic::{DoubleMetaphone, Encoder};
use shared::{
    model::{EpgNamePrefix, EpgSmartMatchConfigDto, PlaylistItem, XtreamCluster},
    utils::{CONSTANTS, Internable},
};
use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    sync::Arc,
};

const MIN_FUZZY_SCORE_MARGIN: u16 = 3;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum SmartMatchKind {
    Fuzzy,
    Exact,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SmartMatchRank {
    kind: SmartMatchKind,
    score: u16,
    source_priority: i16,
    source_order: usize,
}

#[derive(Clone, Debug)]
struct RankedEpgMatch {
    epg_id: Arc<str>,
    rank: SmartMatchRank,
}

impl SmartMatchRank {
    fn cmp_preference(&self, other: &Self) -> Ordering {
        self.kind
            .cmp(&other.kind)
            .then_with(|| self.score.cmp(&other.score))
            .then_with(|| other.source_priority.cmp(&self.source_priority))
            .then_with(|| other.source_order.cmp(&self.source_order))
    }
}

#[derive(Debug)]
struct FuzzyCandidate {
    key: Arc<str>,
    score: u16,
}

#[derive(Debug, Default)]
struct MatchRequirement {
    missing_id: bool,
    direct_ids: HashSet<Arc<str>>,
}

pub struct EpgIdCache {
    /// Membership set of the epg ids collected from playlist channels.
    /// Keys are stored ASCII-lowercased so guide-id matching is case-insensitive.
    /// This set is used only for membership tests, never to source an emitted/display
    /// id, so folding the stored key does not affect output (channels keep their
    /// original-case id). Insert via [`Self::insert_channel_epg_id`] and query via
    /// [`Self::contains_channel_epg_id`] to keep the fold consistent.
    pub channel_epg_id: HashSet<Arc<str>>,
    pub normalized: HashMap<Arc<str>, Option<Arc<str>>>,
    pub phonetics: HashMap<Arc<str>, HashSet<Arc<str>>>,
    prefixes: HashMap<u32, HashSet<Arc<str>>>,
    cores: HashMap<Arc<str>, HashSet<Arc<str>>>,
    match_ranks: HashMap<Arc<str>, SmartMatchRank>,
    match_candidates: HashMap<Arc<str>, HashMap<Arc<str>, RankedEpgMatch>>,
    match_requirements: HashMap<Arc<str>, MatchRequirement>,
    guide_names: HashMap<Arc<str>, HashSet<Arc<str>>>,
    pub processed: HashSet<Arc<str>>,
    pub smart_match_config: EpgSmartMatchConfig,
    pub metaphone: DoubleMetaphone,
    pub smart_match_enabled: bool, // smart match is enabled, normalizing names
    pub fuzzy_match_enabled: bool, // fuzzy matching enabled
}

impl EpgIdCache {
    /// Creates a new `EpgIdCache` with configuration for smart and fuzzy matching.
    ///
    /// Initializes all internal caches and sets matching options based on the provided EPG configuration. If no configuration is given, defaults are used.
    ///
    /// # Examples
    ///
    /// ```
    /// let cache = EpgIdCache::new(None);
    /// assert!(cache.is_empty());
    /// ```
    pub fn new(epg_config: Option<&EpgConfig>) -> Self {
        let normalize_config: EpgSmartMatchConfig = epg_config
            .and_then(|cfg| cfg.smart_match.clone())
            .unwrap_or_else(|| EpgSmartMatchConfigDto::default().into());

        EpgIdCache {
            channel_epg_id: HashSet::new(), // contains the epg_ids collected from playlist channels
            normalized: HashMap::new(),
            phonetics: HashMap::new(),
            prefixes: HashMap::new(),
            cores: HashMap::new(),
            match_ranks: HashMap::new(),
            match_candidates: HashMap::new(),
            match_requirements: HashMap::new(),
            guide_names: HashMap::new(),
            processed: HashSet::new(),
            metaphone: DoubleMetaphone::default(),
            smart_match_enabled: normalize_config.enabled,
            fuzzy_match_enabled: normalize_config.enabled && normalize_config.fuzzy_matching,
            smart_match_config: normalize_config,
        }
    }

    fn is_empty(&self) -> bool { self.channel_epg_id.is_empty() && self.normalized.is_empty() }

    /// Adds an epg id to the case-folded membership set.
    ///
    /// The key is ASCII-lowercased so a `MixedCase` source id matches a guide
    /// `<channel id>` of a different case. ASCII folding (not Unicode) avoids
    /// Turkish-i/locale issues and is faster; channel ids are ASCII.
    pub fn insert_channel_epg_id(&mut self, id: &str) {
        with_folded_epg_id(id, |folded| self.channel_epg_id.insert(folded.intern()));
    }

    /// Case-insensitive (ASCII) membership test against the folded keys stored by
    /// [`Self::insert_channel_epg_id`].
    pub fn contains_channel_epg_id(&self, id: &str) -> bool {
        with_folded_epg_id(id, |folded| self.channel_epg_id.contains(folded))
    }

    pub(crate) fn needs_guide_names(&self, id: &str) -> bool { self.contains_channel_epg_id(id) }

    pub fn insert_processed_epg_id(&mut self, id: &str) {
        with_folded_epg_id(id, |folded| self.processed.insert(folded.intern()));
    }

    pub fn contains_processed_epg_id(&self, id: &str) -> bool {
        with_folded_epg_id(id, |folded| self.processed.contains(folded))
    }

    pub fn selected_epg_ids(&self, available_epg_ids: &HashSet<Arc<str>>) -> HashSet<Arc<str>> {
        let mut selected = self.channel_epg_id.intersection(available_epg_ids).cloned().collect::<HashSet<_>>();
        selected.extend(self.normalized.iter().filter_map(|(key, id)| {
            let requirement = self.match_requirements.get(key)?;
            let needs_smart_match = requirement.missing_id
                || requirement.direct_ids.iter().any(|direct_id| {
                    !available_epg_ids.contains(direct_id)
                        || self.normalized_id_name_compatible(direct_id, key).is_some_and(|compatible| !compatible)
                });
            needs_smart_match.then(|| id.as_ref().map(|id| with_folded_epg_id(id, |folded| folded.intern()))).flatten()
        }));
        selected
    }

    pub fn replace_processed_epg_ids(&mut self, ids: HashSet<Arc<str>>) { self.processed = ids; }

    pub fn register_guide_names<I, S>(&mut self, epg_id: &str, names: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let normalized_names = names
            .into_iter()
            .map(|name| self.normalize(name.as_ref()).intern())
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>();
        if normalized_names.is_empty() {
            return;
        }
        let folded_id = with_folded_epg_id(epg_id, |folded| folded.intern());
        self.guide_names.entry(folded_id).or_default().extend(normalized_names);
    }

    pub fn finalize_matches(&mut self, available_epg_ids: &HashSet<Arc<str>>) {
        self.match_ranks.clear();
        let mut exact = 0usize;
        let mut fuzzy = 0usize;
        let mut ambiguous = 0usize;
        let mut unavailable = 0usize;
        for (key, entry) in &mut self.normalized {
            let candidates = self.match_candidates.get(key);
            let available_count = candidates.map_or(0, |candidates| {
                candidates.keys().filter(|folded_id| available_epg_ids.contains(*folded_id)).count()
            });
            let winner =
                candidates.and_then(|candidates| {
                    best_ranked_match(candidates.iter().filter_map(|(folded_id, candidate)| {
                        available_epg_ids.contains(folded_id).then_some(candidate)
                    }))
                });
            if let Some(winner) = winner {
                entry.replace(Arc::clone(&winner.epg_id));
                self.match_ranks.insert(Arc::clone(key), winner.rank);
                match winner.rank.kind {
                    SmartMatchKind::Exact => exact += 1,
                    SmartMatchKind::Fuzzy => fuzzy += 1,
                }
            } else {
                entry.take();
                if available_count > 0 {
                    ambiguous += 1;
                } else {
                    unavailable += 1;
                }
            }
        }
        debug!("Smart EPG candidates: exact={exact}, fuzzy={fuzzy}, ambiguous={ambiguous}, unavailable={unavailable}");
    }

    /// Normalizes a channel name, computes its phonetic encoding, and stores both in the cache for later EPG matching.
    ///
    /// The normalized name is mapped to the provided EPG ID (if any), and the phonetic encoding is added to the phonetics map.
    /// This facilitates efficient lookup and fuzzy matching of channel names during EPG assignment.
    ///
    /// # Examples
    ///
    /// ```
    /// let mut cache = EpgIdCache::new(None);
    /// cache.normalize_and_store("Discovery Channel", Some(&"discovery.epg".to_string()));
    /// assert!(cache.normalized.contains_key(&cache.normalize("Discovery Channel")));
    /// ```
    fn normalize_and_store(&mut self, name: &str, epg_id: Option<&Arc<str>>) {
        self.insert_normalized(name, epg_id);

        if let Some(chan_epg_id) = epg_id {
            self.insert_normalized(chan_epg_id, epg_id);
        }
    }
    fn insert_normalized(&mut self, key: &str, direct_epg_id: Option<&Arc<str>>) {
        let normalized = self.normalize(key).intern();
        if normalized.is_empty() {
            return;
        }
        let phonetic = self.phonetic(&normalized);

        self.normalized.entry(normalized.clone()).or_insert(None);
        let requirement = self.match_requirements.entry(normalized.clone()).or_default();
        if let Some(id) = direct_epg_id.filter(|id| !id.is_empty()) {
            requirement.direct_ids.insert(with_folded_epg_id(id, |folded| folded.intern()));
        } else {
            requirement.missing_id = true;
        }
        self.phonetics.entry(phonetic).or_default().insert(normalized.clone());
        if let Some(prefix) = candidate_prefix(&normalized) {
            self.prefixes.entry(prefix).or_default().insert(normalized.clone());
        }
        let (core, _) = self.split_country(&normalized);
        self.cores.entry(core.intern()).or_default().insert(normalized);
    }

    /// Returns the normalized form of a channel name using the configured smart match settings.
    ///
    /// # Examples
    ///
    /// ```
    /// let cache = EpgIdCache::new(None);
    /// let normalized = cache.normalize("HBO HD");
    /// assert!(!normalized.is_empty());
    /// ```
    fn normalize(&self, name: &str) -> String { normalize_channel_name(name, &self.smart_match_config) }

    pub(crate) fn phonetic(&self, name: &Arc<str>) -> Arc<str> {
        let result = self.metaphone.encode(name);
        if result.is_empty() { name.clone() } else { result.intern() }
    }

    pub fn collect_epg_id(&mut self, fp: &mut FetchedPlaylist) {
        let smart_match_enabled = self.smart_match_enabled;
        let fuzzy_matching = self.fuzzy_match_enabled;

        // Helper closure to process a single item
        // We use a closure here to capture `self` and avoid code duplication
        let mut process_item = |name: &str, epg_channel_id: Option<&Arc<str>>| {
            let mut missing_epg_id = true;
            // insert epg_id to known channel epg_ids
            if let Some(id) = epg_channel_id {
                if !id.is_empty() {
                    missing_epg_id = false;
                    self.insert_channel_epg_id(id);
                }
            }

            // for fuzzy_matching we need to put the normalized name even if there is an epg_id, because the epg_id
            // could not match to the epg file. And then we try to guess it based on normalized name
            let needs_normalization =
                smart_match_enabled && (fuzzy_matching || missing_epg_id) && !is_decorative_channel_name(name);

            if needs_normalization {
                self.normalize_and_store(name, epg_channel_id);
            }
        };

        for channel in fp.items() {
            if channel.header.xtream_cluster == XtreamCluster::Live && channel.header.item_type.is_live() {
                process_item(&channel.header.name, channel.header.epg_channel_id.as_ref());
            }
        }
    }

    pub fn normalize_candidates<I, S>(&self, candidates: I) -> Vec<Arc<str>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        candidates
            .into_iter()
            .map(|candidate| candidate.as_ref().trim().to_string())
            .filter(|candidate| !candidate.is_empty())
            .map(|candidate| self.normalize(&candidate).intern())
            .filter(|candidate| !candidate.is_empty())
            .collect()
    }

    pub fn match_epg_channel_candidates(
        &mut self,
        epg_id: &Arc<str>,
        normalized_candidates: &[Arc<str>],
        source_priority: i16,
        source_order: usize,
    ) -> bool {
        let normalized_epg_id = self.normalize(epg_id);
        let (_, id_country) = self.split_country(&normalized_epg_id);
        let guide_country = id_country.or_else(|| self.common_country(normalized_candidates));
        let mut exact_candidate_found = false;
        let mut selected = false;

        for key in normalized_candidates {
            if self.normalized.contains_key(key) {
                let (_, playlist_country) = self.split_country(key);
                if !countries_compatible(playlist_country, guide_country) {
                    continue;
                }
                exact_candidate_found = true;
                selected |= self.assign_match(
                    key,
                    epg_id,
                    SmartMatchRank { kind: SmartMatchKind::Exact, score: 100, source_priority, source_order },
                );
            }
        }

        let mut relaxed_keys = HashSet::new();
        for guide_key in normalized_candidates {
            let (guide_core, candidate_country) = self.split_country(guide_key);
            let effective_guide_country = guide_country.or(candidate_country);
            if let Some(keys) = self.cores.get(guide_core) {
                relaxed_keys.extend(
                    keys.iter()
                        .filter(|key| {
                            let (_, playlist_country) = self.split_country(key);
                            countries_compatible(playlist_country, effective_guide_country)
                        })
                        .cloned(),
                );
            }
        }
        for key in relaxed_keys {
            exact_candidate_found = true;
            selected |= self.assign_match(
                &key,
                epg_id,
                SmartMatchRank { kind: SmartMatchKind::Exact, score: 99, source_priority, source_order },
            );
        }

        if exact_candidate_found || !self.fuzzy_match_enabled {
            return selected;
        }

        let Some(candidate) = self.find_best_fuzzy_match(normalized_candidates, guide_country) else {
            return false;
        };
        let rank =
            SmartMatchRank { kind: SmartMatchKind::Fuzzy, score: candidate.score, source_priority, source_order };
        self.assign_match(&candidate.key, epg_id, rank)
    }

    fn assign_match(&mut self, key: &Arc<str>, epg_id: &Arc<str>, rank: SmartMatchRank) -> bool {
        if !self.normalized.contains_key(key) {
            return false;
        }
        let folded_id = with_folded_epg_id(epg_id, |folded| folded.intern());
        let candidates = self.match_candidates.entry(Arc::clone(key)).or_default();
        match candidates.entry(folded_id) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if rank.cmp_preference(&entry.get().rank).is_gt() {
                    entry.insert(RankedEpgMatch { epg_id: Arc::clone(epg_id), rank });
                }
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(RankedEpgMatch { epg_id: Arc::clone(epg_id), rank });
            }
        }

        let winner = candidates.values().max_by(|left, right| compare_ranked_matches(left, right));
        if let Some(winner) = winner {
            self.normalized.get_mut(key).expect("normalized key exists").replace(Arc::clone(&winner.epg_id));
            self.match_ranks.insert(Arc::clone(key), winner.rank);
        }
        true
    }

    fn find_best_fuzzy_match(
        &self,
        normalized_candidates: &[Arc<str>],
        guide_country: Option<&str>,
    ) -> Option<FuzzyCandidate> {
        let mut phonetic_keys = HashSet::new();
        for candidate in normalized_candidates {
            if let Some(keys) = self.phonetics.get(&self.phonetic(candidate)) {
                phonetic_keys.extend(keys.iter().cloned());
            }
        }
        if let Some(best) = self.select_unambiguous_candidate(
            normalized_candidates,
            phonetic_keys.iter(),
            self.smart_match_config.match_threshold,
            guide_country,
        ) {
            return Some(best);
        }

        let mut fallback_keys = HashSet::new();
        for candidate in normalized_candidates {
            if let Some(prefix) = candidate_prefix(candidate) {
                if let Some(keys) = self.prefixes.get(&prefix) {
                    fallback_keys.extend(keys.iter().cloned());
                }
            }
        }
        self.select_unambiguous_candidate(
            normalized_candidates,
            fallback_keys.iter(),
            self.smart_match_config.best_match_threshold,
            guide_country,
        )
    }

    fn select_unambiguous_candidate<'a, I>(
        &self,
        guide_candidates: &[Arc<str>],
        playlist_candidates: I,
        threshold: u16,
        guide_country: Option<&str>,
    ) -> Option<FuzzyCandidate>
    where
        I: IntoIterator<Item = &'a Arc<str>>,
    {
        let mut best: Option<FuzzyCandidate> = None;
        let mut runner_up: Option<FuzzyCandidate> = None;
        for playlist_key in playlist_candidates {
            let (_, playlist_country) = self.split_country(playlist_key);
            let Some(score) = guide_candidates
                .iter()
                .filter(|guide_key| {
                    numeric_signature_matches(playlist_key, guide_key)
                        && countries_compatible(playlist_country, guide_country)
                        && self.normalized_countries_compatible(playlist_key, guide_key)
                })
                .map(|guide_key| similarity_score(playlist_key, guide_key))
                .max()
            else {
                continue;
            };
            let candidate = FuzzyCandidate { key: Arc::clone(playlist_key), score };
            if best.as_ref().is_none_or(|current| candidate_precedes(&candidate, current)) {
                runner_up = best.replace(candidate);
            } else if runner_up.as_ref().is_none_or(|current| candidate_precedes(&candidate, current)) {
                runner_up = Some(candidate);
            }
        }

        let best = best?;
        if best.score < threshold {
            return None;
        }
        if let Some(runner_up) = runner_up {
            if best.score == runner_up.score {
                return None;
            }
            if best.score < self.smart_match_config.best_match_threshold
                && best.score.saturating_sub(runner_up.score) < MIN_FUZZY_SCORE_MARGIN
            {
                return None;
            }
        }
        Some(best)
    }

    fn split_country<'a>(&self, value: &'a str) -> (&'a str, Option<&'a str>) {
        let split = match &self.smart_match_config.name_prefix {
            EpgNamePrefix::Suffix(separator) if !separator.is_empty() => value.rsplit_once(separator),
            EpgNamePrefix::Prefix(separator) if !separator.is_empty() => {
                value.split_once(separator).map(|(country, core)| (core, country))
            }
            EpgNamePrefix::Ignore | EpgNamePrefix::Suffix(_) | EpgNamePrefix::Prefix(_) => None,
        };
        split
            .filter(|(_, country)| CONSTANTS.country_codes.contains(country))
            .map_or((value, None), |(core, country)| (core, Some(country)))
    }

    fn common_country<'a>(&self, candidates: &'a [Arc<str>]) -> Option<&'a str> {
        let mut country = None;
        for candidate in candidates {
            let (_, candidate_country) = self.split_country(candidate);
            if let Some(candidate_country) = candidate_country {
                if country.is_some_and(|current| current != candidate_country) {
                    return None;
                }
                country = Some(candidate_country);
            }
        }
        country
    }

    fn normalized_countries_compatible(&self, left: &str, right: &str) -> bool {
        let (_, left_country) = self.split_country(left);
        let (_, right_country) = self.split_country(right);
        countries_compatible(left_country, right_country)
    }

    fn normalized_id_name_compatible(&self, folded_epg_id: &str, playlist_name: &str) -> Option<bool> {
        let guide_names = self.guide_names.get(folded_epg_id)?;
        let (playlist_core, playlist_country) = self.split_country(playlist_name);
        let best_score = guide_names
            .iter()
            .filter_map(|guide_name| {
                let (guide_core, guide_country) = self.split_country(guide_name);
                (countries_compatible(playlist_country, guide_country)
                    && numeric_signature_matches(playlist_core, guide_core))
                .then(|| similarity_score(playlist_core, guide_core))
            })
            .max();
        Some(best_score.is_some_and(|score| score >= self.smart_match_config.match_threshold))
    }

    fn channel_name_matches_epg_id(&self, epg_id: &str, channel_name: &str) -> Option<bool> {
        let folded_id = with_folded_epg_id(epg_id, |folded| folded.intern());
        let normalized_name = self.normalize(channel_name);
        self.normalized_id_name_compatible(&folded_id, &normalized_name)
    }

    fn matched_epg_id(&self, key: &str) -> Option<(Arc<str>, SmartMatchKind)> {
        let normalized = self.normalize(key).intern();
        let epg_id = self.normalized.get(&normalized)?.as_ref()?;
        let kind = self.match_ranks.get(&normalized)?.kind;
        Some((Arc::clone(epg_id), kind))
    }
}

fn candidate_precedes(left: &FuzzyCandidate, right: &FuzzyCandidate) -> bool {
    left.score > right.score || (left.score == right.score && left.key < right.key)
}

fn compare_ranked_matches(left: &RankedEpgMatch, right: &RankedEpgMatch) -> Ordering {
    left.rank.cmp_preference(&right.rank).then_with(|| right.epg_id.cmp(&left.epg_id))
}

fn best_ranked_match<'a, I>(candidates: I) -> Option<&'a RankedEpgMatch>
where
    I: IntoIterator<Item = &'a RankedEpgMatch>,
{
    let mut best = None;
    let mut ambiguous = false;
    for candidate in candidates {
        let Some(current) = best else {
            best = Some(candidate);
            continue;
        };
        match candidate.rank.cmp_preference(&current.rank) {
            Ordering::Greater => {
                best = Some(candidate);
                ambiguous = false;
            }
            Ordering::Equal if !candidate.epg_id.eq_ignore_ascii_case(&current.epg_id) => ambiguous = true,
            Ordering::Equal | Ordering::Less => {}
        }
    }
    (!ambiguous).then_some(best).flatten()
}

fn countries_compatible(left: Option<&str>, right: Option<&str>) -> bool {
    left.zip(right).is_none_or(|(left, right)| left == right)
}

fn is_decorative_channel_name(name: &str) -> bool {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return false;
    }
    if !trimmed.chars().any(char::is_alphanumeric) {
        return true;
    }
    let separator = trimmed.chars().next().expect("trimmed name is not empty");
    if separator.is_alphanumeric() {
        return false;
    }
    trimmed.chars().take_while(|&character| character == separator).count() >= 2
        && trimmed.chars().rev().take_while(|&character| character == separator).count() >= 2
}

fn candidate_prefix(value: &str) -> Option<u32> {
    let mut prefix = 0u32;
    let mut len = 0u32;
    for byte in value.bytes().filter(u8::is_ascii_alphanumeric).take(3) {
        prefix = (prefix << 8) | u32::from(byte.to_ascii_lowercase());
        len += 1;
    }
    (len > 0).then_some((len << 24) | prefix)
}

fn numeric_signature_matches(left: &str, right: &str) -> bool {
    left.bytes().filter(u8::is_ascii_digit).eq(right.bytes().filter(u8::is_ascii_digit))
}

fn similarity_score(left: &str, right: &str) -> u16 {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let score = (strsim::jaro_winkler(left, right) * 100.0).round() as u16;
    score.min(100)
}

#[derive(Clone, Copy, Debug)]
enum ChannelEpgAssignment {
    Existing,
    Exact { corrected: bool },
    Fuzzy { corrected: bool },
    Unresolved,
}

#[derive(Debug, Default)]
struct EpgAssignmentStats {
    live: usize,
    existing: usize,
    exact: usize,
    fuzzy: usize,
    corrected: usize,
    unresolved: usize,
}

impl EpgAssignmentStats {
    fn record(&mut self, assignment: ChannelEpgAssignment) {
        self.live += 1;
        match assignment {
            ChannelEpgAssignment::Existing => self.existing += 1,
            ChannelEpgAssignment::Exact { corrected } => {
                self.exact += 1;
                self.corrected += usize::from(corrected);
            }
            ChannelEpgAssignment::Fuzzy { corrected } => {
                self.fuzzy += 1;
                self.corrected += usize::from(corrected);
            }
            ChannelEpgAssignment::Unresolved => self.unresolved += 1,
        }
    }
}

fn assign_smart_epg_id(chan: &mut PlaylistItem, id_cache: &EpgIdCache) -> ChannelEpgAssignment {
    let current_id_is_valid =
        chan.header.epg_channel_id.as_deref().is_some_and(|epg_id| id_cache.contains_processed_epg_id(epg_id));
    let current_id_is_incompatible = current_id_is_valid
        && chan
            .header
            .epg_channel_id
            .as_deref()
            .is_some_and(|epg_id| id_cache.channel_name_matches_epg_id(epg_id, &chan.header.name) == Some(false));
    if current_id_is_valid && !current_id_is_incompatible {
        return ChannelEpgAssignment::Existing;
    }

    let matched = id_cache
        .matched_epg_id(&chan.header.name)
        .or_else(|| chan.header.epg_channel_id.as_deref().and_then(|id| id_cache.matched_epg_id(id)));
    let Some((new_id, kind)) = matched else {
        return if current_id_is_valid { ChannelEpgAssignment::Existing } else { ChannelEpgAssignment::Unresolved };
    };
    let changes_id =
        chan.header.epg_channel_id.as_deref().is_none_or(|current_id| !current_id.eq_ignore_ascii_case(&new_id));
    if !changes_id {
        return ChannelEpgAssignment::Existing;
    }

    trace!("Matched channel {} to epg {new_id:?}", chan.header.name);
    chan.header.epg_channel_id = Some(new_id);
    match kind {
        SmartMatchKind::Exact => ChannelEpgAssignment::Exact { corrected: current_id_is_valid },
        SmartMatchKind::Fuzzy => ChannelEpgAssignment::Fuzzy { corrected: current_id_is_valid },
    }
}

fn assign_epg_icon(
    chan: &mut PlaylistItem,
    icon_tags: &HashMap<Arc<str>, &Arc<str>>,
    icon_override_channels: &HashSet<Arc<str>>,
    icon_assigned: &mut HashSet<Arc<str>>,
) {
    let Some(epg_channel_id) = chan.header.epg_channel_id.as_ref() else {
        return;
    };
    with_folded_epg_id(epg_channel_id, |folded_id| {
        let needs_icon = icon_override_channels.contains(folded_id)
            || chan.header.logo.is_empty()
            || chan.header.logo_small.is_empty();
        if icon_assigned.contains(folded_id) || !needs_icon {
            return;
        }
        let Some(icon) = icon_tags.get(folded_id) else {
            return;
        };
        icon_assigned.insert(folded_id.intern());
        if icon_override_channels.contains(folded_id) || chan.header.logo.is_empty() {
            trace!("Matched channel {} to epg icon {icon}", chan.header.name);
            chan.header.logo = Arc::clone(icon);
        }
        if icon_override_channels.contains(folded_id) || chan.header.logo_small.is_empty() {
            chan.header.logo_small = Arc::clone(icon);
        }
    });
}

fn referenced_live_epg_ids(fp: &mut FetchedPlaylist<'_>) -> HashSet<Arc<str>> {
    fp.items()
        .filter(|channel| channel.header.xtream_cluster == XtreamCluster::Live && channel.header.item_type.is_live())
        .filter_map(|channel| {
            channel.header.epg_channel_id.as_ref().map(|id| with_folded_epg_id(id, |folded| folded.intern()))
        })
        .collect()
}

/// Assigns EPG IDs and logos to live playlist channels by matching them with EPG data.
///
/// For each live channel in the playlist missing an EPG ID, attempts to assign one using normalized name matching if smart matching is enabled. If a channel has an EPG ID but lacks logos, assigns logos from the corresponding EPG icon tags. Adds the matched EPG data to the provided vector.
///
/// # Examples
///
/// ```
/// let mut new_epg = Vec::new();
/// let mut playlist = FetchedPlaylist::default();
/// let mut id_cache = EpgIdCache::new(None);
/// assign_channel_epg(&mut new_epg, &mut playlist, &mut id_cache);
/// ```
async fn assign_channel_epg(new_epg: &mut Vec<Epg>, fp: &mut FetchedPlaylist<'_>, id_cache: &mut EpgIdCache) {
    if let Some(tv_guide) = &fp.epg {
        if let Some((mut epg_source, icon_override_channels)) =
            tv_guide.filter_merged_with_icon_overrides(id_cache).await
        {
            let stats = {
                let icon_tags = epg_source
                    .children
                    .iter()
                    .filter_map(|tag| {
                        tag.icon
                            .as_ref()
                            .filter(|icon| !icon.is_empty())
                            .map(|icon| (with_folded_epg_id(&tag.id, |folded| folded.intern()), icon))
                    })
                    .collect::<HashMap<Arc<str>, &Arc<str>>>();
                let icon_override_channels = icon_override_channels
                    .into_iter()
                    .map(|id| with_folded_epg_id(&id, |folded| folded.intern()))
                    .collect::<HashSet<_>>();
                let mut icon_assigned = HashSet::new();
                let mut stats = EpgAssignmentStats::default();

                if fp.is_memory() {
                    fp.items_mut()
                        .filter(|channel| {
                            channel.header.xtream_cluster == XtreamCluster::Live && channel.header.item_type.is_live()
                        })
                        .for_each(|channel| {
                            if id_cache.smart_match_enabled {
                                stats.record(assign_smart_epg_id(channel, id_cache));
                            }
                            assign_epg_icon(channel, &icon_tags, &icon_override_channels, &mut icon_assigned);
                        });
                } else {
                    warn!("Disk based playlist modification is not supported!");
                }
                stats
            };

            let referenced_epg_ids = referenced_live_epg_ids(fp);
            epg_source
                .children
                .retain(|channel| with_folded_epg_id(&channel.id, |folded| referenced_epg_ids.contains(folded)));

            if id_cache.smart_match_enabled {
                debug!(
                    "Smart EPG summary for input '{}': live={}, existing={}, exact={}, fuzzy={}, corrected={}, unresolved={}",
                    fp.input.name,
                    stats.live,
                    stats.existing,
                    stats.exact,
                    stats.fuzzy,
                    stats.corrected,
                    stats.unresolved
                );
            }

            new_epg.push(epg_source);
        }
    }
}

/// Processes a fetched playlist and assigns EPG data to its channels.
///
/// Collects EPG channel IDs from the playlist, initializes an EPG ID cache, and assigns EPG data to channels using normalization and smart matching if enabled. Logs a debug message if no EPG IDs are found and smart matching is disabled.
///
/// # Examples
///
/// ```
/// let mut playlist = FetchedPlaylist::default();
/// let mut epg_data = Vec::new();
/// process_playlist_epg(&mut playlist, &mut epg_data);
/// ```
pub async fn process_playlist_epg(fp: &mut FetchedPlaylist<'_>, epg: &mut Vec<Epg>) {
    if fp.input.epg.is_none() {
        return;
    }
    // collect all epg_channel ids
    let mut id_cache = EpgIdCache::new(fp.input.epg.as_ref());
    id_cache.collect_epg_id(fp);

    if id_cache.is_empty() && !id_cache.smart_match_enabled {
        debug!("No epg ids found for input {}", fp.input.name);
    } else {
        assign_channel_epg(epg, fp, &mut id_cache).await;
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        model::{
            ConfigInput, EpgConfig, EpgSmartMatchConfig, IcsEpgSourceConfig, PersistedEpgSource,
            PersistedEpgSourceKind, TVGuide,
        },
        repository::{FetchedPlaylist, MemoryPlaylistSource},
    };
    use rand::{Rng, distr::Alphanumeric};
    use rphonetic::{DoubleMetaphone, Encoder};
    use shared::{
        model::{ConfigInputDto, PlaylistGroup, PlaylistItemHeader, PlaylistItemType},
        utils::Internable,
    };
    use std::{collections::HashSet, fs, sync::Arc};
    use tempfile::tempdir;
    use tokio::time::Instant;

    fn random_string() -> String { rand::rng().sample_iter(&Alphanumeric).take(30).map(char::from).collect() }

    fn write_ics_file(path: &std::path::Path) {
        fs::write(
            path,
            "BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:Practice 1\nDTSTART:20260306T123000Z\nDTEND:20260306T133000Z\nEND:VEVENT\nEND:VCALENDAR",
        )
        .expect("write ics");
    }

    fn ics_tv_guide(path: std::path::PathBuf, match_names: Vec<Arc<str>>) -> TVGuide {
        TVGuide::new(vec![PersistedEpgSource {
            file_path: path,
            priority: 0,
            logo_override: false,
            kind: PersistedEpgSourceKind::Ics {
                channel_id: "f1.calendar".intern(),
                channel_title: Some("Formula 1".intern()),
                match_names,
                config: Box::new(IcsEpgSourceConfig::default()),
            },
        }])
    }

    fn live_playlist_item(name: &str, epg_channel_id: Option<&str>) -> shared::model::PlaylistItem {
        shared::model::PlaylistItem {
            header: PlaylistItemHeader {
                name: name.intern(),
                epg_channel_id: epg_channel_id.map(Internable::intern),
                xtream_cluster: super::XtreamCluster::Live,
                item_type: PlaylistItemType::Live,
                ..PlaylistItemHeader::default()
            },
        }
    }

    fn smart_cache(match_threshold: u16, best_match_threshold: u16) -> super::EpgIdCache {
        let mut dto = shared::model::EpgSmartMatchConfigDto {
            enabled: true,
            fuzzy_matching: true,
            match_threshold,
            best_match_threshold,
            ..shared::model::EpgSmartMatchConfigDto::default()
        };
        dto.prepare().expect("valid smart-match config");
        let config = EpgConfig { sources: vec![], smart_match: Some(EpgSmartMatchConfig::from(dto)) };
        super::EpgIdCache::new(Some(&config))
    }

    fn smart_cache_with_country_suffix() -> super::EpgIdCache {
        let mut dto = shared::model::EpgSmartMatchConfigDto {
            enabled: true,
            fuzzy_matching: true,
            name_prefix: shared::model::EpgNamePrefix::Suffix(".".to_string()),
            ..shared::model::EpgSmartMatchConfigDto::default()
        };
        dto.prepare().expect("valid smart-match config");
        let config = EpgConfig { sources: vec![], smart_match: Some(EpgSmartMatchConfig::from(dto)) };
        super::EpgIdCache::new(Some(&config))
    }

    async fn run_smart_xmltv_match(
        xmltv: &str,
        channel_name: &str,
        current_epg_id: Option<&str>,
    ) -> (Option<Arc<str>>, Vec<crate::model::Epg>) {
        let (mut assigned_ids, epg) = run_smart_xmltv_matches(xmltv, &[(channel_name, current_epg_id)]).await;
        (assigned_ids.pop().flatten(), epg)
    }

    async fn run_smart_xmltv_matches(
        xmltv: &str,
        channels: &[(&str, Option<&str>)],
    ) -> (Vec<Option<Arc<str>>>, Vec<crate::model::Epg>) {
        run_xmltv_matches(xmltv, channels, true).await
    }

    async fn run_xmltv_matches(
        xmltv: &str,
        channels: &[(&str, Option<&str>)],
        smart_matching: bool,
    ) -> (Vec<Option<Arc<str>>>, Vec<crate::model::Epg>) {
        let dir = tempdir().unwrap();
        let epg_path = dir.path().join("smart-match.xml");
        fs::write(&epg_path, xmltv).unwrap();

        let smart_match = smart_matching.then(|| {
            let mut dto = shared::model::EpgSmartMatchConfigDto {
                enabled: true,
                fuzzy_matching: true,
                ..shared::model::EpgSmartMatchConfigDto::default()
            };
            dto.prepare().expect("smart config");
            EpgSmartMatchConfig::from(dto)
        });
        let mut input = ConfigInput::from(ConfigInputDto::default());
        input.epg = Some(EpgConfig { sources: vec![], smart_match });
        let groups = vec![PlaylistGroup {
            id: 1,
            title: "Live".intern(),
            channels: channels.iter().map(|(name, epg_id)| live_playlist_item(name, *epg_id)).collect(),
            xtream_cluster: super::XtreamCluster::Live,
        }];
        let tv_guide = TVGuide::new(vec![PersistedEpgSource {
            file_path: epg_path,
            priority: 0,
            logo_override: false,
            kind: PersistedEpgSourceKind::Xmltv,
        }]);
        let mut playlist = FetchedPlaylist {
            input: &input,
            source: MemoryPlaylistSource::new(groups).into_source(),
            epg: Some(tv_guide),
        };
        let mut epg = Vec::new();

        super::process_playlist_epg(&mut playlist, &mut epg).await;
        let assigned_ids = playlist.items_mut().map(|item| item.header.epg_channel_id.clone()).collect();
        (assigned_ids, epg)
    }

    #[test]
    fn smart_match_prefers_exact_matches_from_higher_priority_sources() {
        let mut cache = smart_cache(80, 95);
        cache.insert_normalized("TF1", None);
        let candidates = cache.normalize_candidates(["TF1.fr", "TF1"]);

        assert!(cache.match_epg_channel_candidates(&"tf1.low".intern(), &candidates, 10, 1));
        assert!(cache.match_epg_channel_candidates(&"tf1.high".intern(), &candidates, 0, 2));
        assert!(cache.match_epg_channel_candidates(&"tf1.lower".intern(), &candidates, 20, 0));

        cache.finalize_matches(&HashSet::from(["tf1.low".intern(), "tf1.high".intern(), "tf1.lower".intern()]));
        assert_eq!(cache.normalized.get("tf1").and_then(Option::as_deref), Some("tf1.high"));
    }

    #[test]
    fn smart_match_replaces_an_earlier_fuzzy_candidate_with_a_better_score() {
        let mut cache = smart_cache(70, 70);
        cache.insert_normalized("RMC Decouverte", None);
        let key = cache.normalize("RMC Decouverte").intern();
        let weaker = cache.normalize_candidates(["RMC Decouver"]);
        let stronger = cache.normalize_candidates(["RMC Decouvert"]);
        assert!(super::similarity_score(&key, &weaker[0]) < super::similarity_score(&key, &stronger[0]));

        assert!(cache.match_epg_channel_candidates(&"rmc.weak".intern(), &weaker, 0, 0));
        assert!(cache.match_epg_channel_candidates(&"rmc.strong".intern(), &stronger, 0, 0));
        assert_eq!(cache.normalized.get(&key).and_then(Option::as_deref), Some("rmc.strong"));
    }

    #[test]
    fn smart_match_rejects_conflicting_channel_numbers() {
        let mut cache = smart_cache(70, 70);
        cache.insert_normalized("TF1", None);
        let candidates = cache.normalize_candidates(["TF1+1"]);

        assert!(!cache.match_epg_channel_candidates(&"tf1-plus-one.fr".intern(), &candidates, 0, 0));
        assert_eq!(cache.normalized.get("tf1").and_then(Option::as_deref), None);
    }

    #[test]
    fn smart_match_rejects_ambiguous_equal_scores() {
        let cache = smart_cache(70, 95);
        let guide = vec!["abcc".intern()];
        let left = "abca".intern();
        let right = "abcb".intern();
        assert_eq!(super::similarity_score(&left, &guide[0]), super::similarity_score(&right, &guide[0]));

        assert!(cache.select_unambiguous_candidate(&guide, [&left, &right], 70, None).is_none());
    }

    #[test]
    fn smart_match_country_fallback_prefers_the_full_country_match() {
        let mut cache = smart_cache_with_country_suffix();
        cache.insert_normalized("FR|TF1", None);
        let french = cache.normalize_candidates(["TF1.fr", "TF1"]);
        let belgian = cache.normalize_candidates(["TF1.be", "TF1"]);
        assert!(cache.match_epg_channel_candidates(&"TF1.fr".intern(), &french, 0, 0));
        assert!(!cache.match_epg_channel_candidates(&"TF1.be".intern(), &belgian, 0, 0));

        cache.finalize_matches(&HashSet::from(["tf1.fr".intern(), "tf1.be".intern()]));
        assert_eq!(cache.normalized.get("tf1.fr").and_then(Option::as_deref), Some("TF1.fr"));
    }

    #[test]
    fn smart_match_rejects_a_display_country_conflicting_with_the_epg_id() {
        let mut cache = smart_cache_with_country_suffix();
        cache.insert_normalized("FR|COMEDIE+", None);
        let candidates = cache.normalize_candidates(["ComediePlus.mu", "FR|COMEDIE+"]);

        assert!(!cache.match_epg_channel_candidates(&"ComediePlus.mu".intern(), &candidates, 0, 0));
        assert_eq!(cache.normalized.get("comedie.fr").and_then(Option::as_deref), None);
    }

    #[test]
    fn smart_match_rejects_ambiguous_countryless_fallbacks() {
        let mut cache = smart_cache_with_country_suffix();
        cache.insert_normalized("FR|TF1", None);
        let display_name = cache.normalize_candidates(["TF1"]);
        assert!(cache.match_epg_channel_candidates(&"opaque-a".intern(), &display_name, 0, 0));
        assert!(cache.match_epg_channel_candidates(&"opaque-b".intern(), &display_name, 0, 0));

        cache.finalize_matches(&HashSet::from(["opaque-a".intern(), "opaque-b".intern()]));
        assert_eq!(cache.normalized.get("tf1.fr").and_then(Option::as_deref), None);
    }

    #[test]
    fn selected_epg_ids_exclude_unused_smart_matches() {
        let mut cache = smart_cache(80, 95);
        let valid_id = "tf1.fr".intern();
        cache.insert_channel_epg_id(&valid_id);
        cache.normalize_and_store("TF1", Some(&valid_id));
        cache.normalize_and_store("France 2", None);

        let tf1_candidates = cache.normalize_candidates(["TF1"]);
        let france2_candidates = cache.normalize_candidates(["France 2"]);
        assert!(cache.match_epg_channel_candidates(&"tf1.unused".intern(), &tf1_candidates, 0, 0));
        assert!(cache.match_epg_channel_candidates(&"france2.fr".intern(), &france2_candidates, 0, 0));

        let available = HashSet::from([valid_id.clone(), "tf1.unused".intern(), "france2.fr".intern()]);
        cache.finalize_matches(&available);
        let selected = cache.selected_epg_ids(&available);
        assert!(selected.contains(&valid_id));
        assert!(selected.contains("france2.fr"));
        assert!(!selected.contains("tf1.unused"));
    }

    #[test]
    fn guide_names_ignore_empty_normalized_values_and_unreferenced_ids() {
        let mut cache = smart_cache(80, 95);
        cache.insert_channel_epg_id("direct.fr");

        assert!(cache.needs_guide_names("DIRECT.FR"));
        assert!(!cache.needs_guide_names("unreferenced.fr"));
        cache.register_guide_names("direct.fr", ["  "]);

        assert_eq!(cache.channel_name_matches_epg_id("direct.fr", "Direct"), None);
    }

    #[test]
    fn smart_match_ignores_decorative_playlist_entries() {
        let mut cache = smart_cache(80, 95);
        let input = ConfigInput::from(ConfigInputDto::default());
        let groups = vec![PlaylistGroup {
            id: 1,
            title: "Live".intern(),
            channels: vec![live_playlist_item("▬▬ Manga ▬▬", None)],
            xtream_cluster: super::XtreamCluster::Live,
        }];
        let mut playlist =
            FetchedPlaylist { input: &input, source: MemoryPlaylistSource::new(groups).into_source(), epg: None };

        cache.collect_epg_id(&mut playlist);

        assert!(cache.normalized.is_empty());
        assert!(super::is_decorative_channel_name("▬▬ Manga ▬▬"));
        assert!(super::is_decorative_channel_name("----"));
        assert!(!super::is_decorative_channel_name("|Channel|"));
        assert!(!super::is_decorative_channel_name(""));
    }

    #[test]
    fn channel_epg_id_membership_is_case_insensitive_ascii() {
        let mut cache = super::EpgIdCache::new(None);
        // Playlist epg ids stored MixedCase from different origins (Xtream source /
        // mapper literal). Both go through the folding insert.
        cache.insert_channel_epg_id("CNN.us");
        cache.insert_channel_epg_id("BBC.One.UK");

        // A guide <channel id> in any case matches the folded membership key.
        assert!(cache.contains_channel_epg_id("cnn.US"));
        assert!(cache.contains_channel_epg_id("CNN.US"));
        assert!(cache.contains_channel_epg_id("cnn.us"));
        assert!(cache.contains_channel_epg_id("bbc.one.uk"));
        assert!(!cache.contains_channel_epg_id("unknown.tv"));
    }

    #[test]
    fn assign_channel_epg_uses_case_insensitive_processed_and_icon_lookup() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let dir = tempdir().unwrap();
            let epg_path = dir.path().join("mixed-case-icon.xml");

            fs::write(
                &epg_path,
                r#"<?xml version="1.0" encoding="UTF-8"?>
<tv>
  <channel id="Demo.Channel">
    <display-name>Demo</display-name>
    <icon src="http://guide/icon.png" />
  </channel>
  <programme start="20260425000000 +0000" stop="20260425010000 +0000" channel="Demo.Channel">
    <title>Demo Show</title>
  </programme>
</tv>"#,
            )
            .unwrap();

            let mut input = ConfigInput::from(ConfigInputDto::default());
            input.epg = Some(EpgConfig { sources: vec![], smart_match: None });
            let channel = shared::model::PlaylistItem {
                header: PlaylistItemHeader {
                    name: "Demo".intern(),
                    epg_channel_id: Some("demo.channel".intern()),
                    logo: "http://old/icon.png".intern(),
                    logo_small: "".intern(),
                    xtream_cluster: super::XtreamCluster::Live,
                    item_type: PlaylistItemType::Live,
                    ..PlaylistItemHeader::default()
                },
            };
            let groups = vec![PlaylistGroup {
                id: 1,
                title: "Live".intern(),
                channels: vec![channel],
                xtream_cluster: super::XtreamCluster::Live,
            }];
            let tv_guide = TVGuide::new(vec![PersistedEpgSource {
                file_path: epg_path,
                priority: 0,
                logo_override: true,
                kind: PersistedEpgSourceKind::Xmltv,
            }]);
            let mut playlist = FetchedPlaylist {
                input: &input,
                source: MemoryPlaylistSource::new(groups).into_source(),
                epg: Some(tv_guide),
            };
            let mut epg = Vec::new();

            super::process_playlist_epg(&mut playlist, &mut epg).await;

            let updated = playlist.items_mut().next().unwrap();
            assert_eq!(updated.header.epg_channel_id.as_deref(), Some("demo.channel"));
            assert_eq!(updated.header.logo.as_ref(), "http://guide/icon.png");
            assert_eq!(updated.header.logo_small.as_ref(), "http://guide/icon.png");
            assert_eq!(epg[0].children[0].id.as_ref(), "Demo.Channel");
        });
    }

    #[test]
    fn smart_match_replaces_an_existing_id_that_has_no_programmes() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let (assigned_id, epg) = run_smart_xmltv_match(
                r#"<tv>
  <channel id="empty.fr"><display-name>Unused feed</display-name></channel>
  <channel id="tf1.fr"><display-name>TF1</display-name></channel>
  <programme start="20260425000000 +0000" stop="20260425010000 +0000" channel="tf1.fr">
    <title>Programme TF1</title>
  </programme>
</tv>"#,
                "TF1",
                Some("empty.fr"),
            )
            .await;

            assert_eq!(assigned_id.as_deref(), Some("tf1.fr"));
            assert_eq!(epg[0].children.len(), 1);
            assert_eq!(epg[0].children[0].id.as_ref(), "tf1.fr");
            assert_eq!(epg[0].children[0].programmes.len(), 1);
        });
    }

    #[test]
    fn directly_referenced_empty_guides_are_retained_with_or_without_smart_matching() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let xmltv = r#"<tv>
  <channel id="empty.fr"><display-name>Empty channel</display-name></channel>
</tv>"#;
            for smart_matching in [false, true] {
                let (assigned_ids, epg) =
                    run_xmltv_matches(xmltv, &[("Empty channel", Some("empty.fr"))], smart_matching).await;

                assert_eq!(assigned_ids, vec![Some("empty.fr".intern())]);
                assert_eq!(epg[0].children.len(), 1);
                assert_eq!(epg[0].children[0].id.as_ref(), "empty.fr");
                assert!(epg[0].children[0].programmes.is_empty());
            }
        });
    }

    #[test]
    fn smart_match_assigns_variants_when_a_direct_id_is_already_known() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let (assigned_ids, epg) = run_smart_xmltv_matches(
                r#"<tv>
  <channel id="tmc.fr"><display-name>TMC HD</display-name></channel>
  <programme start="20260425000000 +0000" stop="20260425010000 +0000" channel="tmc.fr">
    <title>Programme TMC</title>
  </programme>
</tv>"#,
                &[("TMC HD", Some("tmc.fr")), ("TMC FHD", None), ("TMC SD", None)],
            )
            .await;

            assert_eq!(assigned_ids, vec![Some("tmc.fr".intern()); 3]);
            assert_eq!(epg[0].children.len(), 1);
            assert_eq!(epg[0].children[0].programmes.len(), 1);
        });
    }

    #[test]
    fn smart_match_corrects_a_populated_but_semantically_wrong_id() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let (assigned_id, epg) = run_smart_xmltv_match(
                r#"<tv>
  <channel id="cherie25.fr"><display-name>Cherie 25</display-name></channel>
  <channel id="rmclife.fr"><display-name>RMC Life</display-name></channel>
  <programme start="20260425000000 +0000" stop="20260425010000 +0000" channel="cherie25.fr">
    <title>Programme Cherie 25</title>
  </programme>
  <programme start="20260425000000 +0000" stop="20260425010000 +0000" channel="rmclife.fr">
    <title>Programme RMC Life</title>
  </programme>
</tv>"#,
                "RMC LIFE",
                Some("cherie25.fr"),
            )
            .await;

            assert_eq!(assigned_id.as_deref(), Some("rmclife.fr"));
            assert_eq!(epg[0].children.len(), 1);
            assert_eq!(epg[0].children[0].id.as_ref(), "rmclife.fr");
        });
    }

    #[test]
    fn playlist_item_with_ics_epg_channel_id_gets_ics_programmes() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let dir = tempdir().unwrap();
            let ics_path = dir.path().join("f1.ics");
            write_ics_file(&ics_path);

            let mut input = ConfigInput::from(ConfigInputDto::default());
            input.epg = Some(EpgConfig { sources: vec![], smart_match: None });
            let groups = vec![PlaylistGroup {
                id: 1,
                title: "Live".intern(),
                channels: vec![live_playlist_item("Formula 1", Some("f1.calendar"))],
                xtream_cluster: super::XtreamCluster::Live,
            }];
            let mut playlist = FetchedPlaylist {
                input: &input,
                source: MemoryPlaylistSource::new(groups).into_source(),
                epg: Some(ics_tv_guide(ics_path, Vec::new())),
            };
            let mut epg = Vec::new();

            super::process_playlist_epg(&mut playlist, &mut epg).await;

            assert_eq!(epg.len(), 1);
            assert_eq!(epg[0].children[0].id.as_ref(), "f1.calendar");
            assert_eq!(epg[0].children[0].programmes.len(), 1);
            assert_eq!(epg[0].children[0].programmes[0].title.as_deref(), Some("Practice 1"));
        });
    }

    #[test]
    fn smart_match_uses_ics_channel_title_and_match_names() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let dir = tempdir().unwrap();
            let ics_path = dir.path().join("f1.ics");
            write_ics_file(&ics_path);

            let mut smart_dto = shared::model::EpgSmartMatchConfigDto {
                enabled: true,
                fuzzy_matching: false,
                ..shared::model::EpgSmartMatchConfigDto::default()
            };
            smart_dto.prepare().expect("smart config");
            let mut input = ConfigInput::from(ConfigInputDto::default());
            input.epg = Some(EpgConfig { sources: vec![], smart_match: Some(EpgSmartMatchConfig::from(smart_dto)) });
            let groups = vec![PlaylistGroup {
                id: 1,
                title: "Live".intern(),
                channels: vec![live_playlist_item("F1", None)],
                xtream_cluster: super::XtreamCluster::Live,
            }];
            let mut playlist = FetchedPlaylist {
                input: &input,
                source: MemoryPlaylistSource::new(groups).into_source(),
                epg: Some(ics_tv_guide(ics_path, vec!["F1".intern()])),
            };
            let mut epg = Vec::new();

            super::process_playlist_epg(&mut playlist, &mut epg).await;

            let updated = playlist.items_mut().next().unwrap();
            assert_eq!(updated.header.epg_channel_id.as_deref(), Some("f1.calendar"));
            assert_eq!(epg[0].children[0].id.as_ref(), "f1.calendar");
            assert_eq!(epg[0].children[0].programmes.len(), 1);
        });
    }

    #[test]
    fn test_phonetic() {
        let strings: Vec<String> = (0..5_000).map(|_| random_string()).collect();

        let phonetic = DoubleMetaphone::new(Some(6));

        let now = Instant::now();
        for value in &strings {
            let _ = phonetic.encode(value);
        }

        let elapsed = now.elapsed();
        println!("Elapsed time: {}.{:03} secs", elapsed.as_secs(), elapsed.subsec_millis());
    }
}

use crate::{fetched_playlist::FetchedPlaylist, parser::xmltv::normalize_channel_name};
use log::{debug, trace, warn};
use rphonetic::{DoubleMetaphone, Encoder};
use shared::{
    model::{
        ConfigTargetOptions, EpgChannel, EpgNamePrefix, EpgProgramme, EpgSmartMatchConfigDto, HookCtx, PlaylistGroup,
        PlaylistItem, PluginChannelProgrammes, PluginEpgChannel, PluginHook, PluginHost, PluginMatchRequest,
        PluginRunReport, XtreamCluster,
    },
    utils::{Internable, CONSTANTS},
};
use std::{
    borrow::Cow,
    cmp::Ordering,
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tuliprox_core::{
    model::{Epg, EpgConfig, EpgSmartMatchConfig},
    utils::with_folded_epg_id,
};

const MIN_FUZZY_SCORE_MARGIN: u16 = 3;
/// Candidates a `match-channel` plugin is shown per unresolved channel.
const MATCH_SHORTLIST_LEN: usize = 5;
/// Neighbours read from one phonetic / prefix / core bucket when building a shortlist.
const MATCH_BUCKET_SCAN: usize = 256;

/// Target options that control how matched guide ids are written back to live channels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EpgAssignOptions {
    pub clear_invalid_epg_ids: bool,
    pub adopt_guide_id_case: bool,
    /// `false` while the input is in maintenance (`MaintenanceGate`): no EPG
    /// plugin hook runs for it.
    pub plugin_hooks: bool,
}

impl Default for EpgAssignOptions {
    fn default() -> Self { Self { clear_invalid_epg_ids: false, adopt_guide_id_case: true, plugin_hooks: true } }
}

impl EpgAssignOptions {
    pub fn from_target_options(options: Option<&ConfigTargetOptions>) -> Self {
        options.map_or_else(Self::default, |options| Self {
            clear_invalid_epg_ids: options.clear_invalid_epg_ids(),
            adopt_guide_id_case: options.adopt_guide_epg_id_case(),
            plugin_hooks: true,
        })
    }
}

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

/// What the EPG plugin hooks need while one input's guide is parsed and matched.
///
/// It rides on [`EpgIdCache`] because the cache is the one value that already
/// reaches the streaming parser (`TVGuide::process_epg_file` / `process_ics_file`)
/// through `filter_merged_with_icon_overrides`, whose signature is shared with
/// the rest of the crate. `Default` is "no hooks": the parser then behaves exactly
/// as it did before plugins existed.
#[derive(Debug, Default)]
pub struct EpgHookState {
    /// `EpgAssignOptions::plugin_hooks`: `false` while the input is in maintenance.
    pub(crate) enabled: bool,
    /// The input the guide belongs to (`HookCtx::input`).
    pub(crate) input: String,
    /// Keep the guide channels that matched a playlist name ambiguously, so a
    /// `match-channel` plugin can still pick one of them in the second pass.
    pub(crate) keep_match_candidates: bool,
    /// One playlist item per normalized name, for the early `match-channel` pass
    /// the parser runs over each guide's channel list (filled only when a
    /// `match-channel` plugin runs).
    pub(crate) match_items: HashMap<Arc<str>, PlaylistItem>,
    /// What `on-epg-loaded` / `transform-programmes` did, collected by the parser.
    pub(crate) report: PluginRunReport,
}

/// One guide channel as the early `match-channel` pass sees it: its id and its
/// display names, as the guide spells them.
pub(crate) struct EarlyGuideChannel<'a> {
    pub(crate) id: &'a str,
    pub(crate) names: Vec<&'a str>,
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
    /// Early `match-channel` answers (normalized name -> guide id), used by
    /// [`Self::finalize_matches`] when the built-in matcher has no winner.
    plugin_picks: HashMap<Arc<str>, Arc<str>>,
    /// Names already asked in the early pass (once per input, over all sources).
    early_asked: HashSet<Arc<str>>,
    pub processed: HashSet<Arc<str>>,
    pub smart_match_config: EpgSmartMatchConfig,
    pub metaphone: DoubleMetaphone,
    pub smart_match_enabled: bool, // smart match is enabled, normalizing names
    pub fuzzy_match_enabled: bool, // fuzzy matching enabled
    /// EPG plugin hook state; see [`EpgHookState`].
    pub(crate) hooks: EpgHookState,
}

impl EpgIdCache {
    /// Creates a new `EpgIdCache` with configuration for smart and fuzzy matching.
    ///
    /// Initializes all internal caches and sets matching options based on the provided EPG configuration. If no configuration is given, defaults are used.
    ///
    /// # Examples
    ///
    /// ```text
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
            plugin_picks: HashMap::new(),
            early_asked: HashSet::new(),
            processed: HashSet::new(),
            metaphone: DoubleMetaphone::default(),
            smart_match_enabled: normalize_config.enabled,
            fuzzy_match_enabled: normalize_config.enabled && normalize_config.fuzzy_matching,
            smart_match_config: normalize_config,
            hooks: EpgHookState::default(),
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

    pub fn needs_guide_names(&self, id: &str) -> bool { self.contains_channel_epg_id(id) }

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
        if self.hooks.keep_match_candidates {
            // An ambiguous key has no winner, so nothing above selects its candidates,
            // and those are exactly what a `match-channel` plugin chooses among.
            for (key, entry) in &self.normalized {
                if entry.is_none() {
                    if let Some(candidates) = self.match_candidates.get(key) {
                        selected.extend(candidates.keys().filter(|id| available_epg_ids.contains(*id)).cloned());
                    }
                }
            }
        }
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
        let mut plugin = 0usize;
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
            } else if let Some(pick) = self
                .plugin_picks
                .get(key)
                .filter(|pick| with_folded_epg_id(pick, |folded| available_epg_ids.contains(folded)))
            {
                // No built-in winner, and an early `match-channel` answer that kept programmes.
                entry.replace(Arc::clone(pick));
                self.match_ranks.insert(
                    Arc::clone(key),
                    SmartMatchRank { kind: SmartMatchKind::Fuzzy, score: 0, source_priority: 0, source_order: 0 },
                );
                plugin += 1;
            } else {
                entry.take();
                if available_count > 0 {
                    ambiguous += 1;
                } else {
                    unavailable += 1;
                }
            }
        }
        debug!(
            "Smart EPG candidates: exact={exact}, fuzzy={fuzzy}, plugin={plugin}, ambiguous={ambiguous}, unavailable={unavailable}"
        );
    }

    /// Normalizes a channel name, computes its phonetic encoding, and stores both in the cache for later EPG matching.
    ///
    /// The normalized name is mapped to the provided EPG ID (if any), and the phonetic encoding is added to the phonetics map.
    /// This facilitates efficient lookup and fuzzy matching of channel names during EPG assignment.
    ///
    /// # Examples
    ///
    /// ```text
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
    /// ```text
    /// let cache = EpgIdCache::new(None);
    /// let normalized = cache.normalize("HBO HD");
    /// assert!(!normalized.is_empty());
    /// ```
    fn normalize(&self, name: &str) -> String { normalize_channel_name(name, &self.smart_match_config) }

    pub fn phonetic(&self, name: &Arc<str>) -> Arc<str> {
        let result = self.metaphone.encode(name);
        if result.is_empty() {
            name.clone()
        } else {
            result.intern()
        }
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

    /// The normalized key a playlist channel name is matched under, `None` when
    /// the name normalizes to nothing.
    fn match_key(&self, name: &str) -> Option<Arc<str>> {
        let key = self.normalize(name).intern();
        (!key.is_empty()).then_some(key)
    }

    /// Remember one playlist item per normalized name for the early
    /// `match-channel` pass: the first live channel with that name that has no
    /// guide id of its own. A channel whose own id turns out stale is still
    /// offered by the second pass, after the guides are parsed.
    pub(crate) fn collect_match_items(&mut self, fp: &mut FetchedPlaylist<'_>) {
        for item in fp.items() {
            if item.header.xtream_cluster != XtreamCluster::Live || !item.header.item_type.is_live() {
                continue;
            }
            if item.header.epg_channel_id.as_deref().is_some_and(|id| !id.is_empty()) {
                continue;
            }
            if let Some(key) = self.match_key(&item.header.name) {
                if self.normalized.contains_key(&key) && !self.hooks.match_items.contains_key(&key) {
                    self.hooks.match_items.insert(key, item.into_owned());
                }
            }
        }
    }

    /// The names the early `match-channel` pass asks about now: names the
    /// built-in matcher has no unambiguous candidate for yet, that have a sample
    /// item and were not asked before, in name order.
    pub(crate) fn early_match_keys(&self) -> Vec<Arc<str>> {
        let mut keys: Vec<Arc<str>> = self
            .hooks
            .match_items
            .keys()
            .filter(|key| !self.early_asked.contains(*key) && !self.plugin_picks.contains_key(*key))
            .filter(|key| {
                self.match_candidates
                    .get(*key)
                    .is_none_or(|candidates| best_ranked_match(candidates.values()).is_none())
            })
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    /// Per key, the indexes into `guide` of the top-[`MATCH_SHORTLIST_LEN`] guide
    /// channels by name similarity, best first. Only channels sharing a
    /// phonetic, three-letter-prefix or country-less-core bucket with the key are
    /// scored, and each must pass the country and digit gates of
    /// [`Self::plugin_match_rank`], so the cost stays bounded by the buckets.
    pub(crate) fn early_shortlists(&self, keys: &[Arc<str>], guide: &[EarlyGuideChannel<'_>]) -> Vec<Vec<usize>> {
        let normalized: Vec<Vec<Arc<str>>> = guide
            .iter()
            .map(|channel| {
                let mut names: Vec<Arc<str>> = std::iter::once(channel.id)
                    .chain(channel.names.iter().copied())
                    .map(|name| self.normalize(name).intern())
                    .filter(|name| !name.is_empty())
                    .collect();
                names.sort();
                names.dedup();
                names
            })
            .collect();
        let mut phonetics: HashMap<Arc<str>, Vec<usize>> = HashMap::new();
        let mut prefixes: HashMap<u32, Vec<usize>> = HashMap::new();
        let mut cores: HashMap<Arc<str>, Vec<usize>> = HashMap::new();
        for (at, names) in normalized.iter().enumerate() {
            for name in names {
                phonetics.entry(self.phonetic(name)).or_default().push(at);
                if let Some(prefix) = candidate_prefix(name) {
                    prefixes.entry(prefix).or_default().push(at);
                }
                let (core, _) = self.split_country(name);
                cores.entry(core.intern()).or_default().push(at);
            }
        }
        keys.iter()
            .map(|key| {
                let mut bucket: Vec<usize> = Vec::new();
                bucket.extend(phonetics.get(&self.phonetic(key)).into_iter().flatten().take(MATCH_BUCKET_SCAN));
                bucket.extend(
                    candidate_prefix(key)
                        .and_then(|prefix| prefixes.get(&prefix))
                        .into_iter()
                        .flatten()
                        .take(MATCH_BUCKET_SCAN),
                );
                let (core, _) = self.split_country(key);
                bucket.extend(cores.get(core).into_iter().flatten().take(MATCH_BUCKET_SCAN));
                bucket.sort_unstable();
                bucket.dedup();
                let (playlist_core, playlist_country) = self.split_country(key);
                let mut scored: Vec<(u16, usize)> = bucket
                    .into_iter()
                    .filter_map(|at| {
                        normalized[at]
                            .iter()
                            .filter_map(|guide_name| {
                                let (guide_core, guide_country) = self.split_country(guide_name);
                                (countries_compatible(playlist_country, guide_country)
                                    && numeric_signature_matches(playlist_core, guide_core))
                                .then(|| similarity_score(playlist_core, guide_core))
                            })
                            .max()
                            .filter(|score| *score > 0)
                            .map(|score| (score, at))
                    })
                    .collect();
                scored
                    .sort_by(|left, right| right.0.cmp(&left.0).then_with(|| guide[left.1].id.cmp(guide[right.1].id)));
                scored.into_iter().take(MATCH_SHORTLIST_LEN).map(|(_, at)| at).collect()
            })
            .collect()
    }

    /// Record that `keys` were asked in the early pass.
    pub(crate) fn mark_early_asked(&mut self, keys: &[Arc<str>]) { self.early_asked.extend(keys.iter().cloned()); }

    /// Record an early `match-channel` answer, after it passed the same country
    /// and digit gates as [`Self::plugin_match_rank`] against the guide's own
    /// names. `false` when it did not.
    pub(crate) fn record_early_pick(&mut self, key: &Arc<str>, channel: &EarlyGuideChannel<'_>) -> bool {
        let (playlist_core, playlist_country) = self.split_country(key);
        let passes = std::iter::once(channel.id).chain(channel.names.iter().copied()).any(|name| {
            let normalized = self.normalize(name);
            let (guide_core, guide_country) = self.split_country(&normalized);
            countries_compatible(playlist_country, guide_country)
                && numeric_signature_matches(playlist_core, guide_core)
        });
        if passes {
            self.register_guide_names(channel.id, channel.names.iter().copied());
            self.plugin_picks.insert(Arc::clone(key), channel.id.intern());
        }
        passes
    }

    /// Top-`limit` guide channels a `match-channel` plugin may pick for the
    /// playlist key `key`, best first.
    ///
    /// Only guide channels that are available (`processed`: retained, with
    /// programmes) qualify, since an id without programmes would be cleared
    /// again. They come from the built-in index: the candidates the matcher
    /// ranked for `key` but could not decide (ambiguity), and the guide ids that
    /// resolved for neighbouring playlist keys (same phonetic code, same
    /// three-letter prefix or same country-less core), scored by name similarity.
    fn match_shortlist(
        &self,
        key: &Arc<str>,
        guide_channels: &HashMap<Arc<str>, &EpgChannel>,
        limit: usize,
    ) -> Vec<PluginEpgChannel> {
        // folded id -> (best score, the guide's own spelling of the id)
        let mut scored: HashMap<Arc<str>, (u16, Arc<str>)> = HashMap::new();
        let mut offer = |epg_id: &Arc<str>, score: u16| {
            if !self.contains_processed_epg_id(epg_id) {
                return;
            }
            let folded = with_folded_epg_id(epg_id, |folded| folded.intern());
            let entry = scored.entry(folded).or_insert_with(|| (score, Arc::clone(epg_id)));
            if score > entry.0 {
                *entry = (score, Arc::clone(epg_id));
            }
        };

        if let Some(candidates) = self.match_candidates.get(key) {
            for candidate in candidates.values() {
                offer(&candidate.epg_id, candidate.rank.score);
            }
        }

        let mut neighbours: HashSet<&Arc<str>> = HashSet::new();
        if let Some(keys) = self.phonetics.get(&self.phonetic(key)) {
            neighbours.extend(keys.iter().take(MATCH_BUCKET_SCAN));
        }
        if let Some(keys) = candidate_prefix(key).and_then(|prefix| self.prefixes.get(&prefix)) {
            neighbours.extend(keys.iter().take(MATCH_BUCKET_SCAN));
        }
        let (core, _) = self.split_country(key);
        if let Some(keys) = self.cores.get(core) {
            neighbours.extend(keys.iter().take(MATCH_BUCKET_SCAN));
        }
        for neighbour in neighbours {
            if neighbour == key {
                continue;
            }
            if let Some(Some(epg_id)) = self.normalized.get(neighbour) {
                offer(epg_id, similarity_score(key, neighbour));
            }
        }

        let mut ranked: Vec<(Arc<str>, u16, Arc<str>)> =
            scored.into_iter().map(|(folded, (score, epg_id))| (folded, score, epg_id)).collect();
        ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        ranked
            .into_iter()
            .take(limit)
            .map(|(folded, _, epg_id)| {
                let guide = guide_channels.get(&folded);
                let mut names: Vec<String> =
                    guide.and_then(|guide| guide.title.as_ref()).map(ToString::to_string).into_iter().collect();
                let mut registered: Vec<&Arc<str>> = self.guide_names.get(&folded).into_iter().flatten().collect();
                registered.sort();
                for name in registered.into_iter().map(ToString::to_string) {
                    if !names.contains(&name) {
                        names.push(name);
                    }
                }
                PluginEpgChannel {
                    id: epg_id.to_string(),
                    names,
                    icon: guide.and_then(|guide| guide.icon.as_ref()).map(ToString::to_string),
                }
            })
            .collect()
    }

    /// The rank a plugin's answer for `key` gets when it passes the same safety
    /// gates a built-in fuzzy match does, or `None` when it must be ignored.
    ///
    /// The guide channel has to be available, and neither its id nor any of its
    /// registered names may contradict the playlist name on country or on the
    /// digits in it (the `TF1` / `TF1+1` rule of `select_unambiguous_candidate`).
    /// A plugin can therefore choose among candidates, never override those rules.
    fn plugin_match_rank(&self, key: &str, epg_id: &str) -> Option<SmartMatchRank> {
        if !self.contains_processed_epg_id(epg_id) {
            return None;
        }
        let folded = with_folded_epg_id(epg_id, |folded| folded.intern());
        let normalized_id = self.normalize(epg_id);
        let (playlist_core, playlist_country) = self.split_country(key);
        let names = self.guide_names.get(&folded);
        let best = std::iter::once(normalized_id.as_str())
            .chain(names.into_iter().flatten().map(AsRef::as_ref))
            .filter_map(|guide_name| {
                let (guide_core, guide_country) = self.split_country(guide_name);
                (countries_compatible(playlist_country, guide_country)
                    && numeric_signature_matches(playlist_core, guide_core))
                .then(|| similarity_score(playlist_core, guide_core))
            })
            .max()?;
        Some(SmartMatchRank { kind: SmartMatchKind::Fuzzy, score: best, source_priority: 0, source_order: 0 })
    }

    /// Record a plugin's accepted answer as the match of `key`, so every channel
    /// with that name resolves through the ordinary `matched_epg_id` path.
    fn apply_plugin_match(&mut self, key: &Arc<str>, epg_id: &Arc<str>, rank: SmartMatchRank) -> bool {
        let Some(entry) = self.normalized.get_mut(key) else {
            return false;
        };
        entry.replace(Arc::clone(epg_id));
        self.match_ranks.insert(Arc::clone(key), rank);
        true
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

    /// A channel counted as unresolved was resolved afterwards (`match-channel`).
    fn resolve_late(&mut self, assignment: ChannelEpgAssignment) {
        if matches!(assignment, ChannelEpgAssignment::Unresolved) {
            return;
        }
        self.unresolved = self.unresolved.saturating_sub(1);
        // `record` counts the channel again.
        self.live = self.live.saturating_sub(1);
        self.record(assignment);
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

/// Rewrites the channel's EPG id to the exact spelling of the matched guide channel.
///
/// Guide matching is ASCII case-insensitive, but the EPG db is keyed by the guide channel
/// id and looked up exactly by the playlist id. Adopting the guide spelling keeps the
/// playlist `tvg-id`/`epg_channel_id`, the XMLTV `<channel id>` and the db key identical.
fn adopt_guide_epg_id_case(chan: &mut PlaylistItem, guide_channels: &HashMap<Arc<str>, &EpgChannel>) {
    let guide_id = chan.header.epg_channel_id.as_deref().and_then(|current_id| {
        with_folded_epg_id(current_id, |folded| guide_channels.get(folded))
            .filter(|guide| guide.id.as_ref() != current_id)
            .map(|guide| Arc::clone(&guide.id))
    });
    if let Some(guide_id) = guide_id {
        trace!("Adopted guide epg id casing {guide_id} for channel {}", chan.header.name);
        chan.header.epg_channel_id = Some(guide_id);
    }
}

fn assign_epg_icon(
    chan: &mut PlaylistItem,
    guide_channels: &HashMap<Arc<str>, &EpgChannel>,
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
        let Some(icon) =
            guide_channels.get(folded_id).and_then(|guide| guide.icon.as_ref()).filter(|icon| !icon.is_empty())
        else {
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

fn is_live_epg_item(item: &PlaylistItem) -> bool {
    item.header.xtream_cluster == XtreamCluster::Live && item.header.item_type.is_live()
}

fn has_processed_epg(item: &PlaylistItem, id_cache: &EpgIdCache) -> bool {
    item.header.epg_channel_id.as_deref().is_some_and(|id| id_cache.contains_processed_epg_id(id))
}

/// `smart_pass` is `false` when the built-in matcher already ran over this channel
/// (the two-pass `match-channel` flow); only the post-processing is done then.
#[allow(clippy::too_many_arguments, reason = "the per-channel state of one pass; bundling it only moves the list")]
fn assign_live_channel_epg(
    channel: &mut PlaylistItem,
    id_cache: &EpgIdCache,
    guide_channels: &HashMap<Arc<str>, &EpgChannel>,
    icon_override_channels: &HashSet<Arc<str>>,
    icon_assigned: &mut HashSet<Arc<str>>,
    stats: &mut EpgAssignmentStats,
    options: EpgAssignOptions,
    smart_pass: bool,
) {
    if smart_pass && id_cache.smart_match_enabled {
        stats.record(assign_smart_epg_id(channel, id_cache));
    }
    let has_epg = has_processed_epg(channel, id_cache);
    if has_epg && options.adopt_guide_id_case {
        adopt_guide_epg_id_case(channel, guide_channels);
    }
    assign_epg_icon(channel, guide_channels, icon_override_channels, icon_assigned);
    if options.clear_invalid_epg_ids && !has_epg {
        channel.header.epg_channel_id = None;
    }
}

/// The second pass of `match-channel` (plan §7 *Sync/async boundary*, pattern 2).
///
/// `unresolved` are the positions, in `fp.items_mut()` order, of the live channels
/// the built-in matcher left without a guide id. They are grouped by normalized
/// name, so a name that repeats costs one request, and each group with a
/// non-empty shortlist is asked in ONE batched plugin call. An answer counts only
/// when it names a candidate of that request's shortlist and passes
/// [`EpgIdCache::plugin_match_rank`]; it is then recorded as the match of that
/// name and applied to every channel with the name through the built-in
/// `assign_smart_epg_id`. Everything else about the channel (guide-id case
/// adoption, icons, `clear_invalid_epg_ids`) runs afterwards exactly as for a
/// built-in match.
async fn resolve_unresolved_with_plugins<P: PluginHost>(
    plugins: &P,
    fp: &mut FetchedPlaylist<'_>,
    id_cache: &mut EpgIdCache,
    guide_channels: &HashMap<Arc<str>, &EpgChannel>,
    unresolved: &[usize],
    stats: &mut EpgAssignmentStats,
    report: &mut PluginRunReport,
) {
    let wanted: HashSet<usize> = unresolved.iter().copied().collect();
    let input = fp.input.name.to_string();
    let ctx = HookCtx::new("").with_input(&input);
    let mut key_at: HashMap<usize, Arc<str>> = HashMap::with_capacity(wanted.len());
    let mut applied: HashSet<Arc<str>> = HashSet::new();
    {
        let mut seen: HashSet<Arc<str>> = HashSet::new();
        let mut keys: Vec<Arc<str>> = Vec::new();
        let mut items: Vec<Cow<'_, PlaylistItem>> = Vec::new();
        let mut shortlists: Vec<Vec<PluginEpgChannel>> = Vec::new();
        for (at, item) in fp.items().enumerate() {
            if !wanted.contains(&at) {
                continue;
            }
            let Some(key) = id_cache.match_key(&item.header.name) else {
                continue;
            };
            key_at.insert(at, Arc::clone(&key));
            // Asked in the early pass over the guide's channel list already.
            if id_cache.early_asked.contains(&key) || !seen.insert(Arc::clone(&key)) {
                continue;
            }
            let shortlist = id_cache.match_shortlist(&key, guide_channels, MATCH_SHORTLIST_LEN);
            if shortlist.is_empty() {
                continue;
            }
            keys.push(key);
            items.push(item);
            shortlists.push(shortlist);
        }
        if keys.is_empty() {
            return;
        }
        let requests: Vec<PluginMatchRequest<'_>> =
            items.iter().zip(shortlists).map(|(item, candidates)| PluginMatchRequest { item, candidates }).collect();
        let answers = plugins.match_channels(&ctx, &requests, report).await;
        if answers.len() != requests.len() {
            warn!(
                "match-channel plugins answered {} of {} requests for input '{input}'; the answers are ignored",
                answers.len(),
                requests.len()
            );
            return;
        }
        let mut ignored = 0usize;
        for ((key, request), answer) in keys.iter().zip(&requests).zip(answers) {
            let Some(answer) = answer else {
                continue;
            };
            let Some(candidate) =
                request.candidates.iter().find(|candidate| candidate.id.eq_ignore_ascii_case(&answer))
            else {
                ignored += 1;
                continue;
            };
            let epg_id = candidate.id.as_str().intern();
            match id_cache.plugin_match_rank(key, &epg_id) {
                Some(rank) if id_cache.apply_plugin_match(key, &epg_id, rank) => {
                    applied.insert(Arc::clone(key));
                }
                _ => ignored += 1,
            }
        }
        if ignored > 0 {
            debug!("Ignored {ignored} match-channel answers for input '{input}' outside the shortlist or the matcher's rules");
        }
    }
    if applied.is_empty() {
        return;
    }
    for (at, channel) in fp.items_mut().enumerate() {
        if !wanted.contains(&at) || !key_at.get(&at).is_some_and(|key| applied.contains(key)) {
            continue;
        }
        stats.resolve_late(assign_smart_epg_id(channel, id_cache));
    }
}

fn referenced_live_epg_ids(fp: &mut FetchedPlaylist<'_>) -> HashSet<Arc<str>> {
    fp.items()
        .filter(|channel| is_live_epg_item(channel))
        .filter_map(|channel| {
            channel.header.epg_channel_id.as_ref().map(|id| with_folded_epg_id(id, |folded| folded.intern()))
        })
        .collect()
}

pub(crate) fn retain_epg_referenced_by_groups(groups: &[PlaylistGroup], epg: &mut [Epg]) {
    let referenced_epg_ids = groups
        .iter()
        .flat_map(|group| &group.channels)
        .filter(|channel| is_live_epg_item(channel))
        .filter_map(|channel| {
            channel.header.epg_channel_id.as_ref().map(|id| with_folded_epg_id(id, |folded| folded.intern()))
        })
        .collect::<HashSet<_>>();
    for source in epg {
        source.children.retain(|channel| with_folded_epg_id(&channel.id, |folded| referenced_epg_ids.contains(folded)));
    }
}

pub(crate) fn clear_invalid_live_epg_ids(fp: &mut FetchedPlaylist<'_>, epg: &[Epg]) {
    let processed_epg_ids = epg
        .iter()
        .flat_map(|source| &source.children)
        .map(|channel| with_folded_epg_id(&channel.id, |folded| folded.intern()))
        .collect::<HashSet<_>>();
    let mut cleared = 0usize;
    fp.source.retain_memory_items_mut(|item| {
        if !is_live_epg_item(item) {
            return true;
        }
        let has_epg = item
            .header
            .epg_channel_id
            .as_deref()
            .is_some_and(|id| with_folded_epg_id(id, |folded| processed_epg_ids.contains(folded)));
        if !has_epg && item.header.epg_channel_id.take().is_some() {
            cleared += 1;
        }
        true
    });
    if cleared > 0 {
        debug!("Cleared {cleared} unmatched EPG IDs from input '{}'", fp.input.name);
    }
}

/// Re-adopts the guide id spelling for live channels whose ids were rewritten after EPG assignment,
/// e.g. by `AfterEpg` mappings, so the playlist id stays identical to the processed guide channel id.
pub(crate) fn adopt_guide_live_epg_id_case(fp: &mut FetchedPlaylist<'_>, epg: &[Epg]) {
    if !fp.is_memory() {
        return;
    }
    let guide_channels = epg
        .iter()
        .flat_map(|source| &source.children)
        .map(|guide| (with_folded_epg_id(&guide.id, |folded| folded.intern()), guide.as_ref()))
        .collect::<HashMap<Arc<str>, &EpgChannel>>();
    if guide_channels.is_empty() {
        return;
    }
    fp.items_mut()
        .filter(|item| is_live_epg_item(item))
        .for_each(|item| adopt_guide_epg_id_case(item, &guide_channels));
}

/// Assigns EPG IDs and logos to live playlist channels by matching them with EPG data.
/// Invalid EPG IDs are optionally cleared without removing playlist entries.
///
/// For each live channel in the playlist missing an EPG ID, attempts to assign one using normalized name matching if smart matching is enabled. If a channel has an EPG ID but lacks logos, assigns logos from the corresponding EPG icon tags. Adds the matched EPG data to the provided vector.
///
/// # Examples
///
/// ```text
/// let mut new_epg = Vec::new();
/// let mut playlist = FetchedPlaylist::default();
/// let mut id_cache = EpgIdCache::new(None);
/// assign_channel_epg(&mut new_epg, &mut playlist, &mut id_cache, false);
/// ```
async fn assign_channel_epg<P: PluginHost>(
    plugins: &P,
    new_epg: &mut Vec<Epg>,
    fp: &mut FetchedPlaylist<'_>,
    id_cache: &mut EpgIdCache,
    options: EpgAssignOptions,
) {
    if fp.epg.is_none() {
        return;
    }
    // `match-channel` needs the smart matcher's index for its shortlists.
    let match_hook =
        options.plugin_hooks && id_cache.smart_match_enabled && plugins.wants(PluginHook::MatchChannel, "");
    id_cache.hooks.keep_match_candidates = match_hook;
    if match_hook {
        id_cache.collect_match_items(fp);
    }
    let Some(tv_guide) = &fp.epg else {
        return;
    };
    let mut merged_epg = tv_guide.filter_merged_with_icon_overrides(plugins, id_cache).await;

    let mut stats = EpgAssignmentStats::default();
    if fp.is_memory() {
        let guide_channels = merged_epg
            .as_ref()
            .into_iter()
            .flat_map(|(epg_source, _)| &epg_source.children)
            .map(|guide| (with_folded_epg_id(&guide.id, |folded| folded.intern()), guide.as_ref()))
            .collect::<HashMap<Arc<str>, &EpgChannel>>();
        let icon_override_channels = merged_epg
            .as_ref()
            .into_iter()
            .flat_map(|(_, channels)| channels)
            .map(|id| with_folded_epg_id(id, |folded| folded.intern()))
            .collect::<HashSet<_>>();
        let mut icon_assigned = HashSet::with_capacity(guide_channels.len());

        // Pass 1 (only with a `match-channel` plugin): the built-in matcher alone,
        // to learn which live channels it left unresolved. Without the hook the
        // matcher and the post-processing share one pass, as they always did.
        let mut unresolved = Vec::new();
        if match_hook {
            let mut position = 0usize;
            fp.items_mut().for_each(|channel| {
                let at = position;
                position += 1;
                if is_live_epg_item(channel) {
                    let assignment = assign_smart_epg_id(channel, id_cache);
                    stats.record(assignment);
                    if matches!(assignment, ChannelEpgAssignment::Unresolved) {
                        unresolved.push(at);
                    }
                }
            });
            if !unresolved.is_empty() {
                let mut report = PluginRunReport::default();
                resolve_unresolved_with_plugins(
                    plugins,
                    fp,
                    id_cache,
                    &guide_channels,
                    &unresolved,
                    &mut stats,
                    &mut report,
                )
                .await;
                id_cache.hooks.report.merge(report);
            }
        }

        let mut process_channel = |channel: &mut PlaylistItem| {
            if !is_live_epg_item(channel) {
                return;
            }
            assign_live_channel_epg(
                channel,
                id_cache,
                &guide_channels,
                &icon_override_channels,
                &mut icon_assigned,
                &mut stats,
                options,
                !match_hook,
            );
        };

        fp.items_mut().for_each(&mut process_channel);
    } else {
        warn!("Disk based playlist modification is not supported!");
    }

    if id_cache.smart_match_enabled {
        debug!(
            "Smart EPG summary for input '{}': live={}, existing={}, exact={}, fuzzy={}, corrected={}, unresolved={}",
            fp.input.name, stats.live, stats.existing, stats.exact, stats.fuzzy, stats.corrected, stats.unresolved
        );
    }
    if let Some((mut epg_source, _)) = merged_epg.take() {
        let referenced_epg_ids = referenced_live_epg_ids(fp);
        epg_source
            .children
            .retain(|channel| with_folded_epg_id(&channel.id, |folded| referenced_epg_ids.contains(folded)));
        new_epg.push(epg_source);
    }
}

/// Processes a fetched playlist and assigns EPG data to its channels.
///
/// Collects EPG channel IDs from the playlist, initializes an EPG ID cache, and assigns EPG data to channels using normalization and smart matching if enabled. Logs a debug message if no EPG IDs are found and smart matching is disabled.
///
/// # Examples
///
/// ```text
/// let mut playlist = FetchedPlaylist::default();
/// let mut epg_data = Vec::new();
/// process_playlist_epg(&NoPlugins, &mut playlist, &mut epg_data, false);
/// ```
///
/// `plugins` reaches the streaming parser (`on-epg-loaded`,
/// `transform-programmes`); this fn is also the Phase 3 site of the two-pass
/// `match-channel` call over the channels the built-in matcher left unresolved.
/// Returns what the EPG plugin hooks did, for the target's report; the hook
/// context has no target (`ctx.target` is empty, `ctx.input` is the input).
pub async fn process_playlist_epg<P: PluginHost>(
    plugins: &P,
    fp: &mut FetchedPlaylist<'_>,
    epg: &mut Vec<Epg>,
    options: EpgAssignOptions,
) -> PluginRunReport {
    let mut report = PluginRunReport::default();
    assign_playlist_epg(plugins, fp, epg, options, &mut report).await;
    report
}

/// `enrich-programmes` over the EPG sources one input contributed to a target
/// (`new_epg[input_epg_start..]`), per channel, between `process_playlist_epg`
/// and the `AfterEpg` mapping.
///
/// Every channel of `epg[..].children` goes to the plugins in ONE
/// `PluginHost::enrich_programmes` call, and the programmes come back sorted by
/// start with the host's spelling of the channel id attached.
///
/// Injected programmes are plain `EpgChannel::programmes`, so they take the
/// ordinary `spill_epg_to_disk` -> `finish_into_disk` -> `merge_epg_trees` path.
/// The hook adds programmes to channels, never channels, so the playlist
/// `source_ordinal` order of the generated XMLTV (`epg_repository.rs`) is
/// untouched. The one rule that applies to an injected programme is the merge's:
/// it collapses an EXACT `(start, stop)` collision with an existing programme of
/// the channel, keeping the existing one and backfilling only what it lacks.
pub async fn enrich_playlist_programmes<P: PluginHost>(
    plugins: &P,
    ctx: &HookCtx<'_>,
    epg: &mut [Epg],
    report: &mut PluginRunReport,
) {
    if !plugins.wants(PluginHook::EnrichProgrammes, ctx.target) {
        return;
    }
    let mut places: Vec<(usize, usize)> = Vec::new();
    let mut channels: Vec<PluginChannelProgrammes> = Vec::new();
    for (source_index, source) in epg.iter_mut().enumerate() {
        for (child_index, child) in source.children.iter_mut().enumerate() {
            let channel = Arc::make_mut(child);
            places.push((source_index, child_index));
            channels.push(PluginChannelProgrammes {
                channel_id: Arc::clone(&channel.id),
                programmes: std::mem::take(&mut channel.programmes),
            });
        }
    }
    if channels.is_empty() {
        return;
    }

    plugins.enrich_programmes(ctx, &mut channels, report).await;

    for ((source_index, child_index), entry) in places.into_iter().zip(channels) {
        let PluginChannelProgrammes { channel_id, mut programmes } = entry;
        programmes.sort_by_key(|programme| programme.start);
        let channel = Arc::make_mut(&mut epg[source_index].children[child_index]);
        channel.programmes = programmes.into_iter().map(|programme| with_channel_id(programme, &channel_id)).collect();
    }
}

/// `programme` with the host's spelling of its channel id attached (the id is
/// transient and private, so a programme that carries another one is rebuilt).
fn with_channel_id(programme: EpgProgramme, channel_id: &Arc<str>) -> EpgProgramme {
    if programme.get_transient_channel_id() == channel_id {
        return programme;
    }
    let EpgProgramme {
        start, stop, title, desc, catchup_id, categories, is_live, is_new, previously_shown, icon, ..
    } = programme;
    let mut rebuilt = EpgProgramme::new_all(start, stop, Arc::clone(channel_id), title, desc, catchup_id);
    rebuilt.categories = categories;
    rebuilt.is_live = is_live;
    rebuilt.is_new = is_new;
    rebuilt.previously_shown = previously_shown;
    rebuilt.icon = icon;
    rebuilt
}

async fn assign_playlist_epg<P: PluginHost>(
    plugins: &P,
    fp: &mut FetchedPlaylist<'_>,
    epg: &mut Vec<Epg>,
    options: EpgAssignOptions,
    report: &mut PluginRunReport,
) {
    if fp.input.epg.is_none() {
        return;
    }
    // collect all epg_channel ids
    let mut id_cache = EpgIdCache::new(fp.input.epg.as_ref());
    id_cache.collect_epg_id(fp);
    id_cache.hooks.enabled = options.plugin_hooks;
    id_cache.hooks.input = fp.input.name.to_string();

    if id_cache.is_empty() && !id_cache.smart_match_enabled && !options.clear_invalid_epg_ids {
        debug!("No epg ids found for input {}", fp.input.name);
    } else {
        assign_channel_epg(plugins, epg, fp, &mut id_cache, options).await;
    }
    report.merge(std::mem::take(&mut id_cache.hooks.report));
}

#[cfg(test)]
mod tests {
    use super::{EpgChannel, EpgProgramme, HookCtx, PluginChannelProgrammes, PluginEpgChannel, PluginHook, PluginHost};
    use crate::{fetched_playlist::FetchedPlaylist, parser::xmltv::TVGuide};
    use rand::{distr::Alphanumeric, Rng};
    use rphonetic::{DoubleMetaphone, Encoder};
    use shared::{
        model::{ConfigInputDto, NoPlugins, PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType},
        utils::Internable,
    };
    use std::{
        collections::{HashMap, HashSet},
        fs,
        sync::Arc,
    };
    use tempfile::tempdir;
    use tokio::time::Instant;
    use tuliprox_core::model::{
        ConfigInput, EpgConfig, EpgSmartMatchConfig, IcsEpgSourceConfig, PersistedEpgSource, PersistedEpgSourceKind,
    };
    use tuliprox_repository::MemoryPlaylistSource;

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
    ) -> (Option<Arc<str>>, Vec<tuliprox_core::model::Epg>) {
        let (mut assigned_ids, epg) = run_smart_xmltv_matches(xmltv, &[(channel_name, current_epg_id)]).await;
        (assigned_ids.pop().flatten(), epg)
    }

    async fn run_smart_xmltv_matches(
        xmltv: &str,
        channels: &[(&str, Option<&str>)],
    ) -> (Vec<Option<Arc<str>>>, Vec<tuliprox_core::model::Epg>) {
        run_xmltv_matches(xmltv, channels, true, false).await
    }

    async fn run_xmltv_matches(
        xmltv: &str,
        channels: &[(&str, Option<&str>)],
        smart_matching: bool,
        clear_invalid_epg_ids: bool,
    ) -> (Vec<Option<Arc<str>>>, Vec<tuliprox_core::model::Epg>) {
        run_xmltv_matches_with(&NoPlugins, xmltv, channels, smart_matching, clear_invalid_epg_ids).await
    }

    async fn run_xmltv_matches_with<P: PluginHost>(
        plugins: &P,
        xmltv: &str,
        channels: &[(&str, Option<&str>)],
        smart_matching: bool,
        clear_invalid_epg_ids: bool,
    ) -> (Vec<Option<Arc<str>>>, Vec<tuliprox_core::model::Epg>) {
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

        let options = super::EpgAssignOptions { clear_invalid_epg_ids, ..super::EpgAssignOptions::default() };
        super::process_playlist_epg(plugins, &mut playlist, &mut epg, options).await;
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
    fn early_match_offers_guide_channels_by_their_own_names_and_finalizes_a_kept_pick() {
        let mut cache = smart_cache(80, 95);
        cache.normalize_and_store("BBC Too", None);
        let key = cache.match_key("BBC Too").expect("key");
        cache.hooks.match_items.insert(Arc::clone(&key), PlaylistItem { header: PlaylistItemHeader::default() });
        assert_eq!(cache.early_match_keys(), vec![Arc::clone(&key)], "no built-in candidate yet");

        // No playlist channel keeps `bbc.two`: the shortlist comes from the guide's own names.
        let guide = [
            super::EarlyGuideChannel { id: "sky.news", names: vec!["Sky News"] },
            super::EarlyGuideChannel { id: "bbc.two", names: vec!["BBC Two"] },
        ];
        let shortlists = cache.early_shortlists(std::slice::from_ref(&key), &guide);
        assert_eq!(shortlists[0].first(), Some(&1), "bbc.two is the best candidate");
        assert!(!shortlists[0].contains(&0), "sky.news shares no bucket with the name");

        assert!(cache.record_early_pick(&key, &guide[1]));
        cache.mark_early_asked(std::slice::from_ref(&key));
        assert!(cache.early_match_keys().is_empty(), "asked once");

        cache.finalize_matches(&HashSet::from(["bbc.two".intern()]));
        assert_eq!(cache.normalized.get(&key).cloned().flatten().as_deref(), Some("bbc.two"));
        cache.finalize_matches(&HashSet::new());
        assert_eq!(cache.normalized.get(&key).cloned().flatten(), None, "a pick without programmes is dropped");
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

            super::process_playlist_epg(
                &NoPlugins,
                &mut playlist,
                &mut epg,
                super::EpgAssignOptions { clear_invalid_epg_ids: false, ..super::EpgAssignOptions::default() },
            )
            .await;

            let updated = playlist.items_mut().next().unwrap();
            assert_eq!(updated.header.epg_channel_id.as_deref(), Some("Demo.Channel"));
            assert_eq!(updated.header.logo.as_ref(), "http://guide/icon.png");
            assert_eq!(updated.header.logo_small.as_ref(), "http://guide/icon.png");
            assert_eq!(epg[0].children[0].id.as_ref(), "Demo.Channel");
        });
    }

    #[test]
    fn matched_epg_ids_adopt_guide_channel_casing() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let xmltv = r#"<tv>
  <channel id="USA4K.us"><display-name>USA 4K</display-name></channel>
  <programme start="20260425000000 +0000" stop="20260425010000 +0000" channel="USA4K.us">
    <title>Show</title>
  </programme>
</tv>"#;
            let channels = [
                ("USA 4K", Some("usa4k.us")),
                ("USA 4K HD", Some("USA4K.US")),
                ("Already exact", Some("USA4K.us")),
                ("Unknown", Some("unknown.id")),
            ];
            for smart_matching in [false, true] {
                let (assigned_ids, epg) = run_xmltv_matches(xmltv, &channels, smart_matching, false).await;

                assert_eq!(epg[0].children[0].id.as_ref(), "USA4K.us");
                assert_eq!(
                    assigned_ids.iter().map(Option::as_deref).collect::<Vec<_>>(),
                    vec![Some("USA4K.us"), Some("USA4K.us"), Some("USA4K.us"), Some("unknown.id")],
                    "smart_matching={smart_matching}"
                );
            }
        });
    }

    #[test]
    fn post_mapping_adoption_restores_guide_channel_casing() {
        let input = ConfigInput::from(ConfigInputDto::default());
        let groups = vec![PlaylistGroup {
            id: 1,
            title: "Live".intern(),
            channels: vec![
                live_playlist_item("USA 4K", Some("usa4k.us")),
                live_playlist_item("Unknown", Some("unknown.id")),
            ],
            xtream_cluster: super::XtreamCluster::Live,
        }];
        let mut playlist =
            FetchedPlaylist { input: &input, source: MemoryPlaylistSource::new(groups).into_source(), epg: None };
        let epg = vec![super::Epg {
            priority: 0,
            logo_override: false,
            attributes: None,
            children: vec![Arc::new(shared::model::EpgChannel::new("USA4K.us".intern()))],
        }];

        super::adopt_guide_live_epg_id_case(&mut playlist, &epg);

        let ids = playlist.items_mut().map(|item| item.header.epg_channel_id.clone()).collect::<Vec<_>>();
        assert_eq!(ids.iter().map(Option::as_deref).collect::<Vec<_>>(), vec![Some("USA4K.us"), Some("unknown.id")]);
    }

    #[test]
    fn disabled_guide_casing_adoption_keeps_playlist_epg_ids() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let dir = tempdir().unwrap();
            let epg_path = dir.path().join("keep-case.xml");
            fs::write(
                &epg_path,
                r#"<tv>
  <channel id="USA4K.us"><display-name>USA 4K</display-name></channel>
  <programme start="20260425000000 +0000" stop="20260425010000 +0000" channel="USA4K.us">
    <title>Show</title>
  </programme>
</tv>"#,
            )
            .unwrap();
            let mut input = ConfigInput::from(ConfigInputDto::default());
            input.epg = Some(EpgConfig { sources: vec![], smart_match: None });
            let groups = vec![PlaylistGroup {
                id: 1,
                title: "Live".intern(),
                channels: vec![live_playlist_item("USA 4K", Some("usa4k.us"))],
                xtream_cluster: super::XtreamCluster::Live,
            }];
            let mut playlist = FetchedPlaylist {
                input: &input,
                source: MemoryPlaylistSource::new(groups).into_source(),
                epg: Some(TVGuide::new(vec![PersistedEpgSource {
                    file_path: epg_path,
                    priority: 0,
                    logo_override: false,
                    kind: PersistedEpgSourceKind::Xmltv,
                }])),
            };
            let mut epg = Vec::new();
            let options = super::EpgAssignOptions { adopt_guide_id_case: false, ..super::EpgAssignOptions::default() };

            super::process_playlist_epg(&NoPlugins, &mut playlist, &mut epg, options).await;

            let updated = playlist.items_mut().next().unwrap();
            assert_eq!(updated.header.epg_channel_id.as_deref(), Some("usa4k.us"));
            assert_eq!(epg[0].children[0].id.as_ref(), "USA4K.us");
        });
    }

    #[test]
    fn clear_invalid_epg_ids_preserves_items_and_clears_only_unmatched_live_ids() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let dir = tempdir().unwrap();
            let epg_path = dir.path().join("required-epg.xml");
            fs::write(
                &epg_path,
                r#"<tv>
  <channel id="matched.live"><display-name>Matched Live</display-name></channel>
  <programme start="20260425000000 +0000" stop="20260425010000 +0000" channel="matched.live">
    <title>Programme</title>
  </programme>
</tv>"#,
            )
            .unwrap();

            let mut input = ConfigInput::from(ConfigInputDto::default());
            input.epg = Some(EpgConfig { sources: vec![], smart_match: None });
            let item = |name: &str, epg_id: Option<&str>, item_type: PlaylistItemType| PlaylistItem {
                header: PlaylistItemHeader {
                    name: name.intern(),
                    epg_channel_id: epg_id.map(Internable::intern),
                    xtream_cluster: item_type.cluster(),
                    item_type,
                    ..PlaylistItemHeader::default()
                },
            };
            let groups = vec![
                PlaylistGroup {
                    id: 1,
                    title: "Live".intern(),
                    channels: vec![
                        item("Matched Live", Some("matched.live"), PlaylistItemType::Live),
                        item("Unmatched Live", Some("missing.live"), PlaylistItemType::LiveHls),
                    ],
                    xtream_cluster: super::XtreamCluster::Live,
                },
                PlaylistGroup {
                    id: 2,
                    title: "Non-Live".intern(),
                    channels: vec![
                        item("VOD", None, PlaylistItemType::Video),
                        item("Series", None, PlaylistItemType::Series),
                        item("Local VOD", None, PlaylistItemType::LocalVideo),
                        item("Local Series", None, PlaylistItemType::LocalSeries),
                    ],
                    xtream_cluster: super::XtreamCluster::Video,
                },
            ];
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

            super::process_playlist_epg(
                &NoPlugins,
                &mut playlist,
                &mut epg,
                super::EpgAssignOptions { clear_invalid_epg_ids: true, ..super::EpgAssignOptions::default() },
            )
            .await;

            let items = playlist
                .items_mut()
                .map(|item| (item.header.name.clone(), item.header.epg_channel_id.clone()))
                .collect::<HashMap<_, _>>();
            assert_eq!(items.get("Matched Live").and_then(Option::as_deref), Some("matched.live"));
            assert_eq!(items.get("Unmatched Live"), Some(&None));
            assert!(items.contains_key("VOD"));
            assert!(items.contains_key("Series"));
            assert!(items.contains_key("Local VOD"));
            assert!(items.contains_key("Local Series"));
        });
    }

    #[test]
    fn default_epg_processing_preserves_unmatched_existing_id() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let (assigned_ids, _) = run_xmltv_matches(
                r#"<tv>
  <channel id="matched.live"><display-name>Matched Live</display-name></channel>
  <programme start="20260425000000 +0000" stop="20260425010000 +0000" channel="matched.live">
    <title>Programme</title>
  </programme>
</tv>"#,
                &[("Unmatched Live", Some("missing.live"))],
                false,
                false,
            )
            .await;

            assert_eq!(assigned_ids, vec![Some("missing.live".intern())]);
        });
    }

    #[test]
    fn clear_invalid_epg_ids_is_ignored_without_a_materialized_epg_source() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let mut input = ConfigInput::from(ConfigInputDto::default());
            input.epg = Some(EpgConfig { sources: vec![], smart_match: None });
            let groups = vec![PlaylistGroup {
                id: 1,
                title: "Live".intern(),
                channels: vec![live_playlist_item("Unmatched Live", Some("missing.live"))],
                xtream_cluster: super::XtreamCluster::Live,
            }];
            let mut playlist =
                FetchedPlaylist { input: &input, source: MemoryPlaylistSource::new(groups).into_source(), epg: None };

            super::process_playlist_epg(
                &NoPlugins,
                &mut playlist,
                &mut Vec::new(),
                super::EpgAssignOptions { clear_invalid_epg_ids: true, ..super::EpgAssignOptions::default() },
            )
            .await;

            assert_eq!(playlist.items_mut().count(), 1);
            assert_eq!(
                playlist.items_mut().next().and_then(|item| item.header.epg_channel_id.as_deref()),
                Some("missing.live")
            );
        });
    }

    #[test]
    fn optional_epg_preserves_existing_empty_groups() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let dir = tempdir().unwrap();
            let epg_path = dir.path().join("optional-epg-empty-group.xml");
            fs::write(
                &epg_path,
                r#"<tv>
  <channel id="matched.live"><display-name>Matched Live</display-name></channel>
  <programme start="20260425000000 +0000" stop="20260425010000 +0000" channel="matched.live">
    <title>Programme</title>
  </programme>
</tv>"#,
            )
            .unwrap();

            let mut input = ConfigInput::from(ConfigInputDto::default());
            input.epg = Some(EpgConfig { sources: vec![], smart_match: None });
            let groups = vec![
                PlaylistGroup {
                    id: 1,
                    title: "Empty".intern(),
                    channels: vec![],
                    xtream_cluster: super::XtreamCluster::Live,
                },
                PlaylistGroup {
                    id: 2,
                    title: "Live".intern(),
                    channels: vec![live_playlist_item("Matched Live", Some("matched.live"))],
                    xtream_cluster: super::XtreamCluster::Live,
                },
            ];
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

            super::process_playlist_epg(
                &NoPlugins,
                &mut playlist,
                &mut Vec::new(),
                super::EpgAssignOptions { clear_invalid_epg_ids: false, ..super::EpgAssignOptions::default() },
            )
            .await;

            assert_eq!(playlist.get_group_count(), 2);
        });
    }

    #[test]
    fn clear_invalid_epg_ids_preserves_live_items_without_ids() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let dir = tempdir().unwrap();
            let epg_path = dir.path().join("required-epg-no-ids.xml");
            fs::write(&epg_path, "<tv></tv>").unwrap();

            let mut input = ConfigInput::from(ConfigInputDto::default());
            input.epg = Some(EpgConfig { sources: vec![], smart_match: None });
            let groups = vec![PlaylistGroup {
                id: 1,
                title: "Live".intern(),
                channels: vec![live_playlist_item("Live without ID", None)],
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

            super::process_playlist_epg(
                &NoPlugins,
                &mut playlist,
                &mut Vec::new(),
                super::EpgAssignOptions { clear_invalid_epg_ids: true, ..super::EpgAssignOptions::default() },
            )
            .await;

            assert_eq!(playlist.items_mut().count(), 1);
            assert_eq!(playlist.get_group_count(), 1);
        });
    }

    #[test]
    fn clear_invalid_epg_ids_keeps_ids_assigned_by_smart_matching() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let (assigned_ids, epg) = run_xmltv_matches(
                r#"<tv>
  <channel id="tf1.fr"><display-name>TF1</display-name></channel>
  <programme start="20260425000000 +0000" stop="20260425010000 +0000" channel="tf1.fr">
    <title>Programme TF1</title>
  </programme>
</tv>"#,
                &[("TF1", None)],
                true,
                true,
            )
            .await;

            assert_eq!(assigned_ids, vec![Some("tf1.fr".intern())]);
            assert_eq!(epg[0].children.len(), 1);
            assert_eq!(epg[0].children[0].id.as_ref(), "tf1.fr");
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
                    run_xmltv_matches(xmltv, &[("Empty channel", Some("empty.fr"))], smart_matching, false).await;

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

            super::process_playlist_epg(
                &NoPlugins,
                &mut playlist,
                &mut epg,
                super::EpgAssignOptions { clear_invalid_epg_ids: false, ..super::EpgAssignOptions::default() },
            )
            .await;

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

            super::process_playlist_epg(
                &NoPlugins,
                &mut playlist,
                &mut epg,
                super::EpgAssignOptions { clear_invalid_epg_ids: false, ..super::EpgAssignOptions::default() },
            )
            .await;

            let updated = playlist.items_mut().next().unwrap();
            assert_eq!(updated.header.epg_channel_id.as_deref(), Some("f1.calendar"));
            assert_eq!(epg[0].children[0].id.as_ref(), "f1.calendar");
            assert_eq!(epg[0].children[0].programmes.len(), 1);
        });
    }

    /// A host scripted per hook, standing in for a real plugin.
    #[derive(Clone, Default)]
    struct ScriptedHost {
        /// `match-channel` answers every request with this id.
        match_answer: Option<String>,
        /// The candidate ids each `match-channel` request was shown.
        shortlists: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
        /// `on-epg-loaded` renames every channel to this.
        rename_to: Option<String>,
        /// `transform-programmes` appends this category to every programme.
        category: Option<String>,
        /// `enrich-programmes` injects a programme starting here, in the spelling `inject_as`.
        inject_start: Option<i64>,
        inject_as: String,
    }

    #[allow(clippy::unused_async_trait_impl, reason = "a scripted host answers without awaiting")]
    impl PluginHost for ScriptedHost {
        fn wants(&self, hook: PluginHook, _target: &str) -> bool {
            match hook {
                PluginHook::MatchChannel => self.match_answer.is_some(),
                PluginHook::EpgLoaded => self.rename_to.is_some(),
                PluginHook::TransformProgrammes => self.category.is_some(),
                PluginHook::EnrichProgrammes => self.inject_start.is_some(),
                _ => false,
            }
        }

        async fn match_channels(
            &self,
            ctx: &HookCtx<'_>,
            requests: &[shared::model::PluginMatchRequest<'_>],
            _report: &mut shared::model::PluginRunReport,
        ) -> Vec<Option<String>> {
            assert_eq!(ctx.target, "", "EPG hooks have an empty target");
            let mut seen = self.shortlists.lock().expect("lock");
            for request in requests {
                seen.push(request.candidates.iter().map(|candidate| candidate.id.clone()).collect());
            }
            vec![self.match_answer.clone(); requests.len()]
        }

        async fn on_epg_loaded(
            &self,
            _ctx: &HookCtx<'_>,
            channels: &mut Vec<PluginEpgChannel>,
            _report: &mut shared::model::PluginRunReport,
        ) {
            if let Some(name) = &self.rename_to {
                for channel in channels.iter_mut() {
                    channel.names = vec![name.clone()];
                }
            }
        }

        async fn transform_programmes(
            &self,
            _ctx: &HookCtx<'_>,
            programmes: &mut Vec<EpgProgramme>,
            _report: &mut shared::model::PluginRunReport,
        ) {
            if let Some(category) = &self.category {
                for programme in programmes.iter_mut() {
                    programme
                        .categories
                        .push(shared::model::EpgCategory { value: category.as_str().intern(), lang: None });
                }
            }
        }

        async fn enrich_programmes(
            &self,
            _ctx: &HookCtx<'_>,
            channels: &mut [PluginChannelProgrammes],
            _report: &mut shared::model::PluginRunReport,
        ) {
            let Some(start) = self.inject_start else {
                return;
            };
            for channel in channels.iter_mut().take(1) {
                // Appended last and under another spelling of the channel id.
                channel.programmes.push(EpgProgramme::new_all(
                    start,
                    start + 10,
                    self.inject_as.as_str().intern(),
                    Some("Injected".intern()),
                    None,
                    None,
                ));
            }
        }
    }

    /// Two guide ids that both carry the display name "TF1", so the built-in matcher
    /// has no winner for "FR|TF1": the shape a `match-channel` plugin is asked about.
    fn ambiguous_cache() -> super::EpgIdCache {
        let mut cache = smart_cache_with_country_suffix();
        cache.insert_normalized("FR|TF1", None);
        let display_name = cache.normalize_candidates(["TF1"]);
        assert!(cache.match_epg_channel_candidates(&"opaque-a".intern(), &display_name, 0, 0));
        assert!(cache.match_epg_channel_candidates(&"opaque-b".intern(), &display_name, 0, 0));
        cache.finalize_matches(&HashSet::from(["opaque-a".intern(), "opaque-b".intern()]));
        for id in ["opaque-a", "opaque-b"] {
            cache.insert_processed_epg_id(id);
            cache.register_guide_names(id, ["TF1"]);
        }
        cache
    }

    #[test]
    fn a_shortlist_holds_the_ambiguous_candidates_best_first_and_respects_the_limit() {
        let cache = ambiguous_cache();
        let key = cache.match_key("FR|TF1").expect("key");
        assert_eq!(cache.normalized.get(&key).map(Option::is_none), Some(true), "the built-in matcher left it open");

        let ids = |limit| {
            cache
                .match_shortlist(&key, &HashMap::new(), limit)
                .into_iter()
                .map(|channel| channel.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(5), ["opaque-a", "opaque-b"], "equal scores order by id");
        assert_eq!(ids(1), ["opaque-a"]);
        assert_eq!(ids(0).len(), 0);
    }

    #[test]
    fn ambiguous_candidates_stay_selected_only_while_a_match_plugin_is_asked() {
        let mut cache = ambiguous_cache();
        let available = HashSet::from(["opaque-a".intern(), "opaque-b".intern()]);
        assert!(!cache.selected_epg_ids(&available).contains("opaque-a"));

        cache.hooks.keep_match_candidates = true;
        let selected = cache.selected_epg_ids(&available);
        assert!(selected.contains("opaque-a") && selected.contains("opaque-b"));
    }

    #[test]
    fn a_plugin_answer_obeys_availability_and_the_digit_rule() {
        let mut cache = smart_cache(80, 95);
        for (id, name) in [("sport.one", "Sport 1"), ("sport.two", "Sport 2")] {
            cache.insert_processed_epg_id(id);
            cache.register_guide_names(id, [name]);
        }
        let key = cache.match_key("Sport 1").expect("key");
        let cases = [
            ("same digits, id in another case", "SPORT.ONE", true),
            ("a different digit", "sport.two", false),
            ("not an available guide channel", "sport.three", false),
        ];
        for (name, id, accepted) in cases {
            assert_eq!(cache.plugin_match_rank(&key, id).is_some(), accepted, "{name}");
        }
    }

    #[tokio::test]
    async fn the_second_pass_applies_an_answer_only_from_the_shortlist() {
        let cases = [
            ("a shortlisted id", "opaque-b", Some("opaque-b")),
            ("another case of a shortlisted id", "OPAQUE-A", Some("opaque-a")),
            ("an id outside the shortlist", "opaque-z", None),
        ];
        for (name, answer, expected) in cases {
            let host = ScriptedHost { match_answer: Some(answer.to_string()), ..ScriptedHost::default() };
            let mut cache = ambiguous_cache();
            let input = ConfigInput::from(ConfigInputDto::default());
            let groups = vec![PlaylistGroup {
                id: 1,
                title: "Live".intern(),
                channels: vec![live_playlist_item("FR|TF1", None)],
                xtream_cluster: super::XtreamCluster::Live,
            }];
            let mut playlist =
                FetchedPlaylist { input: &input, source: MemoryPlaylistSource::new(groups).into_source(), epg: None };
            let mut stats = super::EpgAssignmentStats::default();
            let mut report = shared::model::PluginRunReport::default();

            super::resolve_unresolved_with_plugins(
                &host,
                &mut playlist,
                &mut cache,
                &HashMap::new(),
                &[0],
                &mut stats,
                &mut report,
            )
            .await;

            let assigned = playlist.items_mut().next().and_then(|item| item.header.epg_channel_id.clone());
            assert_eq!(assigned.as_deref(), expected, "{name}");
            assert_eq!(
                host.shortlists.lock().expect("lock").as_slice(),
                [vec!["opaque-a".to_string(), "opaque-b".to_string()]],
                "{name}: one batched call, one request, the whole shortlist"
            );
        }
    }

    #[tokio::test]
    async fn without_a_match_plugin_the_matcher_runs_in_one_pass_as_before() {
        let xmltv = r#"<tv><channel id="tf1.fr"><display-name>TF1</display-name></channel>
<programme start="20260425000000 +0000" stop="20260425010000 +0000" channel="tf1.fr"><title>News</title></programme></tv>"#;
        let host = ScriptedHost::default();
        let (with_host, _) = run_xmltv_matches_with(&host, xmltv, &[("TF1", None)], true, false).await;
        let (plain, _) = run_smart_xmltv_matches(xmltv, &[("TF1", None)]).await;
        assert_eq!(with_host, plain);
        assert!(host.shortlists.lock().expect("lock").is_empty(), "no request was made");
    }

    #[tokio::test]
    async fn parsing_hooks_rename_channels_and_rewrite_programmes() {
        let xmltv = r#"<tv><channel id="demo.channel"><display-name>Demo</display-name></channel>
<programme start="20260425000000 +0000" stop="20260425010000 +0000" channel="demo.channel"><title>Show</title><category>News</category></programme></tv>"#;
        let host = ScriptedHost {
            rename_to: Some("Renamed".to_string()),
            category: Some("Plugin".to_string()),
            ..ScriptedHost::default()
        };
        let (_, epg) = run_xmltv_matches_with(&host, xmltv, &[("Demo", Some("demo.channel"))], false, false).await;
        let channel = &epg[0].children[0];
        assert_eq!(channel.title.as_deref(), Some("Renamed"));
        let categories: Vec<&str> = channel.programmes[0].categories.iter().map(|category| &*category.value).collect();
        assert_eq!(categories, ["News", "Plugin"]);

        let (_, epg) = run_xmltv_matches_with(&NoPlugins, xmltv, &[("Demo", Some("demo.channel"))], false, false).await;
        assert_eq!(epg[0].children[0].title.as_deref(), Some("Demo"));
        assert_eq!(epg[0].children[0].programmes[0].categories.len(), 1);
    }

    fn epg_source(channels: &[(&str, &[i64])]) -> tuliprox_core::model::Epg {
        let children = channels
            .iter()
            .map(|(id, starts)| {
                let id: Arc<str> = id.intern();
                let mut channel = EpgChannel::new(Arc::clone(&id));
                channel.programmes = starts
                    .iter()
                    .map(|&start| EpgProgramme::new_all(start, start + 10, Arc::clone(&id), None, None, None))
                    .collect();
                Arc::new(channel)
            })
            .collect();
        tuliprox_core::model::Epg { priority: 0, logo_override: false, attributes: None, children }
    }

    #[tokio::test]
    async fn enriched_programmes_come_back_sorted_with_the_hosts_channel_id() {
        let host = ScriptedHost { inject_start: Some(50), inject_as: "DEMO".to_string(), ..ScriptedHost::default() };
        let mut epg = vec![epg_source(&[("Demo", &[100, 200]), ("Other", &[300])])];
        let ctx = HookCtx::new("target");
        let mut report = shared::model::PluginRunReport::default();

        super::enrich_playlist_programmes(&host, &ctx, &mut epg, &mut report).await;

        let demo = &epg[0].children[0];
        assert_eq!(demo.programmes.iter().map(|programme| programme.start).collect::<Vec<_>>(), [50, 100, 200]);
        assert!(demo.programmes.iter().all(|programme| programme.get_transient_channel_id().as_ref() == "Demo"));
        assert_eq!(epg[0].children[1].programmes.len(), 1, "the other channel is untouched");
    }

    #[tokio::test]
    async fn without_an_enrich_plugin_the_programmes_are_not_touched() {
        let mut epg = vec![epg_source(&[("Demo", &[200, 100])])];
        let shared_channel = Arc::clone(&epg[0].children[0]);
        let mut report = shared::model::PluginRunReport::default();

        super::enrich_playlist_programmes(&NoPlugins, &HookCtx::new("target"), &mut epg, &mut report).await;

        assert!(Arc::ptr_eq(&epg[0].children[0], &shared_channel), "no copy-on-write, no reorder");
        assert_eq!(
            epg[0].children[0].programmes.iter().map(|programme| programme.start).collect::<Vec<_>>(),
            [200, 100]
        );
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

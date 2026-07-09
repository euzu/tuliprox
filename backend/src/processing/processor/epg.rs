use crate::model::Epg;
use crate::model::{EpgConfig, EpgSmartMatchConfig};
use crate::model::FetchedPlaylist;
use crate::processing::parser::xmltv::normalize_channel_name;
use log::{debug, trace, warn};
use rphonetic::{DoubleMetaphone, Encoder};
use std::collections::{HashMap, HashSet};
use shared::model::{EpgSmartMatchConfigDto, PlaylistItem, XtreamCluster};
use std::sync::Arc;
use shared::utils::Internable;

const EPG_ID_STACK_FOLD_LEN: usize = 128;

/// Runs `f` with an ASCII-lowercased EPG id.
///
/// Non-ASCII characters are left unchanged, matching `eq_ignore_ascii_case` semantics.
pub(crate) fn with_folded_epg_id<R>(id: &str, f: impl FnOnce(&str) -> R) -> R {
    if !id.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return f(id);
    }

    if id.len() <= EPG_ID_STACK_FOLD_LEN {
        let mut buf = [0_u8; EPG_ID_STACK_FOLD_LEN];
        for (out, byte) in buf.iter_mut().zip(id.bytes()) {
            *out = byte.to_ascii_lowercase();
        }
        let folded = std::str::from_utf8(&buf[..id.len()]);
        return f(folded.unwrap_or(id));
    }

    let mut folded = String::with_capacity(id.len());
    folded.extend(id.chars().map(|ch| ch.to_ascii_lowercase()));
    f(&folded)
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
            processed: HashSet::new(),
            metaphone: DoubleMetaphone::default(),
            smart_match_enabled: normalize_config.enabled,
            fuzzy_match_enabled: normalize_config.enabled && normalize_config.fuzzy_matching,
            smart_match_config: normalize_config,

        }
    }

    fn is_empty(&self) -> bool {
        self.channel_epg_id.is_empty() && self.normalized.is_empty()
    }

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

    pub fn insert_processed_epg_id(&mut self, id: &str) {
        with_folded_epg_id(id, |folded| self.processed.insert(folded.intern()));
    }

    pub fn contains_processed_epg_id(&self, id: &str) -> bool {
        with_folded_epg_id(id, |folded| self.processed.contains(folded))
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
        self.insert_normalized(name);

        if let Some(chan_epg_id) = epg_id {
            self.insert_normalized(chan_epg_id);
        }
    }
    fn insert_normalized(&mut self, key: &str) {
        let normalized = self.normalize(key).intern();
        let phonetic = self.phonetic(&normalized);

        self.normalized.insert(normalized.clone(), None);
        self.phonetics
            .entry(phonetic.clone())
            .or_default()
            .insert(normalized);
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
    fn normalize(&self, name: &str) -> String {
        normalize_channel_name(name, &self.smart_match_config)
    }

    pub(crate) fn phonetic(&self, name: &Arc<str>) -> Arc<str> {
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
            let needs_normalization = smart_match_enabled && (fuzzy_matching || missing_epg_id);

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

    pub fn match_with_normalized(&mut self, epg_id: &Arc<str>, normalized_epg_ids: &[Arc<str>]) -> bool {
        for key in normalized_epg_ids {
            if let Some(entry) = self.normalized.get_mut(key) {
                entry.replace(Arc::clone(epg_id));
                // Inline fold (mirrors `insert_channel_epg_id`); a `&mut self` call
                // here would conflict with the `entry` borrow of `self.normalized`.
                with_folded_epg_id(epg_id, |folded| self.channel_epg_id.insert(folded.intern()));
                return true;
            }
        }
        false
    }
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
    //id_cache.normalized.retain(|_, v| v.is_some());
    if let Some(tv_guide) = &fp.epg {
        if let Some((epg_source, icon_override_channels)) = tv_guide.filter_merged_with_icon_overrides(id_cache).await {
            let mut icon_assigned = HashSet::new();
            let icon_tags: HashMap<Arc<str>, &Arc<str>> = epg_source.children
                .iter()
                .filter_map(|tag| {
                    tag.icon.as_ref().filter(|icon| !icon.is_empty()).map(|icon| {
                        (with_folded_epg_id(&tag.id, |folded| folded.intern()), icon)
                    })
                })
                .collect();
            let icon_override_channels = icon_override_channels
                .into_iter()
                .map(|id| with_folded_epg_id(&id, |folded| folded.intern()))
                .collect::<HashSet<_>>();

            let assign_values = |chan: &mut PlaylistItem| {
                if id_cache.smart_match_enabled {
                    // id_cache.processed contains all epg_ids found in any xmltv source.
                    let not_found_in_epg = match &chan.header.epg_channel_id {
                        None => true,
                        Some(epg_id) => !id_cache.contains_processed_epg_id(epg_id),
                    };
                    if not_found_in_epg {
                        let try_match = |key: &str| {
                            let normalized = id_cache.normalize(key).intern();
                            id_cache.normalized.get(&normalized).and_then(|epg_id| {
                                epg_id.as_ref().map(|id| {
                                    trace!("Matched channel {} to epg {id:?}", chan.header.name);
                                    id.clone()
                                })
                            })
                        };
                        if let Some(new_id) =
                            try_match(&chan.header.name).or_else(|| chan.header.epg_channel_id.as_deref().and_then(try_match))
                        {
                            chan.header.epg_channel_id = Some(new_id);
                        }
                    }
                }
                if let Some(epg_channel_id) = chan.header.epg_channel_id.as_ref() {
                    with_folded_epg_id(epg_channel_id, |folded_epg_channel_id| {
                    if !icon_assigned.contains(folded_epg_channel_id) &&
                        (icon_override_channels.contains(folded_epg_channel_id) || chan.header.logo.is_empty() || chan.header.logo_small.is_empty()) {
                        if let Some(icon) = icon_tags.get(folded_epg_channel_id) {
                            icon_assigned.insert(folded_epg_channel_id.intern());
                            if icon_override_channels.contains(folded_epg_channel_id) || chan.header.logo.is_empty() {
                                trace!("Matched channel {} to epg icon {icon}", chan.header.name);
                                chan.header.logo = Arc::clone(icon);
                            }
                            if icon_override_channels.contains(folded_epg_channel_id) || chan.header.logo_small.is_empty() {
                                chan.header.logo_small = Arc::clone(icon);
                            }
                        }
                    }
                    });
                }
            };

            let filter_live = |c: &&mut PlaylistItem| c.header.xtream_cluster == XtreamCluster::Live && c.header.item_type.is_live();

            if fp.is_memory() {
                fp.items_mut().filter(filter_live).for_each(assign_values);
            } else {
                warn!("Disk based playlist modification is not supported!");
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
    use crate::model::{ConfigInput, EpgConfig, FetchedPlaylist, PersistedEpgSource, TVGuide};
    use crate::repository::MemoryPlaylistSource;
    use rand::distr::Alphanumeric;
    use rand::Rng;
    use rphonetic::{DoubleMetaphone, Encoder};
    use shared::model::{ConfigInputDto, PlaylistGroup, PlaylistItemHeader, PlaylistItemType};
    use shared::utils::Internable;
    use std::fs;
    use tempfile::tempdir;
    use tokio::time::Instant;

    fn random_string() -> String {
        rand::rng()
            .sample_iter(&Alphanumeric)
            .take(30)
            .map(char::from)
            .collect()
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
    fn test_phonetic() {
        let strings: Vec<String> = (0..5_000)
            .map(|_| random_string())
            .collect();

        let phonetic = DoubleMetaphone::new(Some(6));

        let now = Instant::now();
        for value in &strings {
            let _ = phonetic.encode(value);
        }

        let elapsed = now.elapsed();
        println!("Elapsed time: {}.{:03} secs", elapsed.as_secs(), elapsed.subsec_millis());
    }
}

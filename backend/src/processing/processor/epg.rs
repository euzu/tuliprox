use crate::model::{Epg, TVGuide, XmlTag, XmlTagIcon, EPG_ATTRIB_ID};
use crate::model::{EpgConfig, EpgSmartMatchConfig};
use crate::model::{FetchedPlaylist};
use crate::processing::parser::xmltv::normalize_channel_name;
use log::{debug, trace};
use rphonetic::{DoubleMetaphone, Encoder};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use shared::model::{EpgSmartMatchConfigDto, PlaylistItem, XtreamCluster};

pub struct EpgIdCache<'a> {
    pub channel_epg_id: HashSet<Cow<'a, str>>,
    pub normalized: HashMap<String, Option<String>>,
    pub phonetics: HashMap<String, HashSet<String>>,
    pub processed: HashSet<String>,
    pub smart_match_config: EpgSmartMatchConfig,
    pub metaphone: DoubleMetaphone,
    pub smart_match_enabled: bool, // smart match is enabled, normalizing names
    pub fuzzy_match_enabled: bool, // fuzzy matching enabled
}

impl EpgIdCache<'_> {
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
    fn normalize_and_store(&mut self, name: &str, epg_id: Option<&String>) {
        let normalized_name = self.normalize(name);
        let phonetic = self.phonetic(&normalized_name);
        self.normalized.insert(normalized_name.to_string(), epg_id.map(std::string::ToString::to_string));
        self.phonetics.entry(phonetic.to_string()).or_default().insert(normalized_name);
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

    pub(crate) fn phonetic(&self, name: &str) -> String {
        self.metaphone.encode(name)
    }

    pub fn collect_epg_id(&mut self, fp: &mut FetchedPlaylist) {
        let smart_match_enabled = self.smart_match_enabled;
        let fuzzy_matching = self.fuzzy_match_enabled;

        for channel in fp.playlistgroups.iter().flat_map(|g| &g.channels) {
            let mut missing_epg_id = true;
            // insert epg_id to known channel epg_ids
            if let Some(id) = channel.header.epg_channel_id.as_deref() {
                if !id.is_empty() {
                    missing_epg_id = false;
                    self.channel_epg_id.insert(Cow::Owned(id.to_string()));
                }
            }

            // for fuzzy_matching we need to put the normalized name even if there is an epg_id, because the epg_id
            // could not match to the epg file. And then we try to guess it based on normalized name
            let needs_normalization = smart_match_enabled && (fuzzy_matching || missing_epg_id);

            if needs_normalization {
                let name = &channel.header.name;
                self.normalize_and_store(name, channel.header.epg_channel_id.as_ref());
            }
        }
    }

    pub fn match_with_normalized(&mut self, epg_id: &str, normalized_epg_ids: &[String]) -> bool {
        for key in normalized_epg_ids {
            if let Some(entry) = self.normalized.get_mut(key) {
                entry.replace(epg_id.to_string());
                self.channel_epg_id.insert(epg_id.to_string().into());
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
fn assign_channel_epg(new_epg: &mut Vec<Epg>, fp: &mut FetchedPlaylist, id_cache: &mut EpgIdCache) {
    id_cache.normalized.retain(|_, v| v.is_some());
    if let Some(tv_guide) = &fp.epg {
        let mut processed_epgs = vec![];
        if let Some(epg_sources) = tv_guide.filter(id_cache) {
            let mut icon_assigned = HashSet::new();
            for epg_source in epg_sources {
                // icon tags
                let icon_tags: HashMap<&String, &XmlTag> = epg_source.children.iter()
                    .filter(|tag| tag.icon != XmlTagIcon::Undefined)
                    .filter_map(|tag| tag.get_attribute_value(EPG_ATTRIB_ID).map(|id| (id, tag)))
                    .collect();

                let assign_values = |chan: &mut PlaylistItem| {
                    if id_cache.smart_match_enabled && chan.header.epg_channel_id.is_none() {
                        // if the channel has no epg_id  or the epg_id is not present in xmltv/tvguide then we need to match one from existing tvguide
                        let not_processed = match &chan.header.epg_channel_id {
                            None => true,
                            Some(epg_id) => !id_cache.processed.contains(epg_id),
                        };
                        if not_processed {
                            let normalized = id_cache.normalize(&chan.header.name);
                            if let Some(epg_id) = id_cache.normalized.get(&normalized) {
                                if epg_id.is_some() {
                                    trace!("Matched channel {} to epg {epg_id:?}", chan.header.name);
                                    chan.header.epg_channel_id.clone_from(epg_id);
                                }
                            }
                        }
                    }
                    if let Some(epg_channel_id) = chan.header.epg_channel_id.as_ref() {
                        if !icon_assigned.contains(epg_channel_id) &&
                            (epg_source.logo_override || chan.header.logo.is_empty() || chan.header.logo_small.is_empty()) {
                            if let Some(icon_tag) = icon_tags.get(epg_channel_id) {
                                if let XmlTagIcon::Src(icon) = &icon_tag.icon {
                                    icon_assigned.insert(epg_channel_id.to_string());
                                    if epg_source.logo_override || chan.header.logo.is_empty() {
                                        trace!("Matched channel {} to epg icon {icon}", chan.header.name);
                                        chan.header.logo = (*icon).to_string();
                                    }
                                    if epg_source.logo_override || chan.header.logo_small.is_empty() {
                                        chan.header.logo_small = (*icon).to_string();
                                    }
                                }
                            }
                        }
                    }
                };

                let filter_live = |c: &&mut PlaylistItem| c.header.xtream_cluster == XtreamCluster::Live;
                fp.playlistgroups.iter_mut()
                    .flat_map(|g| &mut g.channels)
                    .filter(filter_live)
                    .for_each(assign_values);
                processed_epgs.push(epg_source);
            }
        }

        if let Some(epg) = TVGuide::merge(processed_epgs) {
            new_epg.push(epg);
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
pub fn process_playlist_epg(fp: &mut FetchedPlaylist, epg: &mut Vec<Epg>) {
    // collect all epg_channel ids
    let mut id_cache = EpgIdCache::new(fp.input.epg.as_ref());
    id_cache.collect_epg_id(fp);

    if id_cache.is_empty() && !id_cache.smart_match_enabled {
        debug!("No epg ids found");
    } else {
        assign_channel_epg(epg, fp, &mut id_cache);
    }
}


#[cfg(test)]
mod tests {
    use rand::distr::Alphanumeric;
    use rand::Rng;
    use rphonetic::{DoubleMetaphone, Encoder};
    use tokio::time::Instant;

    fn random_string() -> String {
        rand::rng()
            .sample_iter(&Alphanumeric)
            .take(30)
            .map(char::from)
            .collect()
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
use crate::model::{Epg, TVGuide, XmlTag, XmlTagIcon, EPG_ATTRIB_ID};
use crate::model::{EpgConfig, EpgSmartMatchConfig};
use crate::model::FetchedPlaylist;
use crate::processing::parser::xmltv::normalize_channel_name;
use log::{debug, trace, warn};
use rphonetic::{DoubleMetaphone, Encoder};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use shared::model::{EpgSmartMatchConfigDto, PlaylistItem, XtreamCluster};
use std::sync::Arc;

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
    fn normalize_and_store(&mut self, name: &str, epg_id: Option<&str>) {
        self.insert_normalized(name);

        if let Some(chan_epg_id) = epg_id {
            self.insert_normalized(chan_epg_id);
        }
    }
    fn insert_normalized(&mut self, key: &str) {
        let normalized = self.normalize(key);
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

    pub(crate) fn phonetic(&self, name: &str) -> String {
        let result = self.metaphone.encode(name);
        if result.is_empty() {
            name.to_owned()
        } else {
            result
        }
    }

    pub fn collect_epg_id(&mut self, fp: &mut FetchedPlaylist) {
        let smart_match_enabled = self.smart_match_enabled;
        let fuzzy_matching = self.fuzzy_match_enabled;

        // Helper closure to process a single item
        // We use a closure here to capture `self` and avoid code duplication
        let mut process_item = |name: &str, epg_channel_id: Option<&str>| {
            let mut missing_epg_id = true;
            // insert epg_id to known channel epg_ids
            if let Some(id) = epg_channel_id {
                if !id.is_empty() {
                    missing_epg_id = false;
                    self.channel_epg_id.insert(Cow::Owned(id.to_string()));
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
                process_item(&channel.header.name, channel.header.epg_channel_id.as_deref());
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
async fn assign_channel_epg(new_epg: &mut Vec<Epg>, fp: &mut FetchedPlaylist<'_>, id_cache: &mut EpgIdCache<'_>) {
    //id_cache.normalized.retain(|_, v| v.is_some());
    if let Some(tv_guide) = &fp.epg {
        let mut processed_epgs = vec![];
        if let Some(epg_sources) = tv_guide.filter(id_cache).await {
            let mut icon_assigned = HashSet::new();
            for epg_source in epg_sources {
                // icon tags
                let icon_tags: HashMap<&str, &Arc<XmlTag>> = epg_source.children.iter()
                    .filter(|tag| tag.icon != XmlTagIcon::Undefined)
                    .filter_map(|tag| tag.get_attribute_value(EPG_ATTRIB_ID).map(|id| (id.as_str(), tag)))
                    .collect();

                let assign_values = |chan: &mut PlaylistItem| {
                    if id_cache.smart_match_enabled {
                        // id_cache.processed contains the epg_ids from the xml epg file.
                        // if the channel has no epg_id or the epg_id is not present in xmltv/tvguide then we need to match one from existing tvguide
                        let not_found_in_epg = match &chan.header.epg_channel_id {
                            None => true,
                            Some(epg_id) => !id_cache.processed.contains(&**epg_id),
                        };
                        if not_found_in_epg {
                            let try_match = |key: &str| {
                                let normalized = id_cache.normalize(key);
                                id_cache.normalized.get(&normalized).and_then(|epg_id| {
                                    epg_id.as_ref().map(|id| {
                                        trace!("Matched channel {} to epg {id:?}", chan.header.name);
                                        id.clone()
                                    })
                                })
                            };
                            if let Some(new_id) = try_match(&chan.header.name)
                                .or_else(|| chan.header.epg_channel_id.as_deref().and_then(try_match))
                            {
                                chan.header.epg_channel_id = Some(new_id.into());
                            }
                        }
                    }
                    if let Some(epg_channel_id) = chan.header.epg_channel_id.as_ref() {
                        if !icon_assigned.contains(epg_channel_id) &&
                            (epg_source.logo_override || chan.header.logo.is_empty() || chan.header.logo_small.is_empty()) {
                            if let Some(icon_tag) = icon_tags.get(&**epg_channel_id) {
                                if let XmlTagIcon::Src(icon) = &icon_tag.icon {
                                    icon_assigned.insert(epg_channel_id.clone());
                                    if epg_source.logo_override || chan.header.logo.is_empty() {
                                        trace!("Matched channel {} to epg icon {icon}", chan.header.name);
                                        chan.header.logo = icon.clone().into();
                                    }
                                    if epg_source.logo_override || chan.header.logo_small.is_empty() {
                                        chan.header.logo_small = icon.clone().into();
                                    }
                                }
                            }
                        }
                    }
                };

                let filter_live = |c: &&mut PlaylistItem| c.header.xtream_cluster == XtreamCluster::Live && c.header.item_type.is_live();

                if fp.is_memory() {
                    fp.items_mut().filter(filter_live).for_each(assign_values);
                } else {
                    warn!("Disk based playlist modification is not supported!");
                }
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
pub async fn process_playlist_epg(fp: &mut FetchedPlaylist<'_>, epg: &mut Vec<Epg>) {
    if fp.input.epg.is_none() {
        return;
    }
    // collect all epg_channel ids
    let mut id_cache = EpgIdCache::new(fp.input.epg.as_ref());
    id_cache.collect_epg_id(fp);

    if id_cache.is_empty() && !id_cache.smart_match_enabled {
        debug!("No epg ids found");
    } else {
        assign_channel_epg(epg, fp, &mut id_cache).await;
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
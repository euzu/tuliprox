use crate::model::{Epg, TVGuide, XmlTag, XmlTagIcon, EPG_ATTRIB_CHANNEL, EPG_ATTRIB_ID, EPG_TAG_CHANNEL, EPG_TAG_DISPLAY_NAME, EPG_TAG_ICON, EPG_TAG_PROGRAMME, EPG_TAG_TV};
use crate::model::{EpgSmartMatchConfig, PersistedEpgSource};
use crate::processing::processor::EpgIdCache;
use crate::utils::compressed_file_reader_async::CompressedFileReaderAsync;
use crate::utils::{async_file_reader, parse_xmltv_time};
use log::error;
use quick_xml::events::{BytesStart, BytesText, Event};
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use shared::concat_string;
use shared::model::{EpgChannel, EpgNamePrefix, EpgProgramme};
use shared::utils::{deunicode_string, Internable, CONSTANTS};
use std::borrow::Cow;
use std::cmp::min;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::io::AsyncRead;

/// Splits a string at the first delimiter if the prefix matches a known country code.
///
/// Returns a tuple containing the country code prefix (if found) and the remainder of the string, both trimmed. If no valid prefix is found, returns `None` and the original input.
///
/// # Examples
///
/// ```
/// let delimiters = vec!['.', '-', '_'];
/// let (prefix, rest) = split_by_first_match("US.HBO", &delimiters);
/// assert_eq!(prefix, Some("US"));
/// assert_eq!(rest, "HBO");
///
/// let (prefix, rest) = split_by_first_match("HBO", &delimiters);
/// assert_eq!(prefix, None);
/// assert_eq!(rest, "HBO");
/// ```
fn split_by_first_match<'a>(input: &'a str, delimiters: &[char]) -> (Option<&'a str>, &'a str) {
    let content = input.trim_start_matches(|c: char| !c.is_alphanumeric());

    for delim in delimiters {
        if let Some(index) = content.find(*delim) {
            let (left, right) = content.split_at(index);
            let right = &right[delim.len_utf8()..].trim();
            if !right.is_empty() {
                let prefix = left.trim();
                if CONSTANTS.country_codes.contains(&prefix) {
                    return (Some(prefix), right.trim());
                }
            }
        }
    }
    (None, input)
}


fn name_prefix<'a>(name: &'a str, smart_config: &EpgSmartMatchConfig) -> (&'a str, Option<&'a str>) {
    if smart_config.name_prefix != EpgNamePrefix::Ignore {
        let (prefix, suffix) = split_by_first_match(name, &smart_config.name_prefix_separator);
        if prefix.is_some() {
            return (suffix, prefix);
        }
    }
    (name, None)
}

fn combine(join: &str, left: &str, right: &str) -> String {
    let mut combined = String::with_capacity(left.len() + join.len() + right.len());
    combined.push_str(left);
    combined.push_str(join);
    combined.push_str(right);
    combined
}

/// # Panics
pub fn normalize_channel_name(name: &str, normalize_config: &EpgSmartMatchConfig) -> String {
    let normalized = deunicode_string(name.trim()).to_lowercase();
    let (channel_name, suffix) = name_prefix(&normalized, normalize_config);
    // Remove all non-alphanumeric characters (except dashes and underscores).
    let cleaned_name = normalize_config.normalize_regex.replace_all(channel_name, "");
    // Remove terms like resolution
    let cleaned_name = normalize_config.strip.iter().fold(cleaned_name.to_string(), |acc, term| {
        acc.replace(term, "")
    });
    match suffix {
        None => cleaned_name,
        Some(sfx) => {
            match &normalize_config.name_prefix {
                EpgNamePrefix::Ignore => cleaned_name,
                EpgNamePrefix::Suffix(sep) => combine(sep, &cleaned_name, sfx),
                EpgNamePrefix::Prefix(sep) => combine(sep, sfx, &cleaned_name),
            }
        }
    }
}


impl TVGuide {
    pub fn merge(epgs: Vec<Epg>) -> Option<Epg> {
        if let Some(first_epg) = epgs.first() {
            let first_epg_attributes = first_epg.attributes.clone();
            let merged_children: Vec<Arc<EpgChannel>> = epgs.into_iter().flat_map(|epg| epg.children).collect();
            Some(Epg {
                logo_override: false,
                priority: 0,
                attributes: first_epg_attributes,
                children: merged_children,
            })
        } else {
            None
        }
    }

    fn prepare_tag(id_cache: &mut EpgIdCache, tag: &mut XmlTag, smart_match: bool) {
        {
            let maybe_epg_id = {
                tag.get_attribute_value(&EPG_ATTRIB_ID.intern()).cloned()
            };
            if let Some(epg_id) = maybe_epg_id {
                tag.normalized_epg_ids
                    .get_or_insert_with(Vec::new)
                    .push(normalize_channel_name(&epg_id, &id_cache.smart_match_config).intern());
            }
        }

        if let Some(children) = &tag.children {
            let src = "src".intern();
            for child in children {
                match child.name.as_ref() {
                    EPG_TAG_DISPLAY_NAME if smart_match => {
                            if let Some(name) = &child.value {
                                tag.normalized_epg_ids
                                    .get_or_insert_with(Vec::new)
                                    .push(normalize_channel_name(name, &id_cache.smart_match_config).intern());
                            }
                    }
                    EPG_TAG_ICON => {
                        if let Some(src) = child.get_attribute_value(&src) {
                            if !src.is_empty() {
                                tag.icon = XmlTagIcon::Src(src.clone());
                                // We cannot easily modify the child icon since it's inside Arc,
                                // but we already set the tag.icon, which is what matters.
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn try_fuzzy_matching(id_cache: &mut EpgIdCache, epg_id: &Arc<str>, tag: &XmlTag, fuzzy_matching: bool) -> bool {
        let mut matched = tag
            .normalized_epg_ids
            .as_ref()
            .is_some_and(|ids| id_cache.match_with_normalized(epg_id, ids));
        if !matched && fuzzy_matching {
            let (fuzzy_matched, matched_normalized_name) = Self::find_best_fuzzy_match(id_cache, tag);
            if fuzzy_matched {
                if let Some(key) = matched_normalized_name {
                    id_cache.normalized.entry(key).and_modify(|entry| {
                        entry.replace(epg_id.clone());
                        id_cache.channel_epg_id.insert(epg_id.clone());
                        matched = true;
                    });
                }
            }
        }
        matched
    }

    /// Finds the best fuzzy match for a channel's normalized EPG ID using phonetic encoding and Jaro-Winkler similarity.
    ///
    /// Iterates over the tag's normalized EPG IDs, computes their phonetic codes, and searches for candidates in the phonetics map.
    /// For each candidate, calculates the Jaro-Winkler similarity score and tracks the best match above the configured threshold.
    /// Returns a tuple indicating whether a suitable match was found and the matched normalized EPG ID if available.
    ///
    /// # Returns
    ///
    /// A tuple where the first element is `true` if a match above the threshold was found, and the second element is the matched normalized EPG ID.
    ///
    /// # Examples
    ///
    /// ```
    /// let (found, matched) = find_best_fuzzy_match(&mut id_cache, &tag);
    /// if found {
    ///     println!("Best match: {:?}", matched);
    /// }
    /// ```
    fn find_best_fuzzy_match(id_cache: &mut EpgIdCache, tag: &XmlTag) -> (bool, Option<Arc<str>>) {
        let match_threshold = id_cache.smart_match_config.match_threshold;
        let best_match_threshold = id_cache.smart_match_config.best_match_threshold;

        let Some(normalized_epg_ids) = tag.normalized_epg_ids.as_ref() else {
            return (false, None);
        };

        // 1) Precalculation: (tag_normalized, tag_code)
        let pre: Vec<(Arc<str>, Arc<str>)> = normalized_epg_ids
            .iter()
            .map(|tn| (tn.clone(), id_cache.phonetic(tn)))
            .collect();

        // 2) Early exit if match >= best_match_threshold
        for (tag_normalized, tag_code) in &pre {
            if let Some(candidates) = id_cache.phonetics.get(tag_code) {
                if let Some(good_enough) = candidates.par_iter().find_any(|norm_key| {
                    let jw = strsim::jaro_winkler(norm_key, tag_normalized);
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let score = min(100, (jw * 100.0).round() as u16);
                    score >= best_match_threshold
                }) {
                    return (true, Some(good_enough.clone()));
                }
            }
        }

        // 3) No full match: find best match with match_threshold
        let best = pre
            .par_iter()
            .filter_map(|(tag_normalized, tag_code)| {
                id_cache.phonetics.get(tag_code).map(|candidates| {
                    candidates
                        .par_iter()
                        .map(|norm_key| {
                            let jw = strsim::jaro_winkler(norm_key, tag_normalized);
                            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                            let score = min(100, (jw * 100.0).round() as u16);
                            (score, norm_key)
                        })
                        .reduce_with(|a, b| if a.0 >= b.0 { a } else { b })
                })
            })
            .flatten()
            .reduce_with(|a, b| if a.0 >= b.0 { a } else { b });

        if let Some((score, best_key)) = best {
            if score >= match_threshold {
                return (true, Some(Arc::clone(best_key)));
            }
        }

        (false, None)
    }

    /// Parses and filters a compressed EPG XML file, extracting relevant channel and program tags based on smart and fuzzy matching criteria.
    ///
    /// Returns an `Epg` containing filtered tags and TV attributes if any matching channels are found; otherwise, returns `None`.
    /// The returned `Epg` will include the priority from the source, which is used for merging multiple EPG sources.
    ///
    /// # Examples
    ///
    /// ```
    /// let mut id_cache = EpgIdCache::default();
    /// let epg_source = PersistedEpgSource { file_path: Path::new("guide.xml.gz"), priority: 0 };
    /// if let Some(epg) = process_epg_file(&mut id_cache, &epg_source) {
    ///     assert!(!epg.children.is_empty());
    /// }
    /// ```
    async fn process_epg_file(id_cache: &mut EpgIdCache, epg_source: &PersistedEpgSource) -> Option<Epg> {
        let epg_attrib_id = EPG_ATTRIB_ID.intern();
        let epg_attrib_channel = EPG_ATTRIB_CHANNEL.intern();
        let start_attrib = "start".intern();
        let stop_attrib = "stop".intern();
        let tag_title = "title".intern();
        let tag_desc = "desc".intern();

        match CompressedFileReaderAsync::new(&epg_source.file_path).await {
            Ok(mut reader) => {
                let mut children: HashMap<Arc<str>, EpgChannel> = HashMap::with_capacity(5000);
                let mut tv_attributes: Option<HashMap<Arc<str>, Arc<str>>> = None;
                let smart_match = id_cache.smart_match_config.enabled;
                let fuzzy_matching = smart_match && id_cache.smart_match_config.fuzzy_matching;
                let mut filter_tags = |mut tag: XmlTag| {
                    match tag.name.as_ref() {
                        EPG_TAG_CHANNEL => {
                            let tag_epg_id = tag.get_attribute_value(&epg_attrib_id).map_or_else(|| "".intern(), Internable::intern);
                            if !tag_epg_id.is_empty() && !id_cache.processed.contains(&tag_epg_id) {
                                Self::prepare_tag(id_cache, &mut tag, smart_match);
                                let mut add_channel = false;
                                if smart_match {
                                    if Self::try_fuzzy_matching(id_cache, &tag_epg_id, &tag, fuzzy_matching) {
                                        add_channel = true;
                                    }
                                } else if id_cache.channel_epg_id.contains(&tag_epg_id) {
                                    add_channel = true;
                                }

                                if add_channel && !children.contains_key(&tag_epg_id) {
                                    let display_name = tag.children.as_ref().and_then(|children| {
                                        children.iter()
                                            .find(|c| c.name.as_ref() == EPG_TAG_DISPLAY_NAME)
                                            .and_then(|c| c.value.clone())
                                    });
                                    children.insert(Arc::clone(&tag_epg_id), EpgChannel {
                                        id: Arc::clone(&tag_epg_id),
                                        title: display_name,
                                        icon: if let XmlTagIcon::Src(src) = &tag.icon { Some(Arc::clone(src)) } else { None },
                                        programmes: vec![],
                                    });
                                    id_cache.processed.insert(tag_epg_id);
                                }
                            }
                        }
                        EPG_TAG_PROGRAMME => {
                            if let Some(epg_id) = tag.get_attribute_value(&epg_attrib_channel) {
                                if id_cache.processed.contains(epg_id) /*&& id_cache.channel_epg_id.contains(epg_id) */{
                                    if let Some(channel) = children.get_mut(epg_id) {
                                        if let Some((Some(start), Some(stop))) = tag.attributes.as_ref().map(|a| (a.get(&start_attrib), a.get(&stop_attrib))) {
                                            if let (Some(start_time), Some(stop_time)) = (parse_xmltv_time(start), parse_xmltv_time(stop)) {
                                                let mut title = None;
                                                let mut desc = None;
                                                if let Some(children) = tag.children.as_ref() {
                                                    for child in children {
                                                        if child.name == tag_title {
                                                            title.clone_from(&child.value);
                                                        } else if child.name == tag_desc {
                                                            desc.clone_from(&child.value);
                                                        }
                                                    }
                                                    channel.programmes.push(EpgProgramme::new_all(start_time, stop_time, Arc::clone(epg_id), title, desc));
                                                }
                                            } else {
                                                error!("Failed to parse epg programme time {start} - {stop}");
                                            }
                                        } else {
                                            error!("Missing start or stop attribute in programme tag, skipping");
                                        }
                                    } else {
                                        error!("Channel {epg_id} not found in EPG, dangling programme");
                                    }
                                }
                            }
                        }
                        EPG_TAG_TV => {
                            tv_attributes.clone_from(&tag.attributes);
                        }
                        _ => {}
                    }
                };

                parse_tvguide(&mut reader, &mut filter_tags).await;

                if children.is_empty() {
                    return None;
                }

                Some(Epg {
                    logo_override: epg_source.logo_override,
                    priority: epg_source.priority,
                    attributes: tv_attributes,
                    children: children.into_values().map(Arc::new).collect(),
                })
            }
            Err(e) => {
                log::warn!("Failed to process EPG file {}: {e}", epg_source.file_path.display());
                None
            }
        }
    }

    pub async fn filter(&self, id_cache: &mut EpgIdCache) -> Option<Vec<Epg>> {
        if id_cache.channel_epg_id.is_empty() && id_cache.normalized.is_empty() {
            return None;
        }
        let mut epg_sources: Vec<Epg> = vec![];
        for epg_source in self.get_epg_sources() {
            if let Some(epg) = Self::process_epg_file(id_cache, epg_source).await {
                epg_sources.push(epg);
            }
        }
        epg_sources.sort_by_key(|a| a.priority);
        Some(epg_sources)
    }
}

fn handle_tag_start<F>(callback: &mut F, stack: &mut Vec<XmlTag>, e: &BytesStart)
where
    F: FnMut(XmlTag),
{
    let binding = e.name();
    let name_raw = String::from_utf8_lossy(binding.as_ref());
    let name = name_raw.intern();
    let tag_type = get_tag_type(&name);
    let attributes = collect_tag_attributes(e, tag_type);
    let attribs = if attributes.is_empty() { None } else { Some(attributes) };
    let tag = XmlTag::new(name, attribs);

    if tag_type.is_tv() {
        callback(tag);
    } else {
        stack.push(tag);
    }
}

fn handle_tag_end<F>(callback: &mut F, stack: &mut Vec<XmlTag>)
where
    F: FnMut(XmlTag),
{
    if !stack.is_empty() {
        if let Some(tag) = stack.pop() {
            if tag.name.as_ref() == EPG_TAG_CHANNEL {
                if let Some(chan_id) = tag.get_attribute_value(&EPG_ATTRIB_ID.intern()) {
                    if !chan_id.is_empty() {
                        callback(tag);
                    }
                }
            } else if tag.name.as_ref() == EPG_TAG_PROGRAMME {
                if let Some(chan_id) = tag.get_attribute_value(&EPG_ATTRIB_CHANNEL.intern()) {
                    if !chan_id.is_empty() {
                        callback(tag);
                    }
                }
            } else if !stack.is_empty() {
                let tag_arc = Arc::new(tag);
                if let Some(mut parent) = stack.pop() {
                    parent.children.get_or_insert_with(Vec::new).push(tag_arc);
                    stack.push(parent);
                }
            }
        }
    }
}

fn handle_text_tag(stack: &mut [XmlTag], e: &BytesText) {
    if let Some(tag) = stack.last_mut() {
        if let Ok(text) = e.decode() {
            let t = text.trim();
            if !t.is_empty() {
                let t_fixed: Cow<str> = if t.ends_with('\\') {
                    let mut owned = t.to_string();
                    owned.pop();
                    owned.push_str("&apos; ");
                    Cow::Owned(owned)
                } else {
                    Cow::Borrowed(t)
                };

                tag.value = Some(match tag.value.take() {
                    None => t_fixed.intern(),
                    Some(old) => concat_string!(old.as_ref(), t_fixed.as_ref()).intern(),
                });
            }
        }
    }
}

pub async fn parse_tvguide<R, F>(content: R, callback: &mut F)
where
    R: AsyncRead + Unpin,
    F: FnMut(XmlTag),
{
    let mut stack: Vec<XmlTag> = vec![];
    let mut xml_reader = quick_xml::reader::Reader::from_reader(async_file_reader(content));
    let mut buf = Vec::<u8>::new();
    loop {
        match xml_reader.read_event_into_async(&mut buf).await {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => handle_tag_start(callback, &mut stack, &e),
            Ok(Event::Empty(e)) => {
                handle_tag_start(callback, &mut stack, &e);
                handle_tag_end(callback, &mut stack);
            }
            Ok(Event::End(_e)) => handle_tag_end(callback, &mut stack),
            Ok(Event::Text(e)) => handle_text_tag(&mut stack, &e),
            _ => {}
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
enum XmlTagType {
    Ignored,
    Tv,
    Channel,
    Programme,
}

impl XmlTagType {
    #[inline]
    pub(crate) fn is_tv(self) -> bool {
        self == XmlTagType::Tv
    }

    #[inline]
    pub(crate) fn is_channel(self) -> bool {
        self == XmlTagType::Channel
    }

    #[inline]
    pub(crate) fn is_program(self) -> bool {
        self == XmlTagType::Programme
    }
}

fn get_tag_type(name: &str) -> XmlTagType {
    match name {
        EPG_TAG_TV => XmlTagType::Tv,
        EPG_TAG_CHANNEL => XmlTagType::Channel,
        EPG_TAG_PROGRAMME => XmlTagType::Programme,
        _ => XmlTagType::Ignored
    }
}

fn collect_tag_attributes(e: &BytesStart, tag_type: XmlTagType) -> HashMap<Arc<str>, Arc<str>> {
    let attributes = e.attributes().filter_map(Result::ok)
        .filter_map(|a| {
            let key_binding = a.key;
            let key_raw = String::from_utf8_lossy(key_binding.as_ref());
            let key = key_raw.intern();
            if let Ok(value) = a.unescape_value().as_ref() {
                if value.is_empty() {
                    None
                } else if (tag_type.is_channel() && key.as_ref() == EPG_ATTRIB_ID) || (tag_type.is_program() && key.as_ref() == EPG_ATTRIB_CHANNEL) {
                    Some((key, value.to_lowercase().intern()))
                } else {
                    Some((key, value.intern()))
                }
            } else {
                None
            }
        }).collect::<HashMap<Arc<str>, Arc<str>>>();
    attributes
}

#[derive(Hash, Eq, PartialEq)]
struct ProgrammeKey {
    start: i64,
    stop: i64,
}

impl From<&EpgProgramme> for ProgrammeKey {
    fn from(p: &EpgProgramme) -> Self {
        Self {
            start: p.start,
            stop: p.stop,
        }
    }
}

struct ChannelAcc {
    priority: i16,
    channel: EpgChannel,
    programmes: HashSet<ProgrammeKey>,
}

pub fn flatten_tvguide(mut tv_guides: Vec<Epg>) -> Option<Epg> {
    if tv_guides.is_empty() {
        return None;
    }

    let epg_attributes = tv_guides
        .first()
        .and_then(|t| t.attributes.clone());

    let mut channels: HashMap<Arc<str>, ChannelAcc> = HashMap::new();

    for guide in tv_guides.drain(..) {
        for channel_arc in guide.children {
            let Ok(mut channel) = Arc::try_unwrap(channel_arc) else {
                error!("Failed to unwrap epg channel");
                continue;
            };
            match channels.entry(Arc::clone(&channel.id)) {
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    let acc = entry.get_mut();

                    if guide.priority < acc.priority {
                        // high priority
                        acc.priority = guide.priority;
                        acc.channel = channel;

                        acc.programmes.clear();
                        for p in &acc.channel.programmes {
                            acc.programmes.insert(ProgrammeKey::from(p));
                        }
                    } else if guide.priority == acc.priority {
                        // same priority → merge
                        for p in channel.programmes.drain(..) {
                            let key = ProgrammeKey::from(&p);
                            if acc.programmes.insert(key) {
                                acc.channel.programmes.push(p);
                            }
                        }
                    }
                }

                std::collections::hash_map::Entry::Vacant(entry) => {
                    let mut set = HashSet::new();
                    for p in &channel.programmes {
                        set.insert(ProgrammeKey::from(p));
                    }

                    entry.insert(ChannelAcc {
                        priority: guide.priority,
                        channel,
                        programmes: set,
                    });
                }
            }
        }
    }

    let children = channels
        .into_values()
        .map(|acc| Arc::new(acc.channel))
        .collect();

    Some(Epg {
        logo_override: false,
        priority: 0,
        attributes: epg_attributes,
        children,
    })
}

#[cfg(test)]
mod tests {
    use crate::model::{EpgSmartMatchConfig, PersistedEpgSource, TVGuide};
    use crate::processing::parser::xmltv::normalize_channel_name;
    use std::collections::HashSet;
    use std::path::PathBuf;

    #[test]
    /// Tests normalization of a channel name using the default smart match configuration.
    ///
    /// # Examples
    ///
    /// ```
    /// parse_normalize().unwrap();
    /// ```
    fn parse_normalize() {
        let epg_normalize_dto = EpgSmartMatchConfigDto { ..Default::default() };
        let epg_normalize = EpgSmartMatchConfig::from(epg_normalize_dto);
        let normalized = normalize_channel_name("Love Nature", &epg_normalize);
        assert_eq!(normalized, "lovenature".to_string());
    }


    #[ignore = "requires a local XMLTV fixture under /tmp"]
    #[test]
    fn parse_test() {
        let run_test = async move || {
            //let file_path = PathBuf::from("/tmp/epg.xml.gz");
            let file_path = PathBuf::from("/tmp/invalid_epg.xml");

            if file_path.exists() {
                let tv_guide = TVGuide::new(vec![PersistedEpgSource { file_path, priority: 0, logo_override: false }]);

                let mut id_cache = EpgIdCache::new(None);
                id_cache.channel_epg_id.insert(342u32.intern());
                //id_cache.collect_epg_id(fp);

                let channel_ids = HashSet::from([342u32.intern()]);
                match tv_guide.filter(&mut id_cache).await {
                    None => panic!("No epg filtered"),
                    Some(epgs) => {
                        for epg in epgs {
                            assert_eq!(epg.children.len(), channel_ids.len() * 2, "Epg size does not match");
                        }
                    }
                }
            }
        };
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(run_test());
    }

    #[test]
    /// Tests normalization of channel names with various prefixes, suffixes, and special characters using a configured `EpgSmartMatchConfig`.
    ///
    /// # Examples
    ///
    /// ```
    /// normalize();
    /// // This will assert that various channel names are normalized as expected.
    /// ```
    fn normalize() {
        let mut epg_smart_cfg_dto = EpgSmartMatchConfigDto { enabled: true, name_prefix: EpgNamePrefix::Suffix(".".to_string()), ..Default::default() };
        let _ = epg_smart_cfg_dto.prepare();
        let epg_smart_cfg = EpgSmartMatchConfig::from(epg_smart_cfg_dto);
        println!("{epg_smart_cfg:?}");
        assert_eq!("supersport6.ru", normalize_channel_name("RU: SUPERSPORT 6 ᴿᴬᵂ", &epg_smart_cfg));
        assert_eq!("odisea.sat", normalize_channel_name("SAT: ODISEA ᴿᴬᵂ", &epg_smart_cfg));
        assert_eq!("odisea.4k", normalize_channel_name("4K: ODISEA ᵁᴴᴰ ³⁸⁴⁰ᴾ", &epg_smart_cfg));
        assert_eq!("odisea", normalize_channel_name("ODISEA ᵁᴴᴰ ³⁸⁴⁰ᴾ", &epg_smart_cfg));
        assert_eq!("odisea.bu", normalize_channel_name("BU | ODISEA ᵁᴴᴰ ³⁸⁴⁰ᴾ", &epg_smart_cfg));
        assert_eq!("odisea.bg", normalize_channel_name("BG | ODISEA ᵁᴴᴰ ³⁸⁴⁰ᴾ", &epg_smart_cfg));
    }

    use crate::processing::processor::EpgIdCache;
    use rphonetic::{Encoder, Metaphone};
    use shared::model::{EpgNamePrefix, EpgSmartMatchConfigDto};
    use shared::utils::Internable;

    #[test]
    /// Demonstrates phonetic encoding (Metaphone) of normalized channel names with various prefixes and suffixes.
    ///
    /// This test prints the Metaphone-encoded representations of several normalized channel names using a configured `EpgSmartMatchConfig`.
    ///
    /// # Examples
    ///
    /// ```
    /// test_metaphone();
    /// // Output will show the Metaphone encodings for different channel name variants.
    /// ```
    fn test_metaphone() {
        let metaphone = Metaphone::default();
        let mut epg_smart_cfg_dto = EpgSmartMatchConfigDto { enabled: true, name_prefix: EpgNamePrefix::Suffix(".".to_string()), ..Default::default() };
        let _ = epg_smart_cfg_dto.prepare();
        let epg_smart_cfg = EpgSmartMatchConfig::from(epg_smart_cfg_dto);
        println!("{epg_smart_cfg:?}");
        // assert_eq!("supersport6.ru", metaphone.encode(&normalize_channel_name("RU: SUPERSPORT 6 ᴿᴬᵂ", &epg_normalize_cfg)));
        // assert_eq!("odisea.sat", metaphone.encode(&normalize_channel_name("SAT: ODISEA ᴿᴬᵂ", &epg_normalize_cfg)));
        // assert_eq!("odisea", metaphone.encode(&normalize_channel_name("4K: ODISEA ᵁᴴᴰ ³⁸⁴⁰ᴾ", &epg_normalize_cfg)));
        // assert_eq!("odisea", metaphone.encode(&normalize_channel_name("ODISEA ᵁᴴᴰ ³⁸⁴⁰ᴾ", &epg_normalize_cfg)));
        // assert_eq!("odisea.bu", metaphone.encode(&normalize_channel_name("BU | ODISEA ᵁᴴᴰ ³⁸⁴⁰ᴾ", &epg_normalize_cfg)));
        // assert_eq!("odisea.bg", metaphone.encode(&normalize_channel_name("BG | ODISEA ᵁᴴᴰ ³⁸⁴⁰ᴾ", &epg_normalize_cfg)));

        println!("{}", metaphone.encode(&normalize_channel_name("RU: SUPERSPORT 6 ᴿᴬᵂ", &epg_smart_cfg)));
        println!("{}", metaphone.encode(&normalize_channel_name("SAT: ODISEA ᴿᴬᵂ", &epg_smart_cfg)));
        println!("{}", metaphone.encode(&normalize_channel_name("4K: ODISEA ᵁᴴᴰ ³⁸⁴⁰ᴾ", &epg_smart_cfg)));
        println!("{}", metaphone.encode(&normalize_channel_name("ODISEA ᵁᴴᴰ ³⁸⁴⁰ᴾ", &epg_smart_cfg)));
        println!("{}", metaphone.encode(&normalize_channel_name("BU | ODISEA ᵁᴴᴰ ³⁸⁴⁰ᴾ", &epg_smart_cfg)));
        println!("{}", metaphone.encode(&normalize_channel_name("BG | ODISEA ᵁᴴᴰ ³⁸⁴⁰ᴾ", &epg_smart_cfg)));
    }
}

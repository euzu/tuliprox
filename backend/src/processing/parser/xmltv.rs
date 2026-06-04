use crate::model::{
    Epg, TVGuide, XmlTag, XmlTagIcon, EPG_ATTRIB_CHANNEL, EPG_ATTRIB_ID, EPG_TAG_CHANNEL, EPG_TAG_DISPLAY_NAME,
    EPG_TAG_ICON, EPG_TAG_PROGRAMME, EPG_TAG_TV,
};
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
        flatten_tvguide(epgs)
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

    fn channel_display_name(tag: &XmlTag) -> Option<Arc<str>> {
        tag.children.as_ref().and_then(|children| {
            children
                .iter()
                .find(|child| child.name.as_ref() == EPG_TAG_DISPLAY_NAME)
                .and_then(|child| child.value.clone())
        })
    }

    fn channel_icon(tag: &XmlTag) -> Option<Arc<str>> {
        match &tag.icon {
            XmlTagIcon::Src(src) => Some(Arc::clone(src)),
            XmlTagIcon::Undefined | XmlTagIcon::Exists => None,
        }
    }

    fn extract_programme(
        tag: &XmlTag,
        epg_id: &Arc<str>,
        start_attrib: &Arc<str>,
        stop_attrib: &Arc<str>,
        catchup_id_attrib: &Arc<str>,
        tag_title: &Arc<str>,
        tag_desc: &Arc<str>,
    ) -> Option<EpgProgramme> {
        let Some((Some(start), Some(stop))) = tag.attributes.as_ref().map(|a| (a.get(start_attrib), a.get(stop_attrib))) else {
            error!("Missing start or stop attribute in programme tag, skipping");
            return None;
        };

        let (Some(start_time), Some(stop_time)) = (parse_xmltv_time(start), parse_xmltv_time(stop)) else {
            error!("Failed to parse epg programme time {start} - {stop}");
            return None;
        };

        let mut title = None;
        let mut desc = None;
        if let Some(children) = tag.children.as_ref() {
            for child in children {
                if child.name == *tag_title {
                    title.clone_from(&child.value);
                } else if child.name == *tag_desc {
                    desc.clone_from(&child.value);
                }
            }
        }

        let catchup_id = tag.attributes.as_ref().and_then(|attributes| attributes.get(catchup_id_attrib)).cloned();

        Some(EpgProgramme::new_all(
            start_time,
            stop_time,
            Arc::clone(epg_id),
            title,
            desc,
            catchup_id,
        ))
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
    async fn process_epg_file(
        id_cache: &mut EpgIdCache,
        epg_source: &PersistedEpgSource,
        source_order: usize,
        accumulator: &mut EpgMergeAccumulator,
    ) -> bool {
        let epg_attrib_id = EPG_ATTRIB_ID.intern();
        let epg_attrib_channel = EPG_ATTRIB_CHANNEL.intern();
        let start_attrib = "start".intern();
        let stop_attrib = "stop".intern();
        let catchup_id_attrib = "catchup-id".intern();
        let tag_title = "title".intern();
        let tag_desc = "desc".intern();

        match CompressedFileReaderAsync::new(&epg_source.file_path).await {
            Ok(mut reader) => {
                let mut source_processed: HashSet<Arc<str>> = HashSet::with_capacity(5000);
                let mut accepted_channels = 0usize;
                let smart_match = id_cache.smart_match_config.enabled;
                let fuzzy_matching = smart_match && id_cache.smart_match_config.fuzzy_matching;
                let mut filter_tags = |mut tag: XmlTag| {
                    match tag.name.as_ref() {
                        EPG_TAG_CHANNEL => {
                            let tag_epg_id = tag.get_attribute_value(&epg_attrib_id).map_or_else(|| "".intern(), Internable::intern);
                            if tag_epg_id.is_empty() {
                                return;
                            }

                            Self::prepare_tag(id_cache, &mut tag, smart_match);
                            let add_channel = if smart_match {
                                id_cache.channel_epg_id.contains(&tag_epg_id)
                                    || Self::try_fuzzy_matching(id_cache, &tag_epg_id, &tag, fuzzy_matching)
                            } else {
                                id_cache.channel_epg_id.contains(&tag_epg_id)
                            };

                            if add_channel {
                                source_processed.insert(Arc::clone(&tag_epg_id));
                                id_cache.processed.insert(Arc::clone(&tag_epg_id));
                                accumulator.upsert_channel(
                                    epg_source.priority,
                                    source_order,
                                    epg_source.logo_override,
                                    EpgChannel {
                                        id: Arc::clone(&tag_epg_id),
                                        title: Self::channel_display_name(&tag),
                                        icon: Self::channel_icon(&tag),
                                        programmes: vec![],
                                    },
                                );
                                accepted_channels += 1;
                            }
                        }
                        EPG_TAG_PROGRAMME => {
                            if let Some(epg_id) = tag.get_attribute_value(&epg_attrib_channel) {
                                if source_processed.contains(epg_id) {
                                    if let Some(programme) =
                                        Self::extract_programme(
                                            &tag,
                                            epg_id,
                                            &start_attrib,
                                            &stop_attrib,
                                            &catchup_id_attrib,
                                            &tag_title,
                                            &tag_desc,
                                        )
                                    {
                                        accumulator.push_programme(epg_source.priority, source_order, programme);
                                    }
                                }
                            }
                        }
                        EPG_TAG_TV => {
                            accumulator.set_attributes_if_preferred(epg_source.priority, source_order, tag.attributes);
                        }
                        _ => {}
                    }
                };

                parse_tvguide(&mut reader, &mut filter_tags).await;
                accepted_channels > 0
            }
            Err(e) => {
                log::warn!("Failed to process EPG file {}: {e}", epg_source.file_path.display());
                false
            }
        }
    }

    pub async fn filter(&self, id_cache: &mut EpgIdCache) -> Option<Vec<Epg>> {
        self.filter_merged(id_cache).await.map(|epg| vec![epg])
    }

    pub async fn filter_merged(&self, id_cache: &mut EpgIdCache) -> Option<Epg> {
        self.filter_merged_with_icon_overrides(id_cache).await.map(|(epg, _)| epg)
    }

    pub(crate) async fn filter_merged_with_icon_overrides(
        &self,
        id_cache: &mut EpgIdCache,
    ) -> Option<MergedEpgWithIconOverrides> {
        if id_cache.channel_epg_id.is_empty() && id_cache.normalized.is_empty() {
            return None;
        }
        let mut accumulator = EpgMergeAccumulator::new();
        for (source_order, epg_source) in self.get_epg_sources().iter().enumerate() {
            Self::process_epg_file(id_cache, epg_source, source_order, &mut accumulator).await;
        }
        accumulator.finish_epg_with_icon_overrides()
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

#[derive(Debug)]
struct PreferredAttributes {
    priority: i16,
    source_order: usize,
    attributes: HashMap<Arc<str>, Arc<str>>,
}

type FinishedEpgChannels = (Option<HashMap<Arc<str>, Arc<str>>>, Vec<EpgChannel>);
pub(crate) type MergedEpgWithIconOverrides = (Epg, HashSet<Arc<str>>);

#[derive(Debug)]
struct ProgrammeMergeEntry {
    priority: i16,
    source_order: usize,
    programme: EpgProgramme,
}

#[derive(Debug)]
struct ChannelMergeAcc {
    priority: i16,
    source_order: usize,
    logo_override: bool,
    icon_logo_override: bool,
    needs_programme_merge: bool,
    channel: EpgChannel,
    programmes: Vec<ProgrammeMergeEntry>,
}

#[derive(Debug, Default)]
struct EpgMergeAccumulator {
    attributes: Option<PreferredAttributes>,
    channels: HashMap<Arc<str>, ChannelMergeAcc>,
}

impl EpgMergeAccumulator {
    fn new() -> Self { Self::default() }

    fn set_attributes_if_preferred(
        &mut self,
        priority: i16,
        source_order: usize,
        attributes: Option<HashMap<Arc<str>, Arc<str>>>,
    ) {
        let Some(attributes) = attributes else {
            return;
        };
        let replace = self
            .attributes
            .as_ref()
            .is_none_or(|current| (priority, source_order) < (current.priority, current.source_order));
        if replace {
            self.attributes = Some(PreferredAttributes { priority, source_order, attributes });
        }
    }

    fn upsert_channel(&mut self, priority: i16, source_order: usize, logo_override: bool, mut channel: EpgChannel) {
        match self.channels.entry(Arc::clone(&channel.id)) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let acc = entry.get_mut();
                acc.needs_programme_merge = true;
                if (priority, source_order) < (acc.priority, acc.source_order) {
                    if channel.title.is_none() {
                        channel.title = acc.channel.title.take();
                    }
                    if channel.icon.is_none() {
                        channel.icon = acc.channel.icon.take();
                    } else {
                        acc.icon_logo_override = logo_override;
                    }
                    acc.priority = priority;
                    acc.source_order = source_order;
                    acc.logo_override = logo_override;
                    acc.channel.title = channel.title;
                    acc.channel.icon = channel.icon;
                } else {
                    if acc.channel.title.is_none() {
                        acc.channel.title = channel.title.take();
                    }
                    if acc.channel.icon.is_none() {
                        acc.channel.icon = channel.icon.take();
                        acc.icon_logo_override = logo_override;
                    }
                }
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                let icon_logo_override = channel.icon.is_some() && logo_override;
                entry.insert(ChannelMergeAcc {
                    priority,
                    source_order,
                    logo_override,
                    icon_logo_override,
                    needs_programme_merge: false,
                    channel,
                    programmes: Vec::new(),
                });
            }
        }
    }

    fn push_programme(&mut self, priority: i16, source_order: usize, programme: EpgProgramme) {
        if let Some(channel) = self.channels.get_mut(programme.get_transient_channel_id()) {
            channel.programmes.push(ProgrammeMergeEntry { priority, source_order, programme });
        } else {
            error!("Channel {} not found in EPG, dangling programme", programme.get_transient_channel_id());
        }
    }

    fn add_channel_with_programmes(
        &mut self,
        priority: i16,
        source_order: usize,
        logo_override: bool,
        mut channel: EpgChannel,
    ) {
        let programmes = std::mem::take(&mut channel.programmes);
        let channel_id = Arc::clone(&channel.id);
        self.upsert_channel(priority, source_order, logo_override, channel);
        if let Some(acc) = self.channels.get_mut(&channel_id) {
            if !acc.programmes.is_empty() {
                acc.needs_programme_merge = true;
            }
            acc.programmes
                .extend(programmes.into_iter().map(|programme| ProgrammeMergeEntry { priority, source_order, programme }));
        }
    }

    fn finish_channels(mut self) -> Option<FinishedEpgChannels> {
        if self.channels.is_empty() {
            return None;
        }

        let mut channels = self
            .channels
            .drain()
            .map(|(_, mut acc)| {
                normalize_channel_programmes(&mut acc);
                acc
            })
            .collect::<Vec<_>>();

        channels.sort_by(|left, right| left.channel.id.cmp(&right.channel.id));
        let channels = channels.into_iter().map(|acc| acc.channel).collect();
        Some((self.attributes.map(|attributes| attributes.attributes), channels))
    }

    fn finish(self) -> Option<Epg> {
        self.finish_channels().map(|(attributes, channels)| Epg {
            logo_override: false,
            priority: 0,
            attributes,
            children: channels.into_iter().map(Arc::new).collect(),
        })
    }

    fn finish_epg_with_icon_overrides(self) -> Option<MergedEpgWithIconOverrides> {
        let EpgMergeAccumulator { attributes, channels } = self;
        let mut channels = channels.into_values().collect::<Vec<_>>();
        if channels.is_empty() {
            return None;
        }

        for acc in &mut channels {
            normalize_channel_programmes(acc);
        }

        channels.sort_by(|left, right| left.channel.id.cmp(&right.channel.id));
        let icon_override_channels = channels
            .iter()
            .filter(|acc| acc.icon_logo_override)
            .map(|acc| Arc::clone(&acc.channel.id))
            .collect::<HashSet<_>>();
        let children = channels.into_iter().map(|acc| Arc::new(acc.channel)).collect();

        Some((
            Epg {
                logo_override: false,
                priority: 0,
                attributes: attributes.map(|attributes| attributes.attributes),
                children,
            },
            icon_override_channels,
        ))
    }
}

fn backfill_programme_metadata(existing: &mut EpgProgramme, incoming: EpgProgramme) {
    if existing.title.is_none() {
        existing.title = incoming.title;
    }
    if existing.desc.is_none() {
        existing.desc = incoming.desc;
    }
    if existing.catchup_id.is_none() {
        existing.catchup_id = incoming.catchup_id;
    }
}

fn normalize_channel_programmes(acc: &mut ChannelMergeAcc) {
    // Always normalize programme order and dedupe, even for single-source channels.
    // This preserves backfill behavior for duplicate entries within the same source.
    acc.programmes.sort_by_key(|entry| {
        (
            entry.programme.start,
            entry.programme.stop,
            entry.priority,
            entry.source_order,
        )
    });

    let programme_entries = std::mem::take(&mut acc.programmes);
    let mut merged_programmes = Vec::with_capacity(programme_entries.len());
    let mut entries = programme_entries.into_iter();
    if let Some(first_entry) = entries.next() {
        let mut current = first_entry.programme;
        for entry in entries {
            if entry.programme.start == current.start && entry.programme.stop == current.stop {
                backfill_programme_metadata(&mut current, entry.programme);
            } else {
                merged_programmes.push(current);
                current = entry.programme;
            }
        }
        merged_programmes.push(current);
    }

    merged_programmes.sort_by_key(|programme| (programme.start, programme.stop));
    acc.channel.programmes = merged_programmes;
}

pub(crate) fn merge_epg_channels_by_priority(channels_by_source: Vec<(i16, Vec<EpgChannel>)>) -> Vec<EpgChannel> {
    let mut accumulator = EpgMergeAccumulator::new();
    for (source_order, (priority, channels)) in channels_by_source.into_iter().enumerate() {
        for channel in channels {
            accumulator.add_channel_with_programmes(priority, source_order, false, channel);
        }
    }
    accumulator.finish_channels().map(|(_, channels)| channels).unwrap_or_default()
}

pub fn flatten_tvguide(tv_guides: Vec<Epg>) -> Option<Epg> {
    let mut accumulator = EpgMergeAccumulator::new();
    for (source_order, guide) in tv_guides.into_iter().enumerate() {
        accumulator.set_attributes_if_preferred(guide.priority, source_order, guide.attributes);
        for channel in guide.children {
            accumulator.add_channel_with_programmes(
                guide.priority,
                source_order,
                guide.logo_override,
                Arc::unwrap_or_clone(channel),
            );
        }
    }
    accumulator.finish()
}

#[cfg(test)]
mod tests {
    use crate::model::{Epg, EpgSmartMatchConfig, PersistedEpgSource, TVGuide};
    use crate::processing::parser::xmltv::{
        flatten_tvguide, merge_epg_channels_by_priority, normalize_channel_name, EpgMergeAccumulator,
    };
    use shared::model::{EpgChannel, EpgProgramme};
    use std::collections::HashSet;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::tempdir;

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

    fn epg_channel(id: &str, title: Option<&str>, icon: Option<&str>, programmes: Vec<EpgProgramme>) -> EpgChannel {
        EpgChannel {
            id: id.intern(),
            title: title.map(Internable::intern),
            icon: icon.map(Internable::intern),
            programmes,
        }
    }

    fn epg_programme(id: &str, start: i64, stop: i64, title: Option<&str>, desc: Option<&str>) -> EpgProgramme {
        EpgProgramme::new_all(
            start,
            stop,
            id.intern(),
            title.map(Internable::intern),
            desc.map(Internable::intern),
            None,
        )
    }

    #[test]
    fn epg_priority_merge_preserves_high_priority_metadata_and_fills_programme_gaps() {
        let merged = merge_epg_channels_by_priority(vec![
            (
                10,
                vec![epg_channel(
                    "demo.channel",
                    Some("Low"),
                    Some("http://low/icon.png"),
                    vec![epg_programme("demo.channel", 10, 20, Some("Low Show"), None)],
                )],
            ),
            (
                0,
                vec![epg_channel(
                    "demo.channel",
                    Some("High"),
                    Some("http://high/icon.png"),
                    vec![epg_programme("demo.channel", 20, 30, Some("High Show"), None)],
                )],
            ),
        ]);

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].title.as_deref(), Some("High"));
        assert_eq!(merged[0].icon.as_deref(), Some("http://high/icon.png"));
        assert_eq!(
            merged[0].programmes.iter().map(|programme| (programme.start, programme.stop)).collect::<Vec<_>>(),
            vec![(10, 20), (20, 30)],
        );
    }

    #[test]
    fn epg_priority_merge_backfills_duplicate_programme_metadata_without_overwriting() {
        let merged = merge_epg_channels_by_priority(vec![
            (
                0,
                vec![epg_channel(
                    "demo.channel",
                    Some("High"),
                    None,
                    vec![epg_programme("demo.channel", 10, 20, Some("High Title"), None)],
                )],
            ),
            (
                10,
                vec![epg_channel(
                    "demo.channel",
                    None,
                    Some("http://fallback/icon.png"),
                    vec![epg_programme("demo.channel", 10, 20, Some("Low Title"), Some("Recovered desc"))],
                )],
            ),
        ]);

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].title.as_deref(), Some("High"));
        assert_eq!(merged[0].icon.as_deref(), Some("http://fallback/icon.png"));
        assert_eq!(merged[0].programmes.len(), 1);
        assert_eq!(merged[0].programmes[0].title.as_deref(), Some("High Title"));
        assert_eq!(merged[0].programmes[0].desc.as_deref(), Some("Recovered desc"));
    }

    #[test]
    fn epg_priority_merge_keeps_same_priority_deterministic_order() {
        let merged = merge_epg_channels_by_priority(vec![
            (
                0,
                vec![epg_channel(
                    "demo.channel",
                    Some("First"),
                    None,
                    vec![epg_programme("demo.channel", 10, 20, Some("First Title"), None)],
                )],
            ),
            (
                0,
                vec![epg_channel(
                    "demo.channel",
                    Some("Second"),
                    Some("http://second/icon.png"),
                    vec![
                        epg_programme("demo.channel", 10, 20, Some("Second Title"), Some("Second desc")),
                        epg_programme("demo.channel", 20, 30, Some("Second Programme"), None),
                    ],
                )],
            ),
        ]);

        assert_eq!(merged[0].title.as_deref(), Some("First"));
        assert_eq!(merged[0].icon.as_deref(), Some("http://second/icon.png"));
        assert_eq!(merged[0].programmes[0].title.as_deref(), Some("First Title"));
        assert_eq!(merged[0].programmes[0].desc.as_deref(), Some("Second desc"));
        assert_eq!(merged[0].programmes.len(), 2);
    }

    #[test]
    fn epg_priority_merge_flatten_uses_same_backfill_rules() {
        let flattened = flatten_tvguide(vec![
            Epg {
                logo_override: false,
                priority: 0,
                attributes: None,
                children: vec![Arc::new(epg_channel(
                    "demo.channel",
                    Some("High"),
                    None,
                    vec![epg_programme("demo.channel", 10, 20, Some("High Show"), None)],
                ))],
            },
            Epg {
                logo_override: false,
                priority: 10,
                attributes: None,
                children: vec![Arc::new(epg_channel(
                    "demo.channel",
                    Some("Low"),
                    Some("http://low/icon.png"),
                    vec![epg_programme("demo.channel", 20, 30, Some("Low Show"), None)],
                ))],
            },
        ])
        .expect("flattened epg");

        assert_eq!(flattened.children.len(), 1);
        assert_eq!(flattened.children[0].title.as_deref(), Some("High"));
        assert_eq!(flattened.children[0].icon.as_deref(), Some("http://low/icon.png"));
        assert_eq!(flattened.children[0].programmes.len(), 2);
    }

    #[test]
    fn epg_priority_merge_flatten_keeps_shared_arc_channels() {
        let shared_channel = Arc::new(epg_channel(
            "demo.channel",
            Some("Shared"),
            Some("http://shared/icon.png"),
            vec![epg_programme("demo.channel", 10, 20, Some("Shared Show"), None)],
        ));

        let flattened = flatten_tvguide(vec![Epg {
            logo_override: false,
            priority: 0,
            attributes: None,
            children: vec![Arc::clone(&shared_channel)],
        }])
        .expect("flattened epg");

        assert_eq!(flattened.children.len(), 1);
        assert_eq!(flattened.children[0].title.as_deref(), Some("Shared"));
        assert_eq!(flattened.children[0].icon.as_deref(), Some("http://shared/icon.png"));
    }

    #[test]
    fn epg_priority_merge_tracks_icon_override_per_channel() {
        let mut accumulator = EpgMergeAccumulator::new();
        accumulator.add_channel_with_programmes(
            0,
            0,
            false,
            epg_channel("demo.keep", Some("Keep"), Some("http://keep/icon.png"), vec![]),
        );
        accumulator.add_channel_with_programmes(
            1,
            1,
            true,
            epg_channel("demo.override", Some("Override"), Some("http://override/icon.png"), vec![]),
        );

        let (_epg, icon_override_channels) = accumulator.finish_epg_with_icon_overrides().expect("merged epg");

        let keep_id: Arc<str> = "demo.keep".intern();
        let override_id: Arc<str> = "demo.override".intern();

        assert!(!icon_override_channels.contains(&keep_id));
        assert!(icon_override_channels.contains(&override_id));
    }

    #[test]
    fn epg_priority_merge_normalizes_duplicates_within_single_source() {
        let merged = merge_epg_channels_by_priority(vec![(
            0,
            vec![epg_channel(
                "demo.channel",
                Some("Demo"),
                None,
                vec![
                    epg_programme("demo.channel", 20, 30, Some("Later"), None),
                    epg_programme("demo.channel", 10, 20, Some("First"), None),
                    epg_programme("demo.channel", 10, 20, None, Some("Recovered desc")),
                ],
            )],
        )]);

        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0].programmes.iter().map(|programme| (programme.start, programme.stop)).collect::<Vec<_>>(),
            vec![(10, 20), (20, 30)],
        );
        assert_eq!(merged[0].programmes[0].title.as_deref(), Some("First"));
        assert_eq!(merged[0].programmes[0].desc.as_deref(), Some("Recovered desc"));
    }

    #[test]
    fn epg_priority_merge_preserves_exact_id_match_when_smart_match_is_enabled() {
        let run_test = async move {
            let dir = tempdir().unwrap();
            let epg_path = dir.path().join("smart-exact.xml");

            fs::write(
                &epg_path,
                r#"<?xml version="1.0" encoding="UTF-8"?>
<tv>
  <channel id="demo.channel">
    <display-name>Completely Different Name</display-name>
  </channel>
  <programme start="20260425000000 +0000" stop="20260425010000 +0000" channel="demo.channel">
    <title>Exact Match Source</title>
  </programme>
</tv>"#,
            )
            .unwrap();

            let mut smart_cfg = EpgSmartMatchConfigDto {
                enabled: true,
                ..Default::default()
            };
            smart_cfg.prepare().expect("smart match config");
            let tv_guide = TVGuide::new(vec![PersistedEpgSource {
                file_path: epg_path,
                priority: 0,
                logo_override: false,
            }]);
            let mut id_cache = EpgIdCache::new(Some(&crate::model::EpgConfig {
                sources: vec![],
                smart_match: Some(EpgSmartMatchConfig::from(smart_cfg)),
            }));
            id_cache.channel_epg_id.insert("demo.channel".intern());

            let merged = tv_guide.filter_merged(&mut id_cache).await.expect("merged epg");

            assert_eq!(merged.children.len(), 1);
            assert_eq!(merged.children[0].id.as_ref(), "demo.channel");
            assert_eq!(merged.children[0].programmes.len(), 1);
        };

        tokio::runtime::Runtime::new().unwrap().block_on(run_test);
    }

    #[test]
    fn epg_priority_merge_filter_accepts_later_source_for_already_processed_channel() {
        let run_test = async move {
            let dir = tempdir().unwrap();
            let high_path = dir.path().join("high.xml");
            let low_path = dir.path().join("low.xml");

            fs::write(
                &high_path,
                r#"<?xml version="1.0" encoding="UTF-8"?>
<tv>
  <channel id="demo.channel">
    <display-name>High Channel</display-name>
  </channel>
  <programme start="20260425000000 +0000" stop="20260425010000 +0000" channel="demo.channel">
    <title>High Source</title>
  </programme>
</tv>"#,
            )
            .unwrap();
            fs::write(
                &low_path,
                r#"<?xml version="1.0" encoding="UTF-8"?>
<tv>
  <channel id="demo.channel">
    <display-name>Low Channel</display-name>
  </channel>
  <programme start="20260425010000 +0000" stop="20260425020000 +0000" channel="demo.channel">
    <title>Low Source</title>
  </programme>
</tv>"#,
            )
            .unwrap();

            let tv_guide = TVGuide::new(vec![
                PersistedEpgSource { file_path: high_path, priority: 0, logo_override: false },
                PersistedEpgSource { file_path: low_path, priority: 10, logo_override: false },
            ]);
            let mut id_cache = EpgIdCache::new(None);
            id_cache.channel_epg_id.insert("demo.channel".intern());

            let merged = tv_guide.filter_merged(&mut id_cache).await.expect("merged epg");

            assert!(id_cache.processed.contains("demo.channel"));
            assert_eq!(merged.children.len(), 1);
            assert_eq!(merged.children[0].title.as_deref(), Some("High Channel"));
            assert_eq!(merged.children[0].programmes.len(), 2);
            assert_eq!(
                merged.children[0]
                    .programmes
                    .iter()
                    .filter_map(|programme| programme.title.as_deref())
                    .collect::<Vec<_>>(),
                vec!["High Source", "Low Source"],
            );
        };

        tokio::runtime::Runtime::new().unwrap().block_on(run_test);
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

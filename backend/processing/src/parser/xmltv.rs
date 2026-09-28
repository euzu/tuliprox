use crate::processor::EpgIdCache;
use log::error;
use quick_xml::events::{BytesStart, BytesText, Event};
use serde::{Deserialize, Serialize};
use shared::{
    concat_string,
    model::{EpgCategory, EpgChannel, EpgNamePrefix, EpgProgramme},
    utils::{deunicode_string, Internable, CONSTANTS},
};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    io,
    path::PathBuf,
    sync::Arc,
};
use tokio::io::AsyncRead;
use tuliprox_core::{
    model::{
        Epg, EpgSmartMatchConfig, IcsDummyPolicy, IcsEpgSourceConfig, PersistedEpgSource, PersistedEpgSourceKind,
        XmlTag, XmlTagIcon, EPG_ATTRIB_CHANNEL, EPG_ATTRIB_ID, EPG_ATTRIB_LANG, EPG_TAG_CATEGORY, EPG_TAG_CHANNEL,
        EPG_TAG_DESC, EPG_TAG_DISPLAY_NAME, EPG_TAG_ICON, EPG_TAG_LIVE, EPG_TAG_NEW, EPG_TAG_PREVIOUSLY_SHOWN,
        EPG_TAG_PROGRAMME, EPG_TAG_TITLE, EPG_TAG_TV,
    },
    utils::{
        arc_str_serde, async_file_reader, compressed_file_reader_async::CompressedFileReaderAsync, parse_xmltv_time,
        with_folded_epg_id, FileLockManager,
    },
};
use tuliprox_parser::ics;
use tuliprox_repository::{BPlusTree, BPlusTreeQuery, BPlusTreeUpdate, FlushPolicy};

struct IcsPersistedSource<'a> {
    channel_id: &'a Arc<str>,
    channel_title: Option<&'a Arc<str>>,
    match_names: &'a [Arc<str>],
    config: &'a IcsEpgSourceConfig,
}

/// Splits a string at the first delimiter if the prefix matches a known country code.
///
/// Returns a tuple containing the country code prefix (if found) and the remainder of the string, both trimmed. If no valid prefix is found, returns `None` and the original input.
///
/// # Examples
///
/// ```text
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

fn strip_markers(input: &str, markers: &[String]) -> String {
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;
    while cursor < input.len() {
        let left_is_boundary = cursor == 0 || !input.as_bytes()[cursor - 1].is_ascii_alphanumeric();
        let matched_end = left_is_boundary
            .then(|| {
                markers.iter().filter(|marker| !marker.is_empty()).find_map(|marker| {
                    let end = cursor + marker.len();
                    let candidate = input.get(cursor..end)?;
                    (candidate.eq_ignore_ascii_case(marker)
                        && input.as_bytes().get(end).is_none_or(|byte| !byte.is_ascii_alphanumeric()))
                    .then_some(end)
                })
            })
            .flatten();
        if let Some(end) = matched_end {
            cursor = end;
        } else {
            let next = input[cursor..].chars().next().expect("cursor is within the string");
            output.push(next);
            cursor += next.len_utf8();
        }
    }
    output
}

/// # Panics
pub fn normalize_channel_name(name: &str, normalize_config: &EpgSmartMatchConfig) -> String {
    let normalized = deunicode_string(name.trim()).to_lowercase();
    let (channel_name, suffix) = name_prefix(&normalized, normalize_config);
    let stripped_name = strip_markers(channel_name, &normalize_config.strip);
    let reconstructed = match suffix {
        None => stripped_name,
        Some(sfx) => match &normalize_config.name_prefix {
            EpgNamePrefix::Ignore => stripped_name,
            EpgNamePrefix::Suffix(separator) => combine(separator, &stripped_name, sfx),
            EpgNamePrefix::Prefix(separator) => combine(separator, sfx, &stripped_name),
        },
    };
    normalize_config.normalize_regex.replace_all(&reconstructed, "").into_owned()
}

/// A set of persisted EPG sources, read and merged into one guide.
///
/// The type lives beside the XMLTV parsing that gives it behaviour: its
/// `impl` needs `EpgIdCache` and the tag types from this module, so keeping the
/// struct in the configuration model would have split a type from its methods
/// across a crate boundary.
#[derive(Debug, Clone)]
pub struct TVGuide {
    epg_sources: Vec<PersistedEpgSource>,
    file_locks: Option<Arc<FileLockManager>>,
}

impl TVGuide {
    pub fn new(mut epg_sources: Vec<PersistedEpgSource>) -> Self {
        epg_sources.sort_by_key(|a| a.priority);
        Self { epg_sources, file_locks: None }
    }

    /// Uses the shared cache lock manager while reading persisted EPG sources.
    pub fn with_file_locks(mut self, file_locks: Arc<FileLockManager>) -> Self {
        self.file_locks = Some(file_locks);
        self
    }

    #[inline]
    pub fn get_epg_sources(&self) -> &Vec<PersistedEpgSource> { &self.epg_sources }

    pub fn get_file_locks(&self) -> Option<&FileLockManager> { self.file_locks.as_deref() }
}

impl TVGuide {
    fn prepare_tag(id_cache: &mut EpgIdCache, tag: &mut XmlTag, smart_match: bool) {
        {
            let maybe_epg_id = { tag.get_attribute_value(&EPG_ATTRIB_ID.intern()).cloned() };
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
    ) -> Option<EpgProgramme> {
        let Some((Some(start), Some(stop))) =
            tag.attributes.as_ref().map(|a| (a.get(start_attrib), a.get(stop_attrib)))
        else {
            error!("Missing start or stop attribute in programme tag, skipping");
            return None;
        };

        let (Some(start_time), Some(stop_time)) = (parse_xmltv_time(start), parse_xmltv_time(stop)) else {
            error!("Failed to parse epg programme time {start} - {stop}");
            return None;
        };

        let mut title = None;
        let mut desc = None;
        let mut icon = None;
        let mut categories = Vec::new();
        let mut is_live = false;
        let mut is_new = false;
        let mut previously_shown = false;
        if let Some(children) = tag.children.as_ref() {
            for child in children {
                match child.name.as_ref() {
                    EPG_TAG_TITLE => title.clone_from(&child.value),
                    EPG_TAG_DESC => desc.clone_from(&child.value),
                    EPG_TAG_ICON => {
                        if let Some(src) = child
                            .attributes
                            .as_ref()
                            .and_then(|attributes| attributes.get("src"))
                            .filter(|src| !src.is_empty())
                        {
                            icon = Some(Arc::clone(src));
                        }
                    }
                    EPG_TAG_CATEGORY => {
                        if let Some(value) = child.value.as_ref().filter(|value| !value.is_empty()) {
                            categories.push(EpgCategory {
                                value: Arc::clone(value),
                                lang: child
                                    .attributes
                                    .as_ref()
                                    .and_then(|attributes| attributes.get(EPG_ATTRIB_LANG))
                                    .cloned(),
                            });
                        }
                    }
                    EPG_TAG_LIVE => is_live = true,
                    EPG_TAG_NEW => is_new = true,
                    EPG_TAG_PREVIOUSLY_SHOWN => previously_shown = true,
                    _ => {}
                }
            }
        }

        let catchup_id = tag.attributes.as_ref().and_then(|attributes| attributes.get(catchup_id_attrib)).cloned();

        let mut programme = EpgProgramme::new_all(start_time, stop_time, Arc::clone(epg_id), title, desc, catchup_id);
        programme.icon = icon;
        programme.categories = categories;
        programme.is_live = is_live;
        programme.is_new = is_new;
        programme.previously_shown = previously_shown;
        Some(programme)
    }

    /// Parses and filters a compressed EPG XML file, extracting relevant channel and program tags based on smart and fuzzy matching criteria.
    ///
    /// Returns an `Epg` containing filtered tags and TV attributes if any matching channels are found; otherwise, returns `None`.
    /// The returned `Epg` will include the priority from the source, which is used for merging multiple EPG sources.
    ///
    /// # Examples
    ///
    /// ```text
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

        match CompressedFileReaderAsync::new(&epg_source.file_path).await {
            Ok(mut reader) => {
                let mut source_processed: HashSet<Arc<str>> = HashSet::with_capacity(5000);
                let mut accepted_channels = 0usize;
                let smart_match = id_cache.smart_match_config.enabled;
                let mut filter_tags = |mut tag: XmlTag| {
                    match tag.name.as_ref() {
                        EPG_TAG_CHANNEL => {
                            let tag_epg_id =
                                tag.get_attribute_value(&epg_attrib_id).map_or_else(|| "".intern(), Internable::intern);
                            if tag_epg_id.is_empty() {
                                return;
                            }

                            Self::prepare_tag(id_cache, &mut tag, smart_match);
                            if smart_match && id_cache.needs_guide_names(&tag_epg_id) {
                                id_cache.register_guide_names(
                                    &tag_epg_id,
                                    tag.children.iter().flatten().filter_map(|child| {
                                        (child.name.as_ref() == EPG_TAG_DISPLAY_NAME)
                                            .then_some(child.value.as_ref())
                                            .flatten()
                                    }),
                                );
                            }
                            // Case-insensitive (ASCII) membership: fold the guide id for the
                            // lookup only; `tag_epg_id` keeps its original case for output.
                            let add_channel = if smart_match {
                                let direct_match = id_cache.contains_channel_epg_id(&tag_epg_id);
                                let normalized_match = tag.normalized_epg_ids.as_ref().is_some_and(|candidates| {
                                    id_cache.match_epg_channel_candidates(
                                        &tag_epg_id,
                                        candidates,
                                        epg_source.priority,
                                        source_order,
                                    )
                                });
                                direct_match || normalized_match
                            } else {
                                id_cache.contains_channel_epg_id(&tag_epg_id)
                            };

                            if add_channel {
                                with_folded_epg_id(&tag_epg_id, |folded| source_processed.insert(folded.intern()));
                                id_cache.insert_processed_epg_id(&tag_epg_id);
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
                                if with_folded_epg_id(epg_id, |folded| source_processed.contains(folded)) {
                                    if let Some(programme) = Self::extract_programme(
                                        &tag,
                                        epg_id,
                                        &start_attrib,
                                        &stop_attrib,
                                        &catchup_id_attrib,
                                    ) {
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

    async fn process_ics_file(
        id_cache: &mut EpgIdCache,
        epg_source: &PersistedEpgSource,
        source_order: usize,
        accumulator: &mut EpgMergeAccumulator,
        source: IcsPersistedSource<'_>,
    ) -> bool {
        let IcsPersistedSource { channel_id, channel_title, match_names, config } = source;
        let mut candidates = Vec::with_capacity(2 + match_names.len());
        candidates.push(channel_id.to_string());
        if let Some(title) = channel_title {
            candidates.push(title.to_string());
        }
        candidates.extend(match_names.iter().map(ToString::to_string));
        let normalized_candidates = id_cache.normalize_candidates(candidates);

        let add_channel = if id_cache.smart_match_enabled {
            let direct_match = id_cache.contains_channel_epg_id(channel_id);
            let normalized_match = id_cache.match_epg_channel_candidates(
                channel_id,
                &normalized_candidates,
                epg_source.priority,
                source_order,
            );
            direct_match || normalized_match
        } else {
            id_cache.contains_channel_epg_id(channel_id)
        };

        if !add_channel {
            return false;
        }

        match ics::parse_ics_file_to_channel(
            &epg_source.file_path,
            Arc::clone(channel_id),
            channel_title.cloned(),
            config,
        )
        .await
        {
            Ok(channel) => {
                id_cache.insert_processed_epg_id(channel_id);
                accumulator.add_channel_with_programmes(
                    epg_source.priority,
                    source_order,
                    epg_source.logo_override,
                    channel,
                );
                accumulator.register_dummy_policy(
                    channel_id,
                    epg_source.priority,
                    source_order,
                    IcsDummyPolicy { timezone: config.timezone.clone(), config: config.dummy.clone() },
                );
                true
            }
            Err(err) => {
                log::warn!("Failed to process ICS EPG file {}: {err}", epg_source.file_path.display());
                false
            }
        }
    }

    // Exercised only by this module's own tests.
    #[cfg(test)]
    pub async fn filter(&self, id_cache: &mut EpgIdCache) -> Option<Vec<Epg>> {
        self.filter_merged(id_cache).await.map(|epg| vec![epg])
    }

    // Exercised only by this module's own tests.
    #[cfg(test)]
    pub async fn filter_merged(&self, id_cache: &mut EpgIdCache) -> Option<Epg> {
        self.filter_merged_with_icon_overrides(id_cache).await.map(|(epg, _)| epg)
    }

    pub async fn filter_merged_with_icon_overrides(
        &self,
        id_cache: &mut EpgIdCache,
    ) -> Option<MergedEpgWithIconOverrides> {
        if id_cache.channel_epg_id.is_empty() && id_cache.normalized.is_empty() {
            return None;
        }
        let mut accumulator = EpgMergeAccumulator::new();
        for (source_order, epg_source) in self.get_epg_sources().iter().enumerate() {
            let _source_read_lock = match self.get_file_locks() {
                Some(file_locks) => Some(file_locks.read_lock(&epg_source.file_path).await),
                None => None,
            };
            match &epg_source.kind {
                PersistedEpgSourceKind::Xmltv => {
                    Self::process_epg_file(id_cache, epg_source, source_order, &mut accumulator).await;
                }
                PersistedEpgSourceKind::Ics { channel_id, channel_title, match_names, config } => {
                    Self::process_ics_file(
                        id_cache,
                        epg_source,
                        source_order,
                        &mut accumulator,
                        IcsPersistedSource {
                            channel_id,
                            channel_title: channel_title.as_ref(),
                            match_names,
                            config: config.as_ref(),
                        },
                    )
                    .await;
                }
            }
        }
        let available_epg_ids = accumulator.channel_ids_with_programmes();
        id_cache.finalize_matches(&available_epg_ids);
        let mut selected_epg_ids = id_cache.selected_epg_ids(&available_epg_ids);
        selected_epg_ids.extend(id_cache.channel_epg_id.iter().cloned());
        let retained_epg_ids = accumulator.retain_channels(&selected_epg_ids);
        id_cache.replace_processed_epg_ids(retained_epg_ids.intersection(&available_epg_ids).cloned().collect());
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
    let attributes = collect_tag_attributes(e);
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
    // Pre-allocate so the first giant `<programme>` block does not trigger a
    // chain of `Vec::grow` reallocs. The buffer is monotonically grown by
    // quick_xml (it is reset by the caller, see below), so the eventual size
    // is the largest single event — for XMLTV that is the biggest programme
    // description. Starting at 0 capacity makes that growth ~25 doubling copies.
    let mut buf = Vec::<u8>::with_capacity(64 * 1024);
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
        // quick_xml does not clear the buffer between events — the borrow
        // returned by `read_event_into_async` is dropped at the end of each
        // match arm, so we can reclaim the capacity here without invalidating
        // any handler output. Without this, `buf` grows monotonically over the
        // whole file, defeating the 64 KiB pre-allocation above.
        buf.clear();
    }
}

/// Channels per `BPlusTree` write batch. Sized so the in-flight batch + its
/// prepared references stay under 1 MiB on the typical 100-KiB-per-channel
/// EPG feed. This is an educated guess; profile-driven tuning is fine.
const EPG_DISK_BATCH_SIZE: usize = 100;

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
enum XmlTagType {
    Ignored,
    Tv,
    Channel,
    Programme,
}

impl XmlTagType {
    #[inline]
    pub fn is_tv(self) -> bool { self == XmlTagType::Tv }
}

fn get_tag_type(name: &str) -> XmlTagType {
    match name {
        EPG_TAG_TV => XmlTagType::Tv,
        EPG_TAG_CHANNEL => XmlTagType::Channel,
        EPG_TAG_PROGRAMME => XmlTagType::Programme,
        _ => XmlTagType::Ignored,
    }
}

fn collect_tag_attributes(e: &BytesStart) -> HashMap<Arc<str>, Arc<str>> {
    let attributes = e
        .attributes()
        .filter_map(Result::ok)
        .filter_map(|a| {
            let key_binding = a.key;
            let key_raw = String::from_utf8_lossy(key_binding.as_ref());
            let key = key_raw.intern();
            if let Ok(value) = a.normalized_value(quick_xml::XmlVersion::Implicit1_0).as_ref() {
                if value.is_empty() {
                    None
                } else {
                    // Ids are no longer lowercased when parsed; EPG matching folds case
                    // at the comparison instead, so the guide's original-case <channel id>
                    // and <programme channel> are preserved in the output.
                    Some((key, value.intern()))
                }
            } else {
                None
            }
        })
        .collect::<HashMap<Arc<str>, Arc<str>>>();
    attributes
}

#[derive(Debug)]
struct PreferredAttributes {
    priority: i16,
    source_order: usize,
    attributes: HashMap<Arc<str>, Arc<str>>,
}

#[derive(Debug)]
struct PreferredDummyPolicy {
    priority: i16,
    source_order: usize,
    policy: IcsDummyPolicy,
}

/// Carries the source rank required to select a preview dummy policy exactly like the main EPG merge.
#[derive(Debug)]
pub struct EpgDummyPolicySource {
    pub priority: i16,
    pub source_order: usize,
    pub channel_id: Arc<str>,
    pub policy: IcsDummyPolicy,
}

type FinishedEpgChannels = (Option<HashMap<Arc<str>, Arc<str>>>, Vec<EpgChannel>);
pub type MergedEpgWithIconOverrides = (Epg, HashSet<Arc<str>>);

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
pub struct EpgMergeAccumulator {
    attributes: Option<PreferredAttributes>,
    channels: HashMap<Arc<str>, ChannelMergeAcc>,
    dummy_policies: HashMap<Arc<str>, PreferredDummyPolicy>,
}

impl EpgMergeAccumulator {
    pub fn new() -> Self { Self::default() }

    fn channel_ids_with_programmes(&self) -> HashSet<Arc<str>> {
        self.channels
            .iter()
            .filter(|(id, channel)| !channel.programmes.is_empty() || self.dummy_policies.contains_key(*id))
            .map(|(id, _)| Arc::clone(id))
            .collect()
    }

    fn retain_channels(&mut self, selected_epg_ids: &HashSet<Arc<str>>) -> HashSet<Arc<str>> {
        self.channels.retain(|id, _| selected_epg_ids.contains(id));
        self.dummy_policies.retain(|id, _| selected_epg_ids.contains(id));
        self.channels.keys().cloned().collect()
    }

    pub fn set_attributes_if_preferred(
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
        let channel_key = with_folded_epg_id(&channel.id, |folded| folded.intern());
        match self.channels.entry(channel_key) {
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
        if let Some(channel) =
            with_folded_epg_id(programme.get_transient_channel_id(), |folded| self.channels.get_mut(folded))
        {
            channel.programmes.push(ProgrammeMergeEntry { priority, source_order, programme });
        } else {
            error!("Channel {} not found in EPG, dangling programme", programme.get_transient_channel_id());
        }
    }

    fn register_dummy_policy(
        &mut self,
        channel_id: &Arc<str>,
        priority: i16,
        source_order: usize,
        policy: IcsDummyPolicy,
    ) {
        if !policy.config.enabled {
            return;
        }
        let key = with_folded_epg_id(channel_id, |folded| folded.intern());
        let replace = self
            .dummy_policies
            .get(&key)
            .is_none_or(|current| (priority, source_order) < (current.priority, current.source_order));
        if replace {
            self.dummy_policies.insert(key, PreferredDummyPolicy { priority, source_order, policy });
        }
    }

    pub fn add_channel_with_programmes(
        &mut self,
        priority: i16,
        source_order: usize,
        logo_override: bool,
        mut channel: EpgChannel,
    ) {
        let programmes = std::mem::take(&mut channel.programmes);
        let channel_key = with_folded_epg_id(&channel.id, |folded| folded.intern());
        self.upsert_channel(priority, source_order, logo_override, channel);
        if let Some(acc) = self.channels.get_mut(&channel_key) {
            if !acc.programmes.is_empty() {
                acc.needs_programme_merge = true;
            }
            acc.programmes.extend(programmes.into_iter().map(|programme| ProgrammeMergeEntry {
                priority,
                source_order,
                programme,
            }));
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
        let EpgMergeAccumulator { attributes, channels, dummy_policies } = self;
        let mut channels = channels.into_values().collect::<Vec<_>>();
        if channels.is_empty() {
            return None;
        }

        for acc in &mut channels {
            normalize_channel_programmes(acc);
        }

        apply_dummy_policies(&mut channels, &dummy_policies);

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

    /// Drain the accumulator directly into a temp `BPlusTree` on disk. Peak RAM
    /// here is `EPG_DISK_BATCH_SIZE × max_channel_size` — channels never sit
    /// in a `Vec`. The returned `DiskEpgSource` removes the file in its
    /// `Drop`.
    ///
    /// `source_priority` and `source_order` are the per-source values used
    /// for `set_attributes_if_preferred` at merge time. The accumulator
    /// itself does not aggregate them, so the caller — which knows the
    /// original Epg's `priority` and `source_order` — must hand them in.
    ///
    /// # Panics
    ///
    /// Panics if `source_order` exceeds `u32`, which would mean four billion
    /// sources in one import.
    pub fn finish_into_disk(self, path: PathBuf, source_priority: i16, source_order: u32) -> io::Result<DiskEpgSource> {
        // Fresh tree at the temp path. `store` creates the file; the
        // subsequent updater opens it for batched writes.
        BPlusTree::<EpgDiskChannelKey, EpgChannel>::new()
            .store(&path)
            .map_err(|e| io::Error::other(format!("create temp EPG tree: {e}")))?;
        // The temp file exists from here until `DiskEpgSource::new` takes
        // ownership at the end. Any `?` in between would otherwise leak the
        // file because no `Drop` is wired up yet. The local guard covers the
        // fallible window and is disarmed just before we hand the path off.
        let mut temp_guard = TempFileGuard(Some(path.clone()));

        let mut updater = BPlusTreeUpdate::<EpgDiskChannelKey, EpgChannel>::try_new_with_backoff(&path)
            .map_err(|e| io::Error::other(format!("open temp EPG tree: {e}")))?;
        updater.set_flush_policy(FlushPolicy::Batch);

        let EpgMergeAccumulator { attributes, channels, dummy_policies: _ } = self;
        let total = channels.len();
        let mut batch: Vec<(EpgDiskChannelKey, EpgChannel)> = Vec::with_capacity(EPG_DISK_BATCH_SIZE);
        let mut written = 0usize;

        for (_, mut acc) in channels {
            normalize_channel_programmes(&mut acc);
            let folded = with_folded_epg_id(&acc.channel.id, |folded| folded.intern());
            batch.push((
                EpgDiskChannelKey {
                    folded_id: folded,
                    priority: acc.priority,
                    // `usize -> u32` overflow requires > 4 billion sources, which
                    // is not representable in the file format anyway; treat it
                    // as a programmer error rather than a recoverable I/O
                    // failure.
                    source_order: u32::try_from(acc.source_order)
                        .expect("source_order exceeds u32 (4 billion sources in one import)"),
                },
                acc.channel,
            ));
            if batch.len() >= EPG_DISK_BATCH_SIZE {
                flush_batch(&mut updater, &mut batch, &mut written)?;
            }
        }
        flush_batch(&mut updater, &mut batch, &mut written)?;
        updater.commit()?;
        // Reclaim space wasted by priority-override entries (multiple keys per
        // channel). Without compact, the file is ~2× its eventual size.
        updater.compact()?;

        log::debug!(
            "Drained {written} channels ({total} total before normalize) into temp EPG tree at {}",
            path.display()
        );

        let source = DiskEpgSource::new(path, attributes.map(|a| a.attributes), source_priority, source_order);
        // DiskEpgSource now owns the path; disarm the guard so its Drop does
        // not double-remove the file. `mem::forget` would also work, but
        // `take()` keeps the guard in scope and is auditable.
        temp_guard.0.take();
        Ok(source)
    }
}

/// Removes the wrapped file path on drop unless `take()` was called first.
/// Mirrors the cleanup logic of `DiskEpgSource::Drop` for the fallible window
/// inside `finish_into_disk` where no `DiskEpgSource` exists yet.
struct TempFileGuard(Option<PathBuf>);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            if let Err(err) = std::fs::remove_file(&path) {
                if err.kind() != std::io::ErrorKind::NotFound {
                    log::warn!("Failed to remove temp EPG tree at {}: {err}", path.display());
                }
            }
        }
    }
}

/// Flush a non-empty batch and bump the written counter. No-op for empty
/// batches, so the caller does not need a guard around the trailing flush.
fn flush_batch(
    updater: &mut BPlusTreeUpdate<EpgDiskChannelKey, EpgChannel>,
    batch: &mut Vec<(EpgDiskChannelKey, EpgChannel)>,
    written: &mut usize,
) -> io::Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let items: Vec<(&EpgDiskChannelKey, &EpgChannel)> = batch.iter().map(|(k, v)| (k, v)).collect();
    let prepared = BPlusTreeUpdate::<EpgDiskChannelKey, EpgChannel>::prepare_upsert_batch(&items)?;
    updater.upsert_batch_encoded(prepared)?;
    *written += batch.len();
    batch.clear();
    Ok(())
}

/// Sort key for the per-source temp `BPlusTree`. Order: folded channel id ascending,
/// then priority ascending, then source order ascending. This matches the existing
/// rule in `EpgMergeAccumulator::upsert_channel` (`(priority, source_order) < (acc.priority, acc.source_order)`)
/// — lower numbers win. Sorted iteration over the tree therefore yields channels in
/// the right order for the multi-way merge downstream.
///
/// The `Ord` implementation below is hand-written to make the value-based,
/// deterministic ordering explicit (`folded_id` → `priority` → `source_order`).
/// `Arc<str>` already compares by value when the inner `str` is `Ord`, so a
/// derived `Ord` would produce the same total order — the custom impl exists
/// to lock the contract in source rather than rely on a derived behaviour.
/// Same key type serialises through `rmp_serde` as a record.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EpgDiskChannelKey {
    #[serde(with = "arc_str_serde")]
    pub folded_id: Arc<str>,
    pub priority: i16,
    pub source_order: u32,
}

// Explicit value-based ordering. `#[derive(Ord)]` would derive the same total
// order via `Arc<str>`'s value-based `Ord` impl, but writing it out documents
// the contract: `folded_id` ascending, then `priority` ascending, then
// `source_order` ascending. Sorted iteration in `merge_epg_trees` depends on
// this exact order for deterministic multi-way merge results.
impl Ord for EpgDiskChannelKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.folded_id
            .as_ref()
            .cmp(other.folded_id.as_ref())
            .then(self.priority.cmp(&other.priority))
            .then(self.source_order.cmp(&other.source_order))
    }
}
impl PartialOrd for EpgDiskChannelKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(other)) }
}

/// Handle for a temp `BPlusTree` that holds the channels of one EPG source.
/// `Drop` removes the file — even on panic — so the temp dir never accumulates
/// stale trees. The caller is expected to hand ownership to `merge_epg_trees`,
/// which opens a query handle and drains the file before letting the guard drop.
///
/// `source_priority` and `source_order` are the per-source values used at
/// merge time by `set_attributes_if_preferred`. They live on the source
/// rather than being reconstructed from the disk tree, because the disk
/// tree only stores per-channel priorities (which can differ across
/// channels if sources overlap), not a single source-level value.
pub struct DiskEpgSource {
    pub(super) path: PathBuf,
    pub(super) attributes: Option<HashMap<Arc<str>, Arc<str>>>,
    pub(super) source_priority: i16,
    pub(super) source_order: u32,
}

impl DiskEpgSource {
    pub fn new(
        path: PathBuf,
        attributes: Option<HashMap<Arc<str>, Arc<str>>>,
        source_priority: i16,
        source_order: u32,
    ) -> Self {
        Self { path, attributes, source_priority, source_order }
    }
}

impl Drop for DiskEpgSource {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(&self.path) {
            // Missing-file is fine (already cleaned up). Anything else is worth
            // a warning — it leaks the temp file until the OS clears /tmp.
            if err.kind() != std::io::ErrorKind::NotFound {
                log::warn!("Failed to remove temp EPG tree at {}: {err}", self.path.display());
            }
        }
    }
}

fn apply_dummy_policies(channels: &mut [ChannelMergeAcc], dummy_policies: &HashMap<Arc<str>, PreferredDummyPolicy>) {
    let now = chrono::Utc::now();
    for acc in channels {
        let key = with_folded_epg_id(&acc.channel.id, |folded| folded.intern());
        if let Some(preferred) = dummy_policies.get(&key) {
            let policy = &preferred.policy;
            if let Err(err) = ics::fill_dummy_gaps(
                &mut acc.channel.programmes,
                &acc.channel.id,
                &policy.timezone,
                &policy.config,
                now,
            ) {
                log::warn!("Failed to apply ICS dummy policy for {}: {err}", acc.channel.id);
            }
        }
    }
}

fn backfill_programme_metadata(existing: &mut EpgProgramme, incoming: EpgProgramme) {
    if existing.title.is_none() {
        existing.title = incoming.title;
    }
    if existing.desc.is_none() {
        existing.desc = incoming.desc;
    }
    if existing.icon.is_none() {
        existing.icon = incoming.icon;
    }
    if existing.catchup_id.is_none() {
        existing.catchup_id = incoming.catchup_id;
    }
    if existing.categories.is_empty() {
        existing.categories = incoming.categories;
    }
    existing.is_live |= incoming.is_live;
    existing.is_new |= incoming.is_new;
}

fn normalize_channel_programmes(acc: &mut ChannelMergeAcc) {
    // `acc.programmes` is the cross-source list of `ProgrammeMergeEntry` records
    // (populated by `add_channel_with_programmes` and `push_programme`).
    // When the list is empty, the channel's own programmes — set by the
    // vacant path of `upsert_channel` — are authoritative; the previous
    // implementation unconditionally overwrote `acc.channel.programmes` with
    // an empty vector here, which silently dropped every programme on the
    // single-source and disk-spill paths.
    if acc.programmes.is_empty() {
        return;
    }
    acc.programmes
        .sort_by_key(|entry| (entry.programme.start, entry.programme.stop, entry.priority, entry.source_order));

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

#[cfg(test)]
pub fn merge_epg_channels_by_priority(channels_by_source: Vec<(i16, Vec<EpgChannel>)>) -> Vec<EpgChannel> {
    let mut accumulator = EpgMergeAccumulator::new();
    for (source_order, (priority, channels)) in channels_by_source.into_iter().enumerate() {
        for channel in channels {
            accumulator.add_channel_with_programmes(priority, source_order, false, channel);
        }
    }
    accumulator.finish_channels().map(|(_, channels)| channels).unwrap_or_default()
}

pub fn merge_epg_channels_by_priority_with_dummy_policies(
    channels_by_source: Vec<(i16, Vec<EpgChannel>)>,
    dummy_policies: Vec<EpgDummyPolicySource>,
) -> Vec<EpgChannel> {
    let mut accumulator = EpgMergeAccumulator::new();
    for (source_order, (priority, channels)) in channels_by_source.into_iter().enumerate() {
        for channel in channels {
            accumulator.add_channel_with_programmes(priority, source_order, false, channel);
        }
    }
    for source in dummy_policies {
        accumulator.register_dummy_policy(&source.channel_id, source.priority, source.source_order, source.policy);
    }
    accumulator
        .finish_epg_with_icon_overrides()
        .map(|(epg, _)| epg.children.into_iter().map(Arc::unwrap_or_clone).collect())
        .unwrap_or_default()
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

/// Merge N temp `BPlusTree`s (one per EPG source) into a single `Epg`,
/// applying dummy policies and the existing priority/sort rules. Sources
/// are consumed one at a time — each `DiskEpgSource`'s temp file is
/// removed by its `Drop` as soon as its iterator is exhausted, so peak RAM
/// is bounded by the largest single source plus the accumulator's
/// per-channel `HashMap`, not the total feed size.
///
/// Complexity: `O(n_sources + total_channels)`. The per-channel priority
/// resolution happens inside `EpgMergeAccumulator::upsert_channel`
/// (`HashMap` insert), not in a heap. Sources are processed in iteration
/// order, so earlier sources win priority ties for shared channels.
///
/// The constant-memory guarantee lives on the write side
/// (`EpgMergeAccumulator::finish_into_disk`); this function is bounded by
/// the union's metadata `HashMap` in the accumulator, which is the same
/// shape the in-memory path already carries.
///
/// The icon-override channel set returned by `finish_epg_with_icon_overrides`
/// is intentionally dropped — the wire-up path persists channels directly,
/// not the override metadata. If a caller later needs that set, the
/// `MergedEpgWithIconOverrides` tuple is already in scope via this
/// function's return type.
pub fn merge_epg_trees(sources: Vec<DiskEpgSource>) -> io::Result<Option<MergedEpgWithIconOverrides>> {
    let mut accumulator = EpgMergeAccumulator::new();
    for source in sources {
        if let Some(attrs) = &source.attributes {
            accumulator.set_attributes_if_preferred(
                source.source_priority,
                source.source_order as usize,
                Some(attrs.clone()),
            );
        }
        let mut query = BPlusTreeQuery::<EpgDiskChannelKey, EpgChannel>::try_new(&source.path)
            .map_err(|e| io::Error::other(format!("open temp EPG tree at {}: {e}", source.path.display())))?;
        for entry in query.iter() {
            let (key, channel) = entry.map_err(|e| io::Error::other(format!("read temp EPG entry: {e}")))?;
            // `add_channel_with_programmes` preserves the channel's programmes
            // on the merge accumulator; plain `upsert_channel` would drop them
            // because the vacant-entry path constructs a fresh `ChannelMergeAcc`
            // with `programmes: Vec::new()`.
            accumulator.add_channel_with_programmes(key.priority, key.source_order as usize, false, channel);
        }
        // `source` drops at the end of this iteration, which removes the
        // temp file. The query's mmap/buffer is closed first because `query`
        // goes out of scope before `source`.
    }
    Ok(accumulator.finish_epg_with_icon_overrides())
}

#[cfg(test)]
mod tests {
    use super::TVGuide;
    use crate::parser::xmltv::{
        flatten_tvguide, merge_epg_channels_by_priority, merge_epg_channels_by_priority_with_dummy_policies,
        normalize_channel_name, EpgDummyPolicySource, EpgMergeAccumulator,
    };
    use shared::model::{EpgCategory, EpgChannel, EpgProgramme};
    use std::{collections::HashSet, fs, path::PathBuf, sync::Arc};
    use tempfile::tempdir;
    use tuliprox_core::{
        model::{
            Epg, EpgSmartMatchConfig, IcsDummyConfig, IcsEpgSourceConfig, PersistedEpgSource, PersistedEpgSourceKind,
        },
        utils::FileLockManager,
    };

    /// Run an async test body on a freshly-created multi-threaded tokio
    /// runtime. Centralizes the `Runtime::new()...block_on(...)` boilerplate
    /// shared by every test in this module that exercises async EPG code.
    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        tokio::runtime::Runtime::new().unwrap().block_on(future);
    }

    fn xmltv_source(file_path: PathBuf, priority: i16, logo_override: bool) -> PersistedEpgSource {
        PersistedEpgSource { file_path, priority, logo_override, kind: PersistedEpgSourceKind::Xmltv }
    }

    fn dummy_policy_source(priority: i16, source_order: usize, title: &str) -> EpgDummyPolicySource {
        EpgDummyPolicySource {
            priority,
            source_order,
            channel_id: "f1.calendar".intern(),
            policy: tuliprox_core::model::IcsDummyPolicy {
                timezone: "UTC".to_string(),
                config: IcsDummyConfig {
                    enabled: true,
                    title: title.to_string(),
                    description: String::new(),
                    days_past: 0,
                    days_future: 0,
                    block_hours: 24,
                    min_gap_minutes: 1,
                },
            },
        }
    }

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

    #[test]
    fn normalization_keeps_country_suffixes_consistent() {
        let mut dto = EpgSmartMatchConfigDto {
            enabled: true,
            name_prefix: EpgNamePrefix::Suffix(".".to_string()),
            ..Default::default()
        };
        dto.prepare().expect("valid smart-match config");
        let config = EpgSmartMatchConfig::from(dto);

        assert_eq!(normalize_channel_name("FR: TF1 FHD", &config), "tf1.fr");
        assert_eq!(normalize_channel_name("TF1.fr", &config), "tf1.fr");
    }

    #[test]
    fn normalization_strips_quality_markers_without_corrupting_names() {
        let mut dto = EpgSmartMatchConfigDto { enabled: true, ..Default::default() };
        dto.prepare().expect("valid smart-match config");
        let config = EpgSmartMatchConfig::from(dto);

        assert_eq!(normalize_channel_name("RMC Story H265 50FPS", &config), "rmcstory");
        assert_eq!(normalize_channel_name("Ashdod TV HD", &config), "ashdodtv");
    }

    #[test]
    fn marker_stripping_skips_non_utf8_boundaries() {
        assert_eq!(super::strip_markers("éclair", &["x".to_string()]), "éclair");
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
    fn epg_priority_merge_backfills_programme_tags() {
        let low_priority_categories = vec![
            EpgCategory { value: "Sports".intern(), lang: None },
            EpgCategory { value: "Live".intern(), lang: Some("en".intern()) },
        ];
        let mut low_programme = epg_programme("demo.channel", 10, 20, None, None);
        low_programme.icon = Some("https://example.com/programme.jpg".intern());
        low_programme.categories = low_priority_categories.clone();
        low_programme.is_live = true;
        low_programme.is_new = true;

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
            (10, vec![epg_channel("demo.channel", None, None, vec![low_programme])]),
        ]);

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].programmes.len(), 1);
        assert_eq!(merged[0].programmes[0].icon.as_deref(), Some("https://example.com/programme.jpg"));
        assert_eq!(merged[0].programmes[0].categories, low_priority_categories);
        assert!(merged[0].programmes[0].is_live);
        assert!(merged[0].programmes[0].is_new);

        let high_programme_with_categories = {
            let mut programme = epg_programme("demo.channel", 30, 40, Some("High Title 2"), None);
            programme.categories = vec![EpgCategory { value: "Drama".intern(), lang: None }];
            programme.is_new = true;
            programme
        };
        let low_programme_extra = {
            let mut programme = epg_programme("demo.channel", 30, 40, None, None);
            programme.categories = vec![EpgCategory { value: "ShouldNotWin".intern(), lang: None }];
            programme.is_live = true;
            programme
        };
        let merged = merge_epg_channels_by_priority(vec![
            (0, vec![epg_channel("demo.channel", None, None, vec![high_programme_with_categories])]),
            (10, vec![epg_channel("demo.channel", None, None, vec![low_programme_extra])]),
        ]);

        assert_eq!(merged.len(), 1);
        let programme = &merged[0].programmes[0];
        assert_eq!(programme.categories, vec![EpgCategory { value: "Drama".intern(), lang: None }],);
        assert!(programme.is_live);
        assert!(programme.is_new);
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
        run_async_test(async move {
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

            let mut smart_cfg = EpgSmartMatchConfigDto { enabled: true, ..Default::default() };
            smart_cfg.prepare().expect("smart match config");
            let tv_guide = TVGuide::new(vec![xmltv_source(epg_path, 0, false)]);
            let mut id_cache = EpgIdCache::new(Some(&tuliprox_core::model::EpgConfig {
                sources: vec![],
                smart_match: Some(EpgSmartMatchConfig::from(smart_cfg)),
            }));
            id_cache.insert_channel_epg_id("demo.channel");

            let merged = tv_guide.filter_merged(&mut id_cache).await.expect("merged epg");

            assert_eq!(merged.children.len(), 1);
            assert_eq!(merged.children[0].id.as_ref(), "demo.channel");
            assert_eq!(merged.children[0].programmes.len(), 1);
        });
    }

    #[test]
    fn persisted_epg_source_read_uses_shared_file_lock() {
        run_async_test(async move {
            let dir = tempdir().expect("temp dir");
            let epg_path = dir.path().join("locked.xml");
            fs::write(
                &epg_path,
                r#"<tv>
  <channel id="demo.channel"><display-name>Demo</display-name></channel>
  <programme start="20260425000000 +0000" stop="20260425010000 +0000" channel="demo.channel">
    <title>Locked read</title>
  </programme>
</tv>"#,
            )
            .expect("write XMLTV fixture");

            let file_locks = Arc::new(FileLockManager::new());
            let guide =
                TVGuide::new(vec![xmltv_source(epg_path.clone(), 0, false)]).with_file_locks(Arc::clone(&file_locks));
            let mut id_cache = EpgIdCache::new(None);
            id_cache.insert_channel_epg_id("demo.channel");
            let write_guard = file_locks.write_lock(&epg_path).await;
            let filter = guide.filter_merged(&mut id_cache);
            tokio::pin!(filter);

            assert!(tokio::time::timeout(std::time::Duration::from_millis(25), filter.as_mut()).await.is_err());
            drop(write_guard);

            let merged = filter.await.expect("EPG parse after write lock release");
            assert_eq!(merged.children[0].programmes[0].title.as_deref(), Some("Locked read"));
        });
    }

    #[test]
    fn epg_channel_id_match_is_case_insensitive_and_preserves_guide_case() {
        run_async_test(async move {
            let dir = tempdir().unwrap();
            let epg_path = dir.path().join("mixed-case.xml");

            // Guide channel ids are MixedCase and differ in case from the playlist ids.
            fs::write(
                &epg_path,
                r#"<?xml version="1.0" encoding="UTF-8"?>
<tv>
  <channel id="Sport.Extra1.DE">
    <display-name>Sport Extra</display-name>
  </channel>
  <channel id="CNN.US">
    <display-name>CNN</display-name>
  </channel>
  <programme start="20260425000000 +0000" stop="20260425010000 +0000" channel="Sport.Extra1.DE">
    <title>Match A</title>
  </programme>
  <programme start="20260425000000 +0000" stop="20260425010000 +0000" channel="CNN.US">
    <title>Match B</title>
  </programme>
</tv>"#,
            )
            .unwrap();

            let tv_guide = TVGuide::new(vec![xmltv_source(epg_path, 0, false)]);
            let mut id_cache = EpgIdCache::new(None);
            // Playlist epg ids arrive in a *different* case than the guide. Both origins
            // (an Xtream source and a mapper-script literal) go through the same
            // `insert_channel_epg_id`, which folds the membership key.
            id_cache.insert_channel_epg_id("sport.EXTRA1.de"); // e.g. from an Xtream source
            id_cache.insert_channel_epg_id("cnn.us"); // e.g. set by a mapper literal @epg_channel_id = "cnn.us"

            let merged = tv_guide.filter_merged(&mut id_cache).await.expect("merged epg");

            // Both MixedCase guide channels matched despite the case difference.
            assert_eq!(merged.children.len(), 2);
            let ids: HashSet<&str> = merged.children.iter().map(|c| c.id.as_ref()).collect();
            // Emitted <channel id> preserves the guide's ORIGINAL case (not folded).
            assert!(ids.contains("Sport.Extra1.DE"), "emitted ids: {ids:?}");
            assert!(ids.contains("CNN.US"), "emitted ids: {ids:?}");
            for channel in &merged.children {
                assert_eq!(channel.programmes.len(), 1, "channel {} programmes", channel.id);
            }
        });
    }

    #[test]
    fn epg_programme_channel_match_is_case_insensitive_within_source() {
        run_async_test(async move {
            let dir = tempdir().unwrap();
            let epg_path = dir.path().join("mixed-case-programme.xml");

            fs::write(
                &epg_path,
                r#"<?xml version="1.0" encoding="UTF-8"?>
<tv>
  <channel id="Demo.Channel">
    <display-name>Demo</display-name>
  </channel>
  <programme start="20260425000000 +0000" stop="20260425010000 +0000" channel="demo.channel">
    <title>Case Variant Programme</title>
  </programme>
</tv>"#,
            )
            .unwrap();

            let tv_guide = TVGuide::new(vec![xmltv_source(epg_path, 0, false)]);
            let mut id_cache = EpgIdCache::new(None);
            id_cache.insert_channel_epg_id("demo.channel");

            let merged = tv_guide.filter_merged(&mut id_cache).await.expect("merged epg");

            assert_eq!(merged.children.len(), 1);
            assert_eq!(merged.children[0].id.as_ref(), "Demo.Channel");
            assert_eq!(merged.children[0].programmes.len(), 1);
            assert_eq!(merged.children[0].programmes[0].get_transient_channel_id().as_ref(), "demo.channel");
        });
    }

    #[test]
    fn epg_priority_merge_filter_accepts_later_source_for_already_processed_channel() {
        run_async_test(async move {
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

            let tv_guide = TVGuide::new(vec![xmltv_source(high_path, 0, false), xmltv_source(low_path, 10, false)]);
            let mut id_cache = EpgIdCache::new(None);
            id_cache.insert_channel_epg_id("demo.channel");

            let merged = tv_guide.filter_merged(&mut id_cache).await.expect("merged epg");

            assert!(id_cache.contains_processed_epg_id("demo.channel"));
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
        });
    }

    #[test]
    fn ics_dummy_policy_fills_after_real_programme_merge_without_overwriting_xmltv() {
        run_async_test(async move {
            use chrono::{Datelike, TimeZone, Utc};

            let dir = tempdir().unwrap();
            let xml_path = dir.path().join("guide.xml");
            let ics_path = dir.path().join("empty.ics");
            let now = Utc::now();
            let real_start = Utc.with_ymd_and_hms(now.year(), now.month(), now.day(), 4, 0, 0).single().expect("start");
            let real_stop = Utc.with_ymd_and_hms(now.year(), now.month(), now.day(), 6, 0, 0).single().expect("stop");

            fs::write(
                &xml_path,
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?>
<tv>
  <channel id="f1.calendar">
    <display-name>Formula 1</display-name>
  </channel>
  <programme start="{start}" stop="{stop}" channel="f1.calendar">
    <title>Real Show</title>
  </programme>
</tv>"#,
                    start = real_start.format("%Y%m%d%H%M%S %z"),
                    stop = real_stop.format("%Y%m%d%H%M%S %z"),
                ),
            )
            .unwrap();
            fs::write(&ics_path, "BEGIN:VCALENDAR\nEND:VCALENDAR\n").unwrap();

            let guide = TVGuide::new(vec![
                xmltv_source(xml_path, 0, false),
                PersistedEpgSource {
                    file_path: ics_path,
                    priority: 1,
                    logo_override: false,
                    kind: PersistedEpgSourceKind::Ics {
                        channel_id: "f1.calendar".intern(),
                        channel_title: Some("Formula 1".intern()),
                        match_names: Vec::new(),
                        config: Box::new(IcsEpgSourceConfig {
                            timezone: "UTC".to_string(),
                            dummy: IcsDummyConfig {
                                enabled: true,
                                title: "No programme".to_string(),
                                description: String::new(),
                                days_past: 0,
                                days_future: 0,
                                block_hours: 4,
                                min_gap_minutes: 1,
                            },
                            ..IcsEpgSourceConfig::default()
                        }),
                    },
                },
            ]);
            let mut id_cache = EpgIdCache::new(None);
            id_cache.insert_channel_epg_id("f1.calendar");

            let merged = guide.filter_merged(&mut id_cache).await.expect("merged");
            let channel = &merged.children[0];
            assert_eq!(channel.id.as_ref(), "f1.calendar");
            assert_eq!(
                channel.programmes.iter().filter(|programme| programme.title.as_deref() == Some("Real Show")).count(),
                1
            );
            let dummy_programmes = channel
                .programmes
                .iter()
                .filter(|programme| programme.title.as_deref() == Some("No programme"))
                .collect::<Vec<_>>();
            assert!(!dummy_programmes.is_empty());
            for dummy in dummy_programmes {
                assert!(
                    dummy.stop <= real_start.timestamp() || dummy.start >= real_stop.timestamp(),
                    "dummy overlaps real programme: {}-{}",
                    dummy.start,
                    dummy.stop
                );
            }
        });
    }

    #[test]
    fn dummy_policy_selection_uses_priority_then_source_order() {
        let channel = || EpgChannel {
            id: "f1.calendar".intern(),
            title: Some("Formula 1".intern()),
            icon: None,
            programmes: Vec::new(),
        };
        let merge = |policies| {
            merge_epg_channels_by_priority_with_dummy_policies(vec![(0, vec![channel()])], policies)
                .into_iter()
                .next()
                .expect("merged channel")
        };

        let priority_winner =
            merge(vec![dummy_policy_source(10, 0, "Low priority"), dummy_policy_source(-10, 1, "High priority")]);
        assert!(!priority_winner.programmes.is_empty());
        assert!(priority_winner.programmes.iter().all(|programme| programme.title.as_deref() == Some("High priority")));

        let source_order_winner =
            merge(vec![dummy_policy_source(0, 2, "Later source"), dummy_policy_source(0, 1, "Earlier source")]);
        assert!(!source_order_winner.programmes.is_empty());
        assert!(source_order_winner
            .programmes
            .iter()
            .all(|programme| programme.title.as_deref() == Some("Earlier source")));
    }

    #[ignore = "requires a local XMLTV fixture under /tmp"]
    #[test]
    fn parse_test() {
        let run_test = async move || {
            //let file_path = PathBuf::from("/tmp/epg.xml.gz");
            let file_path = PathBuf::from("/tmp/invalid_epg.xml");

            if file_path.exists() {
                let tv_guide = TVGuide::new(vec![xmltv_source(file_path, 0, false)]);

                let mut id_cache = EpgIdCache::new(None);
                id_cache.insert_channel_epg_id("342");
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
        tokio::runtime::Runtime::new().unwrap().block_on(run_test());
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
        let mut epg_smart_cfg_dto = EpgSmartMatchConfigDto {
            enabled: true,
            name_prefix: EpgNamePrefix::Suffix(".".to_string()),
            ..Default::default()
        };
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

    use crate::processor::EpgIdCache;
    use rphonetic::{Encoder, Metaphone};
    use shared::{
        model::{EpgNamePrefix, EpgSmartMatchConfigDto},
        utils::Internable,
    };

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
        let mut epg_smart_cfg_dto = EpgSmartMatchConfigDto {
            enabled: true,
            name_prefix: EpgNamePrefix::Suffix(".".to_string()),
            ..Default::default()
        };
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

    #[test]
    fn xmltv_programme_tags_are_extracted() {
        run_async_test(async move {
            let dir = tempdir().expect("temp dir");
            let epg_path = dir.path().join("programme-tags.xml");
            fs::write(
                &epg_path,
                r#"<tv>
  <channel id="ESPN.us"><display-name>ESPN</display-name></channel>
  <programme start="20260718180000 +0000" stop="20260718200000 +0000" channel="ESPN.us">
    <title lang="en">Softball</title>
    <icon src="https://example.com/softball.jpg"/>
    <icon src=""/>
    <category lang="en">Softball</category>
    <category>Sports</category>
    <live/>
    <new></new>
  </programme>
</tv>"#,
            )
            .expect("write XMLTV fixture");

            let guide = TVGuide::new(vec![xmltv_source(epg_path, 0, false)]);
            let mut id_cache = EpgIdCache::new(None);
            id_cache.insert_channel_epg_id("ESPN.us");
            let merged = guide.filter_merged(&mut id_cache).await.expect("merged EPG");
            let programme = &merged.children[0].programmes[0];

            assert_eq!(
                programme.categories,
                vec![
                    EpgCategory { value: "Softball".intern(), lang: Some("en".intern()) },
                    EpgCategory { value: "Sports".intern(), lang: None },
                ],
            );
            assert_eq!(programme.icon.as_deref(), Some("https://example.com/softball.jpg"));
            assert!(programme.is_live);
            assert!(programme.is_new);
        });
    }

    /// Regression guard for the buffer-preallocation in `parse_tvguide`.
    ///
    /// `quick_xml` uses the caller-provided `Vec<u8>` as a monotonically growing
    /// read buffer (it does not call `.clear()` between events; the caller
    /// slices the new portion). For an XMLTV feed with a single giant
    /// `<programme>` description, that buffer used to start at 0 capacity and
    /// double ~25 times on the way to ~163 MiB — every doubling being a
    /// copying realloc. Starting with `Vec::with_capacity(64 * 1024)` removes
    /// the realloc chain. This test feeds a programme whose description is
    /// bigger than that initial capacity and checks that parsing still
    /// succeeds; the absence of the test would have allowed a silent
    /// regression to `Vec::new()` without breaking any visible behaviour.
    #[test]
    fn parse_tvguide_handles_giant_programme_description() {
        use crate::parser::xmltv::{parse_tvguide, EPG_TAG_DESC, EPG_TAG_PROGRAMME};

        run_async_test(async {
            // 200 KiB of text content — well beyond the 64 KiB preallocation.
            let big_text = "x".repeat(200 * 1024);
            // `channel` attribute is required for the parser to fire the
            // callback on a <programme> tag (see `handle_tag_end`).
            let xml = format!(
                r#"<?xml version="1.0"?><tv><programme channel="c1"><title>t</title><desc>{big_text}</desc></programme></tv>"#
            );
            let mut emitted_tags: Vec<tuliprox_core::model::XmlTag> = Vec::new();
            parse_tvguide(xml.as_bytes(), &mut |tag| {
                emitted_tags.push(tag);
            })
            .await;

            // The whole point of the preallocation is that the full payload
            // survives the round trip — not just the presence of a tag.
            let programme = emitted_tags
                .iter()
                .find(|tag| tag.name.as_ref() == EPG_TAG_PROGRAMME)
                .expect("parser emitted no <programme> tag");
            let children = programme.children.as_deref().expect("<programme> has no children");
            let desc = children
                .iter()
                .find(|child| child.name.as_ref() == EPG_TAG_DESC)
                .expect("<programme> emitted no <desc> child");
            assert_eq!(
                desc.value.as_deref().map(str::len),
                Some(big_text.len()),
                "<desc> payload was truncated; got {} bytes, expected {}",
                desc.value.as_deref().map_or(0, str::len),
                big_text.len()
            );
        });
    }

    /// Smallest testable piece of the disk-spilling path. The Drop guard is
    /// what keeps a panic or early return from leaking temp files into /tmp.
    /// If this fails, every other disk-spilling test is built on sand.
    #[test]
    fn disk_epg_source_removes_its_temp_file_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("epg-source.db");
        std::fs::write(&path, b"placeholder").unwrap();
        assert!(path.exists(), "precondition: temp file must exist");

        {
            let _src = super::DiskEpgSource::new(path.clone(), None, 0, 0);
        } // _src dropped here

        assert!(!path.exists(), "DiskEpgSource::Drop must remove its temp file");
    }

    /// Builds an accumulator with N channels and drains it to disk. Asserts:
    /// (a) the temp file exists while the guard is alive, (b) it has
    /// non-trivial size, (c) the file is removed after Drop. Together these
    /// prove the writer runs end-to-end without panicking.
    #[test]
    fn finish_into_disk_writes_a_real_temp_tree() {
        use super::EpgMergeAccumulator;

        let mut acc = EpgMergeAccumulator::new();
        for i in 0..250u32 {
            let id: Arc<str> = format!("channel-{i:04}").into();
            let ch = shared::model::EpgChannel {
                id: Arc::clone(&id),
                title: Some(format!("title {i}").into()),
                icon: None,
                programmes: vec![shared::model::EpgProgramme::new(i64::from(i), i64::from(i + 1), id)],
            };
            acc.upsert_channel(i16::try_from(i).unwrap_or(0), 0, false, ch);
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("epg-source.db");
        let src = acc.finish_into_disk(path.clone(), 5, 0).unwrap();
        assert!(path.exists(), "temp tree file must exist while guard is alive");
        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size > 1024, "expected non-trivial size, got {size}");
        drop(src);
        assert!(!path.exists(), "Drop must remove the temp file");
    }

    /// Contract test: two temp trees written from two accumulators must
    /// merge into the same `Epg` as the in-memory `finish_epg_with_icon_overrides`
    /// would produce from the same inputs. Without this, every other
    /// optimisation is built on sand.
    #[test]
    fn disk_path_matches_in_memory_finish() {
        use super::{merge_epg_trees, EpgMergeAccumulator};

        // Build one source by appending one programme per channel through the
        // accumulator's primary entry point. `add_channel_with_programmes` is
        // the same call the in-memory `merge_epg_channels_by_priority` uses,
        // so the in-memory reference and the disk path go through identical
        // APIs. The two sources use disjoint programme intervals
        // (source 0: 0..1, source 1: 100..101) so the merged channel should
        // retain both programmes end-to-end.
        fn build_acc(source: usize, channels: std::ops::Range<u32>) -> EpgMergeAccumulator {
            let mut acc = EpgMergeAccumulator::new();
            for i in channels {
                let id: Arc<str> = format!("ch-{i:04}").into();
                let priority = if source == 0 { 5 } else { 3 }; // source 1 wins
                let (start, stop) = if source == 0 {
                    (i64::from(i), i64::from(i + 1))
                } else {
                    (100 + i64::from(i), 101 + i64::from(i))
                };
                acc.add_channel_with_programmes(
                    i16::try_from(priority).unwrap_or(0),
                    source,
                    false,
                    shared::model::EpgChannel {
                        id: Arc::clone(&id),
                        title: Some(format!("title-{source}-{i}").into()),
                        icon: None,
                        programmes: vec![shared::model::EpgProgramme::new(start, stop, id)],
                    },
                );
            }
            acc
        }

        // Reference: in-memory merge of the same two sources.
        let ref_acc_a = build_acc(0, 0..50);
        let ref_acc_b = build_acc(1, 0..50);
        // `EpgMergeAccumulator` is single-shot, so the reference merge has to
        // rebuild a fresh accumulator. The two halves contribute disjoint
        // programme intervals, so a single accumulator sees both.
        let mut ref_acc = EpgMergeAccumulator::new();
        for i in 0..50 {
            let id: Arc<str> = format!("ch-{i:04}").into();
            ref_acc.add_channel_with_programmes(
                5,
                0,
                false,
                shared::model::EpgChannel {
                    id: Arc::clone(&id),
                    title: Some(format!("title-0-{i}").into()),
                    icon: None,
                    programmes: vec![shared::model::EpgProgramme::new(i64::from(i), i64::from(i + 1), id)],
                },
            );
        }
        for i in 0..50 {
            let id: Arc<str> = format!("ch-{i:04}").into();
            ref_acc.add_channel_with_programmes(
                3,
                1,
                false,
                shared::model::EpgChannel {
                    id: Arc::clone(&id),
                    title: Some(format!("title-1-{i}").into()),
                    icon: None,
                    programmes: vec![shared::model::EpgProgramme::new(100 + i64::from(i), 101 + i64::from(i), id)],
                },
            );
        }
        let reference = ref_acc.finish_epg_with_icon_overrides().unwrap().0;
        let _ = (ref_acc_a, ref_acc_b); // keep the per-source builder API in the test

        // Disk path: two temp trees, then merge.
        let dir = tempfile::tempdir().unwrap();
        let src_a = build_acc(0, 0..50).finish_into_disk(dir.path().join("a.db"), 5, 0).unwrap();
        let src_b = build_acc(1, 0..50).finish_into_disk(dir.path().join("b.db"), 3, 1).unwrap();
        let merged = merge_epg_trees(vec![src_a, src_b]).unwrap().unwrap().0;

        assert_eq!(reference.children.len(), merged.children.len(), "channel counts must match");
        for (left, right) in reference.children.iter().zip(merged.children.iter()) {
            assert_eq!(left.id, right.id, "channel order must match");
            // Source 1 had priority 3 (lower = wins) so its title should win.
            assert_eq!(left.title, right.title, "priority winner's title must propagate");
            // Both sources contribute distinct, non-overlapping programmes.
            // The merged channel must keep both — the disk path uses
            // `add_channel_with_programmes` so nothing is dropped on the way in.
            assert_eq!(
                left.programmes.len(),
                2,
                "channel {} should retain both source programmes, got {}",
                left.id,
                left.programmes.len()
            );
            assert_eq!(right.programmes.len(), 2, "disk-merged channel {} lost programmes", right.id);
            for (lp, rp) in left.programmes.iter().zip(right.programmes.iter()) {
                assert_eq!(lp.start, rp.start, "programme start must match for channel {}", left.id);
                assert_eq!(lp.stop, rp.stop, "programme stop must match for channel {}", left.id);
            }
        }
    }
}

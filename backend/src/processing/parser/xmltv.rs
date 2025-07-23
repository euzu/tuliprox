use crate::model::{Epg, TVGuide, XmlTag, XmlTagIcon, EPG_ATTRIB_CHANNEL, EPG_ATTRIB_ID, EPG_TAG_CHANNEL, EPG_TAG_DISPLAY_NAME, EPG_TAG_ICON, EPG_TAG_PROGRAMME, EPG_TAG_TV};
use crate::model::{EpgSmartMatchConfig, PersistedEpgSource};
use crate::processing::processor::epg::EpgIdCache;
use crate::utils::compressed_file_reader::CompressedFileReader;
use dashmap::DashMap;
use deunicode::deunicode;
use quick_xml::events::{BytesStart, BytesText, Event};
use quick_xml::Reader;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use shared::model::EpgNamePrefix;
use shared::utils::CONSTANTS;
use std::borrow::Cow;
use std::cmp::min;
use std::collections::HashMap;
use std::mem;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

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
    combined.push('.');
    combined.push_str(right);
    combined
}

/// # Panics
pub fn normalize_channel_name(name: &str, normalize_config: &EpgSmartMatchConfig) -> String {
    let normalized = deunicode(name.trim()).to_lowercase();
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
    pub fn merge(mut epgs: Vec<Epg>) -> Option<Epg> {
        if let Some(first_epg) = epgs.get_mut(0) {
            let first_epg_attributes = first_epg.attributes.take();
            let merged_children: Vec<XmlTag> = epgs.into_iter().flat_map(|epg| epg.children).collect();
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
        if let Some(children) = &mut tag.children {
            for child in children {
                match child.name.as_str() {
                    EPG_TAG_DISPLAY_NAME => {
                        if smart_match {
                            if let Some(name) = &child.value {
                                tag.normalized_epg_ids
                                    .get_or_insert_with(Vec::new)
                                    .push(normalize_channel_name(name, &id_cache.smart_match_config));
                            }
                        }
                    }
                    EPG_TAG_ICON => {
                        if let Some(src) = child.get_attribute_value("src") {
                            if !src.is_empty() {
                                tag.icon = XmlTagIcon::Src(src.to_string());
                                child.icon = XmlTagIcon::Exists;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn try_fuzzy_matching(id_cache: &mut EpgIdCache, epg_id: &str, tag: &XmlTag, fuzzy_matching: bool) -> bool {
        let mut matched = tag
            .normalized_epg_ids
            .as_ref()
            .is_some_and(|ids| id_cache.match_with_normalized(epg_id, ids));
        if !matched && fuzzy_matching {
            let (fuzzy_matched, matched_normalized_name) = Self::find_best_fuzzy_match(id_cache, tag);
            if fuzzy_matched {
                if let Some(key) = matched_normalized_name {
                    let id = epg_id.to_string();
                    id_cache.normalized.entry(key).and_modify(|entry| {
                        entry.replace(id.clone());
                        id_cache.channel_epg_id.insert(Cow::Owned(id));
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
    fn find_best_fuzzy_match(id_cache: &mut EpgIdCache, tag: &XmlTag) -> (bool, Option<String>) {
        let early_exit_flag = Arc::new(AtomicBool::new(false));
        let data: Mutex<(u16, Option<Cow<str>>)> = Mutex::new((0, None));

        let match_threshold = id_cache.smart_match_config.match_threshold;
        let best_match_threshold = id_cache.smart_match_config.best_match_threshold;

        if let Some(normalized_epg_ids) = tag.normalized_epg_ids.as_ref() {
            for tag_normalized in normalized_epg_ids {
                let tag_code = id_cache.phonetic(tag_normalized);
                if let Some(normalized) = id_cache.phonetics.get(&tag_code) {
                    normalized.par_iter().find_any(|norm_key| {
                        let match_jw = strsim::jaro_winkler(norm_key, tag_normalized);
                        #[allow(clippy::cast_possible_truncation)]
                        #[allow(clippy::cast_sign_loss)]
                        let mjw = min(100, (match_jw * 100.0).round() as u16);
                        if mjw >= match_threshold {
                            if let Ok(mut lock) = data.lock() {
                                if lock.0 < mjw {
                                    *lock = (mjw, Some(Cow::Borrowed(norm_key)));
                                }
                            }
                            if mjw > best_match_threshold {
                                return true; // (true, matched_normalized_epg_id.map(|s| s.to_string()));
                            }
                        }
                        false
                    });
                }
            }
        }
        // is there an early exit strategy ???

        if early_exit_flag.load(Ordering::SeqCst) {
            if let Ok(mut result) = data.lock() {
                return (true, result.1.take().as_ref().map(ToString::to_string));
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
    fn process_epg_file(id_cache: &mut EpgIdCache, epg_source: &PersistedEpgSource) -> Option<Epg> {
        match CompressedFileReader::new(&epg_source.file_path) {
            Ok(mut reader) => {
                let mut children: Vec<XmlTag> = vec![];
                let mut tv_attributes: Option<HashMap<String, String>> = None;
                let smart_match = id_cache.smart_match_config.enabled;
                let fuzzy_matching = smart_match && id_cache.smart_match_config.fuzzy_matching;
                let mut filter_tags = |mut tag: XmlTag| {
                    match tag.name.as_str() {
                        EPG_TAG_CHANNEL => {
                            let epg_id = tag.get_attribute_value(EPG_ATTRIB_ID).map_or_else(String::new, std::string::ToString::to_string);
                            if !epg_id.is_empty() && !id_cache.processed.contains(&epg_id) {
                                Self::prepare_tag(id_cache, &mut tag, smart_match);
                                if smart_match {
                                    if Self::try_fuzzy_matching(id_cache, &epg_id, &tag, fuzzy_matching) {
                                        children.push(tag);
                                        id_cache.processed.insert(epg_id);
                                    }
                                } else {
                                    let borrowed_epg_id = Cow::Borrowed(epg_id.as_str());
                                    if id_cache.channel_epg_id.contains(&borrowed_epg_id) {
                                        children.push(tag);
                                        id_cache.processed.insert(epg_id);
                                    }
                                }
                            }
                        }
                        EPG_TAG_PROGRAMME => {
                            if let Some(epg_id) = tag.get_attribute_value(EPG_ATTRIB_CHANNEL) {
                                if id_cache.processed.contains(epg_id) {
                                    let borrowed_epg_id = Cow::Borrowed(epg_id.as_str());
                                    if id_cache.channel_epg_id.contains(&borrowed_epg_id) {
                                        children.push(tag);
                                    }
                                }
                            }
                        }
                        EPG_TAG_TV => {
                            tv_attributes = tag.attributes.take();
                        }
                        _ => {}
                    }
                };

                parse_tvguide(&mut reader, &mut filter_tags);

                if children.is_empty() {
                    return None;
                }

                Some(Epg {
                    logo_override: epg_source.logo_override,
                    priority: epg_source.priority,
                    attributes: tv_attributes,
                    children,
                })
            }
            Err(_) => None
        }
    }

    pub fn filter(&self, id_cache: &mut EpgIdCache) -> Option<Vec<Epg>> {
        if id_cache.channel_epg_id.is_empty() && id_cache.normalized.is_empty() {
            return None;
        }
        let mut epg_sources: Vec<Epg> = self.get_epg_sources().iter()
            .filter_map(|epg_source| Self::process_epg_file(id_cache, epg_source))
            .collect();
        epg_sources.sort_by(|a, b| a.priority.cmp(&b.priority));
        Some(epg_sources)
    }
}


fn handle_tag_start<F>(callback: &mut F, stack: &mut Vec<XmlTag>, e: &BytesStart)
where
    F: FnMut(XmlTag),
{
    let name = String::from_utf8_lossy(e.name().as_ref()).as_ref().to_owned();
    let (is_tv_tag, is_channel, is_program) = get_tag_types(&name);
    let attributes = collect_tag_attributes(e, is_channel, is_program);
    let attribs = if attributes.is_empty() { None } else { Some(attributes) };
    let tag = XmlTag::new(name, attribs);

    if is_tv_tag {
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
            if tag.name == EPG_TAG_CHANNEL {
                if let Some(chan_id) = tag.get_attribute_value(EPG_ATTRIB_ID) {
                    if !chan_id.is_empty() {
                        callback(tag);
                    }
                }
            } else if tag.name == EPG_TAG_PROGRAMME {
                if let Some(chan_id) = tag.get_attribute_value(EPG_ATTRIB_CHANNEL) {
                    if !chan_id.is_empty() {
                        callback(tag);
                    }
                }
            } else if !stack.is_empty() {
                if let Some(old_tag) = stack.pop().map(|mut r| {
                    r.children = Some(match r.children.take() {
                        None => vec![tag],
                        Some(mut tags) => {
                            tags.push(tag);
                            tags
                        }
                    });
                    r
                }) {
                    stack.push(old_tag);
                }
            }
        }
    }
}

fn handle_text_tag(stack: &mut [XmlTag], e: &BytesText) {
    if !stack.is_empty() {
        if let Ok(text) = e.unescape() {
            let t = text.trim();
            if !t.is_empty() {
                if let Some(tag) = stack.last_mut() {
                    tag.value = Some(t.to_string());
                }
            }
        }
    }
}

pub fn parse_tvguide<R, F>(content: R, callback: &mut F)
where
    R: std::io::BufRead,
    F: FnMut(XmlTag),
{
    let mut stack: Vec<XmlTag> = vec![];
    let mut reader = Reader::from_reader(content);
    let mut buf = Vec::<u8>::new();
    loop {
        match reader.read_event_into(&mut buf) {
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

fn get_tag_types(name: &str) -> (bool, bool, bool) {
    let (is_tv_tag, is_channel, is_program) = match name {
        EPG_TAG_TV => (true, false, false),
        EPG_TAG_CHANNEL => (false, true, false),
        EPG_TAG_PROGRAMME => (false, false, true),
        _ => (false, false, false)
    };
    (is_tv_tag, is_channel, is_program)
}

fn collect_tag_attributes(e: &BytesStart, is_channel: bool, is_program: bool) -> HashMap<String, String> {
    let attributes = e.attributes().filter_map(Result::ok)
        .filter_map(|a| {
            let key = String::from_utf8_lossy(a.key.as_ref()).to_string();
            if let Ok(value) = a.unescape_value().as_ref() {
                if value.is_empty() {
                    None
                } else if (is_channel && key == EPG_ATTRIB_ID) || (is_program && key == EPG_ATTRIB_CHANNEL) {
                    Some((key, value.to_lowercase().to_string()))
                } else {
                    Some((key, value.to_string()))
                }
            } else {
                None
            }
        }).collect::<HashMap<String, String>>();
    attributes
}

pub fn flatten_tvguide(tv_guides: &[Epg]) -> Option<Epg> {
    if tv_guides.is_empty() {
        None
    } else {
        let epg_children = Mutex::new(Vec::new());
        let epg_attributes = tv_guides.first().and_then(|t| t.attributes.clone());
        let count = tv_guides.iter().map(|tvg| tvg.children.len()).sum();
        let channel_mapping: DashMap<String, i16> = DashMap::with_capacity(count);

        let mut sorted_guides = tv_guides.to_vec();
        // sort by priority
        sorted_guides.sort_by(|a, b| a.priority.cmp(&b.priority));
        // if executed parallel it does not matter how we sort.
        sorted_guides.par_iter().for_each(|guide| {
            let mut children = vec![];
            guide.children.iter().for_each(|c| {
                if c.name.as_str() == EPG_TAG_CHANNEL {
                    if let Some(chan_id) = c.get_attribute_value(EPG_ATTRIB_ID) {
                        let should_add = {
                            // if not stored
                            !channel_mapping.contains_key(chan_id) ||
                                // or if priority is higher (less means higher priority)
                                channel_mapping.get(chan_id).as_deref().is_none_or(|&priority| guide.priority < priority)
                        };
                        if should_add {
                            if let Some(mut existing) = channel_mapping.get_mut(chan_id) {
                                if guide.priority < *existing {
                                    *existing = guide.priority;
                                    children.push(c.clone());
                                }
                            } else {
                                channel_mapping.insert(chan_id.clone(), guide.priority);
                                children.push(c.clone());
                            }
                        }
                    }
                }
            });
            guide.children.iter().for_each(|c| {
                if c.name.as_str() == EPG_TAG_PROGRAMME {
                    if let Some(chan_id) = c.get_attribute_value(EPG_ATTRIB_CHANNEL) {
                        if let Some(stored_priority) = channel_mapping.get(chan_id) {
                            if *stored_priority == guide.priority {
                                children.push(c.clone());
                            }
                        }
                    }
                }
            });

            if let Ok(mut guard) = epg_children.lock() {
                guard.extend(children);
            }
        });
        let children = if let Ok(mut children) = epg_children.lock() {
            mem::take(&mut *children)
        } else {
            vec![]
        };
        let epg = Epg {
            logo_override: false,
            priority: 0,
            attributes: epg_attributes,
            children,
        };
        Some(epg)
    }
}

#[cfg(test)]
mod tests {
    use crate::model::EpgSmartMatchConfig;
    use crate::processing::parser::xmltv::normalize_channel_name;

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


    // #[test]
    // fn parse_test() -> io::Result<()> {
    //     let file_path = PathBuf::from("/tmp/epg.xml.gz");
    //
    //     if file_path.exists() {
    //         let tv_guide = TVGuide { file: file_path };
    //
    //         let mut channel_ids = HashSet::from(["channel.1".to_string(), "channel.2".to_string(), "channel.3".to_string()]);
    //         let mut nomalized = HashMap::new();
    //         match tv_guide.filter(&mut channel_ids, &mut nomalized) {
    //             None => assert!(false, "No epg filtered"),
    //             Some(epg) => {
    //                 assert_eq!(epg.children.len(), channel_ids.len() * 2, "Epg size does not match")
    //             }
    //         }
    //     }
    //     Ok(())
    // }

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

    use rphonetic::{Encoder, Metaphone};
    use shared::model::{EpgNamePrefix, EpgSmartMatchConfigDto};

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
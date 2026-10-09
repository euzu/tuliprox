use super::{
    merge::{collect_tag_attributes, get_tag_type},
    normalize_channel_name, EpgMergeAccumulator, TVGuide,
};
use crate::processor::EpgIdCache;
use log::error;
use quick_xml::events::{BytesStart, BytesText, Event};
use shared::{
    concat_string,
    model::{EpgCategory, EpgChannel, EpgProgramme},
    utils::Internable,
};
use std::{borrow::Cow, collections::HashSet, sync::Arc};
use tokio::io::AsyncRead;
use tuliprox_core::{
    model::{
        PersistedEpgSource, XmlTag, XmlTagIcon, EPG_ATTRIB_CHANNEL, EPG_ATTRIB_ID, EPG_ATTRIB_LANG, EPG_TAG_CATEGORY,
        EPG_TAG_CHANNEL, EPG_TAG_DESC, EPG_TAG_DISPLAY_NAME, EPG_TAG_ICON, EPG_TAG_LIVE, EPG_TAG_NEW,
        EPG_TAG_PREVIOUSLY_SHOWN, EPG_TAG_PROGRAMME, EPG_TAG_TITLE, EPG_TAG_TV,
    },
    utils::{
        async_file_reader, compressed_file_reader_async::CompressedFileReaderAsync, parse_xmltv_time,
        with_folded_epg_id,
    },
};

impl TVGuide {
    pub(super) fn prepare_tag(id_cache: &mut EpgIdCache, tag: &mut XmlTag, smart_match: bool) {
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

    pub(super) fn channel_display_name(tag: &XmlTag) -> Option<Arc<str>> {
        tag.children.as_ref().and_then(|children| {
            children
                .iter()
                .find(|child| child.name.as_ref() == EPG_TAG_DISPLAY_NAME)
                .and_then(|child| child.value.clone())
        })
    }

    pub(super) fn channel_icon(tag: &XmlTag) -> Option<Arc<str>> {
        match &tag.icon {
            XmlTagIcon::Src(src) => Some(Arc::clone(src)),
            XmlTagIcon::Undefined | XmlTagIcon::Exists => None,
        }
    }

    pub(super) fn extract_programme(
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
    pub(super) async fn process_epg_file(
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

pub(super) fn handle_tag_end<F>(callback: &mut F, stack: &mut Vec<XmlTag>)
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

use super::{
    merge::{collect_tag_attributes, get_tag_type},
    normalize_channel_name, EpgMergeAccumulator, TVGuide,
};
use crate::processor::{EarlyGuideChannel, EpgIdCache};
use log::error;
use quick_xml::events::{BytesStart, BytesText, Event};
use shared::{
    concat_string,
    model::{
        EpgCategory, EpgChannel, EpgProgramme, HookCtx, PlaylistItem, PluginEpgChannel, PluginHook, PluginHost,
        PluginMatchRequest, PluginRunReport,
    },
    utils::Internable,
};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    sync::Arc,
};
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
    ///
    /// The `<channel>` arm of the guide parser: offers the channel to the smart
    /// matcher and, when the playlist wants it, adds it to `accumulator`. Returns
    /// `true` when it was added. Shared by the streaming and the hooked pass.
    fn accept_channel(
        id_cache: &mut EpgIdCache,
        accumulator: &mut EpgMergeAccumulator,
        epg_source: &PersistedEpgSource,
        source_order: usize,
        source_processed: &mut HashSet<Arc<str>>,
        attribs: &GuideAttribs,
        tag: &mut XmlTag,
    ) -> bool {
        let smart_match = id_cache.smart_match_config.enabled;
        let tag_epg_id = tag.get_attribute_value(&attribs.id).map_or_else(|| "".intern(), Internable::intern);
        if tag_epg_id.is_empty() {
            return false;
        }

        Self::prepare_tag(id_cache, tag, smart_match);
        if smart_match && id_cache.needs_guide_names(&tag_epg_id) {
            id_cache.register_guide_names(
                &tag_epg_id,
                tag.children.iter().flatten().filter_map(|child| {
                    (child.name.as_ref() == EPG_TAG_DISPLAY_NAME).then_some(child.value.as_ref()).flatten()
                }),
            );
        }
        // Case-insensitive (ASCII) membership: fold the guide id for the
        // lookup only; `tag_epg_id` keeps its original case for output.
        let add_channel = if smart_match {
            let direct_match = id_cache.contains_channel_epg_id(&tag_epg_id);
            let normalized_match = tag.normalized_epg_ids.as_ref().is_some_and(|candidates| {
                id_cache.match_epg_channel_candidates(&tag_epg_id, candidates, epg_source.priority, source_order)
            });
            direct_match || normalized_match
        } else {
            id_cache.contains_channel_epg_id(&tag_epg_id)
        };

        if add_channel {
            Self::add_channel(id_cache, accumulator, epg_source, source_order, source_processed, &tag_epg_id, tag);
        }
        add_channel
    }

    /// Keep `tag`'s channel for this source: its programmes are accepted as they
    /// stream past, and it joins the merge.
    fn add_channel(
        id_cache: &mut EpgIdCache,
        accumulator: &mut EpgMergeAccumulator,
        epg_source: &PersistedEpgSource,
        source_order: usize,
        source_processed: &mut HashSet<Arc<str>>,
        tag_epg_id: &Arc<str>,
        tag: &XmlTag,
    ) {
        with_folded_epg_id(tag_epg_id, |folded| source_processed.insert(folded.intern()));
        id_cache.insert_processed_epg_id(tag_epg_id);
        accumulator.upsert_channel(
            epg_source.priority,
            source_order,
            epg_source.logo_override,
            EpgChannel {
                id: Arc::clone(tag_epg_id),
                title: Self::channel_display_name(tag),
                icon: Self::channel_icon(tag),
                programmes: vec![],
            },
        );
    }

    /// The `<programme>` arm: the programme of `tag` when its channel was
    /// accepted by this source.
    fn accepted_programme(
        tag: &XmlTag,
        source_processed: &HashSet<Arc<str>>,
        attribs: &GuideAttribs,
    ) -> Option<EpgProgramme> {
        let epg_id = tag.get_attribute_value(&attribs.channel)?;
        if !with_folded_epg_id(epg_id, |folded| source_processed.contains(folded)) {
            return None;
        }
        Self::extract_programme(tag, epg_id, &attribs.start, &attribs.stop, &attribs.catchup_id)
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
    ///
    /// `plugins` runs `on-epg-loaded` once over the source's channel list, before
    /// any channel reaches the matcher, and `transform-programmes` over bounded
    /// batches of programmes as they are built, both only while
    /// `EpgIdCache::hooks` is enabled and a plugin wants them. Otherwise the
    /// source is parsed in one streaming pass with no extra buffering.
    pub(super) async fn process_epg_file<P: PluginHost>(
        plugins: &P,
        id_cache: &mut EpgIdCache,
        epg_source: &PersistedEpgSource,
        source_order: usize,
        accumulator: &mut EpgMergeAccumulator,
    ) -> bool {
        let attribs = GuideAttribs::new();
        let hooks = GuideHooks::probe(plugins, id_cache);

        match CompressedFileReaderAsync::new(&epg_source.file_path).await {
            Ok(mut reader) => {
                let accepted_channels = if hooks.any() {
                    let input = id_cache.hooks.input.clone();
                    HookedPass::new(plugins, hooks, id_cache, accumulator, epg_source, source_order, &input)
                        .run(reader)
                        .await
                } else {
                    let mut source_processed: HashSet<Arc<str>> = HashSet::with_capacity(5000);
                    let mut accepted_channels = 0usize;
                    let mut filter_tags = |mut tag: XmlTag| match tag.name.as_ref() {
                        EPG_TAG_CHANNEL => {
                            if Self::accept_channel(
                                id_cache,
                                accumulator,
                                epg_source,
                                source_order,
                                &mut source_processed,
                                &attribs,
                                &mut tag,
                            ) {
                                accepted_channels += 1;
                            }
                        }
                        EPG_TAG_PROGRAMME => {
                            if let Some(programme) = Self::accepted_programme(&tag, &source_processed, &attribs) {
                                accumulator.push_programme(epg_source.priority, source_order, programme);
                            }
                        }
                        EPG_TAG_TV => {
                            accumulator.set_attributes_if_preferred(epg_source.priority, source_order, tag.attributes);
                        }
                        _ => {}
                    };

                    parse_tvguide(&mut reader, &mut filter_tags).await;
                    accepted_channels
                };
                accepted_channels > 0
            }
            Err(e) => {
                log::warn!("Failed to process EPG file {}: {e}", epg_source.file_path.display());
                false
            }
        }
    }
}

/// The interned attribute names of the tags the guide parser reads.
struct GuideAttribs {
    id: Arc<str>,
    channel: Arc<str>,
    start: Arc<str>,
    stop: Arc<str>,
    catchup_id: Arc<str>,
}

impl GuideAttribs {
    fn new() -> Self {
        Self {
            id: EPG_ATTRIB_ID.intern(),
            channel: EPG_ATTRIB_CHANNEL.intern(),
            start: "start".intern(),
            stop: "stop".intern(),
            catchup_id: "catchup-id".intern(),
        }
    }
}

/// Programmes `transform-programmes` is awaited over at a time while a source is
/// parsed. The plugin host splits a batch further by `plugins.batch_size`, so no
/// guest call sees more than that; this only bounds what the host buffers.
const EPG_HOOK_BATCH: usize = 250;

/// Which EPG source-level hooks run for one guide pass.
#[derive(Debug, Clone, Copy)]
pub(super) struct GuideHooks {
    /// `on-epg-loaded`.
    pub(super) loaded: bool,
    /// `transform-programmes`.
    pub(super) transform: bool,
    /// `match-channel`, asked once per name over the buffered channel list, so
    /// the guide channel it picks keeps its programmes.
    pub(super) early_match: bool,
}

impl GuideHooks {
    /// Nothing runs while the input is in maintenance (`EpgIdCache::hooks`) or
    /// no enabled plugin subscribes: the constant answer for `NoPlugins`.
    pub(super) fn probe<P: PluginHost>(plugins: &P, id_cache: &EpgIdCache) -> Self {
        if !id_cache.hooks.enabled {
            return Self { loaded: false, transform: false, early_match: false };
        }
        Self {
            loaded: plugins.wants(PluginHook::EpgLoaded, ""),
            transform: plugins.wants(PluginHook::TransformProgrammes, ""),
            early_match: !id_cache.hooks.match_items.is_empty() && plugins.wants(PluginHook::MatchChannel, ""),
        }
    }

    pub(super) const fn any(self) -> bool { self.loaded || self.transform || self.early_match }
}

/// `transform-programmes` over `programmes` in batches of [`EPG_HOOK_BATCH`], in order.
pub(super) async fn transform_in_batches<P: PluginHost>(
    plugins: &P,
    ctx: &HookCtx<'_>,
    programmes: Vec<EpgProgramme>,
    report: &mut PluginRunReport,
) -> Vec<EpgProgramme> {
    let mut transformed = Vec::with_capacity(programmes.len());
    let mut remaining = programmes.into_iter();
    loop {
        let mut batch: Vec<EpgProgramme> = remaining.by_ref().take(EPG_HOOK_BATCH).collect();
        if batch.is_empty() {
            return transformed;
        }
        plugins.transform_programmes(ctx, &mut batch, report).await;
        transformed.extend(batch);
    }
}

/// The names and icon of a buffered `<channel>` tag, as `on-epg-loaded` sees them.
fn plugin_channel(tag: &XmlTag, id: &str) -> PluginEpgChannel {
    let mut names = Vec::new();
    let mut icon = None;
    for child in tag.children.iter().flatten() {
        match child.name.as_ref() {
            EPG_TAG_DISPLAY_NAME => names.extend(child.value.as_ref().map(ToString::to_string)),
            EPG_TAG_ICON if icon.is_none() => {
                icon = child
                    .attributes
                    .as_ref()
                    .and_then(|attributes| attributes.get("src"))
                    .filter(|src| !src.is_empty())
                    .map(ToString::to_string);
            }
            _ => {}
        }
    }
    PluginEpgChannel { id: id.to_string(), names, icon }
}

/// Replace the display names and the icon of a buffered `<channel>` tag, which
/// is exactly what a returned `epg-channel` may change. Everything the later
/// `prepare_tag` derives (normalized names, the icon) is computed from the
/// rewritten children afterwards, so matching sees the plugin's names.
fn rewrite_channel_tag(tag: &mut XmlTag, channel: &PluginEpgChannel) {
    let mut children: Vec<Arc<XmlTag>> = tag
        .children
        .take()
        .into_iter()
        .flatten()
        .filter(|child| child.name.as_ref() != EPG_TAG_DISPLAY_NAME && child.name.as_ref() != EPG_TAG_ICON)
        .collect();
    for name in &channel.names {
        let mut display_name = XmlTag::new(EPG_TAG_DISPLAY_NAME.intern(), None);
        display_name.value = Some(name.as_str().intern());
        children.push(Arc::new(display_name));
    }
    if let Some(icon) = channel.icon.as_ref().filter(|icon| !icon.is_empty()) {
        let attributes = HashMap::from([("src".intern(), icon.as_str().intern())]);
        children.push(Arc::new(XmlTag::new(EPG_TAG_ICON.intern(), Some(attributes))));
    }
    tag.children = (!children.is_empty()).then_some(children);
    tag.icon = XmlTagIcon::Undefined;
}

/// One XMLTV source parsed with plugin hooks: the channel list is buffered up to
/// the first `<programme>` (XMLTV puts every `<channel>` first), `on-epg-loaded`
/// runs over it, and only then do the channels reach the matcher. Programmes are
/// built as they stream past and `transform-programmes` is awaited per
/// [`EPG_HOOK_BATCH`], never over the whole source.
struct HookedPass<'a, P: PluginHost> {
    plugins: &'a P,
    hooks: GuideHooks,
    id_cache: &'a mut EpgIdCache,
    accumulator: &'a mut EpgMergeAccumulator,
    epg_source: &'a PersistedEpgSource,
    source_order: usize,
    attribs: GuideAttribs,
    ctx: HookCtx<'a>,
    source_processed: HashSet<Arc<str>>,
    accepted: usize,
    batch: Vec<EpgProgramme>,
    report: PluginRunReport,
}

impl<'a, P: PluginHost> HookedPass<'a, P> {
    /// `input` is `id_cache.hooks.input`, copied out by the caller because the
    /// cache itself is borrowed mutably for the whole pass.
    fn new(
        plugins: &'a P,
        hooks: GuideHooks,
        id_cache: &'a mut EpgIdCache,
        accumulator: &'a mut EpgMergeAccumulator,
        epg_source: &'a PersistedEpgSource,
        source_order: usize,
        input: &'a str,
    ) -> Self {
        Self {
            plugins,
            hooks,
            id_cache,
            accumulator,
            epg_source,
            source_order,
            attribs: GuideAttribs::new(),
            ctx: HookCtx::new("").with_input(input),
            source_processed: HashSet::with_capacity(5000),
            accepted: 0,
            batch: Vec::with_capacity(EPG_HOOK_BATCH),
            report: PluginRunReport::default(),
        }
    }

    /// Streams the source. Returns how many channels were accepted.
    async fn run<R: AsyncRead + Unpin>(mut self, content: R) -> usize {
        let mut stream = TvGuideStream::new(content);
        let mut tags: Vec<XmlTag> = Vec::new();
        let mut channel_tags: Vec<XmlTag> = Vec::new();
        // Everything read from the first `<programme>` on, in file order.
        let mut held: Vec<XmlTag> = Vec::new();
        let mut open = true;
        while open && held.is_empty() {
            open = stream.next_tags(&mut tags).await;
            for tag in tags.drain(..) {
                if !held.is_empty() {
                    held.push(tag);
                } else if tag.name.as_ref() == EPG_TAG_CHANNEL {
                    channel_tags.push(tag);
                } else if tag.name.as_ref() == EPG_TAG_TV {
                    self.set_attributes(tag);
                } else if tag.name.as_ref() == EPG_TAG_PROGRAMME {
                    held.push(tag);
                }
            }
        }

        if self.hooks.loaded {
            self.on_epg_loaded(&mut channel_tags).await;
        }
        if self.hooks.early_match {
            let accepted: Vec<bool> = channel_tags.iter_mut().map(|tag| self.accept_channel(tag)).collect();
            self.early_match(&channel_tags, &accepted).await;
        } else {
            for mut tag in channel_tags {
                self.accept_channel(&mut tag);
            }
        }
        for tag in held {
            self.tag(tag).await;
        }
        while open {
            open = stream.next_tags(&mut tags).await;
            for tag in tags.drain(..) {
                self.tag(tag).await;
            }
        }
        self.flush().await;

        let Self { id_cache, report, accepted, .. } = self;
        id_cache.hooks.report.merge(report);
        accepted
    }

    fn set_attributes(&mut self, tag: XmlTag) {
        self.accumulator.set_attributes_if_preferred(self.epg_source.priority, self.source_order, tag.attributes);
    }

    fn accept_channel(&mut self, tag: &mut XmlTag) -> bool {
        let accepted = TVGuide::accept_channel(
            self.id_cache,
            self.accumulator,
            self.epg_source,
            self.source_order,
            &mut self.source_processed,
            &self.attribs,
            tag,
        );
        if accepted {
            self.accepted += 1;
        }
        accepted
    }

    /// The early `match-channel` pass: every name the built-in matcher left
    /// without a winner is offered the guide channels of this source that share a
    /// matcher bucket with it, in ONE batched plugin call. A pick that passes the
    /// matcher's gates is recorded for `finalize_matches`, and its channel is
    /// kept for this source, so its programmes are not dropped as they stream.
    async fn early_match(&mut self, channel_tags: &[XmlTag], accepted: &[bool]) {
        let keys = self.id_cache.early_match_keys();
        if keys.is_empty() {
            return;
        }
        let mut tag_at: Vec<usize> = Vec::new();
        let mut channels: Vec<EarlyGuideChannel<'_>> = Vec::new();
        for (at, tag) in channel_tags.iter().enumerate() {
            let Some(id) = tag.get_attribute_value(&self.attribs.id).filter(|id| !id.is_empty()) else {
                continue;
            };
            let names = tag
                .children
                .iter()
                .flatten()
                .filter(|child| child.name.as_ref() == EPG_TAG_DISPLAY_NAME)
                .filter_map(|child| child.value.as_deref())
                .collect();
            channels.push(EarlyGuideChannel { id, names });
            tag_at.push(at);
        }
        if channels.is_empty() {
            return;
        }
        let shortlists = self.id_cache.early_shortlists(&keys, &channels);
        let mut asked: Vec<Arc<str>> = Vec::new();
        let mut items: Vec<PlaylistItem> = Vec::new();
        let mut picks: Vec<Vec<usize>> = Vec::new();
        for (key, shortlist) in keys.iter().zip(shortlists) {
            let Some(item) = self.id_cache.hooks.match_items.get(key).filter(|_| !shortlist.is_empty()) else {
                continue;
            };
            asked.push(Arc::clone(key));
            items.push(item.clone());
            picks.push(shortlist);
        }
        if asked.is_empty() {
            return;
        }
        let requests: Vec<PluginMatchRequest<'_>> = items
            .iter()
            .zip(&picks)
            .map(|(item, shortlist)| PluginMatchRequest {
                item,
                candidates: shortlist
                    .iter()
                    .map(|&ci| plugin_channel(&channel_tags[tag_at[ci]], channels[ci].id))
                    .collect(),
            })
            .collect();
        let answers = self.plugins.match_channels(&self.ctx, &requests, &mut self.report).await;
        self.id_cache.mark_early_asked(&asked);
        if answers.len() != requests.len() {
            log::warn!(
                "match-channel plugins answered {} of {} early requests for input '{}'; the answers are ignored",
                answers.len(),
                requests.len(),
                self.ctx.input.unwrap_or_default()
            );
            return;
        }
        for ((key, shortlist), answer) in asked.iter().zip(&picks).zip(answers) {
            let Some(answer) = answer else {
                continue;
            };
            let Some(&ci) = shortlist.iter().find(|&&ci| channels[ci].id.eq_ignore_ascii_case(&answer)) else {
                continue;
            };
            if !self.id_cache.record_early_pick(key, &channels[ci]) {
                continue;
            }
            let at = tag_at[ci];
            if !accepted[at] {
                let tag = &channel_tags[at];
                let epg_id = channels[ci].id.intern();
                TVGuide::add_channel(
                    self.id_cache,
                    self.accumulator,
                    self.epg_source,
                    self.source_order,
                    &mut self.source_processed,
                    &epg_id,
                    tag,
                );
                self.accepted += 1;
            }
        }
    }

    /// One tag after the channel list closed. A late `<channel>` is accepted as
    /// ever, but `on-epg-loaded` has already run and does not see it.
    async fn tag(&mut self, tag: XmlTag) {
        match tag.name.as_ref() {
            EPG_TAG_CHANNEL => {
                let mut tag = tag;
                self.accept_channel(&mut tag);
            }
            EPG_TAG_PROGRAMME => {
                if let Some(programme) = TVGuide::accepted_programme(&tag, &self.source_processed, &self.attribs) {
                    self.batch.push(programme);
                    if self.batch.len() >= EPG_HOOK_BATCH {
                        self.flush().await;
                    }
                }
            }
            EPG_TAG_TV => self.set_attributes(tag),
            _ => {}
        }
    }

    /// Hand the batch to `transform-programmes` and into the accumulator.
    async fn flush(&mut self) {
        if self.batch.is_empty() {
            return;
        }
        if self.hooks.transform {
            self.plugins.transform_programmes(&self.ctx, &mut self.batch, &mut self.report).await;
        }
        for programme in self.batch.drain(..) {
            self.accumulator.push_programme(self.epg_source.priority, self.source_order, programme);
        }
    }

    /// `on-epg-loaded` over the buffered channel list, then the rewrite of the
    /// tags it changed. Sparse: a channel the plugins do not return, or return
    /// unchanged, keeps its tag untouched.
    async fn on_epg_loaded(&mut self, channel_tags: &mut [XmlTag]) {
        let mut seen: HashSet<String> = HashSet::new();
        let mut views: Vec<PluginEpgChannel> = Vec::new();
        for tag in channel_tags.iter() {
            let Some(id) = tag.get_attribute_value(&self.attribs.id).filter(|id| !id.is_empty()) else {
                continue;
            };
            if with_folded_epg_id(id, |folded| seen.insert(folded.to_string())) {
                views.push(plugin_channel(tag, id));
            }
        }
        if views.is_empty() {
            return;
        }
        let before: HashMap<String, PluginEpgChannel> =
            views.iter().map(|view| (with_folded_epg_id(&view.id, str::to_string), view.clone())).collect();
        self.plugins.on_epg_loaded(&self.ctx, &mut views, &mut self.report).await;
        let mut after: HashMap<String, PluginEpgChannel> = HashMap::with_capacity(views.len());
        for view in views {
            after.entry(with_folded_epg_id(&view.id, str::to_string)).or_insert(view);
        }
        for tag in channel_tags {
            let Some(id) = tag.get_attribute_value(&self.attribs.id) else {
                continue;
            };
            let folded = with_folded_epg_id(id, str::to_string);
            if let Some(changed) = after.get(&folded).filter(|view| before.get(&folded) != Some(*view)) {
                rewrite_channel_tag(tag, changed);
            }
        }
    }
}

/// Pull-style twin of [`parse_tvguide`]: the same tags, produced on demand so a
/// caller may `.await` (a plugin hook) between them. `parse_tvguide` keeps its
/// callback form for the plugin-free path.
struct TvGuideStream<R: AsyncRead + Unpin> {
    reader: quick_xml::reader::Reader<tokio::io::BufReader<R>>,
    stack: Vec<XmlTag>,
    buf: Vec<u8>,
}

impl<R: AsyncRead + Unpin> TvGuideStream<R> {
    fn new(content: R) -> Self {
        Self {
            reader: quick_xml::reader::Reader::from_reader(async_file_reader(content)),
            stack: Vec::new(),
            // See `parse_tvguide` for the size.
            buf: Vec::with_capacity(64 * 1024),
        }
    }

    /// Reads until at least one complete tag is appended to `out`, or the input
    /// ends. `false` means the end: no further call produces a tag.
    async fn next_tags(&mut self, out: &mut Vec<XmlTag>) -> bool {
        loop {
            let before = out.len();
            match self.reader.read_event_into_async(&mut self.buf).await {
                Ok(Event::Eof) => return false,
                Ok(event) => dispatch_event(event, &mut |tag| out.push(tag), &mut self.stack),
                Err(_) => {}
            }
            self.buf.clear();
            if out.len() > before {
                return true;
            }
        }
    }
}

fn dispatch_event<F>(event: Event<'_>, callback: &mut F, stack: &mut Vec<XmlTag>)
where
    F: FnMut(XmlTag),
{
    match event {
        Event::Start(e) => handle_tag_start(callback, stack, &e),
        Event::Empty(e) => {
            handle_tag_start(callback, stack, &e);
            handle_tag_end(callback, stack);
        }
        Event::End(_e) => handle_tag_end(callback, stack),
        Event::Text(e) => handle_text_tag(stack, &e),
        _ => {}
    }
}

fn handle_tag_start<F>(callback: &mut F, stack: &mut Vec<XmlTag>, e: &BytesStart)
where
    F: FnMut(XmlTag),
{
    let binding = e.name();
    let name = binding.as_ref().intern();
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
        {
            let text = e.xml10_content();
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
            Ok(event) => dispatch_event(event, callback, &mut stack),
            Err(_) => {}
        }
        // quick_xml does not clear the buffer between events — the borrow
        // returned by `read_event_into_async` is dropped at the end of each
        // match arm, so we can reclaim the capacity here without invalidating
        // any handler output. Without this, `buf` grows monotonically over the
        // whole file, defeating the 64 KiB pre-allocation above.
        buf.clear();
    }
}

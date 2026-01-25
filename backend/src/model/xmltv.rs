use crate::api::model::AppState;
use crate::model::xmltv::XmlTagIcon::Undefined;
use crate::model::InputSource;
use crate::utils::request::get_remote_content_as_stream;
use crate::utils::{async_file_reader, parse_xmltv_time};
use chrono::{Datelike, TimeZone, Utc};
use futures::TryFutureExt;
use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use shared::concat_string;
use shared::error::{TuliproxError, TuliproxErrorKind};
use shared::model::{EpgChannel, EpgProgramme, InputFetchMethod};
use shared::utils::{sanitize_sensitive_info, Internable};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use url::Url;

pub const EPG_TAG_TV: &str = "tv";
pub const EPG_TAG_PROGRAMME: &str = "programme";
pub const EPG_TAG_CHANNEL: &str = "channel";
pub const EPG_ATTRIB_ID: &str = "id";
pub const EPG_ATTRIB_CHANNEL: &str = "channel";
pub const EPG_TAG_DISPLAY_NAME: &str = "display-name";
pub const EPG_TAG_ICON: &str = "icon";

// https://github.com/XMLTV/xmltv/blob/master/xmltv.dtd


#[derive(Debug, Clone, Eq, PartialEq, Default)]
pub enum XmlTagIcon {
    #[default]
    Undefined,
    Src(Arc<str>),
    Exists,
}

#[derive(Debug, Clone)]
pub struct XmlTag {
    pub name: Arc<str>,
    pub value: Option<Arc<str>>,
    pub attributes: Option<HashMap<Arc<str>, Arc<str>>>,
    pub children: Option<Vec<Arc<XmlTag>>>,
    pub icon: XmlTagIcon,
    pub normalized_epg_ids: Option<Vec<Arc<str>>>,
}

impl XmlTag {
    pub(crate) fn new(name: Arc<str>, attribs: Option<HashMap<Arc<str>, Arc<str>>>) -> Self {
        Self {
            name,
            value: None,
            attributes: attribs,
            children: None,
            icon: Undefined,
            normalized_epg_ids: None,
        }
    }

    pub fn get_attribute_value(&self, attr_name: &Arc<str>) -> Option<&Arc<str>> {
        self.attributes.as_ref().and_then(|attr| attr.get(attr_name))
    }
}


#[derive(Debug, Clone)]
pub struct Epg {
    pub priority: i16,
    pub logo_override: bool,
    pub attributes: Option<HashMap<Arc<str>, Arc<str>>>,
    pub children: Vec<Arc<XmlTag>>,
}

impl Epg {
    pub async fn write_to_async<W: AsyncWrite + Unpin>(
        &self,
        writer: &mut quick_xml::writer::Writer<W>,
        rename_map: Option<&HashMap<Arc<str>, Arc<str>>>,
    ) -> Result<(), quick_xml::Error> {
        // Start tv-element
        let mut elem = BytesStart::new("tv");
        if let Some(attrs) = &self.attributes {
            for (k, v) in attrs {
                elem.push_attribute((k.as_ref(), v.as_ref()));
            }
        }
        writer.write_event_async(Event::Start(elem)).await?;

        // Stack for iterative writing
        // bool = End-Event written?
        let mut stack: Vec<(&XmlTag, bool)> = self
            .children
            .iter()
            .rev()
            .map(|c| (c.as_ref(), false))
            .collect();

        let mut write_counter = 0usize;
        let mut current_channel_id: Option<Arc<str>> = None;
        let epg_id_key = EPG_ATTRIB_ID.intern();

        while let Some((tag, ended)) = stack.pop() {
            if ended {
                if tag.name.as_ref() == EPG_TAG_CHANNEL {
                    current_channel_id = None;
                }
                // End-Event
                writer
                    .write_event_async(Event::End(BytesEnd::new(tag.name.as_ref())))
                    .await?;
            } else {
                // Start-Event for the tag
                let mut elem = BytesStart::new(tag.name.as_ref());
                if let Some(attrs) = &tag.attributes {
                    for (k, v) in attrs {
                        elem.push_attribute((k.as_ref(), v.as_ref()));
                    }
                }

                if tag.name.as_ref() == EPG_TAG_CHANNEL {
                    current_channel_id = tag.get_attribute_value(&epg_id_key).cloned();
                }

                writer.write_event_async(Event::Start(elem)).await?;

                // write text
                let value_to_write = if tag.name.as_ref() == EPG_TAG_DISPLAY_NAME {
                    current_channel_id.as_ref()
                        .and_then(|cid| rename_map.and_then(|m| m.get(cid))
                            .or(tag.value.as_ref()))
                } else {
                    tag.value.as_ref()
                };

                if let Some(text) = value_to_write {
                    writer.write_event_async(Event::Text(BytesText::new(text))).await?;
                }

                // End-Marker push + children push
                stack.push((tag, true));
                if let Some(children) = &tag.children {
                    for child in children.iter().rev() {
                        stack.push((child.as_ref(), false));
                    }
                }
            }
            write_counter += 1;
            if write_counter >= 50 {
                writer.get_mut().flush().await?; // flush underlying writer
                write_counter = 0;
            }
        }

        // write tv-end
        writer.write_event_async(Event::End(BytesEnd::new("tv"))).await?;

        let inner = writer.get_mut();
        inner.flush().await?;

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct PersistedEpgSource {
    pub file_path: PathBuf,
    pub priority: i16,
    pub logo_override: bool,
}

#[derive(Debug, Clone)]
pub struct TVGuide {
    epg_sources: Vec<PersistedEpgSource>,
}

impl TVGuide {
    pub fn new(mut epg_sources: Vec<PersistedEpgSource>) -> Self {
        epg_sources.sort_by_key(|a| a.priority);
        Self {
            epg_sources,
        }
    }

    #[inline]
    pub fn get_epg_sources(&self) -> &Vec<PersistedEpgSource> {
        &self.epg_sources
    }
}


fn filter_channels_and_programmes(
    channels: &mut Vec<EpgChannel>,
    programmes: &mut Vec<EpgProgramme>,
) {
    let mut prog_map: HashMap<Arc<str>, Vec<EpgProgramme>> = HashMap::new();
    for prog in programmes.drain(..) {
        prog_map.entry(prog.channel.clone()).or_default().push(prog);
    }

    for channel in channels.iter_mut() {
        if let Some(mut progs) = prog_map.remove(&channel.id) {
            progs.sort_by_key(|p| p.start);
            channel.programmes = progs;
        }
    }

    channels.retain(|c| !c.programmes.is_empty());
}

pub async fn parse_xmltv_for_web_ui_from_file(path: &Path) -> Result<Vec<EpgChannel>, TuliproxError> {
    let file = tokio::fs::File::open(path).map_err(|err| TuliproxError::new(TuliproxErrorKind::Info, err.to_string())).await?;
    parse_xmltv_for_web_ui(file).await
}

pub async fn parse_xmltv_for_web_ui_from_url(app_state: &Arc<AppState>, url: &str) -> Result<Vec<EpgChannel>, TuliproxError> {
    if let Ok(request_url) = Url::parse(url) {
        let client = app_state.http_client.load();
        let input_source: InputSource = InputSource {
            name: "xmltv".intern(),
            url: request_url.to_string(),
            username: None,
            password: None,
            method: InputFetchMethod::GET,
            headers: HashMap::default(),
        };

        match get_remote_content_as_stream(
            &app_state.app_config,
            &client,
            &input_source,
            None,
            &request_url,
        ).await {
            Ok((stream, _url)) => {
                parse_xmltv_for_web_ui(stream).await
            }
            Err(err) => Err(TuliproxError::new(TuliproxErrorKind::Info, format!("Failed to download: {} {err}", sanitize_sensitive_info(url))))
        }
    } else {
        Err(TuliproxError::new(TuliproxErrorKind::Info, format!("Invalid url: {}", sanitize_sensitive_info(url))))
    }
}

fn concat_text(t1: Option<&Arc<str>>, t2: &str) -> Arc<str> {
    match t1 {
        None => t2.intern(),
        Some(s) if s.ends_with('\\') => {
            let mut t = s.to_string();
            t.pop();
            concat_string!(&t, "&apos;", t2).intern()
        }
        Some(s) => concat_string!(s, t2).intern(),
    }
}

pub fn get_attr_value(attr: &quick_xml::events::attributes::Attribute) -> Option<Arc<str>> {
    attr.unescape_value().ok().map(|v| v.intern())
}

// This function filters a timeslot starting from yesterday.
#[allow(clippy::too_many_lines)]
async fn parse_xmltv_for_web_ui<R: AsyncRead + Send + Unpin>(reader: R) -> Result<Vec<EpgChannel>, TuliproxError> {
    let mut reader = quick_xml::reader::Reader::from_reader(async_file_reader(reader));
    let mut buf = Vec::new();

    let mut channels = Vec::new();
    let mut programmes = Vec::new();

    let mut current_channel: Option<EpgChannel> = None;
    let mut current_programme: Option<EpgProgramme> = None;

    let mut current_tag = String::new();

    // only 1 day old epg
    let now = Utc::now();
    let yesterday_start = Utc.with_ymd_and_hms(now.year(), now.month(), now.day(), 0, 0, 0)
        .single().expect("Current date at midnight should always be valid")
        - chrono::Duration::days(1);
    let threshold_ts = yesterday_start.timestamp();

    loop {
        match reader.read_event_into_async(&mut buf).await {
            Ok(Event::Empty(e) | Event::Start(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                current_tag.clone_from(&tag);

                match tag.as_str() {
                    EPG_TAG_CHANNEL => {
                        let mut id = None;
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"id" {
                                if let Some(value) = get_attr_value(&attr) {
                                    id = Some(value);
                                    break;
                                }
                            }
                        }
                        if let Some(cid) = id {
                            current_channel = Some(EpgChannel::new(cid));
                        } else {
                            current_channel = None;
                        }
                    }
                    EPG_TAG_PROGRAMME => {
                        let mut start = None;
                        let mut stop = None;
                        let mut channel = None;
                        current_programme = None;
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"start" => start = get_attr_value(&attr),
                                b"stop" => stop = get_attr_value(&attr),
                                b"channel" => channel = get_attr_value(&attr),
                                _ => {}
                            }
                        }
                        if let (Some(pstart), Some(pstop), Some(pchannel)) = (start, stop, channel) {
                            if let (Some(start_time), Some(stop_time)) = (parse_xmltv_time(&pstart), parse_xmltv_time(&pstop)) {
                                if stop_time >= threshold_ts {
                                    let epg_programme = EpgProgramme::new(start_time, stop_time, pchannel);
                                    current_programme = Some(epg_programme);
                                }
                            }
                        }
                    }
                    EPG_TAG_ICON => {
                        if let Some(channel) = &mut current_channel {
                            if channel.icon.is_none() {
                                for attr in e.attributes().flatten() {
                                    if attr.key.as_ref() == b"src" {
                                        if let Some(icon) = get_attr_value(&attr) {
                                            if !icon.is_empty() {
                                                channel.icon = Some(icon);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(e)) => {
                if let Ok(decoded) = e.decode() {
                    let text = decoded.trim();
                    if !text.is_empty() {
                        if let Some(channel) = &mut current_channel {
                            if current_tag == EPG_TAG_DISPLAY_NAME {
                                channel.title = Some(concat_text(channel.title.as_ref(), text));
                            }
                        }

                        if let Some(program) = &mut current_programme {
                            if current_tag == "title" {
                                program.title = Some(concat_text(program.title.as_ref(), text));
                            } else if current_tag == "desc" {
                                program.desc = Some(concat_text(program.desc.as_ref(), text));
                            }
                        }
                    }
                }
            }
            Ok(Event::End(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                match tag.as_str() {
                    EPG_TAG_CHANNEL => {
                        if let Some(channel) = current_channel.take() {
                            channels.push(channel);
                        }
                    }
                    EPG_TAG_PROGRAMME => {
                        if let Some(program) = current_programme.take() {
                            programmes.push(program);
                        }
                    }
                    _ => {}
                }
                current_tag.clear();
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(TuliproxError::new(TuliproxErrorKind::Info, err.to_string())),
            _ => {}
        }

        buf.clear();
    }

    filter_channels_and_programmes(&mut channels, &mut programmes);

    Ok(channels)
}

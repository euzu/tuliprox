use crate::api::model::AppState;
use crate::model::IcsEpgSourceConfig;
use crate::model::xmltv::XmlTagIcon::Undefined;
use crate::model::InputSource;
use crate::utils::request::get_remote_content_as_stream;
use crate::utils::{async_file_reader, parse_xmltv_time, FileLockManager};
use chrono::{Datelike, TimeZone, Utc};
use futures::TryFutureExt;
use quick_xml::events::{Event};
use shared::concat_string;
use shared::error::TuliproxError;
use shared::model::{EpgCategory, EpgChannel, EpgProgramme, InputFetchMethod};
use shared::utils::{sanitize_sensitive_info, Internable};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncRead};
use url::Url;

pub const EPG_TAG_TV: &str = "tv";
pub const EPG_TAG_PROGRAMME: &str = "programme";
pub const EPG_TAG_CHANNEL: &str = "channel";
pub const EPG_ATTRIB_ID: &str = "id";
pub const EPG_ATTRIB_CHANNEL: &str = "channel";
pub const EPG_TAG_DISPLAY_NAME: &str = "display-name";
pub const EPG_TAG_ICON: &str = "icon";
pub const EPG_TAG_TITLE: &str = "title";
pub const EPG_TAG_DESC: &str = "desc";
pub const EPG_TAG_CATEGORY: &str = "category";
pub const EPG_TAG_LIVE: &str = "live";
pub const EPG_TAG_NEW: &str = "new";
pub const EPG_ATTRIB_START: &str = "start";
pub const EPG_ATTRIB_STOP: &str = "stop";
pub const EPG_ATTRIB_CATCHUP_ID: &str = "catchup-id";
pub const EPG_ATTRIB_SRC: &str = "src";
pub const EPG_ATTRIB_LANG: &str = "lang";

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
    pub children: Vec<Arc<EpgChannel>>,
}

#[derive(Debug, Clone)]
pub enum PersistedEpgSourceKind {
    Xmltv,
    Ics {
        channel_id: Arc<str>,
        channel_title: Option<Arc<str>>,
        match_names: Vec<Arc<str>>,
        config: Box<IcsEpgSourceConfig>,
    },
}

#[derive(Debug, Clone)]
pub struct PersistedEpgSource {
    pub file_path: PathBuf,
    pub priority: i16,
    pub logo_override: bool,
    pub kind: PersistedEpgSourceKind,
}

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
    pub fn get_epg_sources(&self) -> &Vec<PersistedEpgSource> {
        &self.epg_sources
    }

    pub(crate) fn get_file_locks(&self) -> Option<&FileLockManager> { self.file_locks.as_deref() }
}


fn filter_channels_and_programmes(
    channels: &mut Vec<EpgChannel>,
    programmes: &mut Vec<EpgProgramme>,
) {
    let mut prog_map: HashMap<Arc<str>, Vec<EpgProgramme>> = HashMap::new();
    for prog in programmes.drain(..) {
        prog_map.entry(prog.get_transient_channel_id().clone()).or_default().push(prog);
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
    let file = tokio::fs::File::open(path).map_err(|err| TuliproxError::Io(err.to_string())).await?;
    parse_xmltv_for_web_ui(file).await
}

pub async fn parse_xmltv_for_web_ui_from_url(app_state: &Arc<AppState>, url: &str) -> Result<Vec<EpgChannel>, TuliproxError> {
    if let Ok(request_url) = Url::parse(url) {
        let client = app_state.http_client.load();
        let input_source: InputSource = InputSource {
            name: "xmltv".intern(),
            url: request_url.to_string(),
            provider: None,
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
            Err(err) => Err(TuliproxError::Download(format!("Failed to download: {} {err}", sanitize_sensitive_info(url))))
        }
    } else {
        Err(TuliproxError::UrlParse(format!("Invalid url: {}", sanitize_sensitive_info(url))))
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
    attr.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok().map(|v| v.intern())
}

// This function filters a timeslot starting from yesterday.
#[allow(clippy::too_many_lines)]
async fn parse_xmltv_for_web_ui<R: AsyncRead + Send + Unpin>(reader: R) -> Result<Vec<EpgChannel>, TuliproxError> {
    // Tracks which text-bearing element we are currently inside, without allocating
    // a String per XML event just to compare against a few known tag names.
    #[derive(Clone, Copy, PartialEq)]
    enum TextTag {
        DisplayName,
        Title,
        Desc,
        Category,
        Other,
    }

    let mut reader = quick_xml::reader::Reader::from_reader(async_file_reader(reader));
    let mut buf = Vec::new();

    let mut channels = Vec::new();
    let mut programmes = Vec::new();

    let mut current_channel: Option<EpgChannel> = None;
    let mut current_programme: Option<EpgProgramme> = None;

    let mut current_tag = TextTag::Other;
    let mut current_category_lang = None;

    // only 1 day old epg
    let now = Utc::now();
    let yesterday_start = Utc.with_ymd_and_hms(now.year(), now.month(), now.day(), 0, 0, 0)
        .single().expect("Current date at midnight should always be valid")
        - chrono::Duration::days(1);
    let threshold_ts = yesterday_start.timestamp();

    loop {
        match reader.read_event_into_async(&mut buf).await {
            Ok(Event::Empty(e) | Event::Start(e)) => {
                let name = e.name();
                // `from_utf8_lossy` borrows for valid UTF-8 (the common case), so no
                // allocation happens per element here.
                let tag = String::from_utf8_lossy(name.as_ref());
                current_tag = match tag.as_ref() {
                    EPG_TAG_DISPLAY_NAME => TextTag::DisplayName,
                    EPG_TAG_TITLE => TextTag::Title,
                    EPG_TAG_DESC => TextTag::Desc,
                    EPG_TAG_CATEGORY => TextTag::Category,
                    _ => TextTag::Other,
                };
                current_category_lang = None;

                match tag.as_ref() {
                    EPG_TAG_CHANNEL => {
                        let mut id = None;
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == EPG_ATTRIB_ID.as_bytes() {
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
                        let mut catchup_id = None;
                        current_programme = None;
                        for attr in e.attributes().flatten() {
                            let key = attr.key.as_ref();
                            if key == EPG_ATTRIB_START.as_bytes() {
                                start = get_attr_value(&attr);
                            } else if key == EPG_ATTRIB_STOP.as_bytes() {
                                stop = get_attr_value(&attr);
                            } else if key == EPG_ATTRIB_CHANNEL.as_bytes() {
                                channel = get_attr_value(&attr);
                            } else if key == EPG_ATTRIB_CATCHUP_ID.as_bytes() {
                                catchup_id = get_attr_value(&attr);
                            }
                        }
                        if let (Some(pstart), Some(pstop), Some(pchannel)) = (start, stop, channel) {
                            if let (Some(start_time), Some(stop_time)) = (parse_xmltv_time(&pstart), parse_xmltv_time(&pstop)) {
                                if stop_time >= threshold_ts {
                                    let mut epg_programme = EpgProgramme::new(start_time, stop_time, pchannel);
                                    epg_programme.catchup_id = catchup_id;
                                    current_programme = Some(epg_programme);
                                }
                            }
                        }
                    }
                    EPG_TAG_ICON => {
                        if let Some(channel) = &mut current_channel {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == EPG_ATTRIB_SRC.as_bytes() {
                                    if let Some(icon) = get_attr_value(&attr) {
                                        if !icon.is_empty() {
                                            channel.icon = Some(icon);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    EPG_TAG_CATEGORY => {
                        current_category_lang = e
                            .attributes()
                            .flatten()
                            .find(|attr| attr.key.as_ref() == EPG_ATTRIB_LANG.as_bytes())
                            .and_then(|attr| get_attr_value(&attr));
                    }
                    EPG_TAG_LIVE => {
                        if let Some(programme) = &mut current_programme {
                            programme.is_live = true;
                        }
                    }
                    EPG_TAG_NEW => {
                        if let Some(programme) = &mut current_programme {
                            programme.is_new = true;
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
                            if current_tag == TextTag::DisplayName {
                                channel.title = Some(concat_text(channel.title.as_ref(), text));
                            }
                        }

                        if let Some(program) = &mut current_programme {
                            if current_tag == TextTag::Title {
                                program.title = Some(concat_text(program.title.as_ref(), text));
                            } else if current_tag == TextTag::Desc {
                                program.desc = Some(concat_text(program.desc.as_ref(), text));
                            } else if current_tag == TextTag::Category {
                                program.categories.push(EpgCategory {
                                    value: text.intern(),
                                    lang: current_category_lang.take(),
                                });
                            }
                        }
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                match String::from_utf8_lossy(name.as_ref()).as_ref() {
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
                current_tag = TextTag::Other;
                current_category_lang = None;
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(TuliproxError::Parse(err.to_string())),
            _ => {}
        }

        buf.clear();
    }

    filter_channels_and_programmes(&mut channels, &mut programmes);

    Ok(channels)
}

#[cfg(test)]
mod tests {
    use super::parse_xmltv_for_web_ui;

    #[tokio::test]
    async fn web_ui_parser_preserves_programme_tags() {
        let channels = parse_xmltv_for_web_ui(
            br#"<tv>
  <channel id="ESPN.us"><display-name>ESPN</display-name></channel>
  <programme start="20990718180000 +0000" stop="20990718200000 +0000" channel="ESPN.us">
    <title>Softball</title>
    <category lang="en">Softball</category>
    <category>Sports</category>
    <live/>
    <new></new>
  </programme>
</tv>"#
                .as_slice(),
        )
        .await
        .expect("parse XMLTV");

        let programme = &channels[0].programmes[0];
        assert_eq!(programme.categories.len(), 2);
        assert_eq!(programme.categories[0].value.as_ref(), "Softball");
        assert_eq!(programme.categories[0].lang.as_deref(), Some("en"));
        assert_eq!(programme.categories[1].value.as_ref(), "Sports");
        assert!(programme.categories[1].lang.is_none());
        assert!(programme.is_live);
        assert!(programme.is_new);
    }
}

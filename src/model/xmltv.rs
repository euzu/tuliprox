use std::collections::{HashMap};
use std::path::PathBuf;
use quick_xml::{Error, Writer};
use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};

pub const EPG_TAG_TV: &str = "tv";
pub const EPG_TAG_PROGRAMME: &str = "programme";
pub const EPG_TAG_CHANNEL: &str = "channel";
pub const EPG_ATTRIB_ID: &str = "id";
pub const EPG_ATTRIB_CHANNEL: &str = "channel";
pub const EPG_TAG_DISPLAY_NAME: &str = "display-name";
pub const EPG_TAG_ICON: &str = "icon";

// https://github.com/XMLTV/xmltv/blob/master/xmltv.dtd

#[derive(Debug, Clone)]
pub struct XmlTag {
    pub name: String,
    pub value: Option<String>,
    pub attributes: Option<HashMap<String, String>>,
    pub children: Option<Vec<XmlTag>>,
    pub icon: Option<String>,
    pub normalized_epg_ids: Vec<String>,
}

impl XmlTag {

    pub(crate) fn new(name: String, attribs: Option<HashMap<String, String>>) -> Self {
        Self {
            name,
            value: None,
            attributes: attribs,
            children: None,
            icon: None,
            normalized_epg_ids: Vec::new(),
        }
    }

    pub fn get_attribute_value(&self, attr_name: &str) -> Option<&String> {
        self.attributes.as_ref().and_then(|attr| attr.get(attr_name))
    }

    fn write_to<W: std::io::Write>(&self, writer: &mut Writer<W>) -> Result<(), Error> {
        let mut elem = BytesStart::new(self.name.as_str());
        if let Some(attribs) = self.attributes.as_ref() {
            attribs.iter().for_each(|(k, v)| elem.push_attribute((k.as_str(), v.as_str())));
        }
        writer.write_event(Event::Start(elem))?;
        self.value.as_ref().map(|text| writer.write_event(Event::Text(BytesText::new(text.as_str()))));
        if let Some(children) = &self.children {
            for child in children {
                child.write_to(writer)?;
            }
        }
        Ok(writer.write_event(Event::End(BytesEnd::new(self.name.as_str())))?)
    }
}


#[derive(Debug, Clone)]
pub struct Epg {
    pub priority: i16,
    pub attributes: Option<HashMap<String, String>>,
    pub children: Vec<XmlTag>,
}

impl Epg {
    pub fn write_to<W: std::io::Write>(&self, writer: &mut Writer<W>) -> Result<(), quick_xml::Error> {
        let mut elem = BytesStart::new("tv");
        if let Some(attribs) = self.attributes.as_ref() {
            attribs.iter().for_each(|(k, v)| elem.push_attribute((k.as_str(), v.as_str())));
        }
        writer.write_event(Event::Start(elem))?;
        for child in &self.children {
            child.write_to(writer)?;
        }
        Ok(writer.write_event(Event::End(BytesEnd::new("tv")))?)
    }
}

#[derive(Debug, Clone)]
pub struct PersistedEpgSource {
    pub file_path: PathBuf,
    pub priority: i16,
}

#[derive(Debug, Clone)]
pub struct TVGuide {
    epg_sources: Vec<PersistedEpgSource>,
}

impl TVGuide {
    pub fn new(mut epg_sources: Vec<PersistedEpgSource>) -> Self {
        epg_sources.sort_by(|a, b| a.priority.cmp(&b.priority));
        Self {
            epg_sources,
        }
    }

    #[inline]
    pub fn get_epg_sources(&self) -> &Vec<PersistedEpgSource> {
        &self.epg_sources
    }
}

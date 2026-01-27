use enum_iterator::Sequence;
use std::fmt::Display;
use std::str::FromStr;
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use log::{trace};
use crate::error::{TuliproxError, info_err_res};
use crate::foundation::{apply_templates_to_pattern_single, get_filter, prepare_templates, Filter};
use crate::foundation::MapperScript;
use crate::model::PatternTemplate;
pub const COUNTER_FIELDS: &[&str] = &["name", "title", "caption", "chno"];

pub const MAPPER_FIELDS: &[&str] = &[
    "name", "title", "caption", "group", "id", "chno", "logo",
    "logo_small", "parent_code", "audio_track",
    "time_shift", "rec", "url", "epg_channel_id", "epg_id"
];

#[macro_export]
macro_rules! valid_property {
  ($key:expr, $array:expr) => {{
        $array.contains(&$key)
    }};
}
pub use valid_property;


#[derive(Debug, Default, Copy, Clone, serde::Serialize, serde::Deserialize, Sequence, PartialEq, Eq)]
pub enum CounterModifier {
    #[default]
    #[serde(rename = "assign")]
    Assign,
    #[serde(rename = "suffix")]
    Suffix,
    #[serde(rename = "prefix")]
    Prefix,
}

impl CounterModifier {
    const ASSIGN: &'static str = "assign";
    const SUFFIX: &'static str = "suffix";
    const PREFIX: &'static str = "prefix";
}

impl Display for CounterModifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", match self {
            Self::Assign => Self::ASSIGN,
            Self::Suffix => Self::SUFFIX,
            Self::Prefix => Self::PREFIX,
        })
    }
}

impl FromStr for CounterModifier {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        if s.eq("assign") {
            Ok(Self::Assign)
        } else if s.eq("suffix") {
            Ok(Self::Suffix)
        } else if s.eq("prefix") {
            Ok(Self::Prefix)
        } else {
            info_err_res!("Unknown CounterModifier: {}", s)
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
pub struct MappingCounterDefinition {
    pub filter: String,
    pub field: String,
    #[serde(default)]
    pub concat: String,
    #[serde(default)]
    pub modifier: CounterModifier,
    #[serde(default)]
    pub value: u32,
    #[serde(default)]
    pub padding: u8,
}

#[derive(Debug, Clone)]
pub struct MappingCounter {
    pub filter: Filter,
    pub field: String,
    pub concat: String,
    pub modifier: CounterModifier,
    pub value: Arc<AtomicU32>,
    pub padding: u8,
}

impl PartialEq for MappingCounter {
    fn eq(&self, other: &Self) -> bool {
        self.filter == other.filter
            && self.field == other.field
            && self.concat == other.concat
            && self.modifier == other.modifier
            && self.padding == other.padding
        // value is not compared!
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "modifier", rename_all = "snake_case")]
pub enum MapperOperation {
    Lowercase { field: String },
    Uppercase { field: String },
    Capitalize { field: String },
    Split { field: String, value: String },
    Suffix { field: String, value: String },
    Prefix { field: String, value: String },
    Set { field: String, value: String },
    Copy { field: String, source: String },
}

impl MapperOperation {
    pub fn prepare(&mut self, templates: Option<&Vec<PatternTemplate>>) -> Result<(), TuliproxError> {
        match self {
            MapperOperation::Lowercase {ref field }
            | MapperOperation::Uppercase { ref field }
            | MapperOperation::Capitalize { ref field } => {
                if !valid_property!(field.as_str(), MAPPER_FIELDS) {
                    return info_err_res!("Invalid mapper attribute field {field}");
                }
            }

            MapperOperation::Copy { ref field, ref source } => {
                if !valid_property!(field.as_str(), MAPPER_FIELDS) {
                    return info_err_res!("Invalid mapper attribute field {field}");
                }
                if !valid_property!(source.as_str(), MAPPER_FIELDS) {
                    return info_err_res!("Invalid mapper source field {source}");
                }
            }

            MapperOperation::Split { ref field, ref mut value }
            | MapperOperation::Suffix { ref field, ref mut value }
            | MapperOperation::Prefix { ref field, ref mut value }
            | MapperOperation::Set { ref field, ref mut value } => {
                if !valid_property!(field.as_str(), MAPPER_FIELDS) {
                    return info_err_res!("Invalid mapper attribute field {field}");
                }

                if templates.is_some() {
                    *value = apply_templates_to_pattern_single(value, templates)?;
                }
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
pub struct MapperDto {
    pub filter: String,
    pub script: String,
    #[serde(skip_serializing, skip_deserializing)]
    pub t_filter: Option<Filter>,
    #[serde(skip_serializing, skip_deserializing)]
    pub t_script: Option<MapperScript>,
}

impl MapperDto {
    /// # Panics
    ///
    /// Will panic if default `RegEx` gets invalid
    pub fn prepare(&mut self, templates: Option<&Vec<PatternTemplate>>) -> Result<(), TuliproxError> {
        self.t_filter = Some(get_filter(&self.filter, templates)?);
        let script = if templates.is_some() {
            apply_templates_to_pattern_single(&self.script, templates)?
        } else {
            self.script.to_string()
        };
        trace!("Mapper script: {script}");
        self.t_script = Some(MapperScript::parse(&script, templates)?);
        Ok(())
    }
}


#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
pub struct MappingDto {
    pub id: String,
    #[serde(default)]
    pub match_as_ascii: bool,
    pub mapper: Option<Vec<MapperDto>>,
    pub counter: Option<Vec<MappingCounterDefinition>>,
    #[serde(skip_serializing, skip_deserializing)]
    pub t_counter: Option<Vec<MappingCounter>>,
    #[serde(skip_serializing, skip_deserializing)]
    pub templates: Option<Vec<PatternTemplate>>
}

impl MappingDto {
    pub fn prepare(&mut self, templates: Option<&Vec<PatternTemplate>>) -> Result<(), TuliproxError> {
        self.templates = templates.map(|t| t.iter().map(PatternTemplate::clone).collect::<Vec<_>>());
        if let Some(mapper_list) = &mut self.mapper {
            for mapper in mapper_list {
                mapper.prepare(templates)?;
            }
        }

        if let Some(counter_def_list) = &self.counter {
            let mut counters = vec![];
            for def in counter_def_list {
                if !valid_property!(def.field.as_str(), COUNTER_FIELDS) {
                    return info_err_res!("Invalid counter field {}", def.field);
                }
                match get_filter(&def.filter, templates) {
                    Ok(flt) => {
                        counters.push(MappingCounter {
                            filter: flt,
                            field: def.field.clone(),
                            concat: def.concat.clone(),
                            modifier: def.modifier,
                            value: Arc::new(AtomicU32::new(def.value)),
                            padding: def.padding,
                        });
                    }
                    Err(e) => return info_err_res!("Counter field error: {}", e.to_string())
                }
            }
            self.t_counter = Some(counters);
        }

        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct MappingDefinitionDto {
    pub templates: Option<Vec<PatternTemplate>>,
    pub mapping: Vec<MappingDto>,
}

impl MappingDefinitionDto {
    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        if let Some(templates) = &mut self.templates {
            match prepare_templates(templates) {
                Ok(tmplts) => {
                    self.templates = Some(tmplts);
                }
                Err(err) => return Err(err),
            }
        }
        for mapping in &mut self.mapping {
            let template_list = self.templates.as_ref();
            mapping.prepare(template_list)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct MappingsDto {
    pub mappings: MappingDefinitionDto,
}

impl MappingsDto {
    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        self.mappings.prepare()
    }

}

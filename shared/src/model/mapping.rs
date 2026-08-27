use crate::{
    error::TuliproxError,
    foundation::{apply_templates_to_pattern_single, get_filter, prepare_templates, Filter, MapperScript},
    model::{HeaderField, PatternTemplate},
};
use log::trace;
use strum_macros::{AsRefStr, Display, EnumIter, EnumString};

pub const COUNTER_FIELDS: &[&str] = &["name", "title", "caption", "chno"];

pub const MAPPER_FIELDS: &[&str] = &[
    "name",
    "title",
    "caption",
    "group",
    "id",
    "chno",
    "logo",
    "logo_small",
    "parent_code",
    "audio_track",
    "time_shift",
    "rec",
    "url",
    "epg_channel_id",
    "epg_id",
];

#[derive(
    Debug,
    Default,
    Copy,
    Clone,
    serde::Serialize,
    serde::Deserialize,
    PartialEq,
    Eq,
    EnumIter,
    EnumString,
    AsRefStr,
    Display,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum MappingStage {
    #[default]
    Processing,
    AfterEpg,
}

impl MappingStage {
    pub fn is_processing(&self) -> bool { *self == Self::Processing }
}

#[macro_export]
macro_rules! valid_property {
    ($key:expr, $array:expr) => {{
        $array.contains(&$key)
    }};
}
pub use valid_property;

#[derive(
    Debug,
    Default,
    Copy,
    Clone,
    serde::Serialize,
    serde::Deserialize,
    PartialEq,
    Eq,
    EnumIter,
    EnumString,
    AsRefStr,
    Display,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum CounterModifier {
    #[default]
    Assign,
    Suffix,
    Prefix,
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
    /// Parsed once here rather than re-parsed per channel in the counter loop.
    pub field: HeaderField,
    pub concat: String,
    pub modifier: CounterModifier,
    pub start: u32,
    pub padding: u8,
}

impl PartialEq for MappingCounter {
    fn eq(&self, other: &Self) -> bool {
        self.filter == other.filter
            && self.field == other.field
            && self.concat == other.concat
            && self.modifier == other.modifier
            && self.start == other.start
            && self.padding == other.padding
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
    pub fn prepare(&mut self, templates: Option<&[PatternTemplate]>) -> Result<(), TuliproxError> {
        match self {
            MapperOperation::Lowercase { ref field }
            | MapperOperation::Uppercase { ref field }
            | MapperOperation::Capitalize { ref field } => {
                if !valid_property!(field.as_str(), MAPPER_FIELDS) {
                    return Err(TuliproxError::Mapper(format!("Invalid mapper attribute field {field}")));
                }
            }

            MapperOperation::Copy { ref field, ref source } => {
                if !valid_property!(field.as_str(), MAPPER_FIELDS) {
                    return Err(TuliproxError::Mapper(format!("Invalid mapper attribute field {field}")));
                }
                if !valid_property!(source.as_str(), MAPPER_FIELDS) {
                    return Err(TuliproxError::Mapper(format!("Invalid mapper source field {source}")));
                }
            }

            MapperOperation::Split { ref field, ref mut value }
            | MapperOperation::Suffix { ref field, ref mut value }
            | MapperOperation::Prefix { ref field, ref mut value }
            | MapperOperation::Set { ref field, ref mut value } => {
                if !valid_property!(field.as_str(), MAPPER_FIELDS) {
                    return Err(TuliproxError::Mapper(format!("Invalid mapper attribute field {field}")));
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
    pub fn prepare(&mut self, templates: Option<&[PatternTemplate]>) -> Result<(), TuliproxError> {
        self.t_filter = Some(get_filter(&self.filter, templates)?);
        let script = if templates.is_some() {
            apply_templates_to_pattern_single(&self.script, templates)?
        } else {
            self.script.clone()
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
    #[serde(default, skip_serializing_if = "MappingStage::is_processing")]
    pub stage: MappingStage,
    pub mapper: Option<Vec<MapperDto>>,
    pub counter: Option<Vec<MappingCounterDefinition>>,
    #[serde(skip_serializing, skip_deserializing)]
    pub t_counter: Option<Vec<MappingCounter>>,
    #[serde(skip_serializing, skip_deserializing)]
    pub templates: Option<Vec<PatternTemplate>>,
}

impl MappingDto {
    pub fn prepare(&mut self, templates: Option<&[PatternTemplate]>) -> Result<(), TuliproxError> {
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
                    return Err(TuliproxError::Config(format!("Invalid counter field {}", def.field)));
                }
                let Some(field) = HeaderField::parse(&def.field) else {
                    return Err(TuliproxError::Config(format!("Invalid counter field {}", def.field)));
                };
                {
                    let flt = get_filter(&def.filter, templates)?;
                    counters.push(MappingCounter {
                        filter: flt,
                        field,
                        concat: def.concat.clone(),
                        modifier: def.modifier,
                        start: def.value,
                        padding: def.padding,
                    });
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
    pub fn prepare(&mut self, prepared_templates: Option<&[PatternTemplate]>) -> Result<(), TuliproxError> {
        let local_prepared_templates = if prepared_templates.is_none() {
            self.templates
                .as_ref()
                .map(|templates| {
                    let mut cloned_templates = templates.clone();
                    prepare_templates(&mut cloned_templates)
                })
                .transpose()?
        } else {
            None
        };
        let templates_to_use = prepared_templates.or(local_prepared_templates.as_deref());

        for mapping in &mut self.mapping {
            mapping.prepare(templates_to_use)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct MappingsDto {
    pub mappings: MappingDefinitionDto,
}

impl MappingsDto {
    pub fn prepare(&mut self, prepared_templates: Option<&[PatternTemplate]>) -> Result<(), TuliproxError> {
        self.mappings.prepare(prepared_templates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TemplateValue;

    #[test]
    fn mapper_operation_prepare_resolves_value() {
        let mut operation = MapperOperation::Set { field: "name".to_string(), value: "!PREFIX!".to_string() };

        let templates = vec![PatternTemplate {
            name: "PREFIX".to_string(),
            value: TemplateValue::Single("Channel".to_string()),
            placeholder: "!PREFIX!".to_string(),
        }];

        operation.prepare(Some(&templates)).expect("mapper operation should prepare");

        match operation {
            MapperOperation::Set { value, .. } => assert_eq!(value, "Channel"),
            _ => panic!("expected set operation"),
        }
    }

    #[test]
    fn mapping_definition_prepare_keeps_templates_unresolved() {
        let mut mapping_definition = MappingDefinitionDto {
            templates: Some(vec![
                PatternTemplate {
                    name: "BASE".to_string(),
                    value: TemplateValue::Single("news".to_string()),
                    placeholder: String::new(),
                },
                PatternTemplate {
                    name: "CHAIN".to_string(),
                    value: TemplateValue::Single("!BASE!-live".to_string()),
                    placeholder: String::new(),
                },
            ]),
            mapping: vec![],
        };

        let original_templates = mapping_definition.templates.clone();
        mapping_definition.prepare(None).expect("mapping definition should prepare");

        assert_eq!(mapping_definition.templates, original_templates);
    }

    #[test]
    fn mapping_stage_defaults_to_processing() {
        let dto: MappingDto = serde_saphyr::from_str("id: test\n").expect("mapping should parse");
        assert_eq!(dto.stage, MappingStage::Processing);
        assert!(
            !serde_saphyr::to_string(&dto).expect("mapping should serialize").contains("stage:"),
            "default stage must be omitted from serialized output"
        );
    }

    #[test]
    fn mapping_stage_parses_after_epg_and_rejects_unknown_values() {
        let dto: MappingDto = serde_saphyr::from_str("id: test\nstage: after_epg\n").expect("stage should parse");
        assert_eq!(dto.stage, MappingStage::AfterEpg);
        assert!(serde_saphyr::from_str::<MappingDto>("id: test\nstage: before_persistence\n").is_err());
    }
}

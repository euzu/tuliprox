use crate::{
    error::TuliproxError,
    foundation::{apply_templates_to_pattern_single, get_filter, prepare_templates, Filter, MapperScript},
    model::{HeaderField, PatternTemplate, Prepare},
};
use log::trace;
use strum_macros::{AsRefStr, Display, EnumIter, EnumString};

/// Header fields a counter rule may target.
pub const COUNTER_FIELDS: &[HeaderField] =
    &[HeaderField::Name, HeaderField::Title, HeaderField::Caption, HeaderField::Chno];

/// Header fields a mapper operation may read or write.
///
/// One entry shorter than the string list it replaces, because that list spelled
/// the EPG channel id twice -- `epg_channel_id` and `epg_id` -- to cover both
/// accepted names. Aliases now resolve in `HeaderField::parse`, so the allow-list
/// names the *field* and each spelling is handled in exactly one place.
pub const MAPPER_FIELDS: &[HeaderField] = &[
    HeaderField::Name,
    HeaderField::Title,
    HeaderField::Caption,
    HeaderField::Group,
    HeaderField::Id,
    HeaderField::Chno,
    HeaderField::Logo,
    HeaderField::LogoSmall,
    HeaderField::ParentCode,
    HeaderField::AudioTrack,
    HeaderField::TimeShift,
    HeaderField::Rec,
    HeaderField::Url,
    HeaderField::EpgChannelId,
];

/// Whether `name` resolves to one of `allowed`.
///
/// Note this is deliberately more permissive than the case-sensitive string
/// `contains` it replaces: validation used to reject `NAME` even though
/// `set_field` would have accepted it, because the accessor compared
/// case-insensitively while the allow-list did not. Resolving through
/// `HeaderField::parse` makes the two agree. Every previously valid config stays
/// valid; some previously rejected ones are now accepted and behave correctly.
#[must_use]
pub fn is_allowed_field(name: &str, allowed: &[HeaderField]) -> bool {
    HeaderField::parse(name).is_some_and(|field| allowed.contains(&field))
}

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

impl Prepare for MapperOperation {
    type Ctx<'a> = Option<&'a [PatternTemplate]>;

    fn prepare(&mut self, templates: Self::Ctx<'_>) -> Result<(), TuliproxError> {
        match self {
            MapperOperation::Lowercase { ref field }
            | MapperOperation::Uppercase { ref field }
            | MapperOperation::Capitalize { ref field } => {
                if !is_allowed_field(field, MAPPER_FIELDS) {
                    return Err(TuliproxError::Mapper(format!("Invalid mapper attribute field {field}")));
                }
            }

            MapperOperation::Copy { ref field, ref source } => {
                if !is_allowed_field(field, MAPPER_FIELDS) {
                    return Err(TuliproxError::Mapper(format!("Invalid mapper attribute field {field}")));
                }
                if !is_allowed_field(source, MAPPER_FIELDS) {
                    return Err(TuliproxError::Mapper(format!("Invalid mapper source field {source}")));
                }
            }

            MapperOperation::Split { ref field, ref mut value }
            | MapperOperation::Suffix { ref field, ref mut value }
            | MapperOperation::Prefix { ref field, ref mut value }
            | MapperOperation::Set { ref field, ref mut value } => {
                if !is_allowed_field(field, MAPPER_FIELDS) {
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

impl Prepare for MapperDto {
    type Ctx<'a> = Option<&'a [PatternTemplate]>;

    /// # Panics
    ///
    /// Will panic if default `RegEx` gets invalid
    fn prepare(&mut self, templates: Self::Ctx<'_>) -> Result<(), TuliproxError> {
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

impl Prepare for MappingDto {
    type Ctx<'a> = Option<&'a [PatternTemplate]>;

    fn prepare(&mut self, templates: Self::Ctx<'_>) -> Result<(), TuliproxError> {
        self.templates = templates.map(|t| t.iter().map(PatternTemplate::clone).collect::<Vec<_>>());
        // Option<Vec<MapperDto>>: the blanket impls nest, so the walk is gone.
        self.mapper.prepare(templates)?;

        if let Some(counter_def_list) = &self.counter {
            let mut counters = vec![];
            for def in counter_def_list {
                // Resolve and check membership in one step; these used to be a
                // string `contains` followed by a separate parse of the same name.
                let Some(field) = HeaderField::parse(&def.field).filter(|field| COUNTER_FIELDS.contains(field)) else {
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

impl Prepare for MappingDefinitionDto {
    type Ctx<'a> = Option<&'a [PatternTemplate]>;

    fn prepare(&mut self, prepared_templates: Self::Ctx<'_>) -> Result<(), TuliproxError> {
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

impl Prepare for MappingsDto {
    type Ctx<'a> = Option<&'a [PatternTemplate]>;

    fn prepare(&mut self, prepared_templates: Self::Ctx<'_>) -> Result<(), TuliproxError> {
        self.mappings.prepare(prepared_templates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TemplateValue;

    #[test]
    fn allow_lists_accept_both_epg_spellings_through_one_entry() {
        // The string list needed `epg_channel_id` and `epg_id` as separate
        // entries; the typed list has one, and parse resolves both spellings.
        assert!(is_allowed_field("epg_channel_id", MAPPER_FIELDS));
        assert!(is_allowed_field("epg_id", MAPPER_FIELDS));
        assert_eq!(MAPPER_FIELDS.iter().filter(|f| **f == HeaderField::EpgChannelId).count(), 1);
    }

    #[test]
    fn allow_lists_reject_fields_that_are_not_listed() {
        // Valid header fields that are simply not mapper-writable.
        assert!(!is_allowed_field("input", MAPPER_FIELDS));
        assert!(!is_allowed_field("type", MAPPER_FIELDS));
        // Not a header field at all.
        assert!(!is_allowed_field("nonsense", MAPPER_FIELDS));
        assert!(!is_allowed_field("", MAPPER_FIELDS));
        // Counter fields are a strict subset of mapper fields.
        assert!(is_allowed_field("chno", COUNTER_FIELDS));
        assert!(!is_allowed_field("url", COUNTER_FIELDS));
        assert!(COUNTER_FIELDS.iter().all(|field| MAPPER_FIELDS.contains(field)));
    }

    #[test]
    fn allow_lists_agree_with_the_accessor_on_casing() {
        // Previously the allow-list was case-sensitive while set_field was not,
        // so `NAME` failed validation despite being writable. They now agree.
        assert!(is_allowed_field("NAME", MAPPER_FIELDS));
        assert!(is_allowed_field("Logo_Small", MAPPER_FIELDS));
    }

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

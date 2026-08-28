use crate::model::macros;
use shared::{
    foundation::{Filter, MapperScript},
    model::{
        MapperDto, MappingCounter, MappingCounterDefinition, MappingDefinitionDto, MappingDto, MappingStage,
        MappingsDto, PatternTemplate,
    },
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct Mapper {
    pub filter: String,
    pub script: String,
    #[serde(skip_serializing, skip_deserializing)]
    pub t_filter: Option<Filter>,
    #[serde(skip_serializing, skip_deserializing)]
    pub t_script: Option<MapperScript>,
}

macros::from_impl!(Mapper);
impl From<&MapperDto> for Mapper {
    fn from(dto: &MapperDto) -> Self {
        Self {
            filter: dto.filter.clone(),
            script: dto.script.clone(),
            t_filter: dto.t_filter.clone(),
            t_script: dto.t_script.clone(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct Mapping {
    pub id: String,
    #[serde(default)]
    pub match_as_ascii: bool,
    #[serde(default, skip_serializing_if = "MappingStage::is_processing")]
    pub stage: MappingStage,
    pub mapper: Option<Vec<Mapper>>,
    pub counter: Option<Vec<MappingCounterDefinition>>,
    #[serde(skip_serializing, skip_deserializing)]
    pub t_counter: Option<Vec<MappingCounter>>,
    #[serde(skip_serializing, skip_deserializing)]
    pub templates: Option<Vec<PatternTemplate>>,
}

impl From<&MappingDto> for Mapping {
    fn from(dto: &MappingDto) -> Self {
        Self {
            id: dto.id.clone(),
            match_as_ascii: dto.match_as_ascii,
            stage: dto.stage,
            mapper: dto.mapper.as_ref().map(|l| l.iter().map(Mapper::from).collect()),
            counter: dto.counter.clone(),
            t_counter: dto.t_counter.clone(),
            templates: dto.templates.clone(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MappingDefinition {
    pub templates: Option<Vec<PatternTemplate>>,
    pub mapping: Vec<Mapping>,
}

macros::from_impl!(MappingDefinition);
impl From<&MappingDefinitionDto> for MappingDefinition {
    fn from(dto: &MappingDefinitionDto) -> Self {
        Self { templates: dto.templates.clone(), mapping: dto.mapping.iter().map(Into::into).collect() }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Mappings {
    pub mappings: MappingDefinition,
}

impl Mappings {
    pub fn get_mapping(&self, mapping_id: &str) -> Option<Mapping> {
        for mapping in &self.mappings.mapping {
            if mapping.id.eq(mapping_id) {
                return Some(mapping.clone());
            }
        }
        None
    }
}

macros::from_impl!(Mappings);
impl From<&MappingsDto> for Mappings {
    fn from(dto: &MappingsDto) -> Self {
        Mappings { mappings: MappingDefinition::from(&dto.mappings) }
    }
}

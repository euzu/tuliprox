use shared::{
    error::TuliproxError,
    foundation::{Filter, MapperScript},
    model::{MapperDto, MappingCounter, MappingDto, MappingStage, MappingsDto, PatternTemplate},
};
use std::{collections::HashMap, sync::Arc};

#[derive(Debug, Clone)]
pub enum MappingProgram {
    Script(MapperScript),
}

#[derive(Debug, Clone)]
pub struct CompiledMappingRule {
    pub name: Option<String>,
    pub filter: Filter,
    pub program: MappingProgram,
}

impl TryFrom<&MapperDto> for CompiledMappingRule {
    type Error = TuliproxError;

    fn try_from(dto: &MapperDto) -> Result<Self, Self::Error> {
        let filter =
            dto.t_filter.clone().ok_or_else(|| TuliproxError::Config("Mapping filter was not prepared".to_string()))?;
        let script =
            dto.t_script.clone().ok_or_else(|| TuliproxError::Config("Mapping script was not prepared".to_string()))?;
        Ok(Self { name: dto.name.clone(), filter, program: MappingProgram::Script(script) })
    }
}

#[derive(Debug, Clone, Default)]
pub struct CompiledMapping {
    pub id: String,
    pub match_as_ascii: bool,
    pub stage: MappingStage,
    pub rules: Vec<CompiledMappingRule>,
    pub counters: Vec<MappingCounter>,
    pub templates: Option<Vec<PatternTemplate>>,
}

impl TryFrom<&MappingDto> for CompiledMapping {
    type Error = TuliproxError;

    fn try_from(dto: &MappingDto) -> Result<Self, Self::Error> {
        Ok(Self {
            id: dto.id.clone(),
            match_as_ascii: dto.match_as_ascii,
            stage: dto.stage,
            rules: dto
                .mapper
                .as_deref()
                .unwrap_or_default()
                .iter()
                .enumerate()
                .map(|(index, rule)| {
                    CompiledMappingRule::try_from(rule).map_err(|err| {
                        let rule_label = rule.name.as_deref().map_or_else(|| (index + 1).to_string(), str::to_string);
                        TuliproxError::Config(format!("Mapping '{}', rule {rule_label}: {err}", dto.id))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            counters: dto.t_counter.clone().unwrap_or_default(),
            templates: dto.templates.clone(),
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct CompiledMappings {
    pub mappings: Vec<Arc<CompiledMapping>>,
    by_id: HashMap<String, usize>,
}

impl CompiledMappings {
    pub fn new(mappings: Vec<CompiledMapping>) -> Self {
        let mappings = mappings.into_iter().map(Arc::new).collect::<Vec<_>>();
        let mut by_id = HashMap::with_capacity(mappings.len());
        for (index, mapping) in mappings.iter().enumerate() {
            by_id.entry(mapping.id.clone()).or_insert(index);
        }
        Self { mappings, by_id }
    }

    pub fn get_mapping(&self, mapping_id: &str) -> Option<Arc<CompiledMapping>> {
        self.by_id.get(mapping_id).map(|index| Arc::clone(&self.mappings[*index]))
    }
}

impl TryFrom<&MappingsDto> for CompiledMappings {
    type Error = TuliproxError;

    fn try_from(dto: &MappingsDto) -> Result<Self, Self::Error> {
        let mappings = dto.mappings.mapping.iter().map(CompiledMapping::try_from).collect::<Result<Vec<_>, _>>()?;
        Ok(Self::new(mappings))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compilation_identifies_unprepared_mapping_rule() {
        let dto = MappingDto {
            id: "sports".to_string(),
            mapper: Some(vec![MapperDto { filter: "Name ~ \".*\"".to_string(), ..Default::default() }]),
            ..Default::default()
        };

        let error = CompiledMapping::try_from(&dto).expect_err("unprepared rule must fail compilation");

        assert!(error.to_string().contains("Mapping 'sports', rule 1"));
        assert!(error.to_string().contains("filter was not prepared"));
    }

    #[test]
    fn compilation_uses_optional_rule_name_in_errors() {
        let dto = MappingDto {
            id: "sports".to_string(),
            mapper: Some(vec![MapperDto {
                name: Some("rename premium channels".to_string()),
                filter: "Name ~ \".*\"".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let error = CompiledMapping::try_from(&dto).expect_err("unprepared rule must fail compilation");

        assert!(error.to_string().contains("rule rename premium channels"));
    }

    #[test]
    fn indexed_lookup_returns_the_ordered_shared_mapping() {
        let mappings = CompiledMappings::new(vec![
            CompiledMapping { id: "first".to_string(), ..Default::default() },
            CompiledMapping { id: "second".to_string(), ..Default::default() },
        ]);

        let resolved = mappings.get_mapping("second").expect("mapping should resolve");

        assert!(Arc::ptr_eq(&resolved, &mappings.mappings[1]));
        assert!(mappings.get_mapping("missing").is_none());
    }
}

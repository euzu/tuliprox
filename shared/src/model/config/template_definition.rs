use crate::model::PatternTemplate;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TemplateDefinitionDto {
    #[serde(default)]
    pub templates: Vec<PatternTemplate>,
}

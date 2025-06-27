use crate::error::{create_tuliprox_error_result, TuliproxError, TuliproxErrorKind};
use crate::foundation::filter::apply_templates_to_pattern_single;
use crate::model::{ItemField, PatternTemplate};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigRenameDto {
    pub field: ItemField,
    pub pattern: String,
    pub new_name: String,
}

impl ConfigRenameDto {
    pub fn prepare(&mut self, templates: Option<&Vec<PatternTemplate>>) -> Result<(), TuliproxError> {
        self.pattern = apply_templates_to_pattern_single(&self.pattern, templates)?;
        if let Err(err) = regex::Regex::new(&self.pattern) {
            return create_tuliprox_error_result!(TuliproxErrorKind::Info, "cant parse regex: {} {err}", &self.pattern);
        }
        Ok(())
    }
}
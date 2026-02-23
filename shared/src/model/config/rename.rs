use crate::{
    error::{info_err_res, TuliproxError},
    foundation::apply_templates_to_pattern_single,
    model::{ItemField, PatternTemplate},
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigRenameDto {
    pub field: ItemField,
    pub pattern: String,
    pub new_name: String,
    #[serde(skip)]
    pub t_pattern: Option<String>,
}

impl ConfigRenameDto {
    pub fn prepare(&mut self, templates: Option<&Vec<PatternTemplate>>) -> Result<(), TuliproxError> {
        let resolved_pattern = apply_templates_to_pattern_single(&self.pattern, templates)?;
        if let Err(err) = crate::model::REGEX_CACHE.get_or_compile(&resolved_pattern) {
            return info_err_res!("can't parse regex: {} {err}", &resolved_pattern);
        }
        self.t_pattern = Some(resolved_pattern);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TemplateValue;

    #[test]
    fn prepare_does_not_resolve_pattern_in_place() {
        let templates = vec![PatternTemplate {
            name: "GROUP_PATTERN".to_string(),
            value: TemplateValue::Single("US.*".to_string()),
            placeholder: "!GROUP_PATTERN!".to_string(),
        }];

        let mut rename = ConfigRenameDto {
            field: crate::model::ItemField::Group,
            pattern: "!GROUP_PATTERN!".to_string(),
            new_name: "US".to_string(),
            t_pattern: None,
        };

        rename.prepare(Some(&templates)).expect("rename prepare should succeed");

        assert_eq!(rename.pattern, "!GROUP_PATTERN!");
        assert_eq!(rename.t_pattern.as_deref(), Some("US.*"));
    }
}

use crate::{
    error::{info_err_res, TuliproxError},
    foundation::apply_templates_to_pattern_single,
    model::{ItemField, PatternTemplate},
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigRenameDto {
    pub field: ItemField,
    pub pattern: String,
    pub new_name: String,
    #[serde(skip)]
    pub t_pattern: Option<String>,
}

impl PartialEq for ConfigRenameDto {
    fn eq(&self, other: &Self) -> bool {
        self.field == other.field && self.pattern == other.pattern && self.new_name == other.new_name
    }
}

impl ConfigRenameDto {
    pub fn prepare(&mut self, templates: Option<&[PatternTemplate]>) -> Result<(), TuliproxError> {
        if templates.is_none()
            && self.pattern.len() >= 2
            && self.pattern.starts_with('!')
            && self.pattern.ends_with('!')
        {
            log::warn!(
                "Rename pattern '{}' for field {:?} looks like a template placeholder, but no templates were provided. Treating it as literal regex.",
                self.pattern,
                self.field
            );
        }
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

        assert!(rename.prepare(Some(&templates)).is_ok());

        assert_eq!(rename.pattern, "!GROUP_PATTERN!");
        assert_eq!(rename.t_pattern.as_deref(), Some("US.*"));
    }

    #[test]
    fn prepare_without_templates_keeps_pattern_value() {
        let original_pattern = "!GROUP_PATTERN!".to_string();
        let mut rename = ConfigRenameDto {
            field: crate::model::ItemField::Group,
            pattern: original_pattern.clone(),
            new_name: "US".to_string(),
            t_pattern: None,
        };

        assert!(rename.prepare(None).is_ok());

        assert_eq!(rename.pattern, original_pattern);
        assert_eq!(rename.t_pattern.as_deref(), Some("!GROUP_PATTERN!"));
    }
}

use std::fmt::{Display, Formatter};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(untagged)]
pub enum TemplateValue {
    Single(String),
    Multi(Vec<String>),
}

impl Display for TemplateValue {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            TemplateValue::Single(value) => write!(f, "{value}")?,
            TemplateValue::Multi(values) => {
                for (i, value) in values.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{value}")?;
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct PatternTemplate {
    pub name: String,
    pub value: TemplateValue,
    #[serde(skip)]
    pub placeholder: String,
}

impl PatternTemplate {
    pub fn prepare(&mut self) {
        let mut placeholder = String::with_capacity(self.name.len() + 2);
        placeholder.push('!');
        placeholder.push_str(&self.name);
        placeholder.push('!');

        self.placeholder = placeholder;
    }
}

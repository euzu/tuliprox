
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(untagged)]
pub enum TemplateValue {
    Single(String),
    Multi(Vec<String>),
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
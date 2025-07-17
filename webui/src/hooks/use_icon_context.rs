use std::collections::HashMap;
use std::rc::Rc;
use yew::prelude::*;

#[derive(PartialEq, Eq, Clone, serde::Deserialize, Debug)]
pub struct IconDefinition {
    keys: Vec<String>,
    pub(crate) path: String,
    pub(crate) viewport: Option<String>,
}

#[derive(PartialEq, Debug)]
pub struct Icons {
    definitions: Option<HashMap<String, Rc<IconDefinition>>>,
}

impl Icons {
    pub fn new() -> Self {
        Self {
            definitions: None
        }
    }

    pub fn new_with(definitions: &Vec<Rc<IconDefinition>>) -> Self {
        let mut map = HashMap::new();

        for icon in definitions {
            for key in &icon.keys {
                map.insert(key.clone(), icon.clone());
            }
        }

        Self { definitions: Some(map) }
    }

    pub fn get_icon(&self, name: &str) -> Option<Rc<IconDefinition>> {
        if let Some(icon_defs) = &self.definitions {
            return icon_defs.get(name).cloned();
        }
        None
    }
}

impl Default for Icons {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(PartialEq, Debug, Clone)]
pub struct IconContext {
    pub(crate) icons: Rc<Icons>,
}

impl IconContext {
    pub fn new(definitions: &Vec<Rc<IconDefinition>>) -> Self {
        Self {
            icons: Rc::new(Icons::new_with(definitions))
        }
    }

    pub fn icons(&self) -> Rc<Icons> {
        self.icons.clone()
    }
}

#[hook]
pub fn use_icon_context() -> Rc<Icons> {
    use_context::<UseStateHandle<IconContext>>().expect("Icon context not found").icons()
}
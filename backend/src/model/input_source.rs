use std::collections::HashMap;
use std::sync::Arc;
use shared::model::InputFetchMethod;
use crate::model::{ConfigInput, StagedInput};

#[derive(Clone, Debug)]
pub struct InputSource {
    pub name: Arc<str>,
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub method: InputFetchMethod,
    pub headers: HashMap<String, String>,
}

impl InputSource {
    pub fn with_url(&self, url: String) -> Self {
        Self {
            name: self.name.clone(),
            url,
            username: self.username.clone(),
            password: self.password.clone(),
            method: self.method,
            headers: self.headers.clone(),
        }
    }
}

macro_rules! impl_input_source_from {
    ($input_type:ty) => {
        impl From<&$input_type> for InputSource {
            fn from(input: &$input_type) -> Self {
                Self {
                    name: input.name.clone(),
                    url: input.url.clone(),
                    username: input.username.clone(),
                    password: input.password.clone(),
                    method: input.method,
                    headers: input.headers.clone(),
                }
            }
        }
    };
}

impl_input_source_from!(ConfigInput);
impl_input_source_from!(StagedInput);
use shared::model::{ConfigInputAliasDto, ConfigInputDto, InputType, SourcesConfigDto};
use std::sync::Arc;

pub(in crate::api::panel_api::tests) fn source_doc_with_aliases(aliases: Vec<ConfigInputAliasDto>) -> SourcesConfigDto {
    SourcesConfigDto {
        inputs: vec![ConfigInputDto {
            name: Arc::from("cdn-dev"),
            input_type: InputType::Xtream,
            url: "provider://tivione".to_string(),
            username: Some("root-user".to_string()),
            password: Some("root-pass".to_string()),
            aliases: Some(aliases),
            ..ConfigInputDto::default()
        }],
        ..SourcesConfigDto::default()
    }
}

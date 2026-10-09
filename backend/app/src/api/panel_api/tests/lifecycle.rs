use super::source_doc_with_aliases;
use crate::api::source_yml_patch::{apply_sources_yml_patches, SourcesYmlPatch};
use std::sync::Arc;

#[test]
fn sources_yml_persist_provisioned_account_replaces_root_when_current_root_is_expired() {
    let mut doc = source_doc_with_aliases(Vec::new());
    doc.inputs[0].username = Some("current-root".to_string());
    doc.inputs[0].password = Some("current-pass".to_string());
    doc.inputs[0].exp_date = Some(i64::try_from(jsonwebtoken::get_current_timestamp()).expect("timestamp") - 1);

    let changed = apply_sources_yml_patches(
        &mut doc,
        &[SourcesYmlPatch::PersistProvisionedAccount {
            input_name: Arc::from("cdn-dev"),
            username: "new-root".to_string(),
            password: "new-pass".to_string(),
            exp_date: Some(42),
        }],
    )
    .expect("patches apply");

    assert!(changed);
    assert_eq!(doc.inputs[0].username.as_deref(), Some("new-root"));
    assert_eq!(doc.inputs[0].password.as_deref(), Some("new-pass"));
    assert_eq!(doc.inputs[0].exp_date, Some(42));
    assert!(doc.inputs[0].aliases.as_ref().is_none_or(Vec::is_empty));
}

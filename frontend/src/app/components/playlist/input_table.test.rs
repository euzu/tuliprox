use super::*;

#[test]
fn inputs_require_their_name_but_can_hide_provider_credentials() {
    let columns = InputColumn::columns();
    let required: Vec<_> = columns.iter().filter(|column| !column.can_hide).map(|column| column.id.as_str()).collect();
    assert_eq!(required, vec!["name"]);
    assert!(columns.iter().any(|column| column.id == "username" && column.can_hide));
}

use super::*;

#[test]
fn users_require_identity_and_actions_in_their_own_definition() {
    let columns = UsersColumn::columns();
    let required: Vec<_> = columns.iter().filter(|column| !column.can_hide).map(|column| column.id.as_str()).collect();
    assert_eq!(required, vec!["actions", "username"]);
    let controls: Vec<_> = columns.iter().filter(|column| !column.content).map(|column| column.id.as_str()).collect();
    assert_eq!(controls, vec!["actions"]);
}

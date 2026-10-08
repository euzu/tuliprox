use super::*;
#[test]
fn schema_is_header_order_independent_and_keeps_ragged_fields() {
    let (first, columns) = csv_schema(
        &[vec!["a".into(), "a".into(), "b".into()], vec!["1".into(), "2".into(), "3".into(), "4".into()]],
        true,
        "test.csv",
        "v1",
    );
    let (second, _) = csv_schema(
        &[vec!["b".into(), "a".into(), "a".into()], vec!["5".into(), "6".into(), "7".into(), "8".into()]],
        true,
        "test.csv",
        "v1",
    );
    assert_eq!(first, second);
    assert_eq!(columns.len(), 4);
    assert_ne!(columns[0].id, columns[1].id);
    assert_ne!(first, csv_schema(&[vec!["a".into()]], false, "test.csv", "v1").0);
    assert_ne!(csv_schema(&[], false, "test.csv", "v1").0, csv_schema(&[], false, "test.csv", "v2").0);
}

#[test]
fn schemas_from_different_callers_have_separate_layouts() {
    let rows = vec![vec!["username".into(), "password".into()]];
    let (accounts, account_columns) = csv_schema(&rows, true, "playlist.accounts_csv", "accounts-v1");
    let (import, import_columns) = csv_schema(&rows, true, "library.import_csv", "accounts-v1");
    assert!(accounts.starts_with("playlist.accounts_csv."));
    assert!(import.starts_with("library.import_csv."));
    assert_ne!(accounts, import);
    assert_eq!(account_columns, import_columns);
    assert_ne!(accounts, csv_schema(&rows, true, "playlist.accounts_csv", "accounts-v2").0);
}

#[test]
fn csv_headers_remain_literal_when_they_resemble_translation_keys() {
    let headers = ["NAME", "LABEL.NAME", "TABLE_COLUMNS.COLUMNS"];
    let rows = vec![headers.iter().map(|header| (*header).to_owned()).collect()];
    let (_, columns) = csv_schema(&rows, true, "test.csv", "v1");
    let displayed: Vec<_> =
        columns.iter().map(|column| column.label.resolve(|_| "Wrong translation".to_owned())).collect();
    assert_eq!(displayed, headers);
}

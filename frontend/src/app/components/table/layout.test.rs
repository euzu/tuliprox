use super::*;
#[test]
fn projection_retains_original_indices_and_content() {
    let mut columns = Rc::new(vec![
        TableColumn::translated("actions", ""),
        TableColumn::translated("username", "Name"),
        TableColumn::translated("password", "Password"),
        TableColumn::translated("filter", "Filter"),
    ]);
    let metadata = Rc::make_mut(&mut columns);
    metadata[0].can_hide = false;
    metadata[0].content = false;
    metadata[1].can_hide = false;
    let mut layout = TableLayoutPreferencesDto {
        column_order: vec!["filter".into(), "future".into(), "filter".into()],
        ..Default::default()
    };
    layout.column_visibility.insert("password".into(), false);
    assert_eq!(visible_columns(&columns, &layout), vec![3, 0, 1]);
    layout.column_visibility.insert("filter".into(), false);
    layout.column_visibility.insert("username".into(), false);
    assert!(visible_columns(&columns, &layout).iter().any(|i| columns[*i].content));
    assert_eq!(visible_columns(&columns, &TableLayoutPreferencesDto::default()), vec![0, 1, 2, 3]);
}

#[test]
fn column_defaults_do_not_depend_on_id_or_label() {
    for id in ["name", "username", "actions", "other"] {
        let column = TableColumn::new(id, "Label");
        assert!(column.can_hide);
        assert!(column.content);
        assert!(column.default_visible);
        assert!(column.can_reorder);
    }
    let column = TableColumn::translated("actions", "CUSTOM.ACTION_LABEL");
    assert_eq!(column.label, TableColumnLabel::TranslationKey("CUSTOM.ACTION_LABEL".into()));
}

#[test]
fn hidden_content_is_restored_when_only_action_controls_remain() {
    let columns = vec![
        TableColumn { can_hide: false, content: false, ..TableColumn::new("controls", "Actions") },
        TableColumn::new("value", "Value"),
    ];
    let layout = TableLayoutPreferencesDto {
        column_order: vec!["value".into(), "controls".into()],
        column_visibility: [("value".into(), false), ("controls".into(), false)].into(),
    };
    assert_eq!(visible_columns(&columns, &layout), vec![1, 0]);
}

#[test]
fn explicit_labels_preserve_literal_prefixes_and_translate_arbitrary_keys() {
    let literal = TableColumn::new("literal", "LABEL.NAME");
    let translated = TableColumn::translated("translated", "CUSTOM.COLUMN_TITLE");
    assert_eq!(literal.label.resolve(|_| "Wrong translation".to_owned()), "LABEL.NAME");
    assert_eq!(
        translated.label.resolve(|key| match key {
            "CUSTOM.COLUMN_TITLE" => "Column title".to_owned(),
            _ => "Unknown key".to_owned(),
        }),
        "Column title"
    );
}

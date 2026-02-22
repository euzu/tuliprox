pub fn resolve_field_id(field_id: &Option<String>, name: &str, label: &str) -> String {
    let candidate = field_id
        .as_ref()
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .or_else(|| {
            let name = name.trim();
            if name.is_empty() {
                None
            } else {
                Some(name)
            }
        })
        .or_else(|| {
            let label = label.trim();
            if label.is_empty() {
                None
            } else {
                Some(label)
            }
        })
        .unwrap_or("field");

    candidate
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' { ch } else { '_' })
        .collect::<String>()
}

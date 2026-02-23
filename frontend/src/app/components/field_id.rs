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

fn normalize_upper_snake(raw: &str) -> String {
    raw.chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch.to_ascii_uppercase() } else { '_' })
        .collect::<String>()
        .split('_')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

fn to_upper_snake_case(raw: &str) -> String {
    let chars = raw.chars().collect::<Vec<_>>();
    let mut result = String::new();

    for (idx, ch) in chars.iter().enumerate() {
        if ch.is_ascii_alphanumeric() {
            if ch.is_ascii_uppercase() {
                if idx > 0 {
                    let prev = chars[idx - 1];
                    let next = chars.get(idx + 1).copied();
                    let split_here = prev.is_ascii_lowercase()
                        || prev.is_ascii_digit()
                        || (prev.is_ascii_uppercase() && next.is_some_and(|n| n.is_ascii_lowercase()));
                    if split_here && !result.ends_with('_') {
                        result.push('_');
                    }
                }
                result.push(*ch);
            } else {
                result.push(ch.to_ascii_uppercase());
            }
        } else if !result.ends_with('_') {
            result.push('_');
        }
    }

    result.split('_').filter(|part| !part.is_empty()).collect::<Vec<_>>().join("_")
}

fn strip_ref_prefix(type_name: &str) -> &str {
    let mut current = type_name.trim();
    loop {
        let Some(without_ref) = current.strip_prefix('&') else {
            return current;
        };
        current = without_ref.trim_start();
        if let Some(without_mut) = current.strip_prefix("mut ") {
            current = without_mut.trim_start();
        }
    }
}

fn unwrap_known_wrapper(type_name: &str) -> Option<&str> {
    const WRAPPERS: [&str; 8] = [
        "alloc::boxed::Box",
        "std::boxed::Box",
        "alloc::rc::Rc",
        "std::rc::Rc",
        "alloc::sync::Arc",
        "std::sync::Arc",
        "core::option::Option",
        "std::option::Option",
    ];

    for wrapper in WRAPPERS {
        let prefix = format!("{wrapper}<");
        if type_name.starts_with(&prefix) && type_name.ends_with('>') {
            let inner = &type_name[prefix.len()..type_name.len() - 1];
            return Some(inner.trim());
        }
    }
    None
}

fn normalize_type_prefix(raw_type_name: &str) -> String {
    let mut current = strip_ref_prefix(raw_type_name);
    while let Some(inner) = unwrap_known_wrapper(current) {
        current = strip_ref_prefix(inner);
    }

    let type_without_generics = current.split('<').next().unwrap_or(current).trim();
    let simple_name = type_without_generics.rsplit("::").next().unwrap_or(type_without_generics);
    let simple_name = simple_name.strip_suffix("Dto").unwrap_or(simple_name);

    to_upper_snake_case(simple_name)
}

pub fn dto_field_id<T: ?Sized>(_dto: &T, field: &str) -> String {
    let prefix = normalize_type_prefix(std::any::type_name::<T>());
    let field_name = normalize_upper_snake(field);

    if prefix.is_empty() {
        field_name
    } else {
        format!("{prefix}_{field_name}")
    }
}

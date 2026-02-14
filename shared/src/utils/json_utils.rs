use crate::utils::humanize_snake_case;
use serde_json::{self, Value};

pub fn get_u64_from_serde_value(value: &Value) -> Option<u64> {
    match value {
        Value::Number(num_val) => num_val.as_u64(),
        Value::String(str_val) => str_val.parse::<u64>().ok(),
        _ => None,
    }
}

pub fn get_i64_from_serde_value(value: &Value) -> Option<i64> {
    match value {
        Value::Number(num_val) => num_val.as_i64(),
        Value::String(str_val) => str_val.parse::<i64>().ok(),
        _ => None,
    }
}

pub fn get_u32_from_serde_value(value: &Value) -> Option<u32> {
    get_u64_from_serde_value(value).and_then(|val| u32::try_from(val).ok())
}

pub fn get_string_from_serde_value(value: &Value) -> Option<String> {
    match value {
        Value::Number(num_val) => num_val.as_i64().map(|num| num.to_string()),
        Value::String(str_val) => {
            if str_val.is_empty() {
                None
            } else {
                Some(str_val.clone())
            }
        }
        _ => None,
    }
}

const MARKDOWN_SPECIAL_CHARS: &str = r#"_*[]()~`>#+-=|{}.!\"#;

fn escape_markdown_v2(text: &str) -> String {
    let mut escaped = String::new();
    for c in text.chars() {
        if MARKDOWN_SPECIAL_CHARS.contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

fn json_to_markdown(value: &Value) -> String {
    fn format_value(v: &Value, indent: usize) -> String {
        let pad = " ".repeat(indent);
        match v {
            Value::Object(map) => {
                let mut entries: Vec<_> = map.iter().collect();
                entries.sort_by_key(|(k, _)| *k);
                entries
                    .into_iter()
                    .map(|(k, v)| {
                        let formatted = format_value(v, indent + 2);
                        let key = escape_markdown_v2(&humanize_snake_case(k));
                        if v.is_object() || v.is_array() {
                            format!("{pad}*{key}:*\n{formatted}")
                        } else {
                            format!("{pad}*{key}:* {formatted}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            Value::Array(arr) => arr
                .iter()
                .map(|v| format!("{pad}\\- {}", format_value(v, indent + 3).trim()))
                .collect::<Vec<_>>()
                .join("\n"),
            Value::String(s) => escape_markdown_v2(s),
            Value::Number(n) => escape_markdown_v2(&n.to_string()),
            Value::Bool(b) => b.to_string(),
            Value::Null => "null".to_string(),
        }
    }

    format_value(value, 0)
}

pub fn json_str_to_markdown(json_str: &str) -> Result<String, serde_json::Error> {
    let value: Value = serde_json::from_str(json_str)?;
    Ok(json_to_markdown(&value))
}

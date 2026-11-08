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

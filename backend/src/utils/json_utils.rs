use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, Error, Write};
use std::path::Path;
use serde::Serialize;
use serde_json::Value;
use shared::utils::json_iter_array;
use crate::utils::{file_reader, file_writer};

pub fn json_filter_file<S: ::std::hash::BuildHasher>(file_path: &Path, filter: &HashMap<&str, HashSet<String, S>, S>) -> Vec<serde_json::Value> {
    let mut filtered: Vec<serde_json::Value> = Vec::with_capacity(1024);
    if !file_path.exists() {
        return filtered; // Return early if the file does not exist
    }

    let Ok(file) = File::open(file_path) else {
        return filtered;
    };

    let reader = file_reader(file);
    for entry in json_iter_array::<serde_json::Value, BufReader<File>>(reader).flatten() {
        if let Some(item) = entry.as_object() {
            if filter.iter().all(|(&key, filter_set)| {
                item.get(key).is_some_and(|field_value| match field_value {
                    Value::String(s) => filter_set.contains(s.as_str()),
                    Value::Number(n) => filter_set.contains(n.as_str()),
                    _ => false,
                })
            }) {
                filtered.push(entry);
            }
        }
    }

    filtered
}

pub fn json_write_documents_to_file<T>(file: &Path, value: &T) -> Result<(), Error>
where
    T: ?Sized + Serialize,
{
    let file = File::create(file)?;
    let mut writer = file_writer(&file);
    serde_json::to_writer(&mut writer, value)?;
    writer.flush()
}

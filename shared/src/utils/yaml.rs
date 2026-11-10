//! YAML deserialization with the scalar handling Tuliprox configs rely on.
//!
//! `serde-saphyr` 2 rejects an unquoted `.inf`, `-.inf` or `.nan` that reaches a typeless or `Arc<str>` field.
//! Earlier versions passed them on as the strings `.inf`, `-.inf` and `.nan`; `AsString` keeps that, so a name,
//! password or plugin setting with such a value still loads.

use serde::de::{Deserialize, DeserializeOwned};

/// The options every config and data file is read with.
#[must_use]
pub fn yaml_options() -> serde_saphyr::Options {
    serde_saphyr::options! { non_finite_float_policy: serde_saphyr::NonFiniteFloatPolicy::AsString }
}

/// [`serde_saphyr::from_str`] with [`yaml_options`].
pub fn yaml_from_str<'de, T: Deserialize<'de>>(input: &'de str) -> Result<T, serde_saphyr::Error> {
    serde_saphyr::from_str_with_options(input, yaml_options())
}

/// [`serde_saphyr::from_slice`] with [`yaml_options`].
pub fn yaml_from_slice<'de, T: Deserialize<'de>>(input: &'de [u8]) -> Result<T, serde_saphyr::Error> {
    serde_saphyr::from_slice_with_options(input, yaml_options())
}

/// [`serde_saphyr::from_reader`] with [`yaml_options`].
pub fn yaml_from_reader<'a, R: std::io::Read + 'a, T: DeserializeOwned>(reader: R) -> Result<T, serde_saphyr::Error> {
    serde_saphyr::from_reader_with_options(reader, yaml_options())
}

#[cfg(test)]
mod tests {
    use super::{yaml_from_reader, yaml_from_slice, yaml_from_str};
    use std::sync::Arc;

    #[derive(serde::Deserialize)]
    struct Holder {
        text: String,
        interned: Arc<str>,
        any: serde_json::Value,
    }

    #[test]
    fn unquoted_special_floats_load_as_strings() {
        let yaml = "text: .inf\ninterned: -.inf\nany: .nan\n";
        for holder in [
            yaml_from_str::<Holder>(yaml).expect("str"),
            yaml_from_slice::<Holder>(yaml.as_bytes()).expect("slice"),
            yaml_from_reader::<_, Holder>(yaml.as_bytes()).expect("reader"),
        ] {
            assert_eq!(holder.text, ".inf");
            assert_eq!(holder.interned.as_ref(), "-.inf");
            assert_eq!(holder.any, serde_json::Value::String(".nan".to_string()));
        }
    }
}

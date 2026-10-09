use super::BPlusTreeUpdate;
use crate::codec::binary_deserialize;
use std::io;

#[test]
fn prepared_upsert_batch_is_key_sorted_and_stable() -> io::Result<()> {
    let keys = [3u32, 1, 2, 1];
    let values = ["three", "first", "two", "last"].map(String::from);
    let items = keys.iter().zip(&values).collect::<Vec<_>>();

    let prepared = BPlusTreeUpdate::<u32, String>::prepare_upsert_batch(&items)?;
    let decoded = prepared
        .into_iter()
        .map(|(key, value)| binary_deserialize::<String>(&value).map(|value| (key, value)))
        .collect::<io::Result<Vec<_>>>()?;
    assert_eq!(
        decoded,
        vec![
            (1, String::from("first")),
            (1, String::from("last")),
            (2, String::from("two")),
            (3, String::from("three")),
        ]
    );
    Ok(())
}

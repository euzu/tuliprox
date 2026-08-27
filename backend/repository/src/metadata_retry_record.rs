//! On-disk record schema for the metadata retry database.
//!
//! These are the persisted shapes, nothing more. They live beside the other
//! repository record schemas rather than with the manager that writes them,
//! because two things outside `api` name them purely as type parameters: the
//! startup migration, which rewrites the database into the current storage
//! format, and the CLI database viewer, which dumps it.
//!
//! The conversions to and from the in-memory retry state stay with that retry
//! logic in `api::model::metadata_update_manager` - the schema is a repository
//! concern, but what the values mean is not.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum MetadataRetryDbKey {
    VodId(u32),
    VodText(String),
    SeriesId(u32),
    SeriesText(String),
    LiveId(u32),
    LiveText(String),
    Stream { scope: String, id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryStateDbValue {
    pub attempts: u8,
    pub next_allowed_at_ts: i64,
    pub cooldown_until_ts: Option<i64>,
    pub last_error: Option<String>,
    #[serde(default)]
    pub source_last_modified: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataRetryDbValue {
    pub resolve: Option<RetryStateDbValue>,
    pub probe: Option<RetryStateDbValue>,
    pub tmdb: Option<RetryStateDbValue>,
    pub updated_at_ts: i64,
}

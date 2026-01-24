use serde::{Deserialize, Serialize};
use shared::model::{MsgKind, SourceStats, InputStats};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchChanges {
    pub target: String,
    pub group: String,
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProcessingStats {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<Vec<SourceStats>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<String>,
}

impl ProcessingStats {
    pub fn new_stats(stats: Vec<SourceStats>) -> Self {
        Self { stats: Some(stats), errors: None }
    }

    pub fn new_error(error: String) -> Self {
        Self { stats: None, errors: Some(error) }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data")]
pub enum MessageContent {
   Info(String),
   Error(String),
   Watch(WatchChanges),
   ProcessingStats(ProcessingStats),
}

impl MessageContent {
    pub fn event_stats(stats: Vec<SourceStats>) -> Self {
        Self::ProcessingStats(ProcessingStats::new_stats(stats))
    }

    pub fn event_error(error: String) -> Self {
        Self::ProcessingStats(ProcessingStats::new_error(error))
    }
    
    pub fn kind(&self) -> MsgKind {
        match self {
            Self::Info(_) => MsgKind::Info,
            Self::Error(_) => MsgKind::Error,
            Self::Watch(_) => MsgKind::Watch,
            Self::ProcessingStats(e) => {
                if e.errors.is_some() && e.stats.is_none() {
                    MsgKind::Error
                } else {
                    MsgKind::Stats
                }
            }
        }
    }
}

#[derive(Serialize)]
pub struct TemplateContext<'a> {
    pub kind: String,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<&'a Vec<SourceStats>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watch: Option<&'a WatchChanges>,
    // For manual error json or other json events embedded in string
    #[serde(skip_serializing_if = "Option::is_none")]
    pub processing: Option<ProcessingStats>,
    // Flattened stats for first input convenience
    #[serde(flatten)]
    pub flat_stats: Option<InputStats>,
}

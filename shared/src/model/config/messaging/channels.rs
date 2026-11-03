use super::ChannelRoutingDto;

#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CommandMessagingConfigDto {
    /// Program to run. Not passed through a shell, so no quoting rules and
    /// no shell injection.
    pub program: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Kill the child after this many seconds. Defaults to 30.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub templates: std::collections::HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<ChannelRoutingDto>,
}

impl CommandMessagingConfigDto {
    pub fn is_empty(&self) -> bool { self.program.trim().is_empty() }
}

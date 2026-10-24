use crate::model::{FailoverGroupPolicy, VideoCodec, VideoResolution, VirtualId};
use std::collections::{HashMap, HashSet};

pub const FAILOVER_FILE_VERSION: u32 = 1;
pub const MAX_MANUAL_ID_LEN: usize = 64;
pub const MAX_GROUP_NAME_CHARS: usize = 200;
const ETAG_HEX_LEN: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub enum CapacityTier {
    Free,
    OwnRelease,
    Grace,
    Preempt,
    Exhausted,
}

/// Why a running group stream replaced its member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwitchReason {
    UpstreamError,
    UpstreamEof,
    Stall,
    Underrun,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    Closed,
    OpenUntil(std::time::Instant),
    HalfOpenInFlight,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct ChannelMemberRefDto {
    pub input: String,
    pub stream_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ManualFailoverGroupDto {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epg_channel_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlist_group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<FailoverGroupPolicy>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<ChannelMemberRefDto>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AutoGroupOverrideDto {
    pub auto_key: String,
    /// Dissolves the auto group; its members stay ungrouped and keep their natural key.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<FailoverGroupPolicy>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub order: Vec<ChannelMemberRefDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add: Vec<ChannelMemberRefDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<ChannelMemberRefDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epg_channel_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlist_group: Option<String>,
}

/// A manual group that resolved too few members to be materialized.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InactiveManualGroupDto {
    pub id: String,
    pub resolved: usize,
}

/// A member reference of an override that matched no catalog entry.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UnresolvedRefDto {
    /// Manual group id or auto key the reference belongs to.
    pub group: String,
    pub member_ref: ChannelMemberRefDto,
}

/// Diagnostics of one grouping resolution: corrections that had no effect and why.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FailoverOverrideDiagnosticsDto {
    /// Auto overrides whose `auto_key` matches no natural bucket.
    #[serde(default)]
    pub stale_auto_overrides: Vec<String>,
    #[serde(default)]
    pub inactive_manual_groups: Vec<InactiveManualGroupDto>,
    /// `add` refs that could not be claimed because another group already holds them.
    #[serde(default)]
    pub conflicting_refs: Vec<ChannelMemberRefDto>,
    #[serde(default)]
    pub unresolved_refs: Vec<UnresolvedRefDto>,
    /// Auto keys dissolved through `disabled`.
    #[serde(default)]
    pub dissolved: Vec<String>,
}

impl FailoverOverrideDiagnosticsDto {
    pub fn is_empty(&self) -> bool {
        self.stale_auto_overrides.is_empty()
            && self.inactive_manual_groups.is_empty()
            && self.conflicting_refs.is_empty()
            && self.unresolved_refs.is_empty()
            && self.dissolved.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FailoverFileDto {
    pub version: u32,
    #[serde(default)]
    pub targets: Vec<TargetFailoverOverridesDto>,
}

/// Override section of a single target inside a failover file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TargetFailoverOverridesDto {
    pub target: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<ManualFailoverGroupDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auto_overrides: Vec<AutoGroupOverrideDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded: Vec<ChannelMemberRefDto>,
}

/// Non-fatal finding of [`TargetFailoverOverridesDto::validate`].
/// Group related variants carry the manual group id or the auto key of the offending entry.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "error", content = "detail", rename_all = "snake_case")]
pub enum OverrideValidationError {
    EmptyManualId,
    InvalidManualId(String),
    DuplicateManualId(String),
    EmptyName(String),
    NameTooLong(String),
    RefInMultipleGroups(ChannelMemberRefDto),
    DuplicateAutoKey(String),
    InvalidLogo(String),
    /// An auto override `add` ref that is also excluded; the addition wins.
    ExcludedAndAdded(ChannelMemberRefDto),
}

impl std::fmt::Display for OverrideValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyManualId => write!(f, "manual group id is empty"),
            Self::InvalidManualId(id) => {
                write!(f, "manual group id '{id}' must match [a-z0-9-] and be at most {MAX_MANUAL_ID_LEN} characters")
            }
            Self::DuplicateManualId(id) => write!(f, "manual group id '{id}' is used more than once"),
            Self::EmptyName(id) => write!(f, "manual group '{id}' has no name"),
            Self::NameTooLong(id) => {
                write!(f, "name of manual group '{id}' exceeds {MAX_GROUP_NAME_CHARS} characters")
            }
            Self::RefInMultipleGroups(member) => {
                write!(f, "channel {}/{} is claimed by more than one group", member.input, member.stream_id)
            }
            Self::DuplicateAutoKey(key) => write!(f, "auto group override '{key}' is defined more than once"),
            Self::InvalidLogo(id) => write!(f, "logo of '{id}' must be empty or start with http:// or https://"),
            Self::ExcludedAndAdded(member) => write!(
                f,
                "channel {}/{} is excluded and added to a group; the addition wins",
                member.input, member.stream_id
            ),
        }
    }
}

impl std::error::Error for OverrideValidationError {}

fn trim_in_place(value: &mut String) {
    let trimmed = value.trim();
    if trimmed.len() != value.len() {
        *value = trimmed.to_string();
    }
}

fn trim_opt(value: &mut Option<String>) {
    if let Some(inner) = value {
        trim_in_place(inner);
        if inner.is_empty() {
            *value = None;
        }
    }
}

fn canonicalize_refs(refs: &mut Vec<ChannelMemberRefDto>) {
    for member in refs.iter_mut() {
        trim_in_place(&mut member.input);
        trim_in_place(&mut member.stream_id);
    }
    let mut seen = HashSet::with_capacity(refs.len());
    refs.retain(|member| !member.input.is_empty() && !member.stream_id.is_empty() && seen.insert(member.clone()));
}

fn is_valid_manual_id(id: &str) -> bool {
    id.len() <= MAX_MANUAL_ID_LEN && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn is_valid_logo(logo: Option<&str>) -> bool {
    logo.is_none_or(|url| url.is_empty() || url.starts_with("http://") || url.starts_with("https://"))
}

impl TargetFailoverOverridesDto {
    pub fn new(target: &str) -> Self {
        Self { target: target.to_string(), groups: Vec::new(), auto_overrides: Vec::new(), excluded: Vec::new() }
    }

    pub fn canonicalize(&mut self) {
        trim_in_place(&mut self.target);
        for group in &mut self.groups {
            trim_in_place(&mut group.id);
            trim_in_place(&mut group.name);
            trim_opt(&mut group.logo);
            trim_opt(&mut group.epg_channel_id);
            trim_opt(&mut group.playlist_group);
            canonicalize_refs(&mut group.members);
        }
        for auto in &mut self.auto_overrides {
            trim_in_place(&mut auto.auto_key);
            trim_opt(&mut auto.name);
            trim_opt(&mut auto.logo);
            trim_opt(&mut auto.epg_channel_id);
            trim_opt(&mut auto.playlist_group);
            canonicalize_refs(&mut auto.order);
            canonicalize_refs(&mut auto.add);
            canonicalize_refs(&mut auto.remove);
        }
        canonicalize_refs(&mut self.excluded);
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty() && self.auto_overrides.is_empty() && self.excluded.is_empty()
    }

    /// Reports every finding of a canonicalized section; findings are not fatal.
    pub fn validate(&self) -> Vec<OverrideValidationError> {
        let mut errors = Vec::new();

        let mut ids = HashSet::with_capacity(self.groups.len());
        for group in &self.groups {
            if group.id.is_empty() {
                errors.push(OverrideValidationError::EmptyManualId);
            } else {
                if !is_valid_manual_id(&group.id) {
                    errors.push(OverrideValidationError::InvalidManualId(group.id.clone()));
                }
                if !ids.insert(group.id.as_str()) {
                    errors.push(OverrideValidationError::DuplicateManualId(group.id.clone()));
                }
            }
            if group.name.is_empty() {
                errors.push(OverrideValidationError::EmptyName(group.id.clone()));
            } else if group.name.chars().count() > MAX_GROUP_NAME_CHARS {
                errors.push(OverrideValidationError::NameTooLong(group.id.clone()));
            }
            if !is_valid_logo(group.logo.as_deref()) {
                errors.push(OverrideValidationError::InvalidLogo(group.id.clone()));
            }
        }

        let mut auto_keys = HashSet::with_capacity(self.auto_overrides.len());
        for auto in &self.auto_overrides {
            if !auto_keys.insert(auto.auto_key.as_str()) {
                errors.push(OverrideValidationError::DuplicateAutoKey(auto.auto_key.clone()));
            }
            if !is_valid_logo(auto.logo.as_deref()) {
                errors.push(OverrideValidationError::InvalidLogo(auto.auto_key.clone()));
            }
        }

        // A ref may be claimed by one manual group or one auto `add` list, never by two entries.
        let manual_claims =
            self.groups.iter().enumerate().flat_map(|(idx, group)| group.members.iter().map(move |m| (idx, m)));
        let add_claims = self
            .auto_overrides
            .iter()
            .enumerate()
            .flat_map(|(idx, auto)| auto.add.iter().map(move |m| (self.groups.len() + idx, m)));
        let mut owners: HashMap<&ChannelMemberRefDto, usize> = HashMap::new();
        let mut reported: HashSet<&ChannelMemberRefDto> = HashSet::new();
        for (owner, member) in manual_claims.chain(add_claims) {
            let first_owner = *owners.entry(member).or_insert(owner);
            if first_owner != owner && reported.insert(member) {
                errors.push(OverrideValidationError::RefInMultipleGroups(member.clone()));
            }
        }

        // Exclusion only keeps a channel out of the natural buckets; an auto `add` still wins (E3).
        if !self.excluded.is_empty() {
            let excluded: HashSet<&ChannelMemberRefDto> = self.excluded.iter().collect();
            let mut reported: HashSet<&ChannelMemberRefDto> = HashSet::new();
            for member in self.auto_overrides.iter().flat_map(|auto| &auto.add) {
                if excluded.contains(member) && reported.insert(member) {
                    errors.push(OverrideValidationError::ExcludedAndAdded(member.clone()));
                }
            }
        }

        errors
    }

    /// Short content hash of the serialized section. Callers pass a canonicalized section.
    pub fn etag(&self) -> String {
        let bytes = serde_json::to_vec(self).unwrap_or_default();
        blake3::hash(&bytes).to_hex()[..ETAG_HEX_LEN].to_string()
    }
}

/// How a persisted failover group came to be.
#[derive(Debug, Copy, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FailoverGroupOrigin {
    Auto,
    Manual,
    AutoEdited,
}

/// Selection score of a group member, as the ranking computes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct ScoreBreakdown {
    pub qos: u8,
    pub quality: u8,
    pub order: u8,
    pub total: u8,
    pub demoted: bool,
}

/// Whether the target of an override section can be edited and reprocessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailoverTargetState {
    /// The target exists and has failover groups enabled.
    #[default]
    Enabled,
    /// The target exists, but failover groups are disabled for it.
    Disabled,
    /// No target of this name exists.
    Orphan,
}

/// Override section of one target as the targets overview shows it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FailoverGroupTargetStatusDto {
    pub name: String,
    /// Etag of the section in the current failover files.
    pub etag: Option<String>,
    /// Etag of the section that produced the persisted groups.
    pub applied_etag: Option<String>,
    pub file: Option<String>,
    pub findings: Vec<OverrideValidationError>,
    pub group_count: usize,
    pub problem_count: usize,
    #[serde(default)]
    pub state: FailoverTargetState,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FailoverGroupTargetsDto {
    /// File or directory the failover files are read from.
    pub source: Option<String>,
    pub files_used: Vec<String>,
    pub load_error: Option<String>,
    pub targets: Vec<FailoverGroupTargetStatusDto>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FailoverGroupMemberDto {
    /// Virtual id of the member in the persisted group, `None` when the group is not persisted.
    pub member_vid: Option<VirtualId>,
    pub member_ref: ChannelMemberRefDto,
    pub title: String,
    pub input_name: String,
    pub provider_id: u32,
    pub has_archive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FailoverGroupDto {
    /// Virtual id of the persisted group, `None` for a group that only exists in a preview.
    pub group_vid: Option<VirtualId>,
    /// Manual group id or auto key.
    pub key: String,
    pub origin: FailoverGroupOrigin,
    pub policy: FailoverGroupPolicy,
    pub name: String,
    pub logo: Option<String>,
    pub epg_channel_id: Option<String>,
    pub playlist_group: Option<String>,
    pub members: Vec<FailoverGroupMemberDto>,
    /// References of the overrides that matched no channel.
    pub unresolved: Vec<ChannelMemberRefDto>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FailoverGroupListDto {
    /// Etag of the section in the current failover files, also for a preview of a draft.
    pub etag: Option<String>,
    /// Etag of the section that produced the persisted groups.
    pub applied_etag: Option<String>,
    pub groups: Vec<FailoverGroupDto>,
    pub diagnostics: FailoverOverrideDiagnosticsDto,
    /// Keys of groups whose representative changed by a hot apply while the published playlist item still shows
    /// the old one; cleared by the next playlist update.
    #[serde(default)]
    pub display_stale: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TargetOverridesResponseDto {
    pub etag: Option<String>,
    pub file: Option<String>,
    pub overrides: TargetFailoverOverridesDto,
}

/// Body of the save route: the section the editor based on `expected_etag` (`None` = the section is new).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SaveOverridesRequestDto {
    #[serde(default)]
    pub expected_etag: Option<String>,
    pub overrides: TargetFailoverOverridesDto,
}

/// Answer of a successful save.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FailoverGroupSaveResultDto {
    /// Etag of the stored section, `None` when the section was emptied and removed.
    pub etag: Option<String>,
    pub file: Option<String>,
    /// The change was applied to the persisted groups without a playlist update.
    pub hot_applied: bool,
}

/// Answer of a write whose `expected_etag` no longer matches the failover files.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FailoverGroupConflictDto {
    pub current_etag: Option<String>,
}

/// Body of the route that moves a section to another target.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReassignOverridesRequestDto {
    pub from: String,
    pub to: String,
    pub expected_etag: String,
}

/// Which channels of the catalog a candidate search returns.
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailoverCandidateFilter {
    #[default]
    All,
    Ungrouped,
    Grouped,
    Excluded,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CandidateDto {
    pub member_ref: ChannelMemberRefDto,
    pub title: String,
    pub logo: String,
    pub playlist_group: String,
    pub provider_id: u32,
    pub has_archive: bool,
    /// Key of the persisted group the channel is a member of.
    pub group_key: Option<String>,
    pub excluded: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CandidatePageDto {
    /// Number of matches over all pages.
    pub total: usize,
    pub items: Vec<CandidateDto>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MemberHealthDto {
    pub member_vid: VirtualId,
    pub member_ref: ChannelMemberRefDto,
    pub title: String,
    /// Position in the selection order, starting at 1.
    pub rank: usize,
    pub qos_24h: Option<u8>,
    pub qos_7d: Option<u8>,
    pub confidence: Option<u8>,
    pub failure_streak: u32,
    pub resolution: Option<VideoResolution>,
    pub codec: Option<VideoCodec>,
    pub bitrate_kbps: Option<u32>,
    /// `closed`, `counting`, `open` or `half_open`.
    pub circuit: String,
    pub circuit_remaining_secs: Option<u64>,
    /// State of the circuit of the member's whole input, same values as `circuit`.
    pub input_circuit: String,
    /// Connections in use on the accounts of the member's input.
    pub slots_used: u32,
    /// Connection limit of the member's input accounts; accounts without a limit add nothing.
    pub slots_max: u32,
    pub score: ScoreBreakdown,
}

/// Union of the account pools of all members of a group, counted once per account.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GroupCapacityDto {
    pub accounts: u32,
    pub max_connections: u32,
    pub in_use: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FailoverGroupHealthDto {
    /// Members in selection order.
    pub members: Vec<MemberHealthDto>,
    pub capacity: GroupCapacityDto,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(input: &str, stream_id: &str) -> ChannelMemberRefDto {
        ChannelMemberRefDto { input: input.to_string(), stream_id: stream_id.to_string() }
    }

    #[test]
    fn override_diagnostics_default_is_empty_and_roundtrips() {
        let mut diagnostics = FailoverOverrideDiagnosticsDto::default();
        assert!(diagnostics.is_empty());
        diagnostics.unresolved_refs.push(UnresolvedRefDto { group: "zdf".to_string(), member_ref: member("a", "1") });
        assert!(!diagnostics.is_empty());
        let json = serde_json::to_string(&diagnostics).expect("serialize");
        assert_eq!(serde_json::from_str::<FailoverOverrideDiagnosticsDto>(&json).expect("deserialize"), diagnostics);
    }

    fn manual(id: &str, members: Vec<ChannelMemberRefDto>) -> ManualFailoverGroupDto {
        ManualFailoverGroupDto { id: id.to_string(), name: format!("Group {id}"), members, ..Default::default() }
    }

    #[test]
    fn canonicalize_trims_and_dedups_refs() {
        let mut section = TargetFailoverOverridesDto::new("  Main TV ");
        section.groups.push(ManualFailoverGroupDto {
            id: " sport ".to_string(),
            name: " Sport ".to_string(),
            logo: Some("  ".to_string()),
            epg_channel_id: Some(" epg ".to_string()),
            members: vec![member(" a ", " 1 "), member("a", "1"), member("", "2"), member("b", " ")],
            ..Default::default()
        });
        section.auto_overrides.push(AutoGroupOverrideDto {
            auto_key: " zdf ".to_string(),
            name: Some(" ZDF ".to_string()),
            order: vec![member("a", "1"), member(" a", "1 ")],
            add: vec![member("b", "2"), member("b", "2")],
            remove: vec![member(" ", "3")],
            ..Default::default()
        });
        section.excluded = vec![member(" c ", "9"), member("c", " 9"), member("c", "")];
        section.canonicalize();

        assert_eq!(section.target, "Main TV");
        let group = &section.groups[0];
        assert_eq!(group.id, "sport");
        assert_eq!(group.name, "Sport");
        assert_eq!(group.logo, None);
        assert_eq!(group.epg_channel_id.as_deref(), Some("epg"));
        assert_eq!(group.members, vec![member("a", "1")]);
        let auto = &section.auto_overrides[0];
        assert_eq!(auto.auto_key, "zdf");
        assert_eq!(auto.name.as_deref(), Some("ZDF"));
        assert_eq!(auto.order, vec![member("a", "1")]);
        assert_eq!(auto.add, vec![member("b", "2")]);
        assert_eq!(auto.remove.len(), 0);
        assert_eq!(section.excluded, vec![member("c", "9")]);
    }

    #[test]
    fn validate_reports_duplicate_and_invalid_ids() {
        let mut section = TargetFailoverOverridesDto::new("T");
        section.groups = vec![manual("Zdf", vec![]), manual("", vec![]), manual("a", vec![]), manual("a", vec![])];
        let errors = section.validate();
        assert_eq!(
            errors,
            vec![
                OverrideValidationError::InvalidManualId("Zdf".to_string()),
                OverrideValidationError::EmptyManualId,
                OverrideValidationError::DuplicateManualId("a".to_string()),
            ]
        );
    }

    #[test]
    fn validate_reports_name_problems() {
        let mut section = TargetFailoverOverridesDto::new("T");
        let mut empty = manual("a", vec![]);
        empty.name = String::new();
        let mut long = manual("b", vec![]);
        long.name = "x".repeat(MAX_GROUP_NAME_CHARS + 1);
        let mut ok = manual("c", vec![]);
        ok.name = "ä".repeat(MAX_GROUP_NAME_CHARS);
        section.groups = vec![empty, long, ok];
        assert_eq!(
            section.validate(),
            vec![
                OverrideValidationError::EmptyName("a".to_string()),
                OverrideValidationError::NameTooLong("b".to_string()),
            ]
        );
    }

    #[test]
    fn validate_reports_ref_in_two_manual_groups() {
        let mut section = TargetFailoverOverridesDto::new("T");
        section.groups =
            vec![manual("a", vec![member("p", "1")]), manual("b", vec![member("p", "1"), member("p", "2")])];
        assert_eq!(section.validate(), vec![OverrideValidationError::RefInMultipleGroups(member("p", "1"))]);
    }

    #[test]
    fn validate_reports_ref_in_manual_group_and_auto_add() {
        let mut section = TargetFailoverOverridesDto::new("T");
        section.groups = vec![manual("a", vec![member("p", "1")])];
        section.auto_overrides.push(AutoGroupOverrideDto {
            auto_key: "zdf".to_string(),
            add: vec![member("p", "1")],
            ..Default::default()
        });
        assert_eq!(section.validate(), vec![OverrideValidationError::RefInMultipleGroups(member("p", "1"))]);
    }

    #[test]
    fn validate_allows_same_ref_in_add_and_order_of_one_entry() {
        let mut section = TargetFailoverOverridesDto::new("T");
        section.auto_overrides.push(AutoGroupOverrideDto {
            auto_key: "zdf".to_string(),
            add: vec![member("p", "1")],
            order: vec![member("p", "1")],
            ..Default::default()
        });
        assert_eq!(section.validate().len(), 0);
    }

    #[test]
    fn validate_reports_excluded_ref_that_is_added() {
        let mut section = TargetFailoverOverridesDto::new("T");
        section.excluded = vec![member("p", "1"), member("p", "2")];
        section.auto_overrides.push(AutoGroupOverrideDto {
            auto_key: "zdf".to_string(),
            add: vec![member("p", "1"), member("p", "3")],
            ..Default::default()
        });
        let findings = section.validate();
        assert_eq!(findings, vec![OverrideValidationError::ExcludedAndAdded(member("p", "1"))]);
        assert_eq!(findings[0].to_string(), "channel p/1 is excluded and added to a group; the addition wins");
    }

    #[test]
    fn validate_allows_excluded_ref_in_manual_group() {
        let mut section = TargetFailoverOverridesDto::new("T");
        section.excluded = vec![member("p", "1")];
        section.groups = vec![manual("a", vec![member("p", "1")])];
        assert_eq!(section.validate().len(), 0);
    }

    #[test]
    fn validate_reports_duplicate_auto_key() {
        let mut section = TargetFailoverOverridesDto::new("T");
        for _ in 0..2 {
            section.auto_overrides.push(AutoGroupOverrideDto { auto_key: "zdf".to_string(), ..Default::default() });
        }
        assert_eq!(section.validate(), vec![OverrideValidationError::DuplicateAutoKey("zdf".to_string())]);
    }

    #[test]
    fn validate_rejects_non_http_logo() {
        let mut section = TargetFailoverOverridesDto::new("T");
        let mut bad = manual("a", vec![]);
        bad.logo = Some("ftp://x".to_string());
        let mut good = manual("b", vec![]);
        good.logo = Some("https://example.invalid/l.png".to_string());
        section.groups = vec![bad, good];
        section.auto_overrides.push(AutoGroupOverrideDto {
            auto_key: "zdf".to_string(),
            logo: Some("javascript:alert(1)".to_string()),
            ..Default::default()
        });
        assert_eq!(
            section.validate(),
            vec![
                OverrideValidationError::InvalidLogo("a".to_string()),
                OverrideValidationError::InvalidLogo("zdf".to_string()),
            ]
        );
    }

    #[test]
    fn disabled_defaults_false_and_is_not_serialized() {
        let auto: AutoGroupOverrideDto = serde_json::from_str(r#"{"auto_key":"zdf"}"#).expect("parses");
        assert!(!auto.disabled);
        let json = serde_json::to_string(&auto).expect("serializes");
        assert!(!json.contains("disabled"));
        let disabled = AutoGroupOverrideDto { auto_key: "zdf".to_string(), disabled: true, ..Default::default() };
        assert!(serde_json::to_string(&disabled).expect("serializes").contains(r#""disabled":true"#));
    }

    #[test]
    fn etag_is_stable_and_changes_on_any_field() {
        let build = || {
            let mut section = TargetFailoverOverridesDto::new("T");
            section.groups = vec![manual("a", vec![member("p", "1")])];
            section
        };
        let a = build();
        let b = build();
        assert_eq!(a.etag(), b.etag());
        assert_eq!(a.etag().len(), 16);

        let mut renamed = build();
        renamed.groups[0].name = "Other".to_string();
        assert_ne!(a.etag(), renamed.etag());
        let mut excluded = build();
        excluded.excluded.push(member("p", "2"));
        assert_ne!(a.etag(), excluded.etag());
    }

    #[test]
    fn is_empty_ignores_target_name() {
        let mut section = TargetFailoverOverridesDto::new("T");
        assert!(section.is_empty());
        section.excluded.push(member("p", "1"));
        assert!(!section.is_empty());
    }

    #[test]
    fn validation_error_serializes_with_snake_case_tag() {
        let json = serde_json::to_string(&OverrideValidationError::InvalidManualId("Zdf".to_string())).expect("json");
        assert_eq!(json, r#"{"error":"invalid_manual_id","detail":"Zdf"}"#);
        let json = serde_json::to_string(&OverrideValidationError::EmptyManualId).expect("json");
        assert_eq!(json, r#"{"error":"empty_manual_id"}"#);
        let back: OverrideValidationError = serde_json::from_str(&json).expect("roundtrip");
        assert_eq!(back, OverrideValidationError::EmptyManualId);
    }

    #[test]
    fn failover_file_roundtrips_multiple_targets() {
        let mut main = TargetFailoverOverridesDto::new("Main TV");
        main.groups = vec![manual("sport", vec![member("a", "1")])];
        main.auto_overrides.push(AutoGroupOverrideDto {
            auto_key: "rtl|rtl.de".to_string(),
            disabled: true,
            ..Default::default()
        });
        let kids = TargetFailoverOverridesDto::new("Kids");
        let file = FailoverFileDto { version: FAILOVER_FILE_VERSION, targets: vec![main, kids] };
        let json = serde_json::to_string(&file).expect("serializes");
        let back: FailoverFileDto = serde_json::from_str(&json).expect("parses");
        assert_eq!(back, file);
        let empty: FailoverFileDto = serde_json::from_str(r#"{"version":1}"#).expect("parses");
        assert_eq!(empty.targets.len(), 0);
    }

    #[test]
    fn target_state_serializes_snake_case_and_older_status_defaults_to_enabled() {
        assert_eq!(serde_json::to_string(&FailoverTargetState::Orphan).expect("serialize"), "\"orphan\"");
        assert_eq!(serde_json::to_string(&FailoverTargetState::Disabled).expect("serialize"), "\"disabled\"");
        let status: FailoverGroupTargetStatusDto = serde_json::from_str(
            r#"{"name":"t","etag":null,"applied_etag":null,"file":null,"findings":[],"group_count":0,"problem_count":0}"#,
        )
        .expect("deserialize");
        assert_eq!(status.state, FailoverTargetState::Enabled);
        let list: FailoverGroupListDto =
            serde_json::from_str(r#"{"etag":null,"applied_etag":null,"groups":[],"diagnostics":{}}"#).expect("list");
        assert_eq!(list.display_stale.len(), 0);
    }
}

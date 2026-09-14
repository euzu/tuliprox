use crate::{
    oracle::{AdmissionStrategy, GraceMode},
    TestkitError,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
    time::Duration,
};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub schema_version: u16,
    pub name: String,
    pub tuliprox: TuliproxConfig,
    #[serde(default)]
    pub origin: Option<OriginConfig>,
    #[serde(default)]
    pub actors: Vec<Actor>,
    #[serde(default)]
    pub channels: HashMap<String, Channel>,
    #[serde(default)]
    pub agents: AgentsConfig,
    #[serde(default)]
    pub policy_contract: Option<PolicyContract>,
    #[serde(default)]
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentsConfig {
    #[serde(default)]
    pub required: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyContract {
    pub user_access_control: bool,
    #[serde(default)]
    pub users: HashMap<String, UserPolicy>,
    /// `None` preserves an omitted field, which has distinct Tuliprox semantics
    /// from an explicit empty admission-strategy list.
    #[serde(default)]
    pub admission_strategies: Option<Vec<AdmissionStrategy>>,
    #[serde(default)]
    pub grace: Option<GraceContract>,
    #[serde(default)]
    pub provider_max_connections: Option<u16>,
    #[serde(default)]
    pub expected_provider_slots: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserPolicy {
    pub max_connections: usize,
    #[serde(default)]
    pub soft_connections: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraceContract {
    pub mode: GraceMode,
    pub timeout_millis: u64,
}

impl PolicyContract {
    #[must_use]
    pub fn effective_admission_strategies(&self) -> Vec<AdmissionStrategy> {
        self.admission_strategies.clone().unwrap_or_else(|| {
            self.grace.as_ref().map_or_else(Vec::new, |grace| match grace.mode {
                GraceMode::Instant => vec![AdmissionStrategy::GraceInstantStream],
                GraceMode::HoldStream => vec![AdmissionStrategy::GraceHoldStream],
            })
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TuliproxConfig {
    pub base_url: String,
    #[serde(default)]
    pub api_base_url: Option<String>,
    #[serde(default)]
    pub admin_username: Option<String>,
    #[serde(default)]
    pub admin_password_env: Option<String>,
    #[serde(default)]
    pub playlist_url: Option<String>,
    #[serde(default)]
    pub bootstrap: Option<BootstrapConfig>,
    #[serde(default)]
    pub execution_mode: ExecutionMode,
    #[serde(default)]
    pub playback_endpoint: PlaybackEndpoint,
    #[serde(default)]
    pub fixture_stream: FixtureStreamOptions,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginConfig {
    pub run_id: String,
    #[serde(default)]
    pub account_limit: Option<usize>,
    #[serde(default)]
    pub limit_mode: Option<crate::origin_transport::OriginLimitMode>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapConfig {
    pub command: String,
    #[serde(default)]
    pub arguments: Vec<String>,
    pub readiness_url: String,
    #[serde(default = "default_readiness_timeout_millis")]
    pub readiness_timeout_millis: u64,
}

fn default_readiness_timeout_millis() -> u64 { 30_000 }

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    #[default]
    IsolatedFixture,
    ExistingInstance,
}

/// Selects which Tuliprox playback endpoint the controller uses to build stream URLs.
/// `m3u` (default) discovers URLs from the M3U playlist; `xtream` builds an Xtream live URL
/// from the discovered virtual ID. Currently only `xtream_ts` channels support `xtream`.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackEndpoint {
    #[default]
    M3u,
    Xtream,
}

/// Controls per-fixture live-stream sharing. Defaults mirror the current global behaviour
/// (both HLS and MPEG-TS sharing enabled) so existing scenarios are unaffected.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FixtureStreamOptions {
    pub share_live_hls: bool,
    pub share_live_mpeg_ts: bool,
}

impl Default for FixtureStreamOptions {
    fn default() -> Self { Self { share_live_hls: true, share_live_mpeg_ts: true } }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Actor {
    pub id: String,
    pub agent: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password_env: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub client_ip: Option<ClientIp>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientIp {
    pub mode: String,
    #[serde(default)]
    pub value: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Channel {
    pub origin_marker: u32,
    pub protocol: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub command_id: String,
    #[serde(default)]
    pub expect: ExpectedPlayback,
    pub start: Option<Start>,
    #[serde(default)]
    pub stop: Option<Stop>,
    #[serde(default)]
    pub repeat: Option<Repeat>,
    #[serde(default)]
    pub await_frames: Option<u64>,
    #[serde(default, rename = "await")]
    pub await_condition: Option<AwaitCondition>,
    #[serde(default)]
    pub assert_origin: Option<AssertOrigin>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ExpectedRejection {
    #[serde(default)]
    pub custom_video: Option<crate::custom_video::CustomVideoKind>,
    #[serde(default)]
    pub http_status: Option<u16>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpectedPlayback {
    #[default]
    Streaming,
    Rejected,
    RejectedWith(ExpectedRejection),
}

impl ExpectedPlayback {
    #[must_use]
    pub fn is_streaming(&self) -> bool { matches!(self, Self::Streaming) }

    #[must_use]
    pub fn is_rejected(&self) -> bool { matches!(self, Self::Rejected | Self::RejectedWith(_)) }

    #[must_use]
    pub fn matches_outcome(&self, outcome: &crate::protocol::PlaybackOutcome) -> bool {
        use crate::protocol::{PlaybackOutcome, RejectionReason};
        match (self, outcome) {
            (Self::Streaming, PlaybackOutcome::Streaming { .. })
            | (Self::Rejected, PlaybackOutcome::AdmissionRejected { .. }) => true,
            (Self::RejectedWith(expected), PlaybackOutcome::AdmissionRejected { reason }) => match reason {
                RejectionReason::CustomVideo(kind) => {
                    expected.custom_video.is_none_or(|expected_kind| expected_kind == *kind)
                }
                RejectionReason::HttpStatus(status) => {
                    expected.http_status.is_none_or(|expected_status| expected_status == *status)
                }
                _ => expected.custom_video.is_none() && expected.http_status.is_none(),
            },
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AwaitCondition {
    #[serde(default)]
    pub valid_frames: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AssertOrigin {
    #[serde(default)]
    pub max_active_tcp: Option<usize>,
    #[serde(default)]
    pub max_active_body: Option<usize>,
    #[serde(default)]
    pub active_body: Option<usize>,
    #[serde(default)]
    pub no_evictions: Option<bool>,
    #[serde(default)]
    pub no_limit_rejections: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PostReadAction {
    #[default]
    KeepOpen,
    Pause,
    Close,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Start {
    pub actor: String,
    pub playback_id: String,
    pub session_group: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub range: Option<String>,
    #[serde(default)]
    pub expected_status: Option<u16>,
    #[serde(default)]
    pub expected_content_range: Option<String>,
    #[serde(default)]
    pub read_limit_bytes: Option<u64>,
    #[serde(default)]
    pub post_read_action: Option<PostReadAction>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub vod_object: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stop {
    pub playback_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Repeat {
    pub count: usize,
    pub steps: Vec<Step>,
}

impl Scenario {
    pub fn from_path(path: &Path) -> Result<Self, TestkitError> {
        let content = fs::read_to_string(path)?;
        let scenario: Self =
            serde_saphyr::from_str(&content).map_err(|error| TestkitError::Configuration(error.to_string()))?;
        scenario.validate()?;
        Ok(scenario)
    }

    pub fn validate(&self) -> Result<(), TestkitError> {
        if self.schema_version != 1 {
            return Err(TestkitError::Configuration("unsupported scenario schema version".to_owned()));
        }
        if self.name.trim().is_empty() || self.tuliprox.base_url.trim().is_empty() {
            return Err(TestkitError::Configuration("scenario name and Tuliprox URL are required".to_owned()));
        }
        if self.tuliprox.execution_mode == ExecutionMode::IsolatedFixture && self.tuliprox.bootstrap.is_none() {
            return Err(TestkitError::Configuration("isolated_fixture requires a bootstrap definition".to_owned()));
        }
        if self.origin.as_ref().is_some_and(|origin| origin.run_id.trim().is_empty()) {
            return Err(TestkitError::Configuration("origin run_id cannot be empty".to_owned()));
        }
        if self.policy_contract.as_ref().and_then(|contract| contract.provider_max_connections) == Some(0) {
            return Err(TestkitError::Configuration(
                "provider_max_connections must be positive when specified".to_owned(),
            ));
        }
        let actor_ids = self.actors.iter().map(|actor| actor.id.as_str()).collect::<HashSet<_>>();
        if actor_ids.len() != self.actors.len() || actor_ids.iter().any(|id| id.is_empty()) {
            return Err(TestkitError::Configuration("actor IDs must be unique and non-empty".to_owned()));
        }
        let steps = self.expanded_steps()?;
        let mut command_ids = HashSet::new();
        let mut playback_ids = HashSet::new();
        let mut seen_playback_ids = HashSet::new();
        for step in &steps {
            if !command_ids.insert(step.command_id.as_str()) || step.command_id.is_empty() {
                return Err(TestkitError::Configuration("command IDs must be unique and non-empty".to_owned()));
            }
            if step.start.is_some() == step.stop.is_some() {
                return Err(TestkitError::Configuration(
                    "each step must contain exactly one of start or stop".to_owned(),
                ));
            }
            let Some(start) = &step.start else {
                let stop = step.stop.as_ref().ok_or_else(|| TestkitError::Configuration("missing stop".to_owned()))?;
                if stop.playback_id.is_empty() || !playback_ids.contains(stop.playback_id.as_str()) {
                    return Err(TestkitError::Configuration(format!(
                        "stop references inactive playback {}",
                        stop.playback_id
                    )));
                }
                playback_ids.remove(stop.playback_id.as_str());
                continue;
            };
            if !actor_ids.contains(start.actor.as_str()) {
                return Err(TestkitError::Configuration(format!("unknown actor {}", start.actor)));
            }
            if start.playback_id.is_empty() || start.session_group.is_empty() {
                return Err(TestkitError::Configuration("start requires playback_id and session_group".to_owned()));
            }
            let target_count = [start.url.is_some(), start.channel.is_some(), start.vod_object.is_some()]
                .into_iter()
                .filter(|&v| v)
                .count();
            if target_count != 1 {
                return Err(TestkitError::Configuration(
                    "start requires exactly one of url, channel, or vod_object".to_owned(),
                ));
            }
            if start.url.as_deref().is_some_and(str::is_empty)
                || start.channel.as_deref().is_some_and(str::is_empty)
                || start.vod_object.as_deref().is_some_and(str::is_empty)
            {
                return Err(TestkitError::Configuration(
                    "start URL, channel, or vod_object cannot be empty".to_owned(),
                ));
            }
            if let Some(channel) = &start.channel {
                if !self.channels.contains_key(channel) {
                    return Err(TestkitError::Configuration(format!("unknown channel {channel}")));
                }
            }
            if !seen_playback_ids.insert(start.playback_id.as_str()) {
                return Err(TestkitError::Configuration(format!("duplicate playback ID {}", start.playback_id)));
            }
            playback_ids.insert(start.playback_id.as_str());
            if step.expect == ExpectedPlayback::Rejected
                && step
                    .await_frames
                    .or_else(|| step.await_condition.as_ref().and_then(|condition| condition.valid_frames))
                    .is_some()
            {
                return Err(TestkitError::Configuration("rejected playback must not await valid frames".to_owned()));
            }
        }
        self.validate_policy_contract()?;
        self.validate_playback_endpoint()?;
        Ok(())
    }

    fn validate_policy_contract(&self) -> Result<(), TestkitError> {
        if let Some(contract) = &self.policy_contract {
            if !contract.user_access_control && !contract.users.is_empty() {
                return Err(TestkitError::Configuration("policy users require user_access_control".to_owned()));
            }
            if contract.grace.as_ref().is_some_and(|grace| grace.timeout_millis == 0) {
                return Err(TestkitError::Configuration("grace timeout must be non-zero".to_owned()));
            }
        }
        Ok(())
    }

    fn validate_playback_endpoint(&self) -> Result<(), TestkitError> {
        if self.tuliprox.playback_endpoint == PlaybackEndpoint::Xtream {
            for (name, channel) in &self.channels {
                if channel.protocol != "xtream_ts" {
                    return Err(TestkitError::Configuration(format!(
                        "channel {name} uses protocol '{}' which is not supported with playback_endpoint: xtream (only xtream_ts is supported)",
                        channel.protocol
                    )));
                }
            }
        }
        Ok(())
    }

    pub fn expanded_steps(&self) -> Result<Vec<Step>, TestkitError> {
        let mut expanded = Vec::new();
        expand_steps(&self.steps, "", &mut expanded)?;
        Ok(expanded)
    }
}

fn expand_steps(steps: &[Step], prefix: &str, expanded: &mut Vec<Step>) -> Result<(), TestkitError> {
    const MAX_REPEAT_COUNT: usize = 1_000;
    for step in steps {
        if step.command_id.is_empty() {
            return Err(TestkitError::Configuration("command IDs must be unique and non-empty".to_owned()));
        }
        if let Some(repeat) = &step.repeat {
            if step.start.is_some() || step.stop.is_some() {
                return Err(TestkitError::Configuration("repeat cannot be combined with start or stop".to_owned()));
            }
            if repeat.count == 0 || repeat.count > MAX_REPEAT_COUNT || repeat.steps.is_empty() {
                return Err(TestkitError::Configuration("repeat requires 1 through 1000 child steps".to_owned()));
            }
            for iteration in 0..repeat.count {
                expand_steps(&repeat.steps, &format!("{prefix}{}:{iteration}:", step.command_id), expanded)?;
            }
            continue;
        }
        if step.start.is_some() == step.stop.is_some() {
            return Err(TestkitError::Configuration("each step must contain exactly one of start or stop".to_owned()));
        }
        let mut child = step.clone();
        child.command_id = format!("{prefix}{}", step.command_id);
        if let Some(start) = child.start.as_mut() {
            start.playback_id = format!("{prefix}{}", start.playback_id);
            start.session_group = format!("{prefix}{}", start.session_group);
        }
        if let Some(stop) = child.stop.as_mut() {
            stop.playback_id = format!("{prefix}{}", stop.playback_id);
        }
        expanded.push(child);
    }
    Ok(())
}

#[must_use]
pub fn lease_duration(millis: u64) -> Duration { Duration::from_millis(millis.max(1)) }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_playbacks_are_rejected() {
        let scenario = Scenario {
            schema_version: 1,
            name: "test".to_owned(),
            tuliprox: TuliproxConfig {
                base_url: "http://example.invalid".to_owned(),
                api_base_url: None,
                admin_username: None,
                admin_password_env: None,
                playlist_url: None,
                bootstrap: None,
                execution_mode: ExecutionMode::IsolatedFixture,
                playback_endpoint: PlaybackEndpoint::default(),
                fixture_stream: FixtureStreamOptions::default(),
            },
            origin: None,
            actors: vec![Actor {
                id: "a".to_owned(),
                agent: "local".to_owned(),
                username: None,
                password_env: None,
                user_agent: None,
                client_ip: None,
            }],
            channels: HashMap::new(),
            agents: AgentsConfig::default(),
            policy_contract: None,
            steps: (0..2)
                .map(|number| Step {
                    command_id: number.to_string(),
                    expect: ExpectedPlayback::Streaming,
                    start: Some(Start {
                        actor: "a".to_owned(),
                        playback_id: "same".to_owned(),
                        session_group: "s".to_owned(),
                        url: Some("http://example.invalid".to_owned()),
                        channel: None,
                        ..Default::default()
                    }),
                    stop: None,
                    repeat: None,
                    await_frames: None,
                    await_condition: None,
                    assert_origin: None,
                })
                .collect(),
        };
        assert!(scenario.validate().is_err());
    }

    #[test]
    fn checked_in_admission_scenarios_match_the_versioned_schema() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test/fixtures/testkit/scenarios");
        for filename in [
            "hard-limit.yml",
            "evict-same-ip-latest.yml",
            "evict-same-ip-oldest.yml",
            "evict-user-oldest.yml",
            "evict-user-latest.yml",
            "provider-hard-limit.yml",
            "shared-stream-single-provider-slot.yml",
            "vod-range-reopen-preserves-three-live.yml",
            "vod-range-reopen-strict-cap.yml",
            "vod-reopen-backpressured-body.yml",
            "live-ts-same-channel-retry-latest-wins.yml",
        ] {
            assert!(Scenario::from_path(&root.join(filename)).is_ok(), "{filename}");
        }
    }

    #[test]
    fn stop_must_reference_an_active_playback() {
        let scenario: Scenario = serde_saphyr::from_str(
            r"
schema_version: 1
name: stop-validation
tuliprox: { base_url: http://example.invalid, execution_mode: existing_instance }
actors: [{ id: a, agent: local }]
steps:
  - command_id: stop-missing
    stop: { playback_id: missing }
",
        )
        .unwrap();
        assert!(scenario.validate().is_err());
    }

    #[test]
    fn repeat_derives_distinct_playback_and_command_ids_per_iteration() {
        let scenario: Scenario = serde_saphyr::from_str(
            r"
schema_version: 1
name: repeat-validation
tuliprox: { base_url: http://example.invalid, execution_mode: existing_instance }
actors: [{ id: a, agent: local }]
steps:
  - command_id: repeat
    repeat:
      count: 2
      steps:
        - command_id: begin
          start: { actor: a, playback_id: playback, session_group: session, url: http://example.invalid/17.ts }
        - command_id: end
          stop: { playback_id: playback }
",
        )
        .unwrap();
        assert!(scenario.validate().is_ok());
        let expanded = scenario.expanded_steps().unwrap();
        assert_eq!(expanded.len(), 4);
        assert_eq!(expanded[0].command_id, "repeat:0:begin");
        assert_eq!(expanded[0].start.as_ref().map(|start| start.playback_id.as_str()), Some("repeat:0:playback"));
        assert_eq!(expanded[2].command_id, "repeat:1:begin");
        assert_eq!(expanded[3].stop.as_ref().map(|stop| stop.playback_id.as_str()), Some("repeat:1:playback"));
    }

    #[test]
    fn rejected_playback_does_not_match_500_corrupt_data_or_early_eof() {
        use crate::{
            custom_video::CustomVideoKind,
            protocol::{PlaybackOutcome, RejectionReason},
        };

        let expected_rejected = ExpectedPlayback::Rejected;

        // Proved admission rejection matches
        let valid_cv = PlaybackOutcome::AdmissionRejected {
            reason: RejectionReason::CustomVideo(CustomVideoKind::UserConnectionsExhausted),
        };
        assert!(expected_rejected.matches_outcome(&valid_cv));

        let valid_http = PlaybackOutcome::AdmissionRejected { reason: RejectionReason::HttpStatus(429) };
        assert!(expected_rejected.matches_outcome(&valid_http));

        // HTTP 500 MUST NOT pass as expected rejection
        let http_500 = PlaybackOutcome::HttpError { status: 500 };
        assert!(!expected_rejected.matches_outcome(&http_500));

        // Invalid marker/corrupt frame MUST NOT pass
        let invalid_marker = PlaybackOutcome::InvalidData { message: "wrong marker".to_owned() };
        assert!(!expected_rejected.matches_outcome(&invalid_marker));

        // Early EOF MUST NOT pass
        let early_eof = PlaybackOutcome::UnexpectedEof { frames: 0, bytes: 0 };
        assert!(!expected_rejected.matches_outcome(&early_eof));

        // Transport error MUST NOT pass
        let transport_err = PlaybackOutcome::TransportError { message: "connection reset".to_owned() };
        assert!(!expected_rejected.matches_outcome(&transport_err));

        // Idle timeout MUST NOT pass
        assert!(!expected_rejected.matches_outcome(&PlaybackOutcome::IdleTimeout));
    }

    #[test]
    fn playback_endpoint_defaults_to_m3u() {
        let cfg: TuliproxConfig = serde_saphyr::from_str("base_url: 'http://example.invalid'").unwrap();
        assert_eq!(cfg.playback_endpoint, PlaybackEndpoint::M3u);
    }

    #[test]
    fn playback_endpoint_xtream_deserializes() {
        let cfg: TuliproxConfig =
            serde_saphyr::from_str("base_url: 'http://example.invalid'\nplayback_endpoint: xtream").unwrap();
        assert_eq!(cfg.playback_endpoint, PlaybackEndpoint::Xtream);
    }

    #[test]
    fn fixture_stream_defaults_enable_sharing() {
        let opts = FixtureStreamOptions::default();
        assert!(opts.share_live_hls);
        assert!(opts.share_live_mpeg_ts);
    }

    #[test]
    fn playback_endpoint_xtream_rejects_non_xtream_ts_channel() {
        let scenario: Scenario = serde_saphyr::from_str(
            r"
schema_version: 1
name: xtream-validation
tuliprox:
  base_url: 'http://example.invalid'
  execution_mode: existing_instance
  playback_endpoint: xtream
actors: [{ id: a, agent: local }]
channels:
  ch: { origin_marker: 17, protocol: hls }
steps:
  - command_id: s
    start: { actor: a, playback_id: p, session_group: s, channel: ch }
",
        )
        .unwrap();
        assert!(scenario.validate().is_err());
    }
}

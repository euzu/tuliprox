use crate::channel_group::{
    circuit::{CircuitView, MemberKey},
    health::{member_key, MemberHealthIndex},
};
use shared::{
    model::{AudioChannels, AudioCodec, ChannelGroupPolicy, RuntimeAudioLanguage, VideoCodec, VideoResolution},
    utils::quality_rank,
};
use std::{cmp::Reverse, sync::Arc, time::Instant};
use tuliprox_repository::GroupMember;

pub const W_QOS: i32 = 6;
pub const W_QUALITY: i32 = 3;
pub const W_ORDER: i32 = 1;
pub const NEUTRAL_QOS: u8 = 50;
pub const OPEN_CIRCUIT_PENALTY: i32 = 100;
pub const SAME_CODEC_BONUS: i32 = 10;
pub const SAME_RESOLUTION_BONUS: i32 = 10;
pub const SAME_AUDIO_BONUS: i32 = 5;
pub const SAME_LANGUAGE_BONUS: i32 = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSignature {
    pub codec: AudioCodec,
    pub channels: AudioChannels,
    pub language: Option<Arc<str>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionMode {
    Live,
    Runtime {
        current_codec: VideoCodec,
        current_resolution: VideoResolution,
        current_audio: Option<AudioSignature>,
        audio_language: RuntimeAudioLanguage,
    },
    Archive {
        start_ts: i64,
        now_ts: i64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct ScoreBreakdown {
    pub qos: u8,
    pub quality: u8,
    pub order: u8,
    pub total: u8,
    pub demoted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankedCandidate {
    pub index: usize,
    pub key: MemberKey,
    pub score: ScoreBreakdown,
}

fn filter_candidates<'a>(
    target_name: &Arc<str>,
    members: &'a [GroupMember],
    health: &MemberHealthIndex,
    mode: &SelectionMode,
    now: Instant,
) -> Vec<(usize, &'a GroupMember)> {
    let mut candidates = Vec::with_capacity(members.len());
    for (i, member) in members.iter().enumerate() {
        match mode {
            SelectionMode::Live => {
                candidates.push((i, member));
            }
            SelectionMode::Archive { start_ts, now_ts } => {
                if let Some(archive) = member.archive {
                    let earliest_ts = now_ts.saturating_sub(i64::from(archive.duration_days) * 86_400);
                    if *start_ts >= earliest_ts {
                        candidates.push((i, member));
                    }
                }
            }
            SelectionMode::Runtime { current_audio, audio_language, .. } => {
                if *audio_language == RuntimeAudioLanguage::Require {
                    if let Some(target_lang) = current_audio.as_ref().and_then(|a| a.language.as_deref()) {
                        let m_health = health.health(target_name, member, now);
                        if let Some(cand_lang) = m_health.probe.as_ref().and_then(|p| p.audio_language.as_deref()) {
                            if !cand_lang.eq_ignore_ascii_case(target_lang) {
                                continue;
                            }
                        }
                    }
                }
                candidates.push((i, member));
            }
        }
    }
    candidates
}

fn compute_quality_points(
    probe: Option<&crate::channel_group::health::ProbePart>,
    title: &str,
    min_bitrate: u32,
    max_bitrate: u32,
) -> u8 {
    let (res_pts, bitrate_pts) = if let Some(probe) = probe {
        let res = match probe.resolution {
            VideoResolution::SD => 25,
            VideoResolution::P720 => 50,
            VideoResolution::P1080 => 75,
            VideoResolution::P1440 | VideoResolution::P2160 | VideoResolution::P4320 => 100,
            VideoResolution::Unknown => match quality_rank(title) {
                1 => 25,
                2 => 50,
                3 => 75,
                4 | 5 => 100,
                _ => 0,
            },
        };
        let b_pts = if max_bitrate > min_bitrate {
            u32::try_from(
                10 * u64::from(probe.bitrate_kbps.saturating_sub(min_bitrate)) / u64::from(max_bitrate - min_bitrate),
            )
            .unwrap_or(0)
        } else {
            0
        };
        (res, b_pts)
    } else {
        let res = match quality_rank(title) {
            1 => 25,
            2 => 50,
            3 => 75,
            4 | 5 => 100,
            _ => 0,
        };
        (res, 0)
    };
    u8::try_from((res_pts + bitrate_pts).min(100)).unwrap_or(100)
}

fn compute_runtime_bonuses(probe: Option<&crate::channel_group::health::ProbePart>, mode: &SelectionMode) -> i32 {
    let SelectionMode::Runtime { current_codec, current_resolution, current_audio, .. } = mode else {
        return 0;
    };
    let Some(probe) = probe else {
        return 0;
    };

    let mut bonuses = 0i32;
    if *current_codec != VideoCodec::Other && probe.codec == *current_codec {
        bonuses += SAME_CODEC_BONUS;
    }
    if *current_resolution != VideoResolution::Unknown && probe.resolution == *current_resolution {
        bonuses += SAME_RESOLUTION_BONUS;
    }
    if let Some(curr_a) = current_audio {
        if curr_a.codec != AudioCodec::Other
            && probe.audio_codec == curr_a.codec
            && probe.audio_channels == curr_a.channels
        {
            bonuses += SAME_AUDIO_BONUS;
        }
        if let Some(curr_lang) = &curr_a.language {
            if let Some(cand_lang) = &probe.audio_language {
                if cand_lang.eq_ignore_ascii_case(curr_lang) {
                    bonuses += SAME_LANGUAGE_BONUS;
                }
            }
        }
    }
    bonuses
}

pub fn rank_members(
    target_name: &Arc<str>,
    members: &[GroupMember],
    policy: ChannelGroupPolicy,
    health: &MemberHealthIndex,
    mode: &SelectionMode,
    now: Instant,
) -> Vec<RankedCandidate> {
    if members.is_empty() {
        return Vec::new();
    }

    let candidates = filter_candidates(target_name, members, health, mode, now);
    if candidates.is_empty() {
        return Vec::new();
    }

    let member_healths: Vec<_> = candidates.iter().map(|(_, m)| health.health(target_name, m, now)).collect();

    let mut max_bitrate = 0u32;
    let mut min_bitrate = u32::MAX;
    for h in &member_healths {
        let b = h.probe.as_ref().map_or(0, |p| p.bitrate_kbps);
        max_bitrate = max_bitrate.max(b);
        min_bitrate = min_bitrate.min(b);
    }
    if min_bitrate == u32::MAX {
        min_bitrate = 0;
    }

    let mut ranked = Vec::with_capacity(candidates.len());
    for ((index, member), m_health) in candidates.into_iter().zip(member_healths) {
        let key = member_key(member);

        let member_circuit_open = matches!(m_health.circuit, CircuitView::Open { .. });
        let input_circuit_open = matches!(health.input_circuit_view(&member.input_name, now), CircuitView::Open { .. });
        let high_failure_streak = m_health.qos.is_some_and(|q| q.failure_streak >= 3);
        let demoted = member_circuit_open || input_circuit_open || high_failure_streak;

        let qos = m_health.qos.map_or(NEUTRAL_QOS, |q| {
            let conf = u32::from(q.confidence);
            let s24 = u32::from(q.score_24h);
            let s7d = u32::from(q.score_7d);
            u8::try_from((s24 * conf + s7d * (100 - conf)) / 100).unwrap_or(0)
        });

        let quality = compute_quality_points(m_health.probe.as_ref(), member.title.as_ref(), min_bitrate, max_bitrate);
        let order = u8::try_from(100 - (100 * index / members.len())).unwrap_or(0);
        let base_total = (W_QOS * i32::from(qos) + W_QUALITY * i32::from(quality) + W_ORDER * i32::from(order)) / 10;
        let bonuses = compute_runtime_bonuses(m_health.probe.as_ref(), mode);
        let penalty = if member_circuit_open || input_circuit_open { OPEN_CIRCUIT_PENALTY } else { 0 };
        let total = u8::try_from((base_total + bonuses - penalty).clamp(0, 100)).unwrap_or(0);

        ranked.push(RankedCandidate { index, key, score: ScoreBreakdown { qos, quality, order, total, demoted } });
    }

    match policy {
        ChannelGroupPolicy::Auto => {
            ranked.sort_by_key(|c| (c.score.demoted, Reverse(c.score.total), c.index));
        }
        ChannelGroupPolicy::Priority => {
            ranked.sort_by_key(|c| (c.score.demoted, c.index));
        }
    }

    ranked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel_group::{
        circuit::{CircuitPolicy, FailureClass},
        health::{member_key, QosKey, QosPart},
        test_support::test_member,
    };
    use std::time::Duration;
    use tuliprox_repository::MemberArchive;

    const TARGET: &str = "Main";

    fn fixture(spec: &[(&str, u32, Option<u8>, &str)]) -> (Vec<GroupMember>, MemberHealthIndex) {
        let index = MemberHealthIndex::new(CircuitPolicy::new(1, Duration::from_secs(60), Duration::from_secs(60)));
        let mut members = Vec::new();
        for (i, (input, provider_id, qos_24h, probe)) in spec.iter().enumerate() {
            let member = test_member(input, &provider_id.to_string(), *provider_id, 100 + u32::try_from(i).unwrap());
            if let Some(score) = qos_24h {
                index.replace_qos_merge([(
                    QosKey::Provider { input: (*input).into(), provider_id: *provider_id },
                    QosPart { score_24h: *score, score_7d: *score, confidence: 100, failure_streak: 0 },
                )]);
            }
            if !probe.is_empty() {
                let (codec, dims) = probe.split_once(':').unwrap_or(("h264", probe));
                let (w, h) = dims.split_once('x').expect("WxH");
                let json = format!(r#"{{"codec_name":"{codec}","width":{w},"height":{h}}}"#);
                index.patch_probe(&member_key(&member), Some(&json), 5_000);
            }
            members.push(member);
        }
        (members, index)
    }

    fn fixture_names(titles: &[&str]) -> (Vec<GroupMember>, MemberHealthIndex) {
        let (mut members, index) = fixture(
            &titles
                .iter()
                .enumerate()
                .map(|(i, _)| ("solis", u32::try_from(i + 1).unwrap(), None, ""))
                .collect::<Vec<_>>(),
        );
        for (member, title) in members.iter_mut().zip(titles) {
            member.title = (*title).into();
        }
        (members, index)
    }

    fn open_circuit(index: &MemberHealthIndex, member: &GroupMember) {
        index.record_failure(&member_key(member), Instant::now());
    }

    fn rank(
        members: &[GroupMember],
        policy: ChannelGroupPolicy,
        index: &MemberHealthIndex,
        mode: &SelectionMode,
    ) -> Vec<RankedCandidate> {
        rank_members(&Arc::from(TARGET), members, policy, index, mode, Instant::now())
    }

    #[test]
    fn auto_prefers_uhd_over_one_point_better_qos() {
        let (members, health) = fixture(&[("solis", 1, Some(81), "1920x1080"), ("korath", 7, Some(80), "3840x2160")]);
        assert_eq!(rank(&members, ChannelGroupPolicy::Auto, &health, &SelectionMode::Live)[0].index, 1);
    }

    #[test]
    fn auto_prefers_much_better_qos_over_resolution() {
        let (members, health) = fixture(&[("solis", 1, Some(95), "1920x1080"), ("korath", 7, Some(30), "3840x2160")]);
        assert_eq!(rank(&members, ChannelGroupPolicy::Auto, &health, &SelectionMode::Live)[0].index, 0);
    }

    #[test]
    fn score_breakdown_uses_fixed_weights() {
        let (members, health) = fixture(&[("solis", 1, Some(100), "3840x2160")]);
        let ranked = rank(&members, ChannelGroupPolicy::Auto, &health, &SelectionMode::Live);
        assert_eq!(
            (ranked[0].score.qos, ranked[0].score.quality, ranked[0].score.order, ranked[0].score.total),
            (100, 100, 100, 100)
        );
    }

    #[test]
    fn missing_data_is_neutral_and_uses_name_quality() {
        let (members, health) = fixture_names(&["ZDF SD", "ZDF FHD"]);
        let ranked = rank(&members, ChannelGroupPolicy::Auto, &health, &SelectionMode::Live);
        assert_eq!(ranked[0].index, 1);
        assert_eq!(ranked[0].score.qos, 50);
    }

    #[test]
    fn priority_keeps_order_but_demotes_open_circuit_to_end() {
        let (members, health) =
            fixture(&[("solis", 1, Some(10), "720x576"), ("korath", 7, Some(99), "3840x2160"), ("veyra", 3, None, "")]);
        open_circuit(&health, &members[0]);
        let ranked = rank(&members, ChannelGroupPolicy::Priority, &health, &SelectionMode::Live);
        assert_eq!(ranked.iter().map(|c| c.index).collect::<Vec<_>>(), vec![1, 2, 0]);
        assert!(ranked[2].score.demoted);
    }

    #[test]
    fn open_input_circuit_demotes_all_members_of_that_input() {
        let (members, health) =
            fixture(&[("solis", 1, Some(99), ""), ("solis", 2, Some(99), ""), ("korath", 7, Some(40), "")]);
        health.set_policy(
            CircuitPolicy::new(5, Duration::from_secs(60), Duration::from_secs(60))
                .with_input(2, Duration::from_secs(120)),
        );
        let now = Instant::now();
        health.record_classified_failure(&member_key(&members[0]), FailureClass::Transport, now);
        health.record_classified_failure(&member_key(&members[1]), FailureClass::Transport, now);
        let ranked = rank(&members, ChannelGroupPolicy::Auto, &health, &SelectionMode::Live);
        assert_eq!(ranked[0].index, 2);
        assert!(ranked[1].score.demoted && ranked[2].score.demoted);
    }

    #[test]
    fn open_circuit_penalty_saturates_at_zero() {
        let (members, health) = fixture(&[("solis", 1, Some(0), "720x576")]);
        open_circuit(&health, &members[0]);
        assert_eq!(rank(&members, ChannelGroupPolicy::Auto, &health, &SelectionMode::Live)[0].score.total, 0);
    }

    #[test]
    fn runtime_mode_prefers_same_codec() {
        let (members, health) =
            fixture(&[("solis", 1, Some(70), "hevc:3840x2160"), ("korath", 7, Some(70), "h264:1920x1080")]);
        let mode = SelectionMode::Runtime {
            current_codec: VideoCodec::H264,
            current_resolution: VideoResolution::P1080,
            current_audio: None,
            audio_language: RuntimeAudioLanguage::Prefer,
        };
        assert_eq!(rank(&members, ChannelGroupPolicy::Auto, &health, &mode)[0].index, 1);
    }

    fn set_audio(health: &MemberHealthIndex, member: &GroupMember, json: &str) {
        health.patch_probe_audio(&member_key(member), Some(json));
    }

    fn runtime_with_audio(language: Option<&str>, policy: RuntimeAudioLanguage) -> SelectionMode {
        SelectionMode::Runtime {
            current_codec: VideoCodec::H264,
            current_resolution: VideoResolution::P1080,
            current_audio: Some(AudioSignature {
                codec: AudioCodec::AC3,
                channels: AudioChannels::Surround51,
                language: language.map(Into::into),
            }),
            audio_language: policy,
        }
    }

    #[test]
    fn runtime_mode_prefers_same_audio_and_language() {
        let (members, health) =
            fixture(&[("solis", 1, Some(70), "h264:1920x1080"), ("korath", 7, Some(70), "h264:1920x1080")]);
        set_audio(&health, &members[0], r#"[{"codec_name":"aac","channels":2,"tags":{"language":"eng"}}]"#);
        set_audio(&health, &members[1], r#"[{"codec_name":"ac3","channels":6,"tags":{"language":"deu"}}]"#);
        let ranked = rank(
            &members,
            ChannelGroupPolicy::Auto,
            &health,
            &runtime_with_audio(Some("deu"), RuntimeAudioLanguage::Prefer),
        );
        assert_eq!(ranked[0].index, 1);
        assert_eq!(ranked.len(), 2);
    }

    #[test]
    fn require_language_drops_known_mismatch_but_keeps_unknown() {
        let (members, health) =
            fixture(&[("solis", 1, Some(90), ""), ("korath", 7, Some(50), ""), ("veyra", 3, Some(40), "")]);
        set_audio(&health, &members[0], r#"[{"codec_name":"aac","channels":2,"tags":{"language":"eng"}}]"#);
        set_audio(&health, &members[1], r#"[{"codec_name":"aac","channels":2}]"#); // unknown language
        set_audio(&health, &members[2], r#"[{"codec_name":"aac","channels":2,"tags":{"language":"deu"}}]"#);
        let ranked = rank(
            &members,
            ChannelGroupPolicy::Auto,
            &health,
            &runtime_with_audio(Some("deu"), RuntimeAudioLanguage::Require),
        );
        assert_eq!(ranked.iter().map(|c| c.index).collect::<Vec<_>>(), vec![1, 2]);
    }

    #[test]
    fn archive_mode_filters_members_not_covering_start() {
        let (mut members, health) = fixture(&[("solis", 1, Some(90), ""), ("korath", 7, Some(50), "")]);
        members[0].archive = Some(MemberArchive { duration_days: 1 });
        members[1].archive = Some(MemberArchive { duration_days: 7 });
        let now_ts = 1_700_000_000;
        let mode = SelectionMode::Archive { start_ts: now_ts - 3 * 86_400, now_ts };
        assert_eq!(
            rank(&members, ChannelGroupPolicy::Auto, &health, &mode).iter().map(|c| c.index).collect::<Vec<_>>(),
            vec![1]
        );
    }
}

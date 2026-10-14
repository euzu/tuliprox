use super::circuit::{
    compute_cooldown, record_input_failure_in_shared, CircuitAdmission, CircuitPolicy, CircuitSharedState,
    CircuitState, CircuitView, FailureClass, HalfOpenGuard, MemberKey,
};
use arc_swap::ArcSwap;
use shared::model::{AudioChannels, AudioCodec, MediaQuality, VideoCodec, VideoResolution};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tuliprox_repository::GroupMember;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum QosKey {
    Provider { input: Arc<str>, provider_id: u32 },
    TargetMember { target: Arc<str>, member_vid: u32 },
}

pub fn member_key(member: &GroupMember) -> MemberKey {
    (Arc::clone(&member.input_name), Arc::clone(&member.input_stream_id))
}

pub fn qos_key(target_name: &Arc<str>, member: &GroupMember) -> QosKey {
    if member.provider_id > 0 {
        QosKey::Provider { input: Arc::clone(&member.input_name), provider_id: member.provider_id }
    } else {
        QosKey::TargetMember { target: Arc::clone(target_name), member_vid: member.member_vid.get() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QosPart {
    pub score_24h: u8,
    pub score_7d: u8,
    pub confidence: u8,
    pub failure_streak: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbePart {
    pub resolution: VideoResolution,
    pub codec: VideoCodec,
    pub bitrate_kbps: u32,
    pub audio_codec: AudioCodec,
    pub audio_channels: AudioChannels,
    pub audio_language: Option<Arc<str>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberHealth {
    pub qos: Option<QosPart>,
    pub probe: Option<ProbePart>,
    pub circuit: CircuitView,
}

pub struct MemberHealthIndex {
    qos: ArcSwap<HashMap<QosKey, QosPart>>,
    probe: ArcSwap<HashMap<MemberKey, ProbePart>>,
    shared: Arc<CircuitSharedState>,
    policy: ArcSwap<CircuitPolicy>,
    event_sink: ArcSwap<Option<Arc<dyn shared::model::EventSink>>>,
}

impl Default for MemberHealthIndex {
    fn default() -> Self { Self::new(CircuitPolicy::default()) }
}

impl MemberHealthIndex {
    pub fn new(policy: CircuitPolicy) -> Self {
        Self {
            qos: ArcSwap::from_pointee(HashMap::new()),
            probe: ArcSwap::from_pointee(HashMap::new()),
            shared: Arc::new(CircuitSharedState::default()),
            policy: ArcSwap::from_pointee(policy),
            event_sink: ArcSwap::from_pointee(None),
        }
    }

    pub fn set_event_sink(&self, sink: Arc<dyn shared::model::EventSink>) {
        self.event_sink.store(Arc::new(Some(sink)));
    }

    fn emit_circuit_changed(&self, input_name: Arc<str>, input_stream_id: Arc<str>, state: CircuitState) {
        if let Some(sink) = self.event_sink.load().as_ref() {
            sink.emit(shared::model::EventMessage::ChannelGroupCircuitChanged { input_name, input_stream_id, state });
        }
    }

    pub fn health(&self, target_name: &Arc<str>, member: &GroupMember, now: Instant) -> MemberHealth {
        let q_key = qos_key(target_name, member);
        let qos = self.qos.load().get(&q_key).copied();
        let m_key = member_key(member);
        let probe = self.probe.load().get(&m_key).cloned();
        let circuit = self.circuit_view(&m_key, now);
        MemberHealth { qos, probe, circuit }
    }

    pub fn replace_qos(&self, entries: impl IntoIterator<Item = (QosKey, QosPart)>) {
        let mut map = HashMap::new();
        for (k, v) in entries {
            match map.entry(k) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(v);
                }
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    if v.confidence >= e.get().confidence {
                        e.insert(v);
                    }
                }
            }
        }
        self.qos.store(Arc::new(map));
    }

    pub fn replace_qos_merge(&self, entries: impl IntoIterator<Item = (QosKey, QosPart)>) {
        let mut map = (**self.qos.load()).clone();
        for (k, v) in entries {
            match map.entry(k) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(v);
                }
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    if v.confidence >= e.get().confidence {
                        e.insert(v);
                    }
                }
            }
        }
        self.qos.store(Arc::new(map));
    }

    pub fn patch_probe(&self, key: &MemberKey, video_json: Option<&str>, bitrate_kbps: u32) {
        let quality = MediaQuality::from_ffprobe_info(None, video_json);
        let (resolution, codec) =
            quality.map_or((VideoResolution::default(), VideoCodec::default()), |q| (q.resolution, q.video_codec));

        self.probe.rcu(|current| {
            let mut map = (**current).clone();
            let (audio_codec, audio_channels, audio_language) =
                map.get(key).map_or((AudioCodec::default(), AudioChannels::default(), None), |p| {
                    (p.audio_codec.clone(), p.audio_channels.clone(), p.audio_language.clone())
                });
            map.insert(
                key.clone(),
                ProbePart {
                    resolution: resolution.clone(),
                    codec: codec.clone(),
                    bitrate_kbps,
                    audio_codec,
                    audio_channels,
                    audio_language,
                },
            );
            Arc::new(map)
        });
    }

    pub fn patch_probe_audio(&self, key: &MemberKey, audio_json: Option<&str>) {
        let quality = MediaQuality::from_ffprobe_info(audio_json, None);
        let (audio_codec, audio_channels) =
            quality.map_or((AudioCodec::default(), AudioChannels::default()), |q| (q.audio_codec, q.audio_channels));

        let audio_language = audio_json.and_then(|raw| {
            let val = serde_json::from_str::<serde_json::Value>(raw).ok()?;
            let obj = match &val {
                serde_json::Value::Object(map) => Some(map),
                serde_json::Value::Array(arr) => arr.first().and_then(serde_json::Value::as_object),
                _ => None,
            }?;
            let lang = obj
                .get("tags")
                .and_then(|t| t.get("language").or_else(|| t.get("LANGUAGE")))
                .or_else(|| obj.get("language"))
                .and_then(|l| l.as_str())?;
            let lower = lang.trim().to_lowercase();
            if lower.is_empty() || lower == "und" {
                None
            } else {
                Some(Arc::<str>::from(lower))
            }
        });

        self.probe.rcu(|current| {
            let mut map = (**current).clone();
            let (resolution, codec, bitrate_kbps) =
                map.get(key).map_or((VideoResolution::default(), VideoCodec::default(), 0), |p| {
                    (p.resolution.clone(), p.codec.clone(), p.bitrate_kbps)
                });
            map.insert(
                key.clone(),
                ProbePart {
                    resolution,
                    codec,
                    bitrate_kbps,
                    audio_codec: audio_codec.clone(),
                    audio_channels: audio_channels.clone(),
                    audio_language: audio_language.clone(),
                },
            );
            Arc::new(map)
        });
    }

    pub fn admit(&self, key: &MemberKey, now: Instant) -> CircuitAdmission {
        let mut acquired_input_half_open = false;

        // 1. Check input circuit first
        if let Some(mut input_entry) = self.shared.input_circuits.get_mut(&key.0) {
            match input_entry.circuit.state {
                CircuitState::Closed => {}
                CircuitState::OpenUntil(until) => {
                    if now < until {
                        return CircuitAdmission::Open;
                    }
                    input_entry.circuit.state = CircuitState::HalfOpenInFlight;
                    acquired_input_half_open = true;
                }
                CircuitState::HalfOpenInFlight => {
                    return CircuitAdmission::Open;
                }
            }
        }

        // 2. Check member circuit
        let mut revert_input = false;
        let admission = match self.shared.circuits.get_mut(key) {
            None => {
                if acquired_input_half_open {
                    CircuitAdmission::HalfOpen(HalfOpenGuard {
                        member_key: (Arc::clone(&key.0), Arc::clone(&key.1)),
                        input_name: Some(Arc::clone(&key.0)),
                        admit_time: now,
                        shared: Arc::clone(&self.shared),
                        consumed: false,
                    })
                } else {
                    CircuitAdmission::Closed
                }
            }
            Some(mut member_entry) => match member_entry.state {
                CircuitState::Closed => {
                    if acquired_input_half_open {
                        CircuitAdmission::HalfOpen(HalfOpenGuard {
                            member_key: (Arc::clone(&key.0), Arc::clone(&key.1)),
                            input_name: Some(Arc::clone(&key.0)),
                            admit_time: now,
                            shared: Arc::clone(&self.shared),
                            consumed: false,
                        })
                    } else {
                        CircuitAdmission::Closed
                    }
                }
                CircuitState::OpenUntil(until) => {
                    if now < until {
                        revert_input = true;
                        CircuitAdmission::Open
                    } else {
                        member_entry.state = CircuitState::HalfOpenInFlight;
                        self.emit_circuit_changed(
                            Arc::clone(&key.0),
                            Arc::clone(&key.1),
                            CircuitState::HalfOpenInFlight,
                        );
                        CircuitAdmission::HalfOpen(HalfOpenGuard {
                            member_key: (Arc::clone(&key.0), Arc::clone(&key.1)),
                            input_name: if acquired_input_half_open { Some(Arc::clone(&key.0)) } else { None },
                            admit_time: now,
                            shared: Arc::clone(&self.shared),
                            consumed: false,
                        })
                    }
                }
                CircuitState::HalfOpenInFlight => {
                    revert_input = true;
                    CircuitAdmission::Open
                }
            },
        };

        if revert_input && acquired_input_half_open {
            if let Some(mut input_entry) = self.shared.input_circuits.get_mut(&key.0) {
                if input_entry.circuit.state == CircuitState::HalfOpenInFlight {
                    input_entry.circuit.state = CircuitState::OpenUntil(now);
                }
            }
        }

        admission
    }

    pub fn record_failure(&self, key: &MemberKey, now: Instant) {
        self.record_classified_failure(key, FailureClass::Stream, now);
    }

    pub fn record_classified_failure(&self, key: &MemberKey, class: FailureClass, now: Instant) {
        let policy = self.policy.load();
        if class.opens_member_circuit() {
            let mut entry = self.shared.circuits.entry(key.clone()).or_default();
            entry.failures = entry.failures.saturating_add(1);
            if entry.failures >= policy.failures && entry.state == CircuitState::Closed {
                let cooldown = compute_cooldown(&policy, entry.open_level);
                entry.state = CircuitState::OpenUntil(now + cooldown);
                entry.open_level = entry.open_level.saturating_add(1);
                self.emit_circuit_changed(Arc::clone(&key.0), Arc::clone(&key.1), entry.state);
            }
        }
        if class.counts_for_input() {
            record_input_failure_in_shared(&self.shared, key, now, &policy);
        }
    }

    pub fn record_success(&self, key: &MemberKey) {
        if let Some(mut entry) = self.shared.circuits.get_mut(key) {
            let was_open = entry.state != CircuitState::Closed;
            entry.failures = 0;
            entry.open_level = 0;
            entry.state = CircuitState::Closed;
            if was_open {
                self.emit_circuit_changed(Arc::clone(&key.0), Arc::clone(&key.1), CircuitState::Closed);
            }
        }
    }

    pub fn record_failure_with(&self, guard: HalfOpenGuard, now: Instant) {
        self.record_classified_failure_with(guard, FailureClass::Stream, now);
    }

    pub fn record_classified_failure_with(&self, mut guard: HalfOpenGuard, class: FailureClass, now: Instant) {
        guard.consumed = true;
        let policy = self.policy.load();
        if class.opens_member_circuit() {
            let mut entry = self.shared.circuits.entry(guard.member_key.clone()).or_default();
            let cooldown = compute_cooldown(&policy, entry.open_level);
            entry.state = CircuitState::OpenUntil(now + cooldown);
            entry.open_level = entry.open_level.saturating_add(1);
            self.emit_circuit_changed(Arc::clone(&guard.member_key.0), Arc::clone(&guard.member_key.1), entry.state);
        }
        if let Some(input) = &guard.input_name {
            if class.counts_for_input() {
                let mut entry = self.shared.input_circuits.entry(Arc::clone(input)).or_default();
                let cooldown = compute_cooldown(&policy, entry.circuit.open_level);
                entry.circuit.state = CircuitState::OpenUntil(now + cooldown);
                entry.circuit.open_level = entry.circuit.open_level.saturating_add(1);
            }
        }
        if class.counts_for_input() {
            record_input_failure_in_shared(&self.shared, &guard.member_key, now, &policy);
        }
    }

    pub fn record_success_with(&self, mut guard: HalfOpenGuard) {
        guard.consumed = true;
        if let Some(mut entry) = self.shared.circuits.get_mut(&guard.member_key) {
            let was_open = entry.state != CircuitState::Closed;
            entry.failures = 0;
            entry.open_level = 0;
            entry.state = CircuitState::Closed;
            if was_open {
                self.emit_circuit_changed(
                    Arc::clone(&guard.member_key.0),
                    Arc::clone(&guard.member_key.1),
                    CircuitState::Closed,
                );
            }
        }
        if let Some(input) = &guard.input_name {
            if let Some(mut entry) = self.shared.input_circuits.get_mut(input) {
                entry.circuit.failures = 0;
                entry.circuit.open_level = 0;
                entry.circuit.state = CircuitState::Closed;
                entry.recent.clear();
            }
        }
    }

    pub fn circuit_view(&self, key: &MemberKey, now: Instant) -> CircuitView {
        if let Some(input_entry) = self.shared.input_circuits.get(&key.0) {
            match input_entry.circuit.state {
                CircuitState::OpenUntil(until) => {
                    if now < until {
                        return CircuitView::Open { remaining: until.saturating_duration_since(now) };
                    }
                    return CircuitView::Open { remaining: Duration::ZERO };
                }
                CircuitState::HalfOpenInFlight => {
                    return CircuitView::HalfOpenInFlight;
                }
                CircuitState::Closed => {}
            }
        }

        let Some(entry) = self.shared.circuits.get(key) else {
            return CircuitView::Closed;
        };
        match entry.state {
            CircuitState::Closed => {
                if entry.failures > 0 {
                    CircuitView::Counting(entry.failures)
                } else {
                    CircuitView::Closed
                }
            }
            CircuitState::OpenUntil(until) => {
                if now < until {
                    CircuitView::Open { remaining: until.saturating_duration_since(now) }
                } else {
                    CircuitView::Open { remaining: Duration::ZERO }
                }
            }
            CircuitState::HalfOpenInFlight => CircuitView::HalfOpenInFlight,
        }
    }

    pub fn input_circuit_view(&self, input_name: &str, now: Instant) -> CircuitView {
        let Some(entry) = self.shared.input_circuits.get(input_name) else {
            return CircuitView::Closed;
        };
        match entry.circuit.state {
            CircuitState::Closed => {
                let policy = self.policy.load();
                let window = policy.input_window;
                let mut distinct = std::collections::HashSet::new();
                let mut count = 0usize;
                for (k, ts) in &entry.recent {
                    let in_window = if *ts <= now { now.duration_since(*ts) <= window } else { true };
                    if in_window {
                        count += 1;
                        distinct.insert(&k.1);
                    }
                }
                if distinct.len() >= 2 && count > 0 {
                    CircuitView::Counting(u8::try_from(count).unwrap_or(u8::MAX))
                } else {
                    CircuitView::Closed
                }
            }
            CircuitState::OpenUntil(until) => {
                if now < until {
                    CircuitView::Open { remaining: until.saturating_duration_since(now) }
                } else {
                    CircuitView::Open { remaining: Duration::ZERO }
                }
            }
            CircuitState::HalfOpenInFlight => CircuitView::HalfOpenInFlight,
        }
    }

    pub fn set_policy(&self, policy: CircuitPolicy) { self.policy.store(Arc::new(policy)); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::{PlaylistItemType, VirtualId, XtreamCluster};

    fn qos(score_24h: u8, score_7d: u8, confidence: u8) -> QosPart {
        QosPart { score_24h, score_7d, confidence, failure_streak: 0 }
    }

    fn test_member(input: &str, stream_id: &str, provider_id: u32, member_vid: u32) -> GroupMember {
        GroupMember {
            member_vid: VirtualId::new(member_vid),
            input_name: input.into(),
            provider_id,
            input_stream_id: stream_id.into(),
            url: format!("http://{input}.invalid/{stream_id}.ts").into(),
            item_type: PlaylistItemType::Live,
            cluster: XtreamCluster::Live,
            title: "ZDF".into(),
            archive: None,
            epg_channel_id: None,
            additional_properties: None,
        }
    }

    #[test]
    fn qos_lookup_uses_provider_id_or_target_member() {
        let index = MemberHealthIndex::new(CircuitPolicy::default());
        let target: Arc<str> = "Main".into();
        let xtream = test_member("solis", "1", 1, 101);
        let m3u = test_member("m3u-in", "chan-a", 0, 202);
        index.replace_qos([
            (QosKey::Provider { input: "solis".into(), provider_id: 1 }, qos(80, 70, 20)),
            (QosKey::Provider { input: "solis".into(), provider_id: 1 }, qos(40, 50, 90)),
            (QosKey::TargetMember { target: Arc::clone(&target), member_vid: 202 }, qos(66, 66, 50)),
        ]);
        let now = Instant::now();
        assert_eq!(index.health(&target, &xtream, now).qos.map(|q| q.score_24h), Some(40));
        assert_eq!(index.health(&target, &m3u, now).qos.map(|q| q.score_24h), Some(66));
    }

    #[test]
    fn probe_patch_keeps_qos() {
        let index = MemberHealthIndex::new(CircuitPolicy::default());
        let target: Arc<str> = "Main".into();
        let member = test_member("solis", "1", 1, 101);
        index.replace_qos([(QosKey::Provider { input: "solis".into(), provider_id: 1 }, qos(40, 50, 90))]);
        index.patch_probe(&member_key(&member), Some(r#"{"codec_name":"hevc","width":3840,"height":2160}"#), 12_000);
        let health = index.health(&target, &member, Instant::now());
        assert_eq!(health.qos.map(|q| q.score_24h), Some(40));
        let probe = health.probe.expect("probe");
        assert_eq!(probe.resolution, VideoResolution::P2160);
        assert_eq!(probe.codec, VideoCodec::H265);
        assert_eq!(probe.bitrate_kbps, 12_000);
    }

    #[test]
    fn audio_patch_sets_codec_channels_and_language() {
        let index = MemberHealthIndex::new(CircuitPolicy::default());
        let target: Arc<str> = "Main".into();
        let member = test_member("solis", "1", 1, 101);
        index.patch_probe(&member_key(&member), Some(r#"{"codec_name":"h264","width":1920,"height":1080}"#), 5_000);
        index.patch_probe_audio(
            &member_key(&member),
            Some(r#"[{"codec_name":"ac3","channels":6,"tags":{"language":"DEU"}}]"#),
        );
        let probe = index.health(&target, &member, Instant::now()).probe.expect("probe");
        assert_eq!(probe.codec, VideoCodec::H264);
        assert_eq!(probe.audio_codec, AudioCodec::AC3);
        assert_eq!(probe.audio_channels, AudioChannels::Surround51);
        assert_eq!(probe.audio_language.as_deref(), Some("deu"));
    }
}

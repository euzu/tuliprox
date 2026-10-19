//! Pure failover group resolution over catalog entries.
//!
//! Processing feeds it the playlist, the REST preview feeds it the persisted catalog; both get the same
//! groups because this is the only place that decides which channel ends up in which group.

use indexmap::IndexMap;
use shared::{
    model::{
        AutoGroupOverrideDto, ChannelMemberRefDto, EpgConflictMode, FailoverGroupPolicy, FailoverGroupsConfig,
        FailoverOverrideDiagnosticsDto, InactiveManualGroupDto, TargetFailoverOverridesDto, UUIDType, UnresolvedRefDto,
        MIN_GROUP_MEMBERS,
    },
    utils::hash_string,
};
use std::collections::{HashMap, HashSet};

/// One eligible live channel as the resolution sees it.
#[derive(Debug, Clone, Copy)]
pub struct CatalogEntry<'a> {
    pub member_ref: &'a ChannelMemberRefDto,
    /// `None`: the channel opted out of automatic grouping (DSL `@failover_group = ""`) or has no usable key.
    pub natural_key: Option<&'a str>,
    pub dsl_forced: bool,
    pub epg_channel_id: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateOrigin {
    Manual,
    Auto,
    AutoEdited,
}

#[derive(Debug, Clone)]
pub struct ResolvedCandidate {
    /// Manual group id or auto key (including the EPG suffix of split buckets).
    pub key: String,
    pub origin: CandidateOrigin,
    pub policy: FailoverGroupPolicy,
    /// Indices into the catalog in member order.
    pub entries: Vec<usize>,
    pub unresolved: Vec<ChannelMemberRefDto>,
    pub epg_split: bool,
    pub name: Option<String>,
    pub logo: Option<String>,
    pub epg_channel_id: Option<String>,
    pub playlist_group: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct FailoverGroupResolution {
    pub candidates: Vec<ResolvedCandidate>,
    pub diagnostics: FailoverOverrideDiagnosticsDto,
}

/// Manual ids and auto keys live in separate namespaces so `zdf` can exist in both.
pub fn group_uuid(target: &str, origin: CandidateOrigin, key: &str) -> UUIDType {
    let namespace = match origin {
        CandidateOrigin::Manual => "manual",
        CandidateOrigin::Auto | CandidateOrigin::AutoEdited => "auto",
    };
    hash_string(&format!("failover-group|{target}|{namespace}|{key}"))
}

struct Bucket {
    entries: Vec<usize>,
    has_dsl_forced: bool,
    epg_split: bool,
}

type MemberIndex<'a> = HashMap<&'a ChannelMemberRefDto, usize>;

fn natural_buckets(
    config: &FailoverGroupsConfig,
    catalog: &[CatalogEntry<'_>],
    include: impl Fn(usize) -> bool,
) -> IndexMap<String, Bucket> {
    let mut buckets: IndexMap<String, Bucket> = IndexMap::new();
    for (idx, entry) in catalog.iter().enumerate() {
        let Some(key) = entry.natural_key.filter(|key| !key.trim().is_empty()) else {
            continue;
        };
        if !include(idx) {
            continue;
        }
        let slot = match buckets.get_index_of(key) {
            Some(slot) => slot,
            None => {
                buckets
                    .insert_full(
                        key.to_string(),
                        Bucket { entries: Vec::new(), has_dsl_forced: false, epg_split: false },
                    )
                    .0
            }
        };
        let bucket = &mut buckets[slot];
        bucket.entries.push(idx);
        bucket.has_dsl_forced |= entry.dsl_forced;
    }

    if config.epg_conflict == EpgConflictMode::Split {
        split_epg_buckets(catalog, buckets)
    } else {
        buckets
    }
}

fn split_epg_buckets(catalog: &[CatalogEntry<'_>], buckets: IndexMap<String, Bucket>) -> IndexMap<String, Bucket> {
    let mut split: IndexMap<String, Bucket> = IndexMap::with_capacity(buckets.len());
    for (key, bucket) in buckets {
        if bucket.has_dsl_forced {
            split.insert(key, bucket);
            continue;
        }

        let mut by_epg: IndexMap<String, Vec<usize>> = IndexMap::new();
        for &idx in &bucket.entries {
            if let Some(epg) = catalog[idx].epg_channel_id {
                let normalized = epg.trim().to_lowercase();
                if !normalized.is_empty() {
                    by_epg.entry(normalized).or_default().push(idx);
                }
            }
        }

        if by_epg.len() >= 2 {
            for (epg_id, entries) in by_epg {
                split.insert(format!("{key}|{epg_id}"), Bucket { entries, has_dsl_forced: false, epg_split: true });
            }
        } else {
            split.insert(key, bucket);
        }
    }
    split
}

/// Rule 1: manual groups claim their refs, first claim wins. A group with too few resolvable refs is
/// inactive and releases its claims again.
fn resolve_manual_groups(
    config: &FailoverGroupsConfig,
    overrides: &TargetFailoverOverridesDto,
    index: &MemberIndex<'_>,
    taken: &mut [bool],
    out: &mut FailoverGroupResolution,
) {
    for group in &overrides.groups {
        let mut matched = Vec::with_capacity(group.members.len());
        let mut unresolved = Vec::new();
        for member in &group.members {
            match index.get(member) {
                Some(&idx) if !taken[idx] => {
                    taken[idx] = true;
                    matched.push(idx);
                }
                _ => unresolved.push(member.clone()),
            }
        }

        out.diagnostics.unresolved_refs.extend(
            unresolved.iter().map(|member| UnresolvedRefDto { group: group.id.clone(), member_ref: member.clone() }),
        );

        if matched.len() < MIN_GROUP_MEMBERS {
            for &idx in &matched {
                taken[idx] = false;
            }
            out.diagnostics
                .inactive_manual_groups
                .push(InactiveManualGroupDto { id: group.id.clone(), resolved: matched.len() });
            continue;
        }

        out.candidates.push(ResolvedCandidate {
            key: group.id.clone(),
            origin: CandidateOrigin::Manual,
            policy: group.policy.unwrap_or(config.policy),
            entries: matched,
            unresolved,
            epg_split: false,
            name: Some(group.name.clone()),
            logo: group.logo.clone(),
            epg_channel_id: group.epg_channel_id.clone(),
            playlist_group: group.playlist_group.clone(),
        });
    }
}

/// Exclusive `add` claims of the auto overrides (rule 2). Claims are the entries an override pulled out of
/// their natural bucket, in `add` order, indexed by override.
struct AddClaims {
    claimed: Vec<bool>,
    per_override: Vec<Vec<usize>>,
}

impl AddClaims {
    fn release(&mut self, override_idx: usize) {
        for idx in self.per_override[override_idx].drain(..) {
            self.claimed[idx] = false;
        }
    }
}

struct AutoContext<'a, 'b> {
    config: &'a FailoverGroupsConfig,
    catalog: &'a [CatalogEntry<'b>],
    index: &'a MemberIndex<'b>,
    overrides: &'a [AutoGroupOverrideDto],
    /// First override per auto key; later duplicates are ignored (validation flags them).
    by_key: HashMap<&'a str, usize>,
}

impl AutoContext<'_, '_> {
    fn is_effective(&self, override_idx: usize) -> bool {
        self.by_key.get(self.overrides[override_idx].auto_key.as_str()) == Some(&override_idx)
    }

    /// Claims every `add` ref of an override whose bucket exists, before buckets are built, so the result does
    /// not depend on bucket iteration order. Overrides without a natural bucket are stale and claim nothing.
    fn claim_adds(
        &self,
        taken: &[bool],
        excluded: &HashSet<&ChannelMemberRefDto>,
        diagnostics: &mut FailoverOverrideDiagnosticsDto,
    ) -> AddClaims {
        let mut claims = AddClaims {
            claimed: vec![false; self.catalog.len()],
            per_override: vec![Vec::new(); self.overrides.len()],
        };
        if self.overrides.iter().all(|ao| ao.disabled || ao.add.is_empty()) {
            return claims;
        }

        let existing = natural_buckets(self.config, self.catalog, |idx| {
            !taken[idx] && !excluded.contains(self.catalog[idx].member_ref)
        });
        for (override_idx, ao) in self.overrides.iter().enumerate() {
            if ao.disabled || !self.is_effective(override_idx) || !existing.contains_key(ao.auto_key.as_str()) {
                continue;
            }
            for add_ref in &ao.add {
                let Some(&idx) = self.index.get(add_ref) else {
                    continue;
                };
                if taken[idx] || claims.claimed[idx] {
                    if !diagnostics.conflicting_refs.contains(add_ref)
                        && !claims.per_override[override_idx].contains(&idx)
                    {
                        diagnostics.conflicting_refs.push(add_ref.clone());
                    }
                    continue;
                }
                claims.claimed[idx] = true;
                claims.per_override[override_idx].push(idx);
            }
        }
        claims
    }

    /// Natural buckets without claimed entries. A claim is released, and its channels return to their natural
    /// buckets, when its override can not materialize a group: the target bucket vanished (all its natural
    /// members were claimed away) or the resulting group has fewer than `MIN_GROUP_MEMBERS` members. Releasing
    /// can grow other buckets, so one override is released at a time (first in override order) until stable.
    fn final_buckets(
        &self,
        taken: &[bool],
        excluded: &HashSet<&ChannelMemberRefDto>,
        claims: &mut AddClaims,
    ) -> IndexMap<String, Bucket> {
        loop {
            let buckets = natural_buckets(self.config, self.catalog, |idx| {
                !taken[idx] && !claims.claimed[idx] && !excluded.contains(self.catalog[idx].member_ref)
            });
            let unproductive = self.overrides.iter().enumerate().find(|&(override_idx, ao)| {
                let claimed = &claims.per_override[override_idx];
                !claimed.is_empty()
                    && buckets.get(ao.auto_key.as_str()).is_none_or(|bucket| {
                        self.apply_override(bucket.entries.clone(), ao, claimed).len() < MIN_GROUP_MEMBERS
                    })
            });
            let Some((override_idx, _)) = unproductive else {
                return buckets;
            };
            claims.release(override_idx);
        }
    }

    fn unresolved_refs(&self, ao: &AutoGroupOverrideDto) -> Vec<ChannelMemberRefDto> {
        let mut unresolved: Vec<ChannelMemberRefDto> = Vec::new();
        for member in ao.order.iter().chain(&ao.add).chain(&ao.remove) {
            if !self.index.contains_key(member) && !unresolved.contains(member) {
                unresolved.push(member.clone());
            }
        }
        unresolved
    }

    /// Rule 5: `remove`, claimed `add`s, then `order`; members missing from `order` follow in catalog order.
    fn apply_override(&self, mut members: Vec<usize>, ao: &AutoGroupOverrideDto, claimed: &[usize]) -> Vec<usize> {
        if !ao.remove.is_empty() {
            let removed: HashSet<&ChannelMemberRefDto> = ao.remove.iter().collect();
            members.retain(|&idx| !removed.contains(self.catalog[idx].member_ref));
        }
        members.extend_from_slice(claimed);

        if ao.order.is_empty() {
            return members;
        }
        let present: HashSet<usize> = members.iter().copied().collect();
        let mut ordered = Vec::with_capacity(members.len());
        let mut used = HashSet::with_capacity(members.len());
        for ord_ref in &ao.order {
            if let Some(&idx) = self.index.get(ord_ref) {
                if present.contains(&idx) && used.insert(idx) {
                    ordered.push(idx);
                }
            }
        }
        ordered.extend(members.iter().copied().filter(|idx| !used.contains(idx)));
        ordered
    }
}

fn resolve_auto_groups(
    config: &FailoverGroupsConfig,
    overrides: Option<&TargetFailoverOverridesDto>,
    catalog: &[CatalogEntry<'_>],
    index: &MemberIndex<'_>,
    taken: &[bool],
    out: &mut FailoverGroupResolution,
) {
    let auto_overrides: &[AutoGroupOverrideDto] = overrides.map_or(&[], |o| &o.auto_overrides);
    let mut by_key = HashMap::with_capacity(auto_overrides.len());
    for (idx, ao) in auto_overrides.iter().enumerate() {
        by_key.entry(ao.auto_key.as_str()).or_insert(idx);
    }
    let ctx = AutoContext { config, catalog, index, overrides: auto_overrides, by_key };
    let excluded: HashSet<&ChannelMemberRefDto> = overrides.map_or_else(HashSet::new, |o| o.excluded.iter().collect());

    let mut claims = ctx.claim_adds(taken, &excluded, &mut out.diagnostics);
    let buckets = ctx.final_buckets(taken, &excluded, &mut claims);

    out.diagnostics.stale_auto_overrides.extend(
        auto_overrides
            .iter()
            .enumerate()
            .filter(|(idx, ao)| ctx.is_effective(*idx) && !buckets.contains_key(ao.auto_key.as_str()))
            .map(|(_, ao)| ao.auto_key.clone()),
    );

    for (key, bucket) in buckets {
        let override_idx = ctx.by_key.get(key.as_str()).copied();
        let ao = override_idx.map(|idx| &auto_overrides[idx]);
        if ao.is_some_and(|ao| ao.disabled) {
            out.diagnostics.dissolved.push(key);
            continue;
        }

        let (entries, unresolved) = match (override_idx, ao) {
            (Some(override_idx), Some(ao)) => {
                let unresolved = ctx.unresolved_refs(ao);
                out.diagnostics.unresolved_refs.extend(
                    unresolved.iter().map(|member| UnresolvedRefDto { group: key.clone(), member_ref: member.clone() }),
                );
                (ctx.apply_override(bucket.entries, ao, &claims.per_override[override_idx]), unresolved)
            }
            _ => (bucket.entries, Vec::new()),
        };
        if entries.len() < MIN_GROUP_MEMBERS {
            continue;
        }

        out.candidates.push(ResolvedCandidate {
            key,
            origin: if ao.is_some() { CandidateOrigin::AutoEdited } else { CandidateOrigin::Auto },
            policy: ao.and_then(|ao| ao.policy).unwrap_or(config.policy),
            entries,
            unresolved,
            epg_split: bucket.epg_split,
            name: ao.and_then(|ao| ao.name.clone()),
            logo: ao.and_then(|ao| ao.logo.clone()),
            epg_channel_id: ao.and_then(|ao| ao.epg_channel_id.clone()),
            playlist_group: ao.and_then(|ao| ao.playlist_group.clone()),
        });
    }
}

/// Resolves manual and automatic failover groups. Precedence for a channel: manual group, auto override `add`,
/// `excluded`, natural auto bucket; a channel is a member of at most one group.
pub fn resolve_failover_groups(
    config: &FailoverGroupsConfig,
    overrides: Option<&TargetFailoverOverridesDto>,
    catalog: &[CatalogEntry<'_>],
) -> FailoverGroupResolution {
    let mut out = FailoverGroupResolution::default();
    let mut index: MemberIndex<'_> = HashMap::with_capacity(catalog.len());
    for (idx, entry) in catalog.iter().enumerate() {
        index.entry(entry.member_ref).or_insert(idx);
    }
    let mut taken = vec![false; catalog.len()];

    if let Some(overrides) = overrides {
        resolve_manual_groups(config, overrides, &index, &mut taken, &mut out);
    }
    resolve_auto_groups(config, overrides, catalog, &index, &taken, &mut out);
    out
}

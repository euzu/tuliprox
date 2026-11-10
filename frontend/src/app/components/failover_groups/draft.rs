//! Pure edit operations on the failover override section of one target (spec 8.3).
//!
//! Every operation changes the section only, is idempotent and keeps the document minimal: override entries that
//! became empty are removed again.

use shared::model::{
    AutoGroupOverrideDto, ChannelMemberRefDto, FailoverGroupPolicy, ManualFailoverGroupDto, TargetFailoverOverridesDto,
};

const ID_SLUG_MAX_LEN: usize = 59;
const ID_SUFFIX_LEN: usize = 4;
const ID_SLUG_FALLBACK: &str = "group";
const ID_SUFFIX_ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";

/// Identifies a group of the editor: an auto group by its auto key, a manual group by its id.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GroupRef {
    Auto(String),
    Manual(String),
}

/// New metadata of a group. Blank values clear the override; for a manual group a missing or blank `name` keeps
/// the current name because a manual group always needs one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupMetaPatch {
    pub name: Option<String>,
    pub logo: Option<String>,
    pub epg_channel_id: Option<String>,
    pub playlist_group: Option<String>,
}

/// One user action on the draft. `Edit(DraftOp)` of the editor state applies it as a single undo step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DraftOp {
    Reorder {
        group: GroupRef,
        from: usize,
        to: usize,
        current: Vec<ChannelMemberRefDto>,
    },
    MoveFirst {
        group: GroupRef,
        index: usize,
        current: Vec<ChannelMemberRefDto>,
    },
    /// Puts several channels into `group` as one step; each is paired with the group that holds it now, if any.
    AddMembers {
        group: GroupRef,
        members: Vec<(ChannelMemberRefDto, Option<GroupRef>)>,
    },
    RemoveMember {
        group: GroupRef,
        member: ChannelMemberRefDto,
    },
    MoveMember {
        from: GroupRef,
        to: GroupRef,
        member: ChannelMemberRefDto,
    },
    ExcludeMember {
        group: Option<GroupRef>,
        member: ChannelMemberRefDto,
    },
    /// Deletes a reference from the group's entry without recording a correction (problems view).
    ForgetRef {
        group: GroupRef,
        member: ChannelMemberRefDto,
    },
    SetMeta {
        group: GroupRef,
        patch: GroupMetaPatch,
    },
    SetPolicy {
        group: GroupRef,
        policy: Option<FailoverGroupPolicy>,
    },
    Dissolve {
        group: GroupRef,
    },
    ResetAuto {
        auto_key: String,
    },
    /// New manual group; every member is first taken out of the group it currently belongs to.
    NewGroupFromSelection {
        name: String,
        members: Vec<(ChannelMemberRefDto, Option<GroupRef>)>,
        id_suffix: String,
    },
    ConvertStaleToManual {
        auto_key: String,
        id_suffix: String,
    },
}

impl DraftOp {
    /// Applies the operation and returns the group that was created by it, if any.
    pub fn apply(&self, section: &mut TargetFailoverOverridesDto) -> Option<GroupRef> {
        match self {
            Self::Reorder { group, from, to, current } => reorder(section, group, *from, *to, current),
            Self::MoveFirst { group, index, current } => move_first(section, group, *index, current),
            Self::AddMembers { group, members } => add_members(section, group, members),
            Self::RemoveMember { group, member } => remove_member(section, group, member),
            Self::MoveMember { from, to, member } => move_member(section, from, to, member),
            Self::ExcludeMember { group, member } => exclude_member(section, group.as_ref(), member),
            Self::ForgetRef { group, member } => forget_ref(section, group, member),
            Self::SetMeta { group, patch } => set_meta(section, group, patch.clone()),
            Self::SetPolicy { group, policy } => set_policy(section, group, *policy),
            Self::Dissolve { group } => dissolve(section, group),
            Self::ResetAuto { auto_key } => reset_auto(section, auto_key),
            Self::NewGroupFromSelection { name, members, id_suffix } => {
                for (member, source) in members {
                    if let Some(source) = source {
                        remove_member(section, source, member);
                    }
                }
                let refs: Vec<ChannelMemberRefDto> = members.iter().map(|(member, _)| member.clone()).collect();
                let id = create_manual_group(section, name, &refs, id_suffix);
                return Some(GroupRef::Manual(id));
            }
            Self::ConvertStaleToManual { auto_key, id_suffix } => {
                return convert_stale_to_manual(section, auto_key, id_suffix).map(GroupRef::Manual);
            }
        }
        None
    }
}

fn take_ref(list: &mut Vec<ChannelMemberRefDto>, member: &ChannelMemberRefDto) -> bool {
    match list.iter().position(|entry| entry == member) {
        Some(index) => {
            list.remove(index);
            true
        }
        None => false,
    }
}

fn push_ref(list: &mut Vec<ChannelMemberRefDto>, member: &ChannelMemberRefDto) {
    if !list.contains(member) {
        list.push(member.clone());
    }
}

fn normalize(value: Option<String>) -> Option<String> {
    value.map(|text| text.trim().to_string()).filter(|text| !text.is_empty())
}

fn is_blank_override(entry: &AutoGroupOverrideDto) -> bool {
    !entry.disabled
        && entry.policy.is_none()
        && entry.order.is_empty()
        && entry.add.is_empty()
        && entry.remove.is_empty()
        && entry.name.is_none()
        && entry.logo.is_none()
        && entry.epg_channel_id.is_none()
        && entry.playlist_group.is_none()
}

/// Runs `edit` on the override of `auto_key`, creating it on demand and removing it again when it ends up empty.
fn edit_auto<R>(
    section: &mut TargetFailoverOverridesDto,
    auto_key: &str,
    edit: impl FnOnce(&mut AutoGroupOverrideDto) -> R,
) -> R {
    let index = section.auto_overrides.iter().position(|entry| entry.auto_key == auto_key).unwrap_or_else(|| {
        section.auto_overrides.push(AutoGroupOverrideDto { auto_key: auto_key.to_string(), ..Default::default() });
        section.auto_overrides.len() - 1
    });
    let result = edit(&mut section.auto_overrides[index]);
    if is_blank_override(&section.auto_overrides[index]) {
        section.auto_overrides.remove(index);
    }
    result
}

fn manual_mut<'a>(section: &'a mut TargetFailoverOverridesDto, id: &str) -> Option<&'a mut ManualFailoverGroupDto> {
    section.groups.iter_mut().find(|group| group.id == id)
}

/// `current` with the entry at `from` moved to `to`; `None` when nothing would change.
fn moved(current: &[ChannelMemberRefDto], from: usize, to: usize) -> Option<Vec<ChannelMemberRefDto>> {
    if from == to || from >= current.len() || to >= current.len() {
        return None;
    }
    let mut order = current.to_vec();
    let item = order.remove(from);
    order.insert(to, item);
    Some(order)
}

/// Moves the member at `from` to `to` in the effective member list `current`.
/// An auto group stores the full resulting order, a manual group sorts its members accordingly; members of a
/// manual group that are not in `current` (unresolved references) keep their relative place at the end.
pub fn reorder(
    section: &mut TargetFailoverOverridesDto,
    group: &GroupRef,
    from: usize,
    to: usize,
    current: &[ChannelMemberRefDto],
) {
    let Some(order) = moved(current, from, to) else { return };
    match group {
        GroupRef::Auto(key) => edit_auto(section, key, |entry| entry.order = order),
        GroupRef::Manual(id) => {
            if let Some(group) = manual_mut(section, id) {
                group
                    .members
                    .sort_by_key(|member| order.iter().position(|entry| entry == member).unwrap_or(usize::MAX));
            }
        }
    }
}

pub fn move_first(
    section: &mut TargetFailoverOverridesDto,
    group: &GroupRef,
    index: usize,
    current: &[ChannelMemberRefDto],
) {
    reorder(section, group, index, 0, current);
}

/// Adds a channel to the group. For an auto group a channel that was removed from it is only taken out of
/// `remove`. An excluded channel is included again because an excluded channel cannot belong to a group.
pub fn add_member(section: &mut TargetFailoverOverridesDto, group: &GroupRef, member: &ChannelMemberRefDto) {
    match group {
        GroupRef::Auto(key) => edit_auto(section, key, |entry| {
            if !take_ref(&mut entry.remove, member) {
                push_ref(&mut entry.add, member);
            }
        }),
        GroupRef::Manual(id) => {
            let Some(group) = manual_mut(section, id) else { return };
            push_ref(&mut group.members, member);
        }
    }
    take_ref(&mut section.excluded, member);
}

/// Removes a channel from the group. An auto group records it in `remove`, unless it had been added to the group
/// explicitly, in which case dropping that addition is enough.
pub fn remove_member(section: &mut TargetFailoverOverridesDto, group: &GroupRef, member: &ChannelMemberRefDto) {
    match group {
        GroupRef::Auto(key) => edit_auto(section, key, |entry| {
            take_ref(&mut entry.order, member);
            if !take_ref(&mut entry.add, member) {
                push_ref(&mut entry.remove, member);
            }
        }),
        GroupRef::Manual(id) => {
            if let Some(group) = manual_mut(section, id) {
                take_ref(&mut group.members, member);
            }
        }
    }
}

pub fn move_member(
    section: &mut TargetFailoverOverridesDto,
    from: &GroupRef,
    to: &GroupRef,
    member: &ChannelMemberRefDto,
) {
    if from == to {
        return;
    }
    remove_member(section, from, member);
    add_member(section, to, member);
}

/// Puts every channel into `group`: a channel that another group holds is moved, one that already is in `group`
/// stays untouched, all others are added.
pub fn add_members(
    section: &mut TargetFailoverOverridesDto,
    group: &GroupRef,
    members: &[(ChannelMemberRefDto, Option<GroupRef>)],
) {
    for (member, current) in members {
        match current {
            Some(current) if current == group => {}
            Some(current) => move_member(section, current, group, member),
            None => add_member(section, group, member),
        }
    }
}

/// Excludes a channel of the target. `group` is the group it currently belongs to, if any.
pub fn exclude_member(
    section: &mut TargetFailoverOverridesDto,
    group: Option<&GroupRef>,
    member: &ChannelMemberRefDto,
) {
    if let Some(group) = group {
        remove_member(section, group, member);
    }
    push_ref(&mut section.excluded, member);
}

/// Drops a reference from every list of the group's entry; unlike `remove_member` nothing is recorded.
pub fn forget_ref(section: &mut TargetFailoverOverridesDto, group: &GroupRef, member: &ChannelMemberRefDto) {
    match group {
        GroupRef::Auto(key) => {
            if section.auto_overrides.iter().any(|entry| entry.auto_key == *key) {
                edit_auto(section, key, |entry| {
                    take_ref(&mut entry.order, member);
                    take_ref(&mut entry.add, member);
                    take_ref(&mut entry.remove, member);
                });
            }
        }
        GroupRef::Manual(id) => {
            if let Some(group) = manual_mut(section, id) {
                take_ref(&mut group.members, member);
            }
        }
    }
}

pub fn set_meta(section: &mut TargetFailoverOverridesDto, group: &GroupRef, patch: GroupMetaPatch) {
    let GroupMetaPatch { name, logo, epg_channel_id, playlist_group } = patch;
    match group {
        GroupRef::Auto(key) => edit_auto(section, key, |entry| {
            entry.name = normalize(name);
            entry.logo = normalize(logo);
            entry.epg_channel_id = normalize(epg_channel_id);
            entry.playlist_group = normalize(playlist_group);
        }),
        GroupRef::Manual(id) => {
            let Some(group) = manual_mut(section, id) else { return };
            if let Some(name) = normalize(name) {
                group.name = name;
            }
            group.logo = normalize(logo);
            group.epg_channel_id = normalize(epg_channel_id);
            group.playlist_group = normalize(playlist_group);
        }
    }
}

/// Sets the policy of the group; `None` falls back to the policy of the target.
pub fn set_policy(section: &mut TargetFailoverOverridesDto, group: &GroupRef, policy: Option<FailoverGroupPolicy>) {
    match group {
        GroupRef::Auto(key) => edit_auto(section, key, |entry| entry.policy = policy),
        GroupRef::Manual(id) => {
            if let Some(group) = manual_mut(section, id) {
                group.policy = policy;
            }
        }
    }
}

/// Dissolves the group: an auto group is disabled, a manual group is deleted.
pub fn dissolve(section: &mut TargetFailoverOverridesDto, group: &GroupRef) {
    match group {
        GroupRef::Auto(key) => edit_auto(section, key, |entry| entry.disabled = true),
        GroupRef::Manual(id) => section.groups.retain(|group| group.id != *id),
    }
}

/// Deletes the override entry of an auto group.
pub fn reset_auto(section: &mut TargetFailoverOverridesDto, auto_key: &str) {
    section.auto_overrides.retain(|entry| entry.auto_key != auto_key);
}

/// Random suffix of a generated manual group id.
pub fn random_id_suffix() -> String {
    (0..ID_SUFFIX_LEN).map(|_| char::from(ID_SUFFIX_ALPHABET[fastrand::usize(..ID_SUFFIX_ALPHABET.len())])).collect()
}

/// Lowercase ascii alphanumerics of `name`, everything else collapsed to single dashes, at most 59 characters.
fn id_slug(name: &str) -> String {
    let mut slug = String::with_capacity(name.len().min(ID_SLUG_MAX_LEN));
    for ch in name.chars().map(|ch| ch.to_ascii_lowercase()) {
        if slug.len() >= ID_SLUG_MAX_LEN {
            break;
        }
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.truncate(slug.trim_end_matches('-').len());
    if slug.is_empty() {
        slug.push_str(ID_SLUG_FALLBACK);
    }
    slug
}

fn generate_manual_id(section: &TargetFailoverOverridesDto, name: &str, suffix: &str) -> String {
    let slug = id_slug(name);
    let mut id = format!("{slug}-{suffix}");
    while section.groups.iter().any(|group| group.id == id) {
        id = format!("{slug}-{}", random_id_suffix());
    }
    id
}

/// Creates a manual group with the given members and returns its id (`<slug of name>-<suffix>`).
/// The members are no longer excluded; taking them out of other groups is up to the caller.
pub fn create_manual_group(
    section: &mut TargetFailoverOverridesDto,
    name: &str,
    members: &[ChannelMemberRefDto],
    suffix: &str,
) -> String {
    let id = generate_manual_id(section, name, suffix);
    let mut group_members = Vec::with_capacity(members.len());
    for member in members {
        push_ref(&mut group_members, member);
    }
    section.excluded.retain(|excluded| !group_members.contains(excluded));
    section.groups.push(ManualFailoverGroupDto {
        id: id.clone(),
        name: name.trim().to_string(),
        members: group_members,
        ..Default::default()
    });
    id
}

/// Turns the override of a stale auto key into a manual group made of the references it still holds and removes
/// the override. Returns the id of the new group, `None` when there is no override for the key.
pub fn convert_stale_to_manual(
    section: &mut TargetFailoverOverridesDto,
    auto_key: &str,
    suffix: &str,
) -> Option<String> {
    let index = section.auto_overrides.iter().position(|entry| entry.auto_key == auto_key)?;
    let entry = section.auto_overrides.remove(index);
    let mut members = Vec::with_capacity(entry.order.len() + entry.add.len());
    for member in entry.order.iter().chain(&entry.add) {
        if !entry.remove.contains(member) {
            push_ref(&mut members, member);
        }
    }
    let name = entry.name.as_deref().unwrap_or(auto_key);
    let id = create_manual_group(section, name, &members, suffix);
    if let Some(group) = manual_mut(section, &id) {
        group.logo = entry.logo;
        group.epg_channel_id = entry.epg_channel_id;
        group.playlist_group = entry.playlist_group;
        group.policy = entry.policy;
    }
    Some(id)
}

#[cfg(test)]
#[path = "draft.test.rs"]
mod tests;

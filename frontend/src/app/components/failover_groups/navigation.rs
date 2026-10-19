//! What a deep link into the "Failover groups" page asks for: the target, optionally a group and a channel.

use super::draft::GroupRef;
use shared::model::ChannelMemberRefDto;

/// A channel the page should offer in the candidate panel; its title seeds the candidate search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelPreselect {
    pub member_ref: ChannelMemberRefDto,
    pub title: String,
}

impl ChannelPreselect {
    pub fn query(&self) -> &str { self.title.trim() }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailoverGroupsPreselect {
    pub target: String,
    pub group: Option<GroupRef>,
    /// Seeds the search of the group list when the group itself is not known.
    pub group_search: Option<String>,
    pub channel: Option<ChannelPreselect>,
}

impl FailoverGroupsPreselect {
    pub fn for_target(target: &str) -> Self {
        Self { target: target.to_string(), group: None, group_search: None, channel: None }
    }

    #[must_use]
    pub fn with_group_search(mut self, search: &str) -> Self {
        self.group_search = Some(search.to_string());
        self
    }

    #[must_use]
    pub fn with_channel(mut self, member_ref: ChannelMemberRefDto, title: &str) -> Self {
        self.channel = Some(ChannelPreselect { member_ref, title: title.to_string() });
        self
    }
}

/// The channel of a request, tagged like [`NumberedPreselect`] so the candidate panel reacts to every request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumberedChannel {
    pub seq: u32,
    pub channel: ChannelPreselect,
}

/// A request together with the number that tells a target editor whether it applied the request already.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumberedPreselect {
    pub seq: u32,
    pub preselect: FailoverGroupsPreselect,
}

/// The requested target when the page can edit it.
pub fn preselected_target<'a>(preselect: &'a FailoverGroupsPreselect, editable: &[String]) -> Option<&'a str> {
    editable.contains(&preselect.target).then_some(preselect.target.as_str())
}

/// The request unless the editor applied it already; remembers the applied request in `last_applied`.
pub fn take_unapplied<'a>(
    last_applied: &mut u32,
    current: Option<&'a NumberedPreselect>,
) -> Option<&'a FailoverGroupsPreselect> {
    let current = current.filter(|numbered| numbered.seq != *last_applied)?;
    *last_applied = current.seq;
    Some(&current.preselect)
}

#[cfg(test)]
#[path = "navigation.test.rs"]
mod tests;

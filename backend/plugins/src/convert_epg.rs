//! EPG record conversions: `shared::model::EpgProgramme` /
//! `PluginEpgChannel` <-> the WIT `epg-types` records .
//!
//! The channel id of an `EpgProgramme` is transient host-side (`#[serde(skip)]`,
//! private). It goes to the guest as `epg-programme.channel` and is
//! **re-attached on return** from the host's own id: the guest's spelling is
//! only looked up, ASCII case-insensitively, so a plugin that upper-cases an id
//! cannot detach a programme from its channel, and one that names an unknown
//! channel loses the programme. `extra` is host -> guest only in ABI 1.0 and the
//! host has nothing to put in it for a programme, so it is always sent empty and
//! ignored on return.

use shared::{
    model::{EpgCategory, EpgProgramme, PluginEpgChannel},
    utils::Internable,
};
use std::{collections::HashMap, sync::Arc};
use tuliprox_core::utils::with_folded_epg_id;
use tuliprox_plugin_api::bindings::tuliprox::plugin::epg_types::{
    EpgCategory as WitCategory, EpgChannel as WitChannel, EpgProgramme as WitProgramme,
};

pub(crate) fn channel_to_wit(channel: &PluginEpgChannel) -> WitChannel {
    WitChannel { id: channel.id.clone(), names: channel.names.clone(), icon: channel.icon.clone() }
}

#[cfg(test)]
pub(crate) fn channel_from_wit(channel: WitChannel) -> PluginEpgChannel {
    PluginEpgChannel { id: channel.id, names: channel.names, icon: channel.icon }
}

/// The guest-facing view of `programme`.
pub(crate) fn programme_to_wit(programme: &EpgProgramme) -> WitProgramme {
    WitProgramme {
        channel: programme.get_transient_channel_id().to_string(),
        start: programme.start,
        stop: programme.stop,
        title: programme.title.as_ref().map(ToString::to_string),
        description: programme.desc.as_ref().map(ToString::to_string),
        categories: programme
            .categories
            .iter()
            .map(|category| WitCategory {
                value: category.value.to_string(),
                lang: category.lang.as_ref().map(ToString::to_string),
            })
            .collect(),
        catchup_id: programme.catchup_id.as_ref().map(ToString::to_string),
        icon: programme.icon.as_ref().map(ToString::to_string),
        is_live: programme.is_live,
        is_new: programme.is_new,
        previously_shown: programme.previously_shown,
        extra: Vec::new(),
    }
}

/// The channel ids a call carried, to re-attach on return.
///
/// A returned programme names its channel in any ASCII case and gets the host's
/// spelling back. An empty channel is accepted when the call carried exactly one
/// channel (`enrich-programmes`), because there is nothing to disambiguate.
#[derive(Debug, Default)]
pub(crate) struct ChannelIds {
    by_folded: HashMap<String, Arc<str>>,
}

impl ChannelIds {
    /// The distinct channels of `programmes`, first spelling wins.
    pub(crate) fn of(programmes: &[EpgProgramme]) -> Self {
        let mut ids = Self::default();
        for programme in programmes {
            ids.add(programme.get_transient_channel_id());
        }
        ids
    }

    pub(crate) fn single(id: &Arc<str>) -> Self {
        let mut ids = Self::default();
        ids.add(id);
        ids
    }

    fn add(&mut self, id: &Arc<str>) {
        with_folded_epg_id(id, |folded| {
            if !self.by_folded.contains_key(folded) {
                self.by_folded.insert(folded.to_string(), Arc::clone(id));
            }
        });
    }

    fn resolve(&self, id: &str) -> Option<&Arc<str>> {
        if id.is_empty() && self.by_folded.len() == 1 {
            return self.by_folded.values().next();
        }
        with_folded_epg_id(id, |folded| self.by_folded.get(folded))
    }
}

/// The host programme for `wit`, or `None` when its channel is not one of `ids`.
pub(crate) fn programme_from_wit(wit: WitProgramme, ids: &ChannelIds) -> Option<EpgProgramme> {
    let channel = Arc::clone(ids.resolve(&wit.channel)?);
    let mut programme = EpgProgramme::new_all(
        wit.start,
        wit.stop,
        channel,
        wit.title.map(Internable::intern),
        wit.description.map(Internable::intern),
        wit.catchup_id.map(Internable::intern),
    );
    programme.categories = wit
        .categories
        .into_iter()
        .map(|category| EpgCategory { value: category.value.intern(), lang: category.lang.map(Internable::intern) })
        .collect();
    programme.icon = wit.icon.map(Internable::intern);
    programme.is_live = wit.is_live;
    programme.is_new = wit.is_new;
    programme.previously_shown = wit.previously_shown;
    Some(programme)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct LoadedMerge {
    /// Channels whose names or icon changed.
    pub(crate) modified: usize,
    /// Returned ids that match no channel, and repeats of one that did.
    pub(crate) ignored: usize,
}

/// Apply a sparse `on-epg-loaded` result: a returned id (ASCII case-insensitive)
/// replaces that channel's names and icon, nothing else. The channel keeps its
/// own id spelling; channels not returned are untouched; the first entry for an
/// id wins.
pub(crate) fn merge_loaded(channels: &mut [PluginEpgChannel], returned: Vec<WitChannel>) -> LoadedMerge {
    let index: HashMap<String, usize> = channels
        .iter()
        .enumerate()
        .rev()
        .map(|(position, channel)| (with_folded_epg_id(&channel.id, str::to_string), position))
        .collect();
    let mut merge = LoadedMerge::default();
    let mut answered = vec![false; channels.len()];
    for answer in returned {
        let Some(&position) = with_folded_epg_id(&answer.id, |folded| index.get(folded)) else {
            merge.ignored += 1;
            continue;
        };
        if std::mem::replace(&mut answered[position], true) {
            merge.ignored += 1;
            continue;
        }
        let channel = &mut channels[position];
        if channel.names != answer.names || channel.icon != answer.icon {
            channel.names = answer.names;
            channel.icon = answer.icon;
            merge.modified += 1;
        }
    }
    merge
}

#[cfg(test)]
mod tests {
    use super::*;

    fn programme(channel: &str, start: i64) -> EpgProgramme {
        EpgProgramme::new_all(
            start,
            start + 3600,
            channel.intern(),
            Some("Title".intern()),
            Some("Description".intern()),
            Some("catchup-1".intern()),
        )
    }

    fn rich_programme() -> EpgProgramme {
        let mut programme = programme("BBC.One", 1_700_000_000);
        programme.categories = vec![
            EpgCategory { value: "News".intern(), lang: Some("en".intern()) },
            EpgCategory { value: "Sport".intern(), lang: None },
        ];
        programme.icon = Some("http://img/p.png".intern());
        programme.is_live = true;
        programme.is_new = true;
        programme.previously_shown = true;
        programme
    }

    fn assert_same(left: &EpgProgramme, right: &EpgProgramme) {
        assert_eq!(left.get_transient_channel_id(), right.get_transient_channel_id());
        assert_eq!((left.start, left.stop), (right.start, right.stop));
        assert_eq!(left.title, right.title);
        assert_eq!(left.desc, right.desc);
        assert_eq!(left.catchup_id, right.catchup_id);
        assert_eq!(left.categories, right.categories);
        assert_eq!(left.icon, right.icon);
        assert_eq!(
            (left.is_live, left.is_new, left.previously_shown),
            (right.is_live, right.is_new, right.previously_shown)
        );
    }

    #[test]
    fn programmes_round_trip_field_for_field() {
        let cases = [
            ("bare", EpgProgramme::new(10, 20, "ch".intern())),
            ("titled", programme("ch", 100)),
            ("rich", rich_programme()),
            ("negative times", EpgProgramme::new(-5, -1, "ch".intern())),
        ];
        for (name, original) in cases {
            let ids = ChannelIds::of(std::slice::from_ref(&original));
            let back =
                programme_from_wit(programme_to_wit(&original), &ids).unwrap_or_else(|| panic!("{name}: dropped"));
            assert_same(&original, &back);
        }
    }

    #[test]
    fn wit_programme_carries_every_field_and_an_empty_extra() {
        let wit = programme_to_wit(&rich_programme());
        assert_eq!(wit.channel, "BBC.One");
        assert_eq!((wit.start, wit.stop), (1_700_000_000, 1_700_003_600));
        assert_eq!(wit.title.as_deref(), Some("Title"));
        assert_eq!(wit.description.as_deref(), Some("Description"));
        assert_eq!(wit.catchup_id.as_deref(), Some("catchup-1"));
        assert_eq!(wit.icon.as_deref(), Some("http://img/p.png"));
        assert_eq!(wit.categories.len(), 2);
        assert_eq!(wit.categories[0].lang.as_deref(), Some("en"));
        assert_eq!(wit.categories[1].lang, None);
        assert!(wit.is_live && wit.is_new && wit.previously_shown);
        assert_eq!(wit.extra.len(), 0);
    }

    #[test]
    fn the_returned_channel_is_matched_case_insensitively_and_the_host_spelling_wins() {
        let original = programme("BBC.One", 0);
        let ids = ChannelIds::of(std::slice::from_ref(&original));
        let cases = [
            ("same", "BBC.One", Some("BBC.One")),
            ("lowered", "bbc.one", Some("BBC.One")),
            ("shouted", "BBC.ONE", Some("BBC.One")),
            ("unknown", "bbc.two", None),
            ("empty with several channels would be ambiguous, one channel is fine", "", Some("BBC.One")),
        ];
        for (name, guest_channel, expected) in cases {
            let mut wit = programme_to_wit(&original);
            wit.channel = guest_channel.to_string();
            let back = programme_from_wit(wit, &ids);
            assert_eq!(back.as_ref().map(|p| p.get_transient_channel_id().as_ref()), expected, "{name}");
        }
    }

    #[test]
    fn an_empty_channel_is_refused_when_the_call_carried_several() {
        let programmes = [programme("a", 0), programme("b", 0)];
        let ids = ChannelIds::of(&programmes);
        let mut wit = programme_to_wit(&programmes[0]);
        wit.channel = String::new();
        assert!(programme_from_wit(wit, &ids).is_none());
    }

    #[test]
    fn channel_ids_keep_the_first_spelling() {
        let programmes = [programme("Mixed.Case", 0), programme("mixed.case", 60)];
        let ids = ChannelIds::of(&programmes);
        let mut wit = programme_to_wit(&programmes[1]);
        wit.channel = "MIXED.CASE".to_string();
        let back = programme_from_wit(wit, &ids).expect("known channel");
        assert_eq!(back.get_transient_channel_id().as_ref(), "Mixed.Case");
    }

    #[test]
    fn epg_channels_round_trip() {
        let cases = [
            PluginEpgChannel::default(),
            PluginEpgChannel { id: "bbc.one".into(), names: vec!["BBC One".into()], icon: None },
            PluginEpgChannel {
                id: "Mixed.Id".into(),
                names: vec!["First".into(), "Second".into()],
                icon: Some("http://img/c.png".into()),
            },
        ];
        for original in cases {
            assert_eq!(channel_from_wit(channel_to_wit(&original)), original);
        }
    }

    fn channel(id: &str, names: &[&str], icon: Option<&str>) -> PluginEpgChannel {
        PluginEpgChannel {
            id: id.to_string(),
            names: names.iter().map(ToString::to_string).collect(),
            icon: icon.map(ToString::to_string),
        }
    }

    #[test]
    fn a_loaded_result_is_sparse_and_replaces_names_and_icon_only() {
        let mut channels = vec![
            channel("A.one", &["Alpha"], None),
            channel("b.two", &["Beta"], Some("old.png")),
            channel("c.three", &["Gamma"], None),
        ];
        let returned = vec![
            // case-insensitive hit: the host keeps its own spelling of the id
            WitChannel { id: "a.ONE".into(), names: vec!["Alpha HD".into(), "Alpha".into()], icon: None },
            // identical answer is not a modification
            WitChannel { id: "c.three".into(), names: vec!["Gamma".into()], icon: None },
            // unknown id
            WitChannel { id: "nope".into(), names: vec!["x".into()], icon: None },
            // a repeat of an answered id
            WitChannel { id: "A.one".into(), names: vec!["later".into()], icon: None },
        ];
        let merge = merge_loaded(&mut channels, returned);
        assert_eq!(merge, LoadedMerge { modified: 1, ignored: 2 });
        assert_eq!(channels[0], channel("A.one", &["Alpha HD", "Alpha"], None));
        assert_eq!(channels[1], channel("b.two", &["Beta"], Some("old.png")));
        assert_eq!(channels[2], channel("c.three", &["Gamma"], None));
    }

    #[test]
    fn a_loaded_result_may_clear_the_icon() {
        let mut channels = vec![channel("a", &["A"], Some("old.png"))];
        let merge =
            merge_loaded(&mut channels, vec![WitChannel { id: "a".into(), names: vec!["A".into()], icon: None }]);
        assert_eq!(merge.modified, 1);
        assert_eq!(channels[0].icon, None);
    }
}

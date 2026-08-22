use crate::model::config::ConfigInputDto;
use std::{collections::HashMap, hash::Hash, sync::Arc};

/// One provider's configured capacity and current connection count.
#[derive(Debug, Clone)]
pub struct ProviderSlot<Name> {
    pub name: Name,
    pub max_connections: u16,
    pub current: usize,
}

/// An input and its aliases, which together form one capacity group.
pub trait CapacityGroup {
    fn name(&self) -> &Arc<str>;
    fn enabled(&self) -> bool;
    /// Names of the enabled aliases; disabled ones cannot carry connections.
    fn alias_names(&self) -> impl Iterator<Item = &Arc<str>>;
}

impl<T: CapacityGroup> CapacityGroup for &T {
    fn name(&self) -> &Arc<str> { (**self).name() }
    fn enabled(&self) -> bool { (**self).enabled() }
    fn alias_names(&self) -> impl Iterator<Item = &Arc<str>> { (**self).alias_names() }
}

impl<T: CapacityGroup> CapacityGroup for Arc<T> {
    fn name(&self) -> &Arc<str> { (**self).name() }
    fn enabled(&self) -> bool { (**self).enabled() }
    fn alias_names(&self) -> impl Iterator<Item = &Arc<str>> { (**self).alias_names() }
}

impl CapacityGroup for ConfigInputDto {
    fn name(&self) -> &Arc<str> { &self.name }
    fn enabled(&self) -> bool { self.enabled }
    fn alias_names(&self) -> impl Iterator<Item = &Arc<str>> {
        self.aliases.iter().flatten().filter(|alias| alias.enabled).map(|alias| &alias.name)
    }
}

/// Maps every usable group member name (enabled input and its enabled aliases)
/// to the main input name. Disabled members cannot carry connections and are omitted.
pub fn build_group_lookup<I>(inputs: I) -> HashMap<Arc<str>, Arc<str>>
where
    I: IntoIterator,
    I::Item: CapacityGroup,
{
    let mut map = HashMap::new();
    for input in inputs {
        if !input.enabled() {
            continue;
        }
        let main = input.name().clone();
        map.insert(main.clone(), main.clone());
        for alias in input.alias_names() {
            map.insert(alias.clone(), main.clone());
        }
    }
    map
}

/// Returns true when every input group has no spare capacity.
///
/// A group is exhausted when every member with a finite limit is at it and no
/// member offers unlimited capacity. Slots that do not appear in `group_of`
/// are ignored. An empty input returns false (no data, not full).
pub fn is_exhausted<Name, Group, I>(slots: I, group_of: &HashMap<Name, Group>) -> bool
where
    I: IntoIterator<Item = ProviderSlot<Name>>,
    Name: Hash + Eq,
    Group: Hash + Eq,
{
    // Only a handful of input groups is ever configured, so a linear scan is
    // cheaper than allocating a map on every call.
    let mut groups: Vec<(&Group, usize, u16, bool)> = Vec::new();
    for slot in slots {
        let Some(group) = group_of.get(&slot.name) else { continue };
        let index = match groups.iter().position(|(member, ..)| *member == group) {
            Some(index) => index,
            None => {
                groups.push((group, 0, 0, false));
                groups.len() - 1
            }
        };
        let (_, current, max, unlimited) = &mut groups[index];
        if slot.max_connections == 0 {
            *unlimited = true;
        } else {
            *current += slot.current;
            *max += slot.max_connections;
        }
    }
    !groups.is_empty()
        && groups.iter().all(|(_, current, max, unlimited)| !*unlimited && *max > 0 && *current >= usize::from(*max))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(name: &'static str, max: u16, current: usize) -> ProviderSlot<&'static str> {
        ProviderSlot { name, max_connections: max, current }
    }

    fn lookup(pairs: &[(&'static str, &'static str)]) -> HashMap<&'static str, &'static str> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn empty_input_is_not_exhausted() {
        let g = lookup(&[]);
        assert!(!is_exhausted(std::iter::empty::<ProviderSlot<&str>>(), &g));
    }

    #[test]
    fn group_with_unused_alias_is_not_exhausted() {
        let g = lookup(&[("primary", "main"), ("alias", "main")]);
        let slots = [slot("primary", 1, 1), slot("alias", 5, 0)];
        assert!(!is_exhausted(slots, &g));
    }

    #[test]
    fn every_member_of_group_full_is_exhausted() {
        let g = lookup(&[("primary", "main"), ("alias", "main")]);
        let slots = [slot("primary", 1, 1), slot("alias", 5, 5)];
        assert!(is_exhausted(slots, &g));
    }

    #[test]
    fn one_group_full_but_other_has_capacity_is_not_exhausted() {
        let g = lookup(&[("a1", "alpha"), ("a2", "alpha"), ("b1", "beta")]);
        let slots = [slot("a1", 1, 1), slot("a2", 3, 3), slot("b1", 5, 0)];
        assert!(!is_exhausted(slots, &g));
    }

    #[test]
    fn unlimited_member_keeps_group_from_exhausting() {
        let g = lookup(&[("primary", "main"), ("alias", "main")]);
        let slots = [slot("primary", 0, 0), slot("alias", 5, 5)];
        assert!(!is_exhausted(slots, &g));
    }

    #[test]
    fn all_groups_exhausted() {
        let g = lookup(&[("a1", "alpha"), ("b1", "beta")]);
        let slots = [slot("a1", 1, 1), slot("b1", 3, 3)];
        assert!(is_exhausted(slots, &g));
    }

    #[test]
    fn unknown_slot_is_ignored() {
        let g = lookup(&[]);
        let slots = [slot("orphan", 10, 10)];
        assert!(!is_exhausted(slots, &g));
    }

    struct Input {
        name: Arc<str>,
        enabled: bool,
        aliases: Vec<Arc<str>>,
    }

    impl CapacityGroup for Input {
        fn name(&self) -> &Arc<str> { &self.name }
        fn enabled(&self) -> bool { self.enabled }
        fn alias_names(&self) -> impl Iterator<Item = &Arc<str>> { self.aliases.iter() }
    }

    fn enabled_input(name: &'static str, aliases: Vec<Arc<str>>) -> Input {
        Input { name: Arc::from(name), enabled: true, aliases }
    }

    #[test]
    fn group_lookup_maps_members_to_main_input() {
        let inputs = [enabled_input("main", vec![Arc::from("alias")]), enabled_input("solo", Vec::new())];
        let map = build_group_lookup(inputs);
        assert_eq!(map.len(), 3);
        assert_eq!(map.get(&Arc::from("main")), Some(&Arc::from("main")));
        assert_eq!(map.get(&Arc::from("alias")), Some(&Arc::from("main")));
        assert_eq!(map.get(&Arc::from("solo")), Some(&Arc::from("solo")));
    }

    #[test]
    fn group_lookup_omits_disabled_inputs() {
        let inputs = [Input { name: Arc::from("off"), enabled: false, aliases: vec![Arc::from("off-alias")] }];
        assert!(build_group_lookup(inputs).is_empty());
    }
}

use std::collections::HashMap;
use std::sync::Arc;

use crate::model::config::{ConfigInput, ConfigInputAlias};
use shared::model::provider_saturation::{CapacityGroup, ProviderSlot};

#[allow(clippy::implicit_hasher)]
pub fn build_provider_slots(
    inputs: &[Arc<ConfigInput>],
    connections: &HashMap<Arc<str>, usize>,
) -> Vec<ProviderSlot<Arc<str>>> {
    let mut slots = Vec::with_capacity(inputs.len());
    for input in inputs {
        // Disabled members have no lineup and cannot carry connections.
        if !input.enabled {
            continue;
        }
        let current = connections.get(&input.name).copied().unwrap_or(0);
        slots.push(ProviderSlot {
            name: input.name.clone(),
            max_connections: input.max_connections,
            current,
        });
        push_aliases(&mut slots, input.aliases.as_deref(), connections);
    }
    slots
}

impl CapacityGroup for ConfigInput {
    fn name(&self) -> &Arc<str> { &self.name }
    fn enabled(&self) -> bool { self.enabled }
    fn alias_names(&self) -> impl Iterator<Item = &Arc<str>> {
        self.aliases.iter().flatten().filter(|alias| alias.enabled).map(|alias| &alias.name)
    }
}

fn push_aliases(
    slots: &mut Vec<ProviderSlot<Arc<str>>>,
    aliases: Option<&[ConfigInputAlias]>,
    connections: &HashMap<Arc<str>, usize>,
) {
    let Some(aliases) = aliases else { return };
    for alias in aliases {
        if !alias.enabled {
            continue;
        }
        let current = connections.get(&alias.name).copied().unwrap_or(0);
        slots.push(ProviderSlot {
            name: alias.name.clone(),
            max_connections: alias.max_connections,
            current,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::provider_saturation::{build_group_lookup, is_exhausted};

    fn input(name: &str, max: u16) -> Arc<ConfigInput> {
        Arc::new(ConfigInput {
            name: Arc::from(name),
            max_connections: max,
            enabled: true,
            ..ConfigInput::default()
        })
    }

    #[test]
    fn exhausted_when_all_groups_full() {
        let inputs = vec![input("a", 1), input("b", 2)];
        let mut conns = HashMap::new();
        conns.insert(Arc::from("a"), 1usize);
        conns.insert(Arc::from("b"), 2usize);
        let slots = build_provider_slots(&inputs, &conns);
        let groups = build_group_lookup(&inputs);
        assert!(is_exhausted(slots, &groups));
    }

    #[test]
    fn ready_when_any_group_has_capacity() {
        let inputs = vec![input("a", 1), input("b", 5)];
        let mut conns = HashMap::new();
        conns.insert(Arc::from("a"), 1usize);
        conns.insert(Arc::from("b"), 0usize);
        let slots = build_provider_slots(&inputs, &conns);
        let groups = build_group_lookup(&inputs);
        assert!(!is_exhausted(slots, &groups));
    }

    #[test]
    fn disabled_members_offer_no_capacity() {
        let mut full_input = ConfigInput {
            name: Arc::from("a"),
            max_connections: 1,
            enabled: true,
            ..ConfigInput::default()
        };
        full_input.aliases = Some(vec![ConfigInputAlias {
            id: 2,
            name: Arc::from("a-off"),
            url: "http://alias".to_string(),
            username: None,
            password: None,
            priority: 0,
            max_connections: 5,
            exp_date: None,
            enabled: false,
            stalker: None,
        }]);
        // The disabled alias and the disabled input must not add spare capacity.
        let disabled_input = Arc::new(ConfigInput {
            name: Arc::from("off"),
            max_connections: 9,
            ..ConfigInput::default()
        });
        let inputs = vec![Arc::new(full_input), disabled_input];
        let mut conns = HashMap::new();
        conns.insert(Arc::from("a"), 1usize);
        let slots = build_provider_slots(&inputs, &conns);
        let groups = build_group_lookup(&inputs);
        assert!(is_exhausted(slots, &groups));
    }
}

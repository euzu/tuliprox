#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HlsManifestRecoveryBurstLevel {
    #[default]
    Off,
    Friendly,
    Cautious,
    Balanced,
    Intense,
    Aggressive,
    Beast,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct HlsManifestRecoveryBurstPlan {
    pub slots: usize,
    pub lanes_per_slot: usize,
}

impl HlsManifestRecoveryBurstPlan {
    pub const fn total_candidates(self) -> usize {
        self.slots.saturating_mul(self.lanes_per_slot)
    }

    pub const fn slot_for_candidate(self, candidate_index: usize) -> usize {
        match candidate_index.checked_div(self.lanes_per_slot) {
            Some(slot) => slot,
            None => 0,
        }
    }
}

impl HlsManifestRecoveryBurstLevel {
    /// Maps the configured burst level to a concrete fetch plan. The same
    /// constants drive the runtime scheduler; see `HlsCacheConfig` for the
    /// operator-facing description of each level.
    pub const fn plan(self) -> HlsManifestRecoveryBurstPlan {
        match self {
            Self::Off => HlsManifestRecoveryBurstPlan { slots: 1, lanes_per_slot: 1 },
            Self::Friendly => HlsManifestRecoveryBurstPlan { slots: 2, lanes_per_slot: 1 },
            Self::Cautious => HlsManifestRecoveryBurstPlan { slots: 3, lanes_per_slot: 1 },
            Self::Balanced => HlsManifestRecoveryBurstPlan { slots: 4, lanes_per_slot: 1 },
            Self::Intense => HlsManifestRecoveryBurstPlan { slots: 5, lanes_per_slot: 1 },
            Self::Aggressive => HlsManifestRecoveryBurstPlan { slots: 6, lanes_per_slot: 1 },
            Self::Beast => HlsManifestRecoveryBurstPlan { slots: 6, lanes_per_slot: 2 },
        }
    }

    pub const fn extra_candidates(self) -> usize {
        self.plan().total_candidates().saturating_sub(1)
    }

    pub const fn total_candidates(self) -> usize {
        self.plan().total_candidates()
    }
}

crate::impl_str_enum!(HlsManifestRecoveryBurstLevel, "HLS manifest recovery burst level",
    Off => "off",
    Friendly => "friendly",
    Cautious => "cautious",
    Balanced => "balanced",
    Intense => "intense",
    Aggressive => "aggressive",
    Beast => "beast",
);

#[cfg(test)]
mod tests {
    use super::HlsManifestRecoveryBurstLevel;

    #[test]
    fn from_str_parses_known_levels() {
        assert_eq!("off".parse::<HlsManifestRecoveryBurstLevel>(), Ok(HlsManifestRecoveryBurstLevel::Off));
        assert_eq!("beast".parse::<HlsManifestRecoveryBurstLevel>(), Ok(HlsManifestRecoveryBurstLevel::Beast));
        assert_eq!("balanced".parse::<HlsManifestRecoveryBurstLevel>(), Ok(HlsManifestRecoveryBurstLevel::Balanced));
        assert!("nonsense".parse::<HlsManifestRecoveryBurstLevel>().is_err());
    }

    #[test]
    fn beast_plan_keeps_six_slots_with_two_lanes_and_derived_candidate_count() {
        let plan = HlsManifestRecoveryBurstLevel::Beast.plan();

        assert_eq!(plan.slots, 6);
        assert_eq!(plan.lanes_per_slot, 2);
        assert_eq!(plan.total_candidates(), plan.slots * plan.lanes_per_slot);
        assert_eq!(
            (0..plan.total_candidates()).map(|candidate| plan.slot_for_candidate(candidate)).collect::<Vec<_>>(),
            vec![0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5]
        );
    }

    #[test]
    fn off_plan_keeps_exactly_one_candidate() {
        let plan = HlsManifestRecoveryBurstLevel::Off.plan();

        assert_eq!(plan.slots, 1);
        assert_eq!(plan.lanes_per_slot, 1);
        assert_eq!(plan.total_candidates(), 1);
    }
}

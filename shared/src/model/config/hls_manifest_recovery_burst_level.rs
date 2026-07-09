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
    pub const fn total_candidates(self) -> usize { self.slots.saturating_mul(self.lanes_per_slot) }

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

    pub const fn extra_candidates(self) -> usize { self.plan().total_candidates().saturating_sub(1) }

    pub const fn total_candidates(self) -> usize { self.plan().total_candidates() }
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
}

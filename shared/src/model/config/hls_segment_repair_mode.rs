#[derive(Debug, Clone, Copy, Hash, serde::Serialize, serde::Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HlsSegmentRepairMode {
    #[default]
    Off,
    Low,
    Medium,
    High,
}

impl HlsSegmentRepairMode {
    /// Stable lowercase log label — same canonical form as the `Display` impl.
    pub const fn as_log_value(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// Ordering rank used by `execution_plan` to decide whether the configured
    /// `max_level` covers the `required_level`. Higher number = more aggressive
    /// repair.
    pub const fn rank(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Low => 1,
            Self::Medium => 2,
            Self::High => 3,
        }
    }

    /// Decide whether a repair at `self` (configured ceiling) should run for a
    /// segment that requested `required_level` (e.g. by a trigger source).
    pub const fn execution_plan(self, required_level: Self) -> HlsSegmentRepairExecutionPlan {
        if matches!(self, Self::Off) || matches!(required_level, Self::Off) {
            HlsSegmentRepairExecutionPlan::SkipNoTrigger
        } else if self.rank() < required_level.rank() {
            HlsSegmentRepairExecutionPlan::SkipConfiguredMaxBelowRequired
        } else {
            HlsSegmentRepairExecutionPlan::Repair(required_level)
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsSegmentRepairExecutionPlan {
    Repair(HlsSegmentRepairMode),
    SkipNoTrigger,
    SkipConfiguredMaxBelowRequired,
}

crate::impl_str_enum!(HlsSegmentRepairMode, "HLS segment repair mode",
    Off => "off",
    Low => "low",
    Medium => "medium",
    High => "high",
);

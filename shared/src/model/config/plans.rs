use crate::{error::TuliproxError, model::UserPlanDto};
use std::collections::HashSet;

/// Standalone `plans.yml` document: reusable user capability tiers.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PlansConfigDto {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plans: Vec<UserPlanDto>,
}

impl PlansConfigDto {
    pub fn is_empty(&self) -> bool { self.plans.is_empty() }

    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        let mut errors = Vec::new();
        let mut plan_names = HashSet::new();
        for plan in &mut self.plans {
            if let Err(err) = plan.prepare() {
                errors.push(err.to_string());
            }
            if !plan_names.insert(plan.name.clone()) {
                errors.push(format!("Non-unique user plan name found {}", plan.name));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(TuliproxError::ConfigApiProxy(errors.join("\n")))
        }
    }
}

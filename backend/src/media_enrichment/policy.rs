/// Source-neutral policy for fill-only media fact enrichment.
///
/// Xtream still exposes this through its legacy `ResolveTmdb` switch, but the
/// policy describes fact families and suppliers rather than the legacy option
/// name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MissingFactEnrichmentPolicy {
    missing_facts_enabled: bool,
    parsed_title_supplier_enabled: bool,
    tmdb_supplier_enabled: bool,
}

impl MissingFactEnrichmentPolicy {
    /// Builds a missing-fact policy from Xtream's current compatibility flags.
    pub fn from_xtream_missing_fact_options(resolve_missing_facts: bool, input_enabled: bool) -> Self {
        let enabled = resolve_missing_facts && input_enabled;
        Self {
            missing_facts_enabled: enabled,
            parsed_title_supplier_enabled: enabled,
            tmdb_supplier_enabled: enabled,
        }
    }

    /// Returns true when at least one supported fact family is missing and the policy is enabled.
    pub fn should_resolve_missing_facts(&self, missing_tmdb_id: bool, missing_release_date: bool) -> bool {
        self.missing_facts_enabled && (missing_tmdb_id || missing_release_date)
    }

    /// Returns true when the local parsed-title supplier may contribute temporal facts.
    pub fn should_try_parsed_title_supplier(&self, missing_release_date: bool) -> bool {
        self.parsed_title_supplier_enabled && missing_release_date
    }

    /// Returns true when the TMDB supplier may contribute identity or temporal facts.
    pub fn should_try_tmdb_supplier(&self, missing_tmdb_id: bool, missing_release_date: bool) -> bool {
        self.tmdb_supplier_enabled && (missing_tmdb_id || missing_release_date)
    }

    /// Returns whether calls to the TMDB supplier are allowed by the policy.
    pub fn tmdb_supplier_enabled(&self) -> bool { self.tmdb_supplier_enabled }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disables_missing_fact_resolution_when_request_is_off() {
        let policy = MissingFactEnrichmentPolicy::from_xtream_missing_fact_options(false, true);

        assert!(!policy.should_resolve_missing_facts(true, true));
        assert!(!policy.should_try_parsed_title_supplier(true));
        assert!(!policy.should_try_tmdb_supplier(true, true));
    }

    #[test]
    fn disables_missing_fact_resolution_when_input_flag_is_off() {
        let policy = MissingFactEnrichmentPolicy::from_xtream_missing_fact_options(true, false);

        assert!(!policy.should_resolve_missing_facts(true, true));
        assert!(!policy.should_try_parsed_title_supplier(true));
        assert!(!policy.should_try_tmdb_supplier(true, true));
    }

    #[test]
    fn allows_suppliers_only_for_missing_fact_families() {
        let policy = MissingFactEnrichmentPolicy::from_xtream_missing_fact_options(true, true);

        assert!(policy.should_resolve_missing_facts(true, false));
        assert!(policy.should_resolve_missing_facts(false, true));
        assert!(!policy.should_resolve_missing_facts(false, false));
        assert!(policy.should_try_parsed_title_supplier(true));
        assert!(!policy.should_try_parsed_title_supplier(false));
        assert!(policy.should_try_tmdb_supplier(true, false));
        assert!(policy.should_try_tmdb_supplier(false, true));
        assert!(!policy.should_try_tmdb_supplier(false, false));
    }
}

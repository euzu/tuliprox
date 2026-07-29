//! QoS aggregation defaults.

default_eq_fns!(
    default_qos_aggregation_interval_secs, is_default_qos_aggregation_interval_secs, u64, 300;
    default_qos_aggregation_compaction_interval_secs, is_default_qos_aggregation_compaction_interval_secs, u64, 86_400;
);

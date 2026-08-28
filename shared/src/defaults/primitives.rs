//! Generic predicates and tiny defaults shared across all config DTOs.

use crate::model::{ClusterFlags, RuntimeConfigReportFormat};

pub const fn is_zero_u16(v: &u16) -> bool { *v == 0 }
pub const fn is_zero_i16(v: &i16) -> bool { *v == 0 }
pub const fn is_zero_u32(v: &u32) -> bool { *v == 0 }
pub const fn is_true(v: &bool) -> bool { *v }
pub const fn is_false(v: &bool) -> bool { !*v }
pub const fn default_as_true() -> bool { true }

pub fn is_empty_optional_vec<T>(s: &Option<Vec<T>>) -> bool { s.as_ref().is_none_or(std::vec::Vec::is_empty) }

pub fn default_as_default() -> String { "default".into() }

pub fn default_page() -> u32 { 1 }

pub fn default_page_size() -> u16 { 25 }

pub const fn is_default_runtime_config_report_format(value: &RuntimeConfigReportFormat) -> bool {
    matches!(value, RuntimeConfigReportFormat::Yaml)
}

pub fn is_cluster_optional(cf: &Option<ClusterFlags>) -> bool { cf.is_none_or(|c| c.is_all()) }

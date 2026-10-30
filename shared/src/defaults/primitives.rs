//! Generic predicates and tiny defaults shared across all config DTOs.

use crate::model::{ClusterFlags, RuntimeConfigReportFormat, SyslogFacility, SyslogFormat};

pub const fn is_zero_u8(v: &u8) -> bool { *v == 0 }
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

pub const fn default_syslog_port() -> u16 { 514 }
pub const fn is_default_syslog_port(v: &u16) -> bool { *v == default_syslog_port() }

pub const fn default_syslog_format() -> SyslogFormat { SyslogFormat::Rfc5424 }
pub const fn is_default_syslog_format(v: &SyslogFormat) -> bool { matches!(v, SyslogFormat::Rfc5424) }

pub const fn default_syslog_facility() -> SyslogFacility { SyslogFacility::Local0 }
pub const fn is_default_syslog_facility(v: &SyslogFacility) -> bool { matches!(v, SyslogFacility::Local0) }

pub fn default_syslog_app_name() -> String { "tuliprox".to_string() }
pub fn is_default_syslog_app_name(v: &str) -> bool { v == "tuliprox" }

pub const fn default_syslog_queue_capacity() -> u32 { 4096 }
pub const fn is_default_syslog_queue_capacity(v: &u32) -> bool { *v == default_syslog_queue_capacity() }

pub const fn default_syslog_max_message_len() -> u16 { 1024 }
pub const fn is_default_syslog_max_message_len(v: &u16) -> bool { *v == default_syslog_max_message_len() }

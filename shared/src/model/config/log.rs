use crate::{
    defaults::{
        default_as_true, default_syslog_app_name, default_syslog_facility, default_syslog_format,
        default_syslog_max_message_len, default_syslog_port, default_syslog_queue_capacity,
        is_default_runtime_config_report_format, is_default_syslog_app_name, is_default_syslog_facility,
        is_default_syslog_format, is_default_syslog_max_message_len, is_default_syslog_port,
        is_default_syslog_queue_capacity, is_false, is_true,
    },
    error::TuliproxError,
    model::LogLevel,
    utils::{is_blank_optional_str, is_blank_optional_string},
};
use strum_macros::{AsRefStr, Display, EnumIter, EnumString};

#[derive(
    Debug,
    Clone,
    Copy,
    serde::Serialize,
    serde::Deserialize,
    PartialEq,
    Eq,
    Default,
    EnumIter,
    EnumString,
    AsRefStr,
    Display,
)]
#[strum(serialize_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum RuntimeConfigReportFormat {
    #[default]
    Yaml,
    Json,
}

#[derive(
    Debug,
    Clone,
    Copy,
    serde::Serialize,
    serde::Deserialize,
    PartialEq,
    Eq,
    Default,
    EnumIter,
    EnumString,
    AsRefStr,
    Display,
)]
#[strum(serialize_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum SyslogFormat {
    #[default]
    Rfc5424,
    Rfc3164,
    Json,
}

/// The complete RFC 5424 §6.2.1 facility set, all 24 values.
///
/// A facility names the *source* of a message, and a collector routes by it: a stock rsyslog writes `cron` to
/// `/var/log/cron`, `mail` to `/var/log/mail.log`, and pushes everything except `auth`/`authpriv` into the general
/// log. The value is therefore a routing key, not a severity, and most of these names refer to subsystems tuliprox
/// does not have — `kern`, `mail`, `lpr`, `news`, `uucp`, `cron`, `ftp`, `ntp`, `audit`, `alert`, `clock2`, and
/// `syslog`, which RFC 5424 reserves for the syslog daemon itself.
///
/// The list is offered in full for completeness and interop with existing collector rules. In practice
/// `local0`..`local7` are the values that make sense: they are reserved for site-local use and are the conventional
/// slot for a third-party service such as this one. `local0` is the default. Operators who need a finer split route
/// on `app_name` in the collector, which does not change the PRI.
#[derive(
    Debug,
    Clone,
    Copy,
    serde::Serialize,
    serde::Deserialize,
    PartialEq,
    Eq,
    Default,
    EnumIter,
    EnumString,
    AsRefStr,
    Display,
)]
#[strum(serialize_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum SyslogFacility {
    Kern,
    User,
    Mail,
    Daemon,
    Auth,
    Syslog,
    Lpr,
    News,
    Uucp,
    Cron,
    Authpriv,
    Ftp,
    Ntp,
    Audit,
    Alert,
    Clock2,
    #[default]
    Local0,
    Local1,
    Local2,
    Local3,
    Local4,
    Local5,
    Local6,
    Local7,
}

impl SyslogFacility {
    /// The IANA facility code that goes into `PRI = code * 8 + severity`.
    pub const fn code(&self) -> u8 {
        match self {
            Self::Kern => 0,
            Self::User => 1,
            Self::Mail => 2,
            Self::Daemon => 3,
            Self::Auth => 4,
            Self::Syslog => 5,
            Self::Lpr => 6,
            Self::News => 7,
            Self::Uucp => 8,
            Self::Cron => 9,
            Self::Authpriv => 10,
            Self::Ftp => 11,
            Self::Ntp => 12,
            Self::Audit => 13,
            Self::Alert => 14,
            Self::Clock2 => 15,
            Self::Local0 => 16,
            Self::Local1 => 17,
            Self::Local2 => 18,
            Self::Local3 => 19,
            Self::Local4 => 20,
            Self::Local5 => 21,
            Self::Local6 => 22,
            Self::Local7 => 23,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SyslogConfigDto {
    /// Off by default: the feature is opt-in (Web-UI toggle or YAML).
    #[serde(default, skip_serializing_if = "is_false")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    #[serde(default = "default_syslog_port", skip_serializing_if = "is_default_syslog_port")]
    pub port: u16,
    #[serde(default, skip_serializing_if = "is_default_syslog_format")]
    pub format: SyslogFormat,
    #[serde(default, skip_serializing_if = "is_default_syslog_facility")]
    pub facility: SyslogFacility,
    #[serde(default = "default_syslog_app_name", skip_serializing_if = "is_default_syslog_app_name")]
    pub app_name: String,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_level: Option<LogLevel>,
    #[serde(default = "default_syslog_queue_capacity", skip_serializing_if = "is_default_syslog_queue_capacity")]
    pub queue_capacity: u32,
    #[serde(default = "default_syslog_max_message_len", skip_serializing_if = "is_default_syslog_max_message_len")]
    pub max_message_len: u16,
}

impl Default for SyslogConfigDto {
    fn default() -> Self {
        Self {
            enabled: false,
            host: String::new(),
            port: default_syslog_port(),
            format: default_syslog_format(),
            facility: default_syslog_facility(),
            app_name: default_syslog_app_name(),
            hostname: None,
            log_level: None,
            queue_capacity: default_syslog_queue_capacity(),
            max_message_len: default_syslog_max_message_len(),
        }
    }
}

impl SyslogConfigDto {
    pub fn clean(&mut self) {
        self.host = self.host.trim().to_string();
        match self.hostname.as_deref() {
            Some(h) if h.trim().is_empty() => self.hostname = None,
            Some(h) => self.hostname = Some(h.trim().to_string()),
            None => {}
        }
        let app_name = self.app_name.trim();
        self.app_name = if app_name.is_empty() { default_syslog_app_name() } else { app_name.to_string() };
    }

    pub fn is_empty(&self) -> bool {
        !self.enabled
            && self.host.is_empty()
            && is_blank_optional_string(&self.hostname)
            && self.log_level.is_none()
            && self.port == default_syslog_port()
            && self.format == default_syslog_format()
            && self.facility == default_syslog_facility()
            && self.app_name == default_syslog_app_name()
            && self.queue_capacity == default_syslog_queue_capacity()
            && self.max_message_len == default_syslog_max_message_len()
    }

    pub fn validate(&self) -> Result<(), TuliproxError> {
        if self.enabled && self.host.trim().is_empty() {
            return Err(TuliproxError::Config("syslog host must not be empty when enabled".to_string()));
        }
        if self.port == 0 {
            return Err(TuliproxError::Config("syslog port must be > 0".to_string()));
        }
        if self.queue_capacity == 0 || self.queue_capacity > 65536 {
            return Err(TuliproxError::Config("syslog queue_capacity must be between 1 and 65536".to_string()));
        }
        let app_name = self.app_name.as_bytes();
        if app_name.is_empty() || app_name.len() > 48 || app_name.iter().any(|&b| !(0x21..=0x7E).contains(&b)) {
            return Err(TuliproxError::Config(
                "syslog app_name must be 1..=48 ASCII characters without spaces (0x21..=0x7E)".to_string(),
            ));
        }
        if let Some(hostname) = self.hostname.as_deref() {
            if hostname.trim().len() > 255 {
                return Err(TuliproxError::Config("syslog hostname must not exceed 255 bytes".to_string()));
            }
        }
        let min_len = match self.format {
            SyslogFormat::Json => 1024,
            SyslogFormat::Rfc5424 | SyslogFormat::Rfc3164 => 480,
        };
        if self.max_message_len < min_len {
            return Err(TuliproxError::Config(format!(
                "syslog max_message_len must be at least {min_len} for format {}",
                self.format
            )));
        }
        // 65507 is the largest payload an IPv4 UDP datagram can carry (65535 - 20 IP - 8 UDP); anything above it
        // would be rejected by the socket on every send.
        if self.max_message_len > 65507 {
            return Err(TuliproxError::Config(
                "syslog max_message_len must not exceed 65507 (maximum UDP payload)".to_string(),
            ));
        }
        if self.log_level == Some(LogLevel::Trace) {
            return Err(TuliproxError::Config(
                "syslog log_level 'trace' is not supported; use 'debug', 'info', 'warn', or 'error'".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LogConfigDto {
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub sanitize_sensitive_info: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub log_active_user: bool,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub log_level: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub runtime_config_report_enabled: bool,
    #[serde(default, skip_serializing_if = "is_default_runtime_config_report_format")]
    pub runtime_config_report_format: RuntimeConfigReportFormat,
    #[serde(default, skip_serializing_if = "SyslogConfigDto::is_empty")]
    pub syslog: SyslogConfigDto,
}

impl Default for LogConfigDto {
    fn default() -> Self {
        LogConfigDto {
            sanitize_sensitive_info: default_as_true(),
            log_active_user: false,
            log_level: None,
            runtime_config_report_enabled: false,
            runtime_config_report_format: RuntimeConfigReportFormat::default(),
            syslog: SyslogConfigDto::default(),
        }
    }
}

impl LogConfigDto {
    pub fn is_empty(&self) -> bool {
        self.sanitize_sensitive_info
            && !self.log_active_user
            && is_blank_optional_str(self.log_level.as_deref())
            && !self.runtime_config_report_enabled
            && is_default_runtime_config_report_format(&self.runtime_config_report_format)
            && self.syslog.is_empty()
    }

    pub fn clean(&mut self) {
        if is_blank_optional_str(self.log_level.as_deref()) {
            self.log_level = None;
        }
        self.syslog.clean();
    }

    pub fn validate(&self) -> Result<(), TuliproxError> { self.syslog.validate() }
}

#[cfg(test)]
mod tests {
    use super::{LogConfigDto, SyslogConfigDto, SyslogFacility, SyslogFormat};
    use crate::model::LogLevel;

    #[test]
    fn runtime_config_report_defaults_to_disabled_yaml() {
        let cfg = LogConfigDto::default();
        assert!(!cfg.runtime_config_report_enabled);
        assert_eq!(cfg.runtime_config_report_format, super::RuntimeConfigReportFormat::Yaml);
    }

    #[test]
    fn runtime_config_report_yaml_round_trips() {
        let json = r#"{
            "runtime_config_report_enabled": true,
            "runtime_config_report_format": "yaml"
        }"#;
        let cfg: LogConfigDto = serde_json::from_str(json).expect("deserialize");
        assert!(cfg.runtime_config_report_enabled);
        assert_eq!(cfg.runtime_config_report_format, super::RuntimeConfigReportFormat::Yaml);

        let encoded = serde_json::to_string(&cfg).expect("serialize");
        assert!(encoded.contains("\"runtime_config_report_enabled\":true"));
        assert!(!encoded.contains("\"runtime_config_report_format\":\"yaml\""));
    }

    // Shared tests 1-6 from plan
    #[test]
    fn test_shared_1_syslog_default_and_yaml_without_syslog() {
        let default_syslog = SyslogConfigDto::default();
        assert!(!default_syslog.enabled);
        assert_eq!(default_syslog.port, 514);
        assert_eq!(default_syslog.format, SyslogFormat::Rfc5424);
        assert_eq!(default_syslog.facility, SyslogFacility::Local0);
        assert_eq!(default_syslog.app_name, "tuliprox");
        assert_eq!(default_syslog.queue_capacity, 4096);
        assert_eq!(default_syslog.max_message_len, 1024);

        let yaml = "log_level: info\n";
        let log_dto: LogConfigDto = serde_saphyr::from_str(yaml).expect("deserialize yaml without syslog");
        assert_eq!(log_dto.syslog, SyslogConfigDto::default());
    }

    #[test]
    fn test_shared_2_syslog_yaml_parse_and_deny_unknown_fields() {
        let yaml = r#"
log_level: info
syslog:
  enabled: true
  host: "127.0.0.1"
  port: 5514
  format: json
  facility: daemon
  app_name: "testapp"
  log_level: debug
  queue_capacity: 2048
  max_message_len: 2048
"#;
        let log_dto: LogConfigDto = serde_saphyr::from_str(yaml).expect("deserialize syslog yaml");
        assert!(log_dto.syslog.enabled);
        assert_eq!(log_dto.syslog.host, "127.0.0.1");
        assert_eq!(log_dto.syslog.port, 5514);
        assert_eq!(log_dto.syslog.format, SyslogFormat::Json);
        assert_eq!(log_dto.syslog.facility, SyslogFacility::Daemon);
        assert_eq!(log_dto.syslog.app_name, "testapp");
        assert_eq!(log_dto.syslog.log_level, Some(LogLevel::Debug));
        assert_eq!(log_dto.syslog.queue_capacity, 2048);
        assert_eq!(log_dto.syslog.max_message_len, 2048);

        let bad_yaml = r#"
syslog:
  enabled: true
  host: "127.0.0.1"
  protocol: udp
"#;
        assert!(serde_saphyr::from_str::<LogConfigDto>(bad_yaml).is_err());
    }

    #[test]
    fn test_shared_3_syslog_round_trip_omits_empty_and_preserves_enabled() {
        let mut cfg = LogConfigDto::default();
        cfg.clean();
        let serialized = serde_saphyr::to_string(&cfg).expect("serialize default");
        assert!(!serialized.contains("syslog"));

        let mut cfg_enabled = LogConfigDto::default();
        cfg_enabled.syslog.enabled = true;
        cfg_enabled.syslog.host = "192.168.1.100".to_string();
        cfg_enabled.clean();
        let serialized_enabled = serde_saphyr::to_string(&cfg_enabled).expect("serialize enabled");
        assert!(serialized_enabled.contains("syslog"));
        assert!(serialized_enabled.contains("enabled: true"));
        assert!(serialized_enabled.contains("192.168.1.100"));

        let restored: LogConfigDto = serde_saphyr::from_str(&serialized_enabled).expect("round-trip deserialize");
        assert_eq!(restored.syslog, cfg_enabled.syslog);
    }

    #[test]
    fn test_shared_4_syslog_prestaged_host_survives_round_trip() {
        let mut cfg = LogConfigDto::default();
        cfg.syslog.host = "collector.example.com".to_string();
        cfg.clean();
        assert!(!cfg.syslog.is_empty());

        let serialized = serde_saphyr::to_string(&cfg).expect("serialize prestaged");
        assert!(serialized.contains("syslog"));
        assert!(serialized.contains("collector.example.com"));
        assert!(!serialized.contains("enabled: true"));

        let restored: LogConfigDto = serde_saphyr::from_str(&serialized).expect("deserialize prestaged");
        assert_eq!(restored.syslog.host, "collector.example.com");
        assert!(!restored.syslog.enabled);
    }

    #[test]
    fn test_shared_5_syslog_validation_rejects_invalid_fields() {
        // enabled == true && trimmed host empty
        let mut cfg = SyslogConfigDto { enabled: true, host: "   ".to_string(), ..SyslogConfigDto::default() };
        assert!(cfg.validate().is_err());

        // port == 0
        cfg.host = "127.0.0.1".to_string();
        cfg.port = 0;
        assert!(cfg.validate().is_err());
        cfg.port = 514;

        // app_name empty
        cfg.app_name = String::new();
        assert!(cfg.validate().is_err());

        // app_name > 48
        cfg.app_name = "a".repeat(49);
        assert!(cfg.validate().is_err());

        // app_name containing space
        cfg.app_name = "app name".to_string();
        assert!(cfg.validate().is_err());

        // app_name outside 0x21..=0x7E
        cfg.app_name = "app\x1f".to_string();
        assert!(cfg.validate().is_err());
        cfg.app_name = "tuliprox".to_string();

        // queue_capacity outside 1..=65536
        cfg.queue_capacity = 0;
        assert!(cfg.validate().is_err());
        cfg.queue_capacity = 65537;
        assert!(cfg.validate().is_err());
        cfg.queue_capacity = 65536;
        assert!(cfg.validate().is_ok());

        // hostname above the RFC 5424 HOSTNAME limit
        cfg.hostname = Some("h".repeat(256));
        assert!(cfg.validate().is_err());
        cfg.hostname = Some("h".repeat(255));
        assert!(cfg.validate().is_ok());
        cfg.hostname = None;

        // max_message_len below floor (480 for rfc5424/rfc3164, 1024 for json)
        cfg.format = SyslogFormat::Rfc5424;
        cfg.max_message_len = 479;
        assert!(cfg.validate().is_err());
        cfg.max_message_len = 480;
        assert!(cfg.validate().is_ok());

        cfg.format = SyslogFormat::Rfc3164;
        cfg.max_message_len = 479;
        assert!(cfg.validate().is_err());
        cfg.max_message_len = 480;
        assert!(cfg.validate().is_ok());

        cfg.format = SyslogFormat::Json;
        cfg.max_message_len = 1023;
        assert!(cfg.validate().is_err());
        cfg.max_message_len = 1024;
        assert!(cfg.validate().is_ok());

        // 65507 is the maximum IPv4 UDP payload; a larger datagram is rejected by the socket on every send
        cfg.max_message_len = 65507;
        assert!(cfg.validate().is_ok());
        cfg.max_message_len = 65508;
        assert!(cfg.validate().is_err());
        cfg.max_message_len = 65535;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_shared_6_syslog_validation_log_level() {
        let mut cfg = SyslogConfigDto { enabled: true, host: "127.0.0.1".to_string(), ..SyslogConfigDto::default() };

        cfg.log_level = Some(LogLevel::Trace);
        assert!(cfg.validate().is_err());

        cfg.log_level = Some(LogLevel::Debug);
        assert!(cfg.validate().is_ok());

        cfg.log_level = Some(LogLevel::Info);
        assert!(cfg.validate().is_ok());

        cfg.log_level = Some(LogLevel::Warn);
        assert!(cfg.validate().is_ok());

        cfg.log_level = Some(LogLevel::Error);
        assert!(cfg.validate().is_ok());

        cfg.log_level = None;
        assert!(cfg.validate().is_ok());
    }
}

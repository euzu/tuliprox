use crate::error::{TuliproxError, TuliproxErrorKind};
use crate::utils::{is_blank_optional_str, is_blank_optional_string};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct IpCheckConfigDto {
    /// URL that may return both IPv4 and IPv6 in one response
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub url: Option<String>,

    /// Dedicated URL to fetch only IPv4
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub url_ipv4: Option<String>,

    /// Dedicated URL to fetch only IPv6
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub url_ipv6: Option<String>,

    /// Optional regex pattern to extract IPv4
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub pattern_ipv4: Option<String>,

    /// Optional regex pattern to extract IPv6
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub pattern_ipv6: Option<String>,
}

impl IpCheckConfigDto {
    pub fn is_empty(&self) -> bool {
        is_blank_optional_str(self.url.as_deref())
            && is_blank_optional_str(self.url_ipv4.as_deref())
            && is_blank_optional_str(self.url_ipv6.as_deref())
            && is_blank_optional_str(self.pattern_ipv4.as_deref())
            && is_blank_optional_str(self.pattern_ipv6.as_deref())
    }

    pub fn clean(&mut self) {
        let norm = |s: &mut Option<String>| {
            if let Some(val) = s.as_mut() {
                let trimmed = val.trim();
                if trimmed.is_empty() {
                    *s = None;
                } else if trimmed.len() != val.len() {
                    *val = trimmed.to_owned();
                }
            }
        };
        norm(&mut self.url);
        norm(&mut self.url_ipv4);
        norm(&mut self.url_ipv6);
        norm(&mut self.pattern_ipv4);
        norm(&mut self.pattern_ipv6);
    }

    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        self.clean();
        if self.url.is_none() && self.url_ipv4.is_none() && self.url_ipv6.is_none() {
            return Err(TuliproxError::new(
                TuliproxErrorKind::Info,
                "No url provided!".to_owned(),
            ));
        }

        // TODO allow or do not allow ?
        // if self.url.is_some() && (self.url_ipv4.is_some() || self.url_ipv6.is_some()) {
        //     return Err(TuliproxError::new(TuliproxErrorKind::Info, "url in combination with ipv4 and/or ipv6 url not allowed!".to_owned()));
        // }

        if let Some(p4) = &self.pattern_ipv4 {
            crate::model::REGEX_CACHE.get_or_compile(p4).map_err(|err| {
                TuliproxError::new(
                    TuliproxErrorKind::Info,
                    format!("Invalid IPv4 regex: {p4} {err}"),
                )
            })?;
        }
        if let Some(p6) = &self.pattern_ipv6 {
            crate::model::REGEX_CACHE.get_or_compile(p6).map_err(|err| {
                TuliproxError::new(
                    TuliproxErrorKind::Info,
                    format!("Invalid IPv6 regex: {p6} {err}"),
                )
            })?;
        }
        Ok(())
    }
}

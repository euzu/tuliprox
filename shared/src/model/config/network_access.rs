use crate::error::TuliproxError;
use std::net::IpAddr;

fn is_empty_vec<T>(v: &Option<Vec<T>>) -> bool { v.as_ref().is_none_or(|v| v.is_empty()) }

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct NetworkAccessDto {
    #[serde(default, skip_serializing_if = "is_empty_vec")]
    pub allowed_countries: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "is_empty_vec")]
    pub allowed_networks: Option<Vec<String>>,
}

impl NetworkAccessDto {
    pub fn is_empty(&self) -> bool { is_empty_vec(&self.allowed_countries) && is_empty_vec(&self.allowed_networks) }

    /// Prepares the DTO for storage/comparison: trim whitespace, uppercase country codes,
    /// deduplicate country codes, validate CIDRs, normalize empty lists to None.
    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        if let Some(countries) = &mut self.allowed_countries {
            let mut seen = std::collections::HashSet::new();
            let mut deduped = Vec::new();
            for country in countries.drain(..) {
                let upper = country.trim().to_uppercase();
                if upper.is_empty() {
                    continue;
                }
                if upper.len() != 2 || !upper.chars().all(|ch| ch.is_ascii_alphabetic()) {
                    return Err(TuliproxError::ProxyUser(format!(
                        "Invalid network_access.allowed_countries entry '{upper}', expected 2-letter ISO code"
                    )));
                }
                if seen.insert(upper.clone()) {
                    deduped.push(upper);
                }
            }
            *countries = deduped;
        }
        if let Some(networks) = &mut self.allowed_networks {
            let mut deduped = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for network in networks.drain(..) {
                let trimmed = network.trim().to_string();
                if trimmed.is_empty() {
                    continue;
                }
                let Some((addr, prefix)) = trimmed.split_once('/') else {
                    return Err(TuliproxError::ProxyUser(format!(
                        "Invalid network_access.allowed_networks entry '{trimmed}', expected CIDR"
                    )));
                };
                let ip = addr.trim().parse::<IpAddr>().map_err(|_| {
                    TuliproxError::ProxyUser(format!(
                        "Invalid network_access.allowed_networks entry '{trimmed}', expected CIDR"
                    ))
                })?;
                let prefix = prefix.trim().parse::<u8>().map_err(|_| {
                    TuliproxError::ProxyUser(format!(
                        "Invalid network_access.allowed_networks entry '{trimmed}', expected CIDR"
                    ))
                })?;
                let prefix_valid = match ip {
                    IpAddr::V4(_) => prefix <= 32,
                    IpAddr::V6(_) => prefix <= 128,
                };
                if !prefix_valid {
                    return Err(TuliproxError::ProxyUser(format!(
                        "Invalid network_access.allowed_networks entry '{trimmed}', expected CIDR"
                    )));
                }
                if seen.insert(trimmed.clone()) {
                    deduped.push(trimmed);
                }
            }
            *networks = deduped;
        }
        if self.allowed_countries.as_ref().is_some_and(|c| c.is_empty()) {
            self.allowed_countries = None;
        }
        if self.allowed_networks.as_ref().is_some_and(|n| n.is_empty()) {
            self.allowed_networks = None;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_empty() {
        let dto = NetworkAccessDto::default();
        assert_eq!(dto.allowed_countries, None);
        assert_eq!(dto.allowed_networks, None);
        assert!(dto.is_empty());
    }

    #[test]
    fn empty_vecs_is_empty() {
        let dto = NetworkAccessDto { allowed_countries: Some(vec![]), allowed_networks: Some(vec![]) };
        assert!(dto.is_empty());
    }

    #[test]
    fn non_empty_is_not_empty() {
        let dto = NetworkAccessDto { allowed_countries: Some(vec!["DE".to_string()]), allowed_networks: None };
        assert!(!dto.is_empty());
    }

    #[test]
    fn prepare_uppercases_and_deduplicates_countries() {
        let mut dto = NetworkAccessDto {
            allowed_countries: Some(vec!["de".to_string(), "DE".to_string(), "At".to_string()]),
            allowed_networks: None,
        };
        dto.prepare().unwrap();
        let countries = dto.allowed_countries.unwrap();
        assert_eq!(countries, vec!["DE", "AT"]);
    }

    #[test]
    fn prepare_normalizes_empty_to_none() {
        let mut dto = NetworkAccessDto { allowed_countries: Some(vec![]), allowed_networks: Some(vec![]) };
        dto.prepare().unwrap();
        assert_eq!(dto.allowed_countries, None);
        assert_eq!(dto.allowed_networks, None);
        assert!(dto.is_empty());
    }

    #[test]
    fn deserialize_from_yaml_fragment() {
        let value = serde_json::json!({
            "allowed_countries": ["DE", "AT", "CH"],
            "allowed_networks": ["192.168.1.0/24", "10.0.0.0/8"]
        });
        let dto: NetworkAccessDto = serde_json::from_value(value).unwrap();
        assert_eq!(dto.allowed_countries, Some(vec!["DE".to_string(), "AT".to_string(), "CH".to_string()]));
        assert_eq!(dto.allowed_networks, Some(vec!["192.168.1.0/24".to_string(), "10.0.0.0/8".to_string()]));
    }

    #[test]
    fn serialize_omits_none_fields() {
        let dto = NetworkAccessDto::default();
        let serialized = serde_json::to_value(&dto).unwrap();
        assert!(!serialized.as_object().unwrap().contains_key("allowed_countries"));
        assert!(!serialized.as_object().unwrap().contains_key("allowed_networks"));
    }

    #[test]
    fn serialize_omits_empty_vec_fields() {
        let dto = NetworkAccessDto { allowed_countries: Some(vec![]), allowed_networks: Some(vec![]) };
        let serialized = serde_json::to_value(&dto).unwrap();
        assert!(!serialized.as_object().unwrap().contains_key("allowed_countries"));
        assert!(!serialized.as_object().unwrap().contains_key("allowed_networks"));
    }

    #[test]
    fn roundtrip_preserves_content() {
        let dto = NetworkAccessDto {
            allowed_countries: Some(vec!["DE".to_string()]),
            allowed_networks: Some(vec!["10.0.0.0/8".to_string()]),
        };
        let serialized = serde_json::to_value(&dto).unwrap();
        let deserialized: NetworkAccessDto = serde_json::from_value(serialized).unwrap();
        assert_eq!(dto, deserialized);
    }

    #[test]
    fn prepare_trims_whitespace() {
        let mut dto = NetworkAccessDto {
            allowed_countries: Some(vec!["  de  ".to_string()]),
            allowed_networks: Some(vec!["  10.0.0.0/8  ".to_string()]),
        };
        dto.prepare().unwrap();
        assert_eq!(dto.allowed_countries, Some(vec!["DE".to_string()]));
        assert_eq!(dto.allowed_networks, Some(vec!["10.0.0.0/8".to_string()]));
    }

    #[test]
    fn prepare_rejects_invalid_country_code() {
        let mut dto = NetworkAccessDto { allowed_countries: Some(vec!["DEU".to_string()]), allowed_networks: None };
        assert!(dto.prepare().is_err());
    }

    #[test]
    fn prepare_rejects_invalid_cidr() {
        let mut dto =
            NetworkAccessDto { allowed_countries: None, allowed_networks: Some(vec!["not-a-cidr".to_string()]) };
        assert!(dto.prepare().is_err());
    }
}

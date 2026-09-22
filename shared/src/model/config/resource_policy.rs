use serde::{Deserialize, Serialize};

/// Trusted private destinations for the resource URLs supplied by one input.
///
/// `allowed_hosts` holds exact DNS names, `allowed_networks` holds private CIDR ranges. Both
/// empty (or an absent policy) means public destinations only, which is the default for every
/// input. A private target is authorized only when the host name *and* the resolved address are
/// covered, so enabling one private logo host cannot open a whole subnet.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePolicyDto {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_hosts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_networks: Vec<String>,
}

impl ResourcePolicyDto {
    pub fn is_empty(&self) -> bool { self.allowed_hosts.is_empty() && self.allowed_networks.is_empty() }
}

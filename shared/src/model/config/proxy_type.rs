use std::fmt::Display;
use std::hash::{Hash, Hasher};
use std::str::FromStr;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use crate::error::{TuliproxError, info_err_res};
use crate::model::{ClusterFlags, PlaylistItemType};

#[derive(Debug, Default, Copy, Clone)]
pub enum ProxyType {
    Reverse(Option<ClusterFlags>),
    #[default]
    Redirect,
}

impl PartialEq for ProxyType {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (ProxyType::Redirect, ProxyType::Redirect) => true,
            (ProxyType::Reverse(a), ProxyType::Reverse(b)) => {
                let a_flags = a.map_or(0u16, |f| if f.has_full_flags() { 0u16 } else { f.bits() } );
                let b_flags = b.map_or(0u16, |f| if f.has_full_flags() { 0u16 } else { f.bits() } );
                a_flags == b_flags
            }
            _ => false,
        }
    }
}

impl Eq for ProxyType {}

impl Ord for ProxyType {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (ProxyType::Redirect, ProxyType::Redirect) => std::cmp::Ordering::Equal,
            (ProxyType::Reverse(_), ProxyType::Reverse(_)) => std::cmp::Ordering::Equal,
            (ProxyType::Redirect, _) => std::cmp::Ordering::Less,
            (ProxyType::Reverse(_), _) => std::cmp::Ordering::Greater,
        }
    }
}

impl PartialOrd for ProxyType {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Hash for ProxyType {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            ProxyType::Redirect => {
                0u8.hash(state);
            }
            ProxyType::Reverse(flags_opt) => {
                1u8.hash(state);
                let flags = flags_opt.map_or(0u16, |f| if f.has_full_flags() { 0u16 } else { f.bits() } );
                flags.hash(state);
            }
        }
    }
}

impl ProxyType {
    const REVERSE: &'static str = "reverse";
    const REDIRECT: &'static str = "redirect";

    pub fn is_redirect(&self, item_type: PlaylistItemType) -> bool {
        if item_type.is_local() {
            return false;
        }
        match self {
            ProxyType::Reverse(Some(flags)) => {
                if flags.is_empty() {
                    return false;
                }
                if flags.has_cluster(item_type) {
                    return false;
                }
                true
            },
            ProxyType::Reverse(None) => false,
            ProxyType::Redirect => true
        }
    }

    pub fn is_reverse(&self, item_type: PlaylistItemType) -> bool {
        !self.is_redirect(item_type)
    }
}

impl Display for ProxyType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reverse(force_redirect) => {
                if let Some(force) = force_redirect {
                    write!(f, "{}{force:?}", Self::REVERSE)
                } else {
                    write!(f, "{}", Self::REVERSE)
                }
            }
            Self::Redirect => write!(f, "{}", Self::REDIRECT),
        }
    }
}

impl FromStr for ProxyType {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        if s == Self::REDIRECT {
            return Ok(Self::Redirect);
        }
        if s == Self::REVERSE {
            return Ok(Self::Reverse(None));
        }

        if let Some(suffix) = s.strip_prefix(Self::REVERSE) {
            if let Ok(force_redirect) = ClusterFlags::try_from(suffix) {
                if force_redirect.has_full_flags() {
                    return Ok(ProxyType::Reverse(None));
                }
                return Ok(Self::Reverse(Some(force_redirect)));
            }
        }

        info_err_res!("Unknown ProxyType: {}", s)
    }
}

impl<'de> Deserialize<'de> for ProxyType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw: String = Deserialize::deserialize(deserializer)?;
        if raw == ProxyType::REDIRECT {
            return Ok(ProxyType::Redirect);
        } else if raw.starts_with(ProxyType::REVERSE) {
            return ProxyType::from_str(raw.as_str()).map_err(serde::de::Error::custom);
        }
        Err(serde::de::Error::custom("Unknown proxy type"))
    }
}

impl Serialize for ProxyType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match *self {
            ProxyType::Redirect => serializer.serialize_str(ProxyType::REDIRECT),
            ProxyType::Reverse(None) => serializer.serialize_str(ProxyType::REVERSE),
            ProxyType::Reverse(Some(ref force_redirect)) => {
                serializer.serialize_str(&format!("{}{}", ProxyType::REVERSE, force_redirect))
            },
        }
    }
}

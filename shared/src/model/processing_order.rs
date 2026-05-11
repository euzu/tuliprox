use crate::error::TuliproxError;
use std::{fmt::Display, str::FromStr};
use strum_macros::AsRefStr;
#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Default, AsRefStr)]
#[strum(serialize_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum ProcessingOrder {
    #[default]
    Frm,
    Fmr,
    Rfm,
    Rmf,
    Mfr,
    Mrf,
}

impl ProcessingOrder {
    const FRM: &'static str = "filter, rename, map";
    const FMR: &'static str = "filter, map, rename";
    const RFM: &'static str = "rename, filter, map";
    const RMF: &'static str = "rename, map, filter";
    const MFR: &'static str = "map, filter, rename";
    const MRF: &'static str = "map, rename, filter";
}

impl Display for ProcessingOrder {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match *self {
                Self::Frm => Self::FRM,
                Self::Fmr => Self::FMR,
                Self::Rfm => Self::RFM,
                Self::Rmf => Self::RMF,
                Self::Mfr => Self::MFR,
                Self::Mrf => Self::MRF,
            }
        )
    }
}

impl FromStr for ProcessingOrder {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized = s.trim().to_lowercase();

        match normalized.as_str() {
            // Short codes
            "frm" => Ok(Self::Frm),
            "fmr" => Ok(Self::Fmr),
            "rfm" => Ok(Self::Rfm),
            "rmf" => Ok(Self::Rmf),
            "mfr" => Ok(Self::Mfr),
            "mrf" => Ok(Self::Mrf),

            // Accept the exact Display text (using your constants)
            x if x == Self::FRM => Ok(Self::Frm),
            x if x == Self::FMR => Ok(Self::Fmr),
            x if x == Self::RFM => Ok(Self::Rfm),
            x if x == Self::RMF => Ok(Self::Rmf),
            x if x == Self::MFR => Ok(Self::Mfr),
            x if x == Self::MRF => Ok(Self::Mrf),

            _ => Err(TuliproxError::Config(format!("Invalid processing order {s}"))),
        }
    }
}

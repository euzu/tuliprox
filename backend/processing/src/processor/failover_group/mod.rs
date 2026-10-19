mod assemble;
mod resolve;
#[cfg(test)]
mod tests;

pub(in crate::processor) use assemble::*;
pub use assemble::{derive_group_metadata, GroupMetadata, MemberMetadata};
pub use resolve::{
    group_uuid, resolve_failover_groups, CandidateOrigin, CatalogEntry, FailoverGroupResolution, ResolvedCandidate,
};

//! Local media library: scanning, metadata resolution and enrichment.
//!
//! `library` scans and resolves; `ptt` parses release titles; `media_enrichment`
//! maps resolved metadata onto playlist items. `ptt` exists only for the other
//! two and `media_enrichment` reads `library`'s types directly, so the three are
//! one package rather than three joined by interfaces.
//!
//! Nothing here names the API layer, a repository, or the playlist pipeline.

pub mod library;
pub mod media_enrichment;
pub mod ptt;

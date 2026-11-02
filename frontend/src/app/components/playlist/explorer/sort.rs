use shared::model::{ExplorerEpgState, UiPlaylistItem, XtreamCluster};
use std::rc::Rc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExplorerSort {
    ChannelNo,
    Name,
    Group,
    Added,
    Title,
    Year,
    Rating,
    LastModified,
}

impl ExplorerSort {
    const ALL: [Self; 8] = [
        Self::ChannelNo,
        Self::Name,
        Self::Group,
        Self::Added,
        Self::Title,
        Self::Year,
        Self::Rating,
        Self::LastModified,
    ];

    /// Stable id used as the option value of the sort selector.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::ChannelNo => "ChannelNo",
            Self::Name => "Name",
            Self::Group => "Group",
            Self::Added => "Added",
            Self::Title => "Title",
            Self::Year => "Year",
            Self::Rating => "Rating",
            Self::LastModified => "LastModified",
        }
    }

    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> { Self::ALL.into_iter().find(|s| s.id() == id) }

    /// Direction a sort starts with: newest, best rated and latest year first; text and numbers ascending.
    #[must_use]
    pub fn default_desc(self) -> bool { matches!(self, Self::Added | Self::Year | Self::Rating | Self::LastModified) }

    #[must_use]
    pub fn label_key(&self) -> &'static str {
        match self {
            Self::ChannelNo => "LABEL.EXPLORER_SORT_CHANNEL_NO",
            Self::Name => "LABEL.NAME",
            Self::Group => "LABEL.GROUP",
            Self::Added => "LABEL.EXPLORER_SORT_ADDED",
            Self::Title => "LABEL.TITLE",
            Self::Year => "LABEL.EXPLORER_SORT_YEAR",
            Self::Rating => "LABEL.EXPLORER_SORT_RATING",
            Self::LastModified => "LABEL.EXPLORER_SORT_LAST_MODIFIED",
        }
    }
}

#[must_use]
pub fn default_sort_for_kind(cluster: XtreamCluster) -> (ExplorerSort, bool) {
    match cluster {
        XtreamCluster::Live => (ExplorerSort::ChannelNo, false),
        XtreamCluster::Video => (ExplorerSort::Added, true),
        XtreamCluster::Series => (ExplorerSort::LastModified, true),
    }
}

/// Channel number the stored playlist carries for `item` (explorer metadata), `None` when unset.
#[must_use]
pub fn item_chno(item: &UiPlaylistItem) -> Option<u32> { item.explorer_meta.as_ref().and_then(|m| m.chno) }

#[must_use]
pub fn extract_year(name: &str, title: &str) -> Option<u32> {
    for s in [title, name] {
        let bytes = s.as_bytes();
        for i in 0..bytes.len().saturating_sub(3) {
            if (i == 0 || !bytes[i - 1].is_ascii_digit()) && (i + 4 == bytes.len() || !bytes[i + 4].is_ascii_digit()) {
                if let Ok(candidate) = std::str::from_utf8(&bytes[i..i + 4]) {
                    if let Ok(year) = candidate.parse::<u32>() {
                        if (1900..=2099).contains(&year) {
                            return Some(year);
                        }
                    }
                }
            }
        }
    }
    None
}

#[must_use]
pub fn extract_rating(rating: f64) -> Option<u64> {
    if rating > 0.001 && !rating.is_nan() {
        Some((rating * 1000.0).round() as u64)
    } else {
        None
    }
}

#[must_use]
pub fn extract_name(name: &str) -> Option<&str> {
    if name.trim().is_empty() {
        None
    } else {
        Some(name)
    }
}

#[must_use]
pub fn extract_title(title: &str) -> Option<&str> {
    if title.trim().is_empty() {
        None
    } else {
        Some(title)
    }
}

#[must_use]
pub fn extract_group(group: &str) -> Option<&str> {
    if group.trim().is_empty() {
        None
    } else {
        Some(group)
    }
}

/// Translation key of the playlist cluster name.
#[must_use]
pub fn cluster_label_key(cluster: XtreamCluster) -> &'static str {
    match cluster {
        XtreamCluster::Live => "LABEL.LIVE",
        XtreamCluster::Video => "LABEL.VOD",
        XtreamCluster::Series => "LABEL.SERIES",
    }
}

#[must_use]
pub fn extract_added(item: &UiPlaylistItem) -> Option<u64> { item.explorer_meta.as_ref().and_then(|m| m.added) }

#[must_use]
pub fn extract_last_modified(item: &UiPlaylistItem) -> Option<u64> {
    item.explorer_meta.as_ref().and_then(|m| {
        if item.xtream_cluster == XtreamCluster::Series {
            m.last_modified.or(m.added)
        } else {
            m.last_modified
        }
    })
}

/// Window in which a newly added VOD item, or an updated series, is considered fresh.
pub const NEW_ITEM_WINDOW_SECS: u64 = 7 * 24 * 60 * 60;

/// `true` when `timestamp` lies inside the last [`NEW_ITEM_WINDOW_SECS`] before `now`.
///
/// Timestamps come from the provider and may be missing or in the future; only a real value
/// inside the window counts, so the UI never invents a "new" badge.
#[must_use]
pub fn is_within_new_window(timestamp: Option<u64>, now: u64) -> bool {
    timestamp.is_some_and(|ts| ts <= now && now - ts <= NEW_ITEM_WINDOW_SECS)
}

fn compare_with_missing_last<T: Ord>(
    opt_a: Option<T>,
    opt_b: Option<T>,
    desc: bool,
    provider_id_a: &str,
    provider_id_b: &str,
) -> std::cmp::Ordering {
    let both_missing = opt_a.is_none() && opt_b.is_none();
    let cmp = match (opt_a, opt_b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(a), Some(b)) => {
            if desc {
                b.cmp(&a)
            } else {
                a.cmp(&b)
            }
        }
    };
    // Both values missing: keep the playlist order (the sort is stable). Only equal values tie-break
    // on provider id, so an input without channel numbers is not reshuffled by id.
    if cmp == std::cmp::Ordering::Equal && !both_missing {
        provider_id_a.cmp(provider_id_b)
    } else {
        cmp
    }
}

pub fn sort_items(items: &mut [Rc<UiPlaylistItem>], sort: ExplorerSort, desc: bool) {
    items.sort_by(|a, b| match sort {
        ExplorerSort::ChannelNo => {
            compare_with_missing_last(item_chno(a), item_chno(b), desc, &a.provider_id, &b.provider_id)
        }
        ExplorerSort::Name => compare_with_missing_last(
            extract_name(&a.name),
            extract_name(&b.name),
            desc,
            &a.provider_id,
            &b.provider_id,
        ),
        ExplorerSort::Group => compare_with_missing_last(
            extract_group(&a.group),
            extract_group(&b.group),
            desc,
            &a.provider_id,
            &b.provider_id,
        ),
        ExplorerSort::Title => compare_with_missing_last(
            extract_title(&a.title),
            extract_title(&b.title),
            desc,
            &a.provider_id,
            &b.provider_id,
        ),
        ExplorerSort::Year => compare_with_missing_last(
            extract_year(&a.name, &a.title),
            extract_year(&b.name, &b.title),
            desc,
            &a.provider_id,
            &b.provider_id,
        ),
        ExplorerSort::Rating => compare_with_missing_last(
            extract_rating(a.rating),
            extract_rating(b.rating),
            desc,
            &a.provider_id,
            &b.provider_id,
        ),
        ExplorerSort::Added => {
            compare_with_missing_last(extract_added(a), extract_added(b), desc, &a.provider_id, &b.provider_id)
        }
        ExplorerSort::LastModified => compare_with_missing_last(
            extract_last_modified(a),
            extract_last_modified(b),
            desc,
            &a.provider_id,
            &b.provider_id,
        ),
    });
}

#[must_use]
pub fn is_without_epg(item: &UiPlaylistItem) -> bool {
    match item.epg_state() {
        Some(ExplorerEpgState::MissingId | ExplorerEpgState::NotInSources | ExplorerEpgState::NoProgramme) => true,
        Some(ExplorerEpgState::Ok) => false,
        None => item.epg_channel_id.is_none(),
    }
}

#[must_use]
pub fn count_without_epg(items: &[Rc<UiPlaylistItem>]) -> usize { items.iter().filter(|i| is_without_epg(i)).count() }

#[must_use]
pub fn is_edited(item: &UiPlaylistItem) -> bool { item.override_state().is_some_and(|s| s.edited) }

#[must_use]
pub fn count_edited(items: &[Rc<UiPlaylistItem>]) -> usize { items.iter().filter(|i| is_edited(i)).count() }

#[must_use]
pub fn epg_badge_label_key(state: Option<ExplorerEpgState>) -> Option<&'static str> {
    match state {
        Some(ExplorerEpgState::MissingId) => Some("LABEL.EXPLORER_EPG_MISSING_ID"),
        Some(ExplorerEpgState::NotInSources) => Some("LABEL.EXPLORER_EPG_NOT_IN_SOURCES"),
        Some(ExplorerEpgState::NoProgramme) => Some("LABEL.EXPLORER_EPG_NO_PROGRAMME"),
        Some(ExplorerEpgState::Ok) | None => None,
    }
}

#[cfg(test)]
#[path = "sort.test.rs"]
mod tests;

/// Rating filter of VOD/series: when active, keep items rated at least `min` stars (0-10 scale).
#[must_use]
pub fn passes_rating_filter(item: &UiPlaylistItem, active: bool, min: u8) -> bool {
    !active || item.rating >= f64::from(min)
}

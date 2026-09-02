use shared::model::{PlaylistClusterBouquetDto, XtreamCluster};
use std::collections::HashSet;

/// Target-local compiled bouquet filter evaluated per item before transformation.
#[derive(Debug, Clone, PartialEq)]
pub struct TargetBouquetFilter {
    live: Option<HashSet<String>>,
    vod: Option<HashSet<String>>,
    series: Option<HashSet<String>>,
}

impl TargetBouquetFilter {
    pub fn from_dto(mut groups: PlaylistClusterBouquetDto) -> Option<Self> {
        groups.canonicalize_for_target();
        if groups.is_target_unrestricted() {
            return None;
        }
        let live = groups.live.map(|v| v.into_iter().collect::<HashSet<String>>());
        let vod = groups.vod.map(|v| v.into_iter().collect::<HashSet<String>>());
        let series = groups.series.map(|v| v.into_iter().collect::<HashSet<String>>());
        Some(Self { live, vod, series })
    }

    #[inline]
    fn for_cluster(&self, cluster: XtreamCluster) -> Option<&HashSet<String>> {
        match cluster {
            XtreamCluster::Live => self.live.as_ref(),
            XtreamCluster::Video => self.vod.as_ref(),
            XtreamCluster::Series => self.series.as_ref(),
        }
    }

    #[inline]
    pub fn allows(&self, cluster: XtreamCluster, group: &str) -> bool {
        self.for_cluster(cluster).is_none_or(|allowed| allowed.contains(group))
    }

    pub fn cluster_counts(&self) -> (Option<usize>, Option<usize>, Option<usize>) {
        (
            self.live.as_ref().map(HashSet::len),
            self.vod.as_ref().map(HashSet::len),
            self.series.as_ref().map(HashSet::len),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_all_when_cluster_is_none() {
        let filter = TargetBouquetFilter::from_dto(PlaylistClusterBouquetDto {
            live: Some(vec!["News".to_string()]),
            vod: None,
            series: None,
        })
        .expect("filter should be Some");

        assert!(filter.allows(XtreamCluster::Live, "News"));
        assert!(!filter.allows(XtreamCluster::Live, "Sports"));
        assert!(filter.allows(XtreamCluster::Video, "Any Movie"));
        assert!(filter.allows(XtreamCluster::Series, "Any Series"));
    }

    #[test]
    fn case_sensitive_exact_matching() {
        let filter = TargetBouquetFilter::from_dto(PlaylistClusterBouquetDto {
            live: Some(vec!["news".to_string(), "".to_string()]),
            vod: None,
            series: None,
        })
        .expect("filter should be Some");

        assert!(filter.allows(XtreamCluster::Live, "news"));
        assert!(!filter.allows(XtreamCluster::Live, "News"));
        assert!(filter.allows(XtreamCluster::Live, ""));
    }

    #[test]
    fn returns_none_when_unrestricted() {
        let filter = TargetBouquetFilter::from_dto(PlaylistClusterBouquetDto { live: None, vod: None, series: None });
        assert!(filter.is_none());

        let filter_empty =
            TargetBouquetFilter::from_dto(PlaylistClusterBouquetDto { live: Some(vec![]), vod: None, series: None });
        assert!(filter_empty.is_none());
    }

    #[test]
    fn uncanonicalized_empty_cluster_is_canonicalized_to_none() {
        let filter = TargetBouquetFilter::from_dto(PlaylistClusterBouquetDto {
            live: Some(vec!["News".to_string()]),
            vod: Some(vec![]),
            series: None,
        })
        .expect("filter should be Some");

        assert!(filter.allows(XtreamCluster::Live, "News"));
        assert!(!filter.allows(XtreamCluster::Live, "Sports"));
        // vod was Some(vec![]), but canonicalization converts it to None, so all VOD is allowed
        assert!(filter.allows(XtreamCluster::Video, "Any Movie"));
    }
}

use indexmap::IndexMap;
use shared::model::XtreamCluster;

pub trait WithCapacity {
    fn with_capacity(capacity: usize) -> Self;
}

impl<T> WithCapacity for Vec<T> {
    fn with_capacity(capacity: usize) -> Self { Vec::with_capacity(capacity) }
}

impl<K, V> WithCapacity for IndexMap<K, V> {
    fn with_capacity(capacity: usize) -> Self { IndexMap::with_capacity(capacity) }
}

pub trait IsEmpty {
    fn is_empty(&self) -> bool;
}

impl<T> IsEmpty for Vec<T> {
    fn is_empty(&self) -> bool { Vec::is_empty(self) }
}

impl<K, V> IsEmpty for IndexMap<K, V> {
    fn is_empty(&self) -> bool { IndexMap::is_empty(self) }
}

pub struct PlaylistScratch<C> {
    live: C,
    vods: C,
    series: C,
}

impl<C: WithCapacity> PlaylistScratch<C> {
    pub fn new(capacity: usize) -> Self {
        Self { live: C::with_capacity(capacity), vods: C::with_capacity(capacity), series: C::with_capacity(capacity) }
    }
}

impl<C> PlaylistScratch<C> {
    pub fn get_mut(&mut self, cluster: XtreamCluster) -> &mut C {
        match cluster {
            XtreamCluster::Live => &mut self.live,
            XtreamCluster::Video => &mut self.vods,
            XtreamCluster::Series => &mut self.series,
        }
    }

    // pub fn get(&self, cluster: XtreamCluster) -> &C {
    //     match cluster {
    //         XtreamCluster::Live => &self.live,
    //         XtreamCluster::Video => &self.vods,
    //         XtreamCluster::Series => &self.series,
    //     }
    // }

    pub fn set(&mut self, cluster: XtreamCluster, content: C) {
        match cluster {
            XtreamCluster::Live => self.live = content,
            XtreamCluster::Video => self.vods = content,
            XtreamCluster::Series => self.series = content,
        }
    }
}

impl<C: Default> PlaylistScratch<C> {
    pub fn take(&mut self, cluster: XtreamCluster) -> C {
        match cluster {
            XtreamCluster::Live => std::mem::take(&mut self.live),
            XtreamCluster::Video => std::mem::take(&mut self.vods),
            XtreamCluster::Series => std::mem::take(&mut self.series),
        }
    }
}

impl<C: IsEmpty> PlaylistScratch<C> {
    pub fn is_empty(&self, cluster: XtreamCluster) -> bool {
        match cluster {
            XtreamCluster::Live => self.live.is_empty(),
            XtreamCluster::Video => self.vods.is_empty(),
            XtreamCluster::Series => self.series.is_empty(),
        }
    }
}

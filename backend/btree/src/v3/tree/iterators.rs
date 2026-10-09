use super::{
    invalid_data, search_leaf, BPlusTreeDiskIterator, BPlusTreeDiskIteratorOwned, BPlusTreeQuery,
    BPlusTreeRangeIterator, PageType, SlottedPage,
};
use serde::Deserialize;
use std::{
    collections::HashSet,
    io,
    ops::{Bound, Range},
};

pub(super) struct CursorState<K, V> {
    pub(super) leaf_page_id: u64,
    pub(super) slot_index: usize,
    pub(super) cell_ranges: Vec<Range<usize>>,
    pub(super) right_sibling: u64,
    pub(super) expected_left_sibling: Option<u64>,
    pub(super) page_loaded: bool,
    pub(super) start: Bound<K>,
    pub(super) initialized: bool,
    pub(super) finished: bool,
    pub(super) visited_leaves: HashSet<u64>,
    pub(super) pending: Option<(K, V)>,
}

impl<K, V> CursorState<K, V> {
    pub(super) fn new(start: Bound<K>) -> Self {
        Self {
            leaf_page_id: 0,
            slot_index: 0,
            cell_ranges: Vec::new(),
            right_sibling: 0,
            expected_left_sibling: None,
            page_loaded: false,
            start,
            initialized: false,
            finished: false,
            visited_leaves: HashSet::new(),
            pending: None,
        }
    }
}

fn load_cursor_page<K, V>(query: &mut BPlusTreeQuery<K, V>, state: &mut CursorState<K, V>) -> io::Result<()> {
    let page_id = state.leaf_page_id;
    let next_page_id = query.header.next_page_id;
    let expected_left = state.expected_left_sibling;
    let mut cell_ranges = std::mem::take(&mut state.cell_ranges);
    let right_sibling = query.with_page(page_id, |bytes, _| {
        let page = SlottedPage::open(bytes, page_id, next_page_id)?;
        if page.header().page_type != PageType::Leaf {
            return Err(invalid_data("iterator sibling is not a leaf"));
        }
        if expected_left.is_some_and(|left| page.header().left != left) {
            return Err(invalid_data("asymmetric leaf sibling link"));
        }
        let count = usize::from(page.header().cell_count);
        cell_ranges.clear();
        cell_ranges.try_reserve(count).map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
        for index in 0..count {
            cell_ranges.push(page.cell_range(index)?);
        }
        Ok(page.header().right)
    })?;
    state.cell_ranges = cell_ranges;
    state.right_sibling = right_sibling;
    state.page_loaded = true;
    Ok(())
}

pub(super) fn record_page_visit(visited: &mut HashSet<u64>, page_id: u64, cycle_error: &'static str) -> io::Result<()> {
    visited.try_reserve(1).map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
    if !visited.insert(page_id) {
        return Err(invalid_data(cycle_error));
    }
    Ok(())
}

fn cursor_next<K, V>(query: &mut BPlusTreeQuery<K, V>, state: &mut CursorState<K, V>) -> Option<io::Result<(K, V)>>
where
    K: Ord + for<'de> Deserialize<'de>,
    V: for<'de> Deserialize<'de>,
{
    if let Some(entry) = state.pending.take() {
        return Some(Ok(entry));
    }
    if state.finished {
        return None;
    }
    let mut entry_error = false;
    let result = (|| {
        if !state.initialized {
            state.leaf_page_id = match &state.start {
                Bound::Included(key) | Bound::Excluded(key) => query.locate_leaf(key)?,
                Bound::Unbounded => query.leftmost_leaf()?,
            };
            record_page_visit(&mut state.visited_leaves, state.leaf_page_id, "right sibling chain contains a cycle")?;
            if let Bound::Included(key) | Bound::Excluded(key) = &state.start {
                let page_id = state.leaf_page_id;
                let next_page_id = query.header.next_page_id;
                state.slot_index = query.with_page(page_id, |bytes, _| {
                    let page = SlottedPage::open(bytes, page_id, next_page_id)?;
                    let index = match search_leaf(&page, key)? {
                        Ok(index) if matches!(state.start, Bound::Excluded(_)) => index + 1,
                        Ok(index) | Err(index) => index,
                    };
                    Ok(index)
                })?;
            }
            state.initialized = true;
        }
        loop {
            if !state.page_loaded {
                load_cursor_page(query, state)?;
            }
            let page_id = state.leaf_page_id;
            while state.slot_index < state.cell_ranges.len() {
                let index = state.slot_index;
                state.slot_index += 1;
                match query.decode_entry_range(page_id, state.cell_ranges[index].clone()) {
                    Ok(Some(entry)) => return Ok(Some(entry)),
                    Ok(None) => {}
                    Err(error) => {
                        entry_error = true;
                        return Err(error);
                    }
                }
            }
            let right = state.right_sibling;
            if right == 0 {
                return Ok(None);
            }
            record_page_visit(&mut state.visited_leaves, right, "right sibling chain contains a cycle")?;
            state.expected_left_sibling = Some(page_id);
            state.leaf_page_id = right;
            state.slot_index = 0;
            state.page_loaded = false;
            state.cell_ranges.clear();
        }
    })();
    match result {
        Ok(Some(entry)) => Some(Ok(entry)),
        Ok(None) => {
            state.finished = true;
            None
        }
        Err(err) => {
            state.finished = !entry_error;
            Some(Err(err))
        }
    }
}

impl<'a, K, V> BPlusTreeDiskIterator<'a, K, V> {
    pub(super) fn new(query: &'a mut BPlusTreeQuery<K, V>) -> Self {
        Self { query, state: CursorState::new(Bound::Unbounded) }
    }

    pub(super) fn from_bound(query: &'a mut BPlusTreeQuery<K, V>, start: Bound<K>) -> Self {
        Self { query, state: CursorState::new(start) }
    }
}

impl<K, V> BPlusTreeDiskIterator<'_, K, V>
where
    K: Ord + for<'de> Deserialize<'de>,
    V: for<'de> Deserialize<'de>,
{
    pub fn try_is_empty(&mut self) -> io::Result<bool> {
        match self.next() {
            None => Ok(true),
            Some(Ok(entry)) => {
                self.state.pending = Some(entry);
                Ok(false)
            }
            Some(Err(err)) => Err(err),
        }
    }
}

impl<K, V> Iterator for BPlusTreeDiskIterator<'_, K, V>
where
    K: Ord + for<'de> Deserialize<'de>,
    V: for<'de> Deserialize<'de>,
{
    type Item = io::Result<(K, V)>;

    fn next(&mut self) -> Option<Self::Item> { cursor_next(self.query, &mut self.state) }
}

impl<K, V> BPlusTreeDiskIteratorOwned<K, V> {
    pub(super) fn new(query: BPlusTreeQuery<K, V>) -> Self { Self { query, state: CursorState::new(Bound::Unbounded) } }
}

impl<K, V> BPlusTreeDiskIteratorOwned<K, V>
where
    K: Ord + for<'de> Deserialize<'de>,
    V: for<'de> Deserialize<'de>,
{
    pub fn try_is_empty(&mut self) -> io::Result<bool> {
        match self.next() {
            None => Ok(true),
            Some(Ok(entry)) => {
                self.state.pending = Some(entry);
                Ok(false)
            }
            Some(Err(err)) => Err(err),
        }
    }
}

impl<K, V> Iterator for BPlusTreeDiskIteratorOwned<K, V>
where
    K: Ord + for<'de> Deserialize<'de>,
    V: for<'de> Deserialize<'de>,
{
    type Item = io::Result<(K, V)>;

    fn next(&mut self) -> Option<Self::Item> { cursor_next(&mut self.query, &mut self.state) }
}

fn within_start<K: Ord>(key: &K, bound: &Bound<K>) -> bool {
    match bound {
        Bound::Included(start) => key >= start,
        Bound::Excluded(start) => key > start,
        Bound::Unbounded => true,
    }
}

fn past_end<K: Ord>(key: &K, bound: &Bound<K>) -> bool {
    match bound {
        Bound::Included(end) => key > end,
        Bound::Excluded(end) => key >= end,
        Bound::Unbounded => false,
    }
}

impl<K, V> Iterator for BPlusTreeRangeIterator<'_, K, V>
where
    K: Ord + for<'de> Deserialize<'de>,
    V: for<'de> Deserialize<'de>,
{
    type Item = io::Result<(K, V)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let entry = self.iterator.next()?;
            match entry {
                Ok((key, _value)) if past_end(&key, &self.end) => {
                    self.iterator.state.finished = true;
                    return None;
                }
                Ok((key, value)) if within_start(&key, &self.start) => return Some(Ok((key, value))),
                Ok(_) => {}
                Err(err) => return Some(Err(err)),
            }
        }
    }
}

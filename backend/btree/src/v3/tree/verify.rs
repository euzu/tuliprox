use super::{
    decode_key, invalid_data, minimum_tree_key, push_child_visits, record_page_visit, validate_leaf_backlink,
    validated_overflow_chain_pages, verify_internal_page, verify_leaf_page, BPlusTreeQuery, Compression,
    InternalCellRef, InternalPreamble, LeafCellRef, LeafValueRef, PageType, SlottedPage, VerificationReport,
    WriteTransaction, PAGE_SIZE,
};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    io,
};

fn validate_transaction_free_list(transaction: &WriteTransaction, base: &[u8]) -> io::Result<()> {
    let mut free = HashSet::new();
    let mut page_id = transaction.next_header.free_page_head;
    while page_id != 0 {
        if !free.insert(page_id) {
            return Err(invalid_data("free list contains a cycle"));
        }
        if transaction.allocated_pages.contains(&page_id) {
            return Err(invalid_data("allocated page remains linked from free list"));
        }
        let page = SlottedPage::open(transaction.page(base, page_id)?, page_id, transaction.next_header.next_page_id)?;
        if page.header().page_type != PageType::Free {
            return Err(invalid_data("free list references a non-free page"));
        }
        page_id = page.header().right;
    }
    if !transaction.freed_pages.iter().all(|page| free.contains(page)) {
        return Err(invalid_data("freed page is missing from free list"));
    }
    for page_id in &transaction.allocated_pages {
        let page = transaction.page(base, *page_id)?;
        if SlottedPage::open(page, *page_id, transaction.next_header.next_page_id)?.header().page_type == PageType::Free
        {
            return Err(invalid_data("allocated page still has free-page type"));
        }
    }
    Ok(())
}

pub(super) fn validate_transaction_links<K>(transaction: &WriteTransaction, base: &[u8]) -> io::Result<()>
where
    K: Ord + for<'de> Deserialize<'de>,
{
    let original_length = transaction
        .original_header
        .next_page_id
        .checked_mul(u64::try_from(PAGE_SIZE).map_err(|_| invalid_data("page size exceeds u64"))?)
        .ok_or_else(|| invalid_data("original database length overflow"))?;
    if original_length != transaction.original_file_len {
        return Err(invalid_data("transaction original length does not match header"));
    }

    let mut owned_overflow_pages = HashSet::new();
    for (page_id, bytes) in &transaction.dirty_pages {
        if *page_id == 0 {
            continue;
        }
        let page = SlottedPage::open(bytes.as_slice(), *page_id, transaction.next_header.next_page_id)?;
        match page.header().page_type {
            PageType::Leaf => {
                validate_leaf_backlink(transaction, base, *page_id, page.header().left, false)?;
                validate_leaf_backlink(transaction, base, *page_id, page.header().right, true)?;
                let mut previous_key = None;
                for cell in page.cells() {
                    let cell = LeafCellRef::decode(cell?, *page_id, transaction.next_header.next_page_id)?;
                    let key = decode_key::<K>(cell.key_bytes)?;
                    if previous_key.as_ref().is_some_and(|previous| previous >= &key) {
                        return Err(invalid_data("leaf keys are not strictly increasing"));
                    }
                    previous_key = Some(key);
                    if let LeafValueRef::Overflow { stored_len, head, crc32, .. } = cell.value {
                        for overflow_page in validated_overflow_chain_pages(transaction, base, head, stored_len, crc32)?
                        {
                            if !owned_overflow_pages.insert(overflow_page) {
                                return Err(invalid_data("overflow page is owned by multiple values"));
                            }
                        }
                    }
                }
            }
            PageType::Internal => {
                let preamble =
                    InternalPreamble::decode(bytes.as_slice(), *page_id, transaction.next_header.next_page_id)?;
                let child = SlottedPage::open(
                    transaction.page(base, preamble.leftmost_child)?,
                    preamble.leftmost_child,
                    transaction.next_header.next_page_id,
                )?;
                if !matches!(child.header().page_type, PageType::Leaf | PageType::Internal) {
                    return Err(invalid_data("internal page references a non-tree child"));
                }
                let mut previous_key = None;
                for cell in page.cells() {
                    let cell = InternalCellRef::decode(cell?, *page_id, transaction.next_header.next_page_id)?;
                    let key = decode_key::<K>(cell.key_bytes)?;
                    if previous_key.as_ref().is_some_and(|previous| previous >= &key) {
                        return Err(invalid_data("internal separator keys are not strictly increasing"));
                    }
                    previous_key = Some(key);
                    let minimum = minimum_tree_key(transaction, base, cell.right_child)?
                        .ok_or_else(|| invalid_data("internal separator references an empty subtree"))?;
                    if minimum != cell.key_bytes {
                        return Err(invalid_data("internal separator differs from right subtree minimum"));
                    }
                }
            }
            PageType::Overflow => {
                if page.header().right != 0 {
                    let next = SlottedPage::open(
                        transaction.page(base, page.header().right)?,
                        page.header().right,
                        transaction.next_header.next_page_id,
                    )?;
                    if next.header().page_type != PageType::Overflow {
                        return Err(invalid_data("overflow page references a non-overflow page"));
                    }
                }
            }
            PageType::Free => {}
        }
    }

    validate_transaction_free_list(transaction, base)
}

pub(super) struct VerifyLeaf<K> {
    pub(super) page_id: u64,
    pub(super) left: u64,
    pub(super) right: u64,
    pub(super) minimum: Option<K>,
    pub(super) maximum: Option<K>,
}

pub(super) enum Visit<K> {
    Enter(u64, Option<K>, Option<K>),
    Exit(u64),
}

pub(super) enum VerifyValue {
    Inline,
    Overflow(Compression, u32, u32, u64, u32),
    Tombstone,
}

fn finish_verified_page<K: Ord + Clone>(
    page_id: u64,
    active: &mut HashSet<u64>,
    internal_children: &mut HashMap<u64, Vec<(u64, Option<K>)>>,
    page_minimum: &mut HashMap<u64, Option<K>>,
) -> io::Result<()> {
    if !active.remove(&page_id) {
        return Err(invalid_data("tree verifier active set is inconsistent"));
    }
    let Some(children) = internal_children.remove(&page_id) else { return Ok(()) };
    for (child, separator) in children.iter().skip(1) {
        let actual = page_minimum
            .get(child)
            .and_then(Option::as_ref)
            .ok_or_else(|| invalid_data("internal child has no minimum key"))?;
        if separator.as_ref() != Some(actual) {
            return Err(invalid_data("internal separator is not the right child minimum"));
        }
    }
    let minimum = children
        .first()
        .and_then(|(child, _)| page_minimum.get(child))
        .cloned()
        .ok_or_else(|| invalid_data("internal leftmost child has no verified minimum"))?;
    page_minimum.insert(page_id, minimum);
    Ok(())
}

fn verify_leaf_links<K: Ord>(leaves: &[VerifyLeaf<K>]) -> io::Result<()> {
    for (index, leaf) in leaves.iter().enumerate() {
        let expected_left = index.checked_sub(1).map_or(0, |previous| leaves[previous].page_id);
        let expected_right = leaves.get(index + 1).map_or(0, |next| next.page_id);
        if leaf.left != expected_left || leaf.right != expected_right {
            return Err(invalid_data("leaf sibling links do not match tree order"));
        }
        if index > 0
            && leaves[index - 1]
                .maximum
                .as_ref()
                .zip(leaf.minimum.as_ref())
                .is_some_and(|(previous, current)| previous >= current)
        {
            return Err(invalid_data("keys are inverted across leaf siblings"));
        }
    }
    Ok(())
}

fn verify_free_pages<K, V>(
    query: &mut BPlusTreeQuery<K, V>,
    tree_pages: &HashSet<u64>,
    overflow_pages: &HashSet<u64>,
) -> io::Result<HashSet<u64>> {
    let mut free_pages = HashSet::new();
    let mut free = query.header.free_page_head;
    while free != 0 {
        record_page_visit(&mut free_pages, free, "free list contains a duplicate or cycle")?;
        if tree_pages.contains(&free) || overflow_pages.contains(&free) {
            return Err(invalid_data("page is reachable from both live data and free list"));
        }
        let next_page_id = query.header.next_page_id;
        free = query.with_page(free, |bytes, _| {
            let page = SlottedPage::open(bytes, free, next_page_id)?;
            if page.header().page_type != PageType::Free {
                return Err(invalid_data("free list references a non-free page"));
            }
            Ok(page.header().right)
        })?;
    }
    Ok(free_pages)
}

fn verify_page_ownership(
    next_page_id: u64,
    tree_pages: &HashSet<u64>,
    overflow_pages: &HashSet<u64>,
    free_pages: &HashSet<u64>,
) -> io::Result<()> {
    for page_id in 1..next_page_id {
        let memberships = u8::from(tree_pages.contains(&page_id))
            + u8::from(overflow_pages.contains(&page_id))
            + u8::from(free_pages.contains(&page_id));
        if memberships != 1 {
            return Err(invalid_data("database contains an orphan or multiply-owned page"));
        }
    }
    Ok(())
}

pub(crate) fn verify_full<K, V>(query: &mut BPlusTreeQuery<K, V>) -> io::Result<VerificationReport>
where
    K: Ord + for<'de> Deserialize<'de> + Clone,
{
    let mut tree_pages = HashSet::new();
    let mut active = HashSet::new();
    let mut overflow_pages = HashSet::new();
    let mut leaves = Vec::<VerifyLeaf<K>>::new();
    let mut page_minimum = HashMap::<u64, Option<K>>::new();
    let mut internal_children = HashMap::<u64, Vec<(u64, Option<K>)>>::new();
    let mut live_entries = 0u64;
    let mut stack = vec![Visit::Enter(query.header.root_page_id, None, None)];

    while let Some(visit) = stack.pop() {
        let (page_id, lower, upper) = match visit {
            Visit::Exit(page_id) => {
                finish_verified_page(page_id, &mut active, &mut internal_children, &mut page_minimum)?;
                continue;
            }
            Visit::Enter(page_id, lower, upper) => (page_id, lower, upper),
        };
        if active.contains(&page_id) {
            return Err(invalid_data("tree child graph contains a cycle"));
        }
        if !tree_pages.insert(page_id) {
            return Err(invalid_data("tree page has multiple parents"));
        }
        active.insert(page_id);
        stack.push(Visit::Exit(page_id));
        let next_page_id = query.header.next_page_id;
        let page_type = query.with_page(page_id, |bytes, _| {
            SlottedPage::open(bytes, page_id, next_page_id).map(|page| page.header().page_type)
        })?;
        match page_type {
            PageType::Leaf => {
                let (leaf, page_live_entries) =
                    verify_leaf_page(query, page_id, lower.as_ref(), upper.as_ref(), &mut overflow_pages)?;
                live_entries = live_entries
                    .checked_add(page_live_entries)
                    .ok_or_else(|| invalid_data("live entry count overflow"))?;
                page_minimum.insert(page_id, leaf.minimum.clone());
                leaves.push(leaf);
            }
            PageType::Internal => {
                let children = verify_internal_page(query, page_id)?;
                push_child_visits(&mut stack, &children, lower.as_ref(), upper.as_ref())?;
                internal_children.insert(page_id, children);
            }
            PageType::Overflow | PageType::Free => return Err(invalid_data("tree child has the wrong page type")),
        }
    }

    verify_leaf_links(&leaves)?;
    let free_pages = verify_free_pages(query, &tree_pages, &overflow_pages)?;
    verify_page_ownership(query.header.next_page_id, &tree_pages, &overflow_pages, &free_pages)?;
    Ok(VerificationReport {
        live_entries,
        tree_pages: u64::try_from(tree_pages.len()).map_err(|_| invalid_data("tree page count exceeds u64"))?,
        overflow_pages: u64::try_from(overflow_pages.len())
            .map_err(|_| invalid_data("overflow page count exceeds u64"))?,
        free_pages: u64::try_from(free_pages.len()).map_err(|_| invalid_data("free page count exceeds u64"))?,
    })
}

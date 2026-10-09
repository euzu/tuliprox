use super::{
    decode_key, encode_inline_leaf_cell, encode_internal_cell, encode_overflow_leaf_cell, encode_overflow_page,
    encode_tombstone_leaf_cell, internal_page, invalid_data, invalid_input, leaf_page, overflow_payload, search_leaf,
    stored_value_checksum, Compression, InternalCellRef, InternalPreamble, InternalSplit, LeafCellRef, LeafValueRef,
    PageType, SlottedPage, Visit, WriteTransaction, MAX_CELL_FOOTPRINT, MAX_INLINE_STORED_VALUE, OVERFLOW_PAYLOAD_LEN,
    PAGE_HEADER_LEN, PAGE_SIZE, SLOT_LEN,
};
use serde::Deserialize;
use std::{collections::HashSet, io};

pub(super) fn checked_page_usage<'a, I>(base: usize, cells: I, maximum_cell_footprint: usize) -> io::Result<usize>
where
    I: IntoIterator<Item = &'a [u8]>,
{
    cells.into_iter().try_fold(base, |used, cell| {
        let footprint = SLOT_LEN.checked_add(cell.len()).ok_or_else(|| invalid_input("cell footprint overflow"))?;
        if footprint > maximum_cell_footprint {
            return Err(invalid_input("cell footprint exceeds format limit"));
        }
        used.checked_add(footprint).ok_or_else(|| invalid_input("page usage overflow"))
    })
}

pub(crate) fn used_leaf_bytes<T: AsRef<[u8]>>(cells: &[T]) -> io::Result<usize> {
    checked_page_usage(PAGE_HEADER_LEN, cells.iter().map(AsRef::as_ref), MAX_CELL_FOOTPRINT)
}

pub(crate) fn choose_leaf_split<T: AsRef<[u8]>>(cells: &[T]) -> io::Result<usize> {
    if cells.len() < 2 {
        return Err(invalid_input("leaf split requires two non-empty outputs"));
    }
    let mut total = 0usize;
    for cell in cells {
        let footprint =
            SLOT_LEN.checked_add(cell.as_ref().len()).ok_or_else(|| invalid_input("leaf cell footprint overflow"))?;
        if footprint > MAX_CELL_FOOTPRINT {
            return Err(invalid_input("leaf cell footprint exceeds format limit"));
        }
        total = total.checked_add(footprint).ok_or_else(|| invalid_input("leaf split usage overflow"))?;
    }

    let mut left_payload = 0usize;
    let mut best = None;
    for boundary in 1..cells.len() {
        let cell = cells.get(boundary - 1).ok_or_else(|| invalid_input("leaf split boundary is outside cells"))?;
        let footprint =
            SLOT_LEN.checked_add(cell.as_ref().len()).ok_or_else(|| invalid_input("leaf cell footprint overflow"))?;
        left_payload = left_payload.checked_add(footprint).ok_or_else(|| invalid_input("leaf split usage overflow"))?;
        let right_payload =
            total.checked_sub(left_payload).ok_or_else(|| invalid_input("leaf split usage underflow"))?;
        let left_used =
            PAGE_HEADER_LEN.checked_add(left_payload).ok_or_else(|| invalid_input("leaf split usage overflow"))?;
        let right_used =
            PAGE_HEADER_LEN.checked_add(right_payload).ok_or_else(|| invalid_input("leaf split usage overflow"))?;
        if left_used <= PAGE_SIZE && right_used <= PAGE_SIZE {
            let imbalance = left_used.abs_diff(right_used);
            if best.is_none_or(|(_, best_imbalance)| imbalance < best_imbalance) {
                best = Some((boundary, imbalance));
            }
        }
    }
    best.map(|(boundary, _)| boundary).ok_or_else(|| invalid_input("leaf split has no valid boundary"))
}

fn used_internal_bytes<'a, I>(cells: I) -> io::Result<usize>
where
    I: IntoIterator<Item = &'a [u8]>,
{
    checked_page_usage(PAGE_HEADER_LEN + 8, cells, SLOT_LEN + 12 + 2004)
}

pub(crate) fn choose_internal_split<T: AsRef<[u8]>>(
    cells: &[T],
    page_id: u64,
    next_page_id: u64,
) -> io::Result<InternalSplit<'_, T>> {
    if cells.len() < 3 {
        return Err(invalid_input("internal split requires two non-empty outputs and a promoted separator"));
    }
    for cell in cells {
        InternalCellRef::decode(cell.as_ref(), page_id, next_page_id)?;
    }

    let mut best = None;
    for promoted_index in 1..cells.len() - 1 {
        let left_used = used_internal_bytes(cells[..promoted_index].iter().map(AsRef::as_ref))?;
        let right_used = used_internal_bytes(cells[promoted_index + 1..].iter().map(AsRef::as_ref))?;
        if left_used <= PAGE_SIZE && right_used <= PAGE_SIZE {
            let imbalance = left_used.abs_diff(right_used);
            if best.is_none_or(|(_, best_imbalance)| imbalance < best_imbalance) {
                best = Some((promoted_index, imbalance));
            }
        }
    }
    let promoted_index =
        best.map(|(index, _)| index).ok_or_else(|| invalid_input("internal split has no valid boundary"))?;
    let promoted = InternalCellRef::decode(
        cells.get(promoted_index).ok_or_else(|| invalid_input("promoted separator is outside cells"))?.as_ref(),
        page_id,
        next_page_id,
    )?;
    Ok(InternalSplit {
        #[cfg(test)]
        promoted_index,
        right_leftmost_child: promoted.right_child,
        promoted,
        left_cells: cells.get(..promoted_index).ok_or_else(|| invalid_input("left split range is outside cells"))?,
        right_cells: cells
            .get(promoted_index + 1..)
            .ok_or_else(|| invalid_input("right split range is outside cells"))?,
    })
}

#[cfg(test)]
pub(super) fn database_page(database: &[u8], page_id: u64, next_page_id: u64) -> io::Result<&[u8]> {
    if page_id == 0 || page_id >= next_page_id {
        return Err(invalid_data("overflow page id is outside database"));
    }
    let page_id = usize::try_from(page_id).map_err(|_| invalid_data("overflow page id exceeds usize"))?;
    let offset = page_id.checked_mul(PAGE_SIZE).ok_or_else(|| invalid_data("overflow page offset overflow"))?;
    let end = offset.checked_add(PAGE_SIZE).ok_or_else(|| invalid_data("overflow page end overflow"))?;
    database.get(offset..end).ok_or_else(|| invalid_data("truncated overflow page"))
}

pub(super) fn minimum_tree_key(
    transaction: &WriteTransaction,
    base: &[u8],
    mut page_id: u64,
) -> io::Result<Option<Vec<u8>>> {
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(page_id) {
            return Err(invalid_data("minimum-key descent contains a cycle"));
        }
        let page = SlottedPage::open(transaction.page(base, page_id)?, page_id, transaction.next_header.next_page_id)?;
        match page.header().page_type {
            PageType::Leaf => {
                let Some(cell) = page.cells().next() else { return Ok(None) };
                return LeafCellRef::decode(cell?, page_id, transaction.next_header.next_page_id)
                    .map(|cell| Some(cell.key_bytes.to_vec()));
            }
            PageType::Internal => {
                page_id = InternalPreamble::decode(page.as_bytes(), page_id, transaction.next_header.next_page_id)?
                    .leftmost_child;
            }
            PageType::Overflow | PageType::Free => return Err(invalid_data("tree references a non-tree page")),
        }
    }
}

pub(super) fn validate_leaf_backlink(
    transaction: &WriteTransaction,
    base: &[u8],
    page_id: u64,
    sibling_id: u64,
    sibling_points_left: bool,
) -> io::Result<()> {
    if sibling_id == 0 {
        return Ok(());
    }
    let sibling =
        SlottedPage::open(transaction.page(base, sibling_id)?, sibling_id, transaction.next_header.next_page_id)?;
    let backlink = if sibling_points_left { sibling.header().left } else { sibling.header().right };
    if sibling.header().page_type != PageType::Leaf || backlink != page_id {
        return Err(invalid_data("asymmetric leaf sibling link"));
    }
    Ok(())
}

#[derive(Debug)]
struct Promotion {
    key: Vec<u8>,
    right_child: u64,
}

fn internal_child_position<K: Ord + for<'de> Deserialize<'de>, B: AsRef<[u8]>>(
    page: &SlottedPage<B>,
    key: &K,
) -> io::Result<(usize, u64)> {
    let mut left = 0usize;
    let mut right = usize::from(page.header().cell_count);
    while left < right {
        let middle = left + (right - left) / 2;
        let cell = InternalCellRef::decode(page.cell(middle)?, page.page_id(), page.next_page_id())?;
        if decode_key::<K>(cell.key_bytes)? <= *key {
            left = middle + 1;
        } else {
            right = middle;
        }
    }
    let child = if left == 0 {
        InternalPreamble::decode(page.as_bytes(), page.page_id(), page.next_page_id())?.leftmost_child
    } else {
        InternalCellRef::decode(page.cell(left - 1)?, page.page_id(), page.next_page_id())?.right_child
    };
    Ok((left, child))
}

pub(super) fn locate_transaction_leaf<K: Ord + for<'de> Deserialize<'de>>(
    transaction: &WriteTransaction,
    base: &[u8],
    key: &K,
) -> io::Result<(u64, Vec<(u64, usize)>)> {
    let mut page_id = transaction.next_header.root_page_id;
    let mut path = Vec::new();
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(page_id) {
            return Err(invalid_data("tree descent contains a cycle"));
        }
        let page = SlottedPage::open(transaction.page(base, page_id)?, page_id, transaction.next_header.next_page_id)?;
        match page.header().page_type {
            PageType::Leaf => return Ok((page_id, path)),
            PageType::Internal => {
                let (position, child) = internal_child_position(&page, key)?;
                path.try_reserve(1).map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
                path.push((page_id, position));
                page_id = child;
            }
            PageType::Overflow | PageType::Free => return Err(invalid_data("tree references a non-tree page")),
        }
    }
}

pub(super) fn overflow_chain_pages(
    transaction: &WriteTransaction,
    base: &[u8],
    mut page_id: u64,
) -> io::Result<Vec<u64>> {
    let mut pages = Vec::new();
    let mut visited = HashSet::new();
    while page_id != 0 {
        if !visited.insert(page_id) {
            return Err(invalid_data("overflow chain contains a cycle"));
        }
        let page = SlottedPage::open(transaction.page(base, page_id)?, page_id, transaction.next_header.next_page_id)?;
        if page.header().page_type != PageType::Overflow || overflow_payload(&page)?.is_empty() {
            return Err(invalid_data("invalid overflow chain page"));
        }
        pages.try_reserve(1).map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
        pages.push(page_id);
        page_id = page.header().right;
    }
    if pages.is_empty() {
        return Err(invalid_data("overflow chain is empty"));
    }
    Ok(pages)
}

pub(super) fn validated_overflow_chain_pages(
    transaction: &WriteTransaction,
    base: &[u8],
    head: u64,
    stored_len: u32,
    crc32: u32,
) -> io::Result<Vec<u64>> {
    let pages = overflow_chain_pages(transaction, base, head)?;
    let expected = usize::try_from(stored_len).map_err(|_| invalid_data("stored length exceeds usize"))?;
    let mut actual = 0usize;
    let mut hasher = crc32fast::Hasher::new();
    for page_id in &pages {
        let page =
            SlottedPage::open(transaction.page(base, *page_id)?, *page_id, transaction.next_header.next_page_id)?;
        let payload = overflow_payload(&page)?;
        actual = actual.checked_add(payload.len()).ok_or_else(|| invalid_data("overflow chain length overflow"))?;
        if actual > expected {
            return Err(invalid_data("overflow chain exceeds declared length"));
        }
        hasher.update(payload);
    }
    if actual != expected || hasher.finalize() != crc32 {
        return Err(invalid_data("overflow value checksum or length mismatch"));
    }
    Ok(pages)
}

pub(super) fn free_pages(transaction: &mut WriteTransaction, pages: &[u64]) -> io::Result<()> {
    for page_id in pages.iter().rev() {
        transaction.free_page(*page_id)?;
    }
    Ok(())
}

fn write_overflow_chain(
    transaction: &mut WriteTransaction,
    base: &[u8],
    stored: &[u8],
    old_pages: &[u64],
) -> io::Result<u64> {
    let required = stored.len().div_ceil(OVERFLOW_PAYLOAD_LEN);
    if required == 0 {
        return Err(invalid_input("overflow value must not be empty"));
    }
    let mut pages = Vec::new();
    pages.try_reserve_exact(required).map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
    if old_pages.len() >= required {
        pages.extend_from_slice(
            old_pages.get(..required).ok_or_else(|| invalid_data("overflow reuse range is invalid"))?,
        );
        free_pages(
            transaction,
            old_pages.get(required..).ok_or_else(|| invalid_data("overflow tail range is invalid"))?,
        )?;
    } else {
        for _ in 0..required {
            pages.push(transaction.allocate_page(base)?);
        }
        free_pages(transaction, old_pages)?;
    }
    for (index, payload) in stored.chunks(OVERFLOW_PAYLOAD_LEN).enumerate() {
        let page_id = *pages.get(index).ok_or_else(|| invalid_data("overflow page allocation is missing"))?;
        let next = pages.get(index + 1).copied().unwrap_or(0);
        transaction
            .write_page(page_id, encode_overflow_page(page_id, transaction.next_header.next_page_id, next, payload)?)?;
    }
    pages.first().copied().ok_or_else(|| invalid_data("overflow head is missing"))
}

fn repair_right_leaf_backlink(
    transaction: &mut WriteTransaction,
    base: &[u8],
    right_page_id: u64,
    left_page_id: u64,
) -> io::Result<()> {
    if right_page_id == 0 {
        return Ok(());
    }
    let next_page_id = transaction.next_header.next_page_id;
    let page = transaction.page_mut(base, right_page_id)?;
    let mut header = SlottedPage::open(page.as_slice(), right_page_id, next_page_id)?.header();
    if header.page_type != PageType::Leaf {
        return Err(invalid_data("leaf sibling references a non-leaf page"));
    }
    header.left = left_page_id;
    header.encode_into(page, right_page_id, next_page_id)
}

fn mutate_leaf(
    transaction: &mut WriteTransaction,
    base: &[u8],
    leaf_id: u64,
    index: usize,
    replace: bool,
    cell: &[u8],
) -> io::Result<Option<Promotion>> {
    let snapshot = transaction.page_copy(base, leaf_id)?;
    let page = SlottedPage::open(snapshot.as_slice(), leaf_id, transaction.next_header.next_page_id)?;
    if page.header().page_type != PageType::Leaf {
        return Err(invalid_data("mutation target is not a leaf page"));
    }
    let count = usize::from(page.header().cell_count);
    if (replace && index >= count) || (!replace && index > count) {
        return Err(invalid_input("leaf mutation index is outside page"));
    }
    if replace && page.cell(index)?.len() == cell.len() {
        let next_page_id = transaction.next_header.next_page_id;
        let dirty = transaction.page_mut(base, leaf_id)?;
        return SlottedPage::open(dirty.as_mut_slice(), leaf_id, next_page_id)?
            .replace_same_len(index, cell)
            .map(|()| None);
    }
    let mut cells = Vec::<&[u8]>::new();
    cells
        .try_reserve_exact(count + usize::from(!replace))
        .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
    for current in 0..count {
        if current == index {
            cells.push(cell);
            if replace {
                continue;
            }
        }
        cells.push(page.cell(current)?);
    }
    if index == count {
        cells.push(cell);
    }
    if used_leaf_bytes(&cells)? <= PAGE_SIZE {
        let next_page_id = transaction.next_header.next_page_id;
        let dirty = transaction.page_mut(base, leaf_id)?;
        SlottedPage::open(dirty.as_mut_slice(), leaf_id, next_page_id)?.rebuild_ordered(cells.iter().copied())?;
        return Ok(None);
    }

    let new_leaf_id = transaction.allocate_page(base)?;
    let boundary = choose_leaf_split(&cells)?;
    let (left_cells, right_cells) = cells.split_at(boundary);
    let right = page.header().right;
    let left_page = leaf_page(leaf_id, page.header().left, new_leaf_id, left_cells)?;
    let right_page = leaf_page(new_leaf_id, leaf_id, right, right_cells)?;
    let separator = LeafCellRef::decode(
        right_cells.first().ok_or_else(|| invalid_data("split right leaf is empty"))?,
        new_leaf_id,
        transaction.next_header.next_page_id,
    )?
    .key_bytes
    .to_vec();
    transaction.write_page(leaf_id, left_page)?;
    transaction.write_page(new_leaf_id, right_page)?;
    repair_right_leaf_backlink(transaction, base, right, new_leaf_id)?;
    Ok(Some(Promotion { key: separator, right_child: new_leaf_id }))
}

fn insert_internal_promotion(
    transaction: &mut WriteTransaction,
    base: &[u8],
    page_id: u64,
    position: usize,
    promotion: &Promotion,
    cell_scratch: &mut Vec<u8>,
) -> io::Result<Option<Promotion>> {
    let snapshot = transaction.page_copy(base, page_id)?;
    let page = SlottedPage::open(snapshot.as_slice(), page_id, transaction.next_header.next_page_id)?;
    if page.header().page_type != PageType::Internal {
        return Err(invalid_data("promotion target is not an internal page"));
    }
    let count = usize::from(page.header().cell_count);
    if position > count {
        return Err(invalid_input("internal insertion position is outside page"));
    }
    encode_internal_cell(
        &promotion.key,
        promotion.right_child,
        page_id,
        transaction.next_header.next_page_id,
        cell_scratch,
    )?;
    let mut cells = Vec::<&[u8]>::new();
    cells.try_reserve_exact(count + 1).map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
    for current in 0..count {
        if current == position {
            cells.push(cell_scratch);
        }
        cells.push(page.cell(current)?);
    }
    if position == count {
        cells.push(cell_scratch);
    }
    if used_internal_bytes(cells.iter().copied())? <= PAGE_SIZE {
        let next_page_id = transaction.next_header.next_page_id;
        let dirty = transaction.page_mut(base, page_id)?;
        SlottedPage::open(dirty.as_mut_slice(), page_id, next_page_id)?.rebuild_ordered(cells.iter().copied())?;
        return Ok(None);
    }

    let right_page_id = transaction.allocate_page(base)?;
    let split = choose_internal_split(&cells, page_id, transaction.next_header.next_page_id)?;
    let leftmost =
        InternalPreamble::decode(snapshot.as_slice(), page_id, transaction.next_header.next_page_id)?.leftmost_child;
    let promoted = Promotion { key: split.promoted.key_bytes.to_vec(), right_child: right_page_id };
    let left_page = internal_page(page_id, leftmost, split.left_cells)?;
    let right_page = internal_page(right_page_id, split.right_leftmost_child, split.right_cells)?;
    transaction.write_page(page_id, left_page)?;
    transaction.write_page(right_page_id, right_page)?;
    Ok(Some(promoted))
}

fn propagate_promotion(
    transaction: &mut WriteTransaction,
    base: &[u8],
    mut path: Vec<(u64, usize)>,
    mut promotion: Promotion,
    cell_scratch: &mut Vec<u8>,
) -> io::Result<()> {
    while let Some((parent, position)) = path.pop() {
        let Some(next) = insert_internal_promotion(transaction, base, parent, position, &promotion, cell_scratch)?
        else {
            return Ok(());
        };
        promotion = next;
    }
    let old_root = transaction.next_header.root_page_id;
    let new_root = transaction.allocate_page(base)?;
    encode_internal_cell(
        &promotion.key,
        promotion.right_child,
        new_root,
        transaction.next_header.next_page_id,
        cell_scratch,
    )?;
    transaction.write_page(new_root, internal_page(new_root, old_root, &[cell_scratch.as_slice()])?)?;
    transaction.next_header.root_page_id = new_root;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn stage_upsert<K: Ord + for<'de> Deserialize<'de>>(
    transaction: &mut WriteTransaction,
    base: &[u8],
    key: &K,
    encoded_key: &[u8],
    logical_len: u32,
    compression: Compression,
    stored: &[u8],
    cell_scratch: &mut Vec<u8>,
) -> io::Result<()> {
    let (leaf_id, path) = locate_transaction_leaf(transaction, base, key)?;
    let page = SlottedPage::open(transaction.page(base, leaf_id)?, leaf_id, transaction.next_header.next_page_id)?;
    let search = search_leaf(&page, key)?;
    let (replace, index, old_overflow) = match search {
        Ok(index) => {
            let old = LeafCellRef::decode(page.cell(index)?, leaf_id, transaction.next_header.next_page_id)?;
            let overflow = match old.value {
                LeafValueRef::Overflow { stored_len, head, crc32, .. } => {
                    validated_overflow_chain_pages(transaction, base, head, stored_len, crc32)?
                }
                LeafValueRef::Inline { .. } | LeafValueRef::Tombstone => Vec::new(),
            };
            (true, index, overflow)
        }
        Err(index) => (false, index, Vec::new()),
    };
    let inline_footprint = SLOT_LEN
        .checked_add(24)
        .and_then(|size| size.checked_add(encoded_key.len()))
        .and_then(|size| size.checked_add(stored.len()))
        .ok_or_else(|| invalid_input("leaf cell footprint overflow"))?;
    if stored.len() <= MAX_INLINE_STORED_VALUE && inline_footprint <= MAX_CELL_FOOTPRINT {
        free_pages(transaction, &old_overflow)?;
        encode_inline_leaf_cell(encoded_key, logical_len, compression, stored, cell_scratch)?;
    } else {
        let head = write_overflow_chain(transaction, base, stored, &old_overflow)?;
        encode_overflow_leaf_cell(
            encoded_key,
            logical_len,
            compression,
            u32::try_from(stored.len()).map_err(|_| invalid_input("stored value exceeds u32"))?,
            head,
            stored_value_checksum(stored),
            leaf_id,
            transaction.next_header.next_page_id,
            cell_scratch,
        )?;
    }
    if let Some(promotion) = mutate_leaf(transaction, base, leaf_id, index, replace, cell_scratch)? {
        propagate_promotion(transaction, base, path, promotion, cell_scratch)?;
    }
    Ok(())
}

pub(super) fn stage_delete<K: Ord + for<'de> Deserialize<'de>>(
    transaction: &mut WriteTransaction,
    base: &[u8],
    key: &K,
    encoded_key: &[u8],
    cell_scratch: &mut Vec<u8>,
) -> io::Result<bool> {
    let (leaf_id, _) = locate_transaction_leaf(transaction, base, key)?;
    let page = SlottedPage::open(transaction.page(base, leaf_id)?, leaf_id, transaction.next_header.next_page_id)?;
    let Ok(index) = search_leaf(&page, key)? else { return Ok(false) };
    let old = LeafCellRef::decode(page.cell(index)?, leaf_id, transaction.next_header.next_page_id)?;
    match old.value {
        LeafValueRef::Tombstone => return Ok(false),
        LeafValueRef::Overflow { stored_len, head, crc32, .. } => {
            let pages = validated_overflow_chain_pages(transaction, base, head, stored_len, crc32)?;
            free_pages(transaction, &pages)?;
        }
        LeafValueRef::Inline { .. } => {}
    }
    encode_tombstone_leaf_cell(encoded_key, cell_scratch)?;
    let _ = mutate_leaf(transaction, base, leaf_id, index, true, cell_scratch)?;
    Ok(true)
}

pub(super) fn push_child_visits<K: Clone>(
    stack: &mut Vec<Visit<K>>,
    children: &[(u64, Option<K>)],
    lower: Option<&K>,
    upper: Option<&K>,
) -> io::Result<()> {
    for index in (0..children.len()).rev() {
        let child_lower =
            if index == 0 { lower.cloned() } else { children.get(index).and_then(|(_, key)| key.clone()) };
        let child_upper = children.get(index + 1).and_then(|(_, key)| key.clone()).or_else(|| upper.cloned());
        let child = children
            .get(index)
            .map(|(child, _)| *child)
            .ok_or_else(|| invalid_data("internal child index is invalid"))?;
        stack.push(Visit::Enter(child, child_lower, child_upper));
    }
    Ok(())
}

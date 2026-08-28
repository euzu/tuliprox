use super::format::{
    encode_free_body, validate_free_body, write_page_checksum, InternalPreamble, OverflowHeader, PageHeader, PageType,
    Slot, INTERNAL_PREAMBLE_LEN, OVERFLOW_PAYLOAD_LEN, PAGE_HEADER_LEN, PAGE_SIZE, SLOT_LEN,
};
use std::{io, iter::FusedIterator};

#[cfg(test)]
thread_local! {
    static PAGE_OPEN_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_page_open_count() { PAGE_OPEN_COUNT.set(0); }

#[cfg(test)]
pub(crate) fn page_open_count() -> usize { PAGE_OPEN_COUNT.get() }

fn invalid_data(message: &'static str) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message) }

fn invalid_input(message: &'static str) -> io::Error { io::Error::new(io::ErrorKind::InvalidInput, message) }

fn checked_end(offset: usize, length: usize, kind: io::ErrorKind) -> io::Result<usize> {
    offset.checked_add(length).ok_or_else(|| io::Error::new(kind, "page offset overflow"))
}

const _: () = assert!(PAGE_HEADER_LEN + INTERNAL_PREAMBLE_LEN <= PAGE_SIZE);

fn slot_base(page_type: PageType) -> io::Result<usize> {
    match page_type {
        PageType::Leaf => Ok(PAGE_HEADER_LEN),
        PageType::Internal => {
            PAGE_HEADER_LEN.checked_add(INTERNAL_PREAMBLE_LEN).ok_or_else(|| invalid_data("slot base overflow"))
        }
        PageType::Overflow | PageType::Free => Err(invalid_input("chain pages do not contain slots")),
    }
}

pub(crate) struct SlottedPage<B> {
    bytes: B,
    header: PageHeader,
    page_id: u64,
    next_page_id: u64,
    overflow_payload_length: Option<u16>,
}

#[derive(Clone, Copy)]
pub(crate) struct PageValidation {
    header: PageHeader,
    page_id: u64,
    next_page_id: u64,
    overflow_payload_length: Option<u16>,
}

impl<B: AsRef<[u8]>> SlottedPage<B> {
    pub(crate) fn open(bytes: B, page_id: u64, next_page_id: u64) -> io::Result<Self> {
        #[cfg(test)]
        PAGE_OPEN_COUNT.with(|count| count.set(count.get().saturating_add(1)));
        let page = bytes.as_ref();
        if page.len() != PAGE_SIZE {
            return Err(invalid_data("page must be exactly 4096 bytes"));
        }
        let header = PageHeader::decode(page, page_id, next_page_id)?;
        let overflow_payload_length = match header.page_type {
            PageType::Internal => {
                InternalPreamble::decode(page, page_id, next_page_id)?;
                None
            }
            PageType::Overflow => Some(OverflowHeader::decode(page)?.payload_length),
            PageType::Free => {
                validate_free_body(page)?;
                None
            }
            PageType::Leaf => None,
        };
        let slotted = Self { bytes, header, page_id, next_page_id, overflow_payload_length };
        slotted.validate_slots()?;
        Ok(slotted)
    }

    pub(crate) fn from_immutable_snapshot(bytes: B, validation: PageValidation) -> io::Result<Self> {
        if bytes.as_ref().len() != PAGE_SIZE {
            return Err(invalid_data("page must be exactly 4096 bytes"));
        }
        Ok(Self {
            bytes,
            header: validation.header,
            page_id: validation.page_id,
            next_page_id: validation.next_page_id,
            overflow_payload_length: validation.overflow_payload_length,
        })
    }

    pub(crate) const fn validation(&self) -> PageValidation {
        PageValidation {
            header: self.header,
            page_id: self.page_id,
            next_page_id: self.next_page_id,
            overflow_payload_length: self.overflow_payload_length,
        }
    }

    pub(crate) const fn header(&self) -> PageHeader { self.header }

    pub(crate) const fn page_id(&self) -> u64 { self.page_id }

    pub(crate) const fn next_page_id(&self) -> u64 { self.next_page_id }

    pub(crate) fn as_bytes(&self) -> &[u8] { self.bytes.as_ref() }

    fn slot(&self, index: usize) -> io::Result<Slot> {
        if index >= usize::from(self.header.cell_count) {
            return Err(invalid_input("cell index is outside page"));
        }
        let base = slot_base(self.header.page_type)?;
        let offset = index
            .checked_mul(SLOT_LEN)
            .and_then(|size| base.checked_add(size))
            .ok_or_else(|| invalid_data("slot offset overflow"))?;
        let end = checked_end(offset, SLOT_LEN, io::ErrorKind::InvalidData)?;
        Slot::decode(self.bytes.as_ref().get(offset..end).ok_or_else(|| invalid_data("slot is outside page"))?)
    }

    pub(crate) fn cell(&self, index: usize) -> io::Result<&[u8]> {
        let range = self.cell_range(index)?;
        self.bytes.as_ref().get(range).ok_or_else(|| invalid_data("cell is outside page"))
    }

    pub(crate) fn cell_range(&self, index: usize) -> io::Result<std::ops::Range<usize>> {
        let slot = self.slot(index)?;
        let offset = usize::from(slot.offset);
        let end = checked_end(offset, usize::from(slot.length), io::ErrorKind::InvalidData)?;
        Ok(offset..end)
    }

    pub(crate) fn cells(&self) -> Cells<'_, B> { Cells { page: self, next: 0 } }

    fn validate_slots(&self) -> io::Result<()> {
        if matches!(self.header.page_type, PageType::Overflow | PageType::Free) {
            return Ok(());
        }
        if self.header.cell_count == 0 {
            return Ok(());
        }

        let free_start = usize::from(self.header.free_start);
        let mut previous_offset = PAGE_SIZE;
        for index in 0..usize::from(self.header.cell_count) {
            let slot = self.slot(index)?;
            let offset = usize::from(slot.offset);
            let length = usize::from(slot.length);
            let end = checked_end(offset, length, io::ErrorKind::InvalidData)?;
            if length == 0
                || offset < free_start
                || end > PAGE_SIZE
                || offset >= previous_offset
                || end > previous_offset
            {
                return Err(invalid_data("invalid or overlapping cell range"));
            }
            previous_offset = offset;
        }
        if previous_offset != usize::from(self.header.free_end) {
            return Err(invalid_data("free_end does not match the lowest cell offset"));
        }
        Ok(())
    }
}

pub(crate) fn encode_overflow_page(
    page_id: u64,
    next_page_id: u64,
    next: u64,
    payload: &[u8],
) -> io::Result<[u8; PAGE_SIZE]> {
    if payload.len() > OVERFLOW_PAYLOAD_LEN {
        return Err(invalid_input("overflow payload is too large"));
    }
    let mut page = [0; PAGE_SIZE];
    let payload_end = checked_end(40, payload.len(), io::ErrorKind::InvalidInput)?;
    page.get_mut(40..payload_end)
        .ok_or_else(|| invalid_input("overflow payload exceeds page"))?
        .copy_from_slice(payload);
    OverflowHeader {
        payload_length: u16::try_from(payload.len()).map_err(|_| invalid_input("overflow payload exceeds u16"))?,
    }
    .encode_into(&mut page)?;
    PageHeader { page_type: PageType::Overflow, cell_count: 0, free_start: 0, free_end: 0, left: 0, right: next }
        .encode_into(&mut page, page_id, next_page_id)?;
    Ok(page)
}

pub(crate) fn encode_free_page(page_id: u64, next_page_id: u64, next: u64) -> io::Result<[u8; PAGE_SIZE]> {
    let mut page = [0; PAGE_SIZE];
    encode_free_body(&mut page)?;
    PageHeader { page_type: PageType::Free, cell_count: 0, free_start: 0, free_end: 0, left: 0, right: next }
        .encode_into(&mut page, page_id, next_page_id)?;
    Ok(page)
}

pub(crate) fn overflow_payload<B: AsRef<[u8]>>(page: &SlottedPage<B>) -> io::Result<&[u8]> {
    let payload_length = page.overflow_payload_length.ok_or_else(|| invalid_data("expected overflow page"))?;
    let end = checked_end(40, usize::from(payload_length), io::ErrorKind::InvalidData)?;
    page.bytes.as_ref().get(40..end).ok_or_else(|| invalid_data("truncated overflow payload"))
}

pub(crate) struct Cells<'a, B> {
    page: &'a SlottedPage<B>,
    next: usize,
}

impl<'a, B: AsRef<[u8]>> Iterator for Cells<'a, B> {
    type Item = io::Result<&'a [u8]>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next >= usize::from(self.page.header.cell_count) {
            return None;
        }
        let index = self.next;
        self.next += 1;
        Some(self.page.cell(index))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.len();
        (remaining, Some(remaining))
    }
}

impl<B: AsRef<[u8]>> ExactSizeIterator for Cells<'_, B> {
    fn len(&self) -> usize { usize::from(self.page.header.cell_count).saturating_sub(self.next) }
}

impl<B: AsRef<[u8]>> FusedIterator for Cells<'_, B> {}

impl<B: AsRef<[u8]> + AsMut<[u8]>> SlottedPage<B> {
    pub(crate) fn rebuild_ordered<'a, I>(&mut self, cells: I) -> io::Result<()>
    where
        I: IntoIterator<Item = &'a [u8]>,
    {
        if self.bytes.as_ref().len() != PAGE_SIZE {
            return Err(invalid_data("page must be exactly 4096 bytes"));
        }
        let base = slot_base(self.header.page_type)?;
        let mut rebuilt = [0; PAGE_SIZE];
        if self.header.page_type == PageType::Internal {
            // INTERNAL_PREAMBLE_LEN is a compile-time constant; the range is always in bounds.
            let preamble_end = PAGE_HEADER_LEN + INTERNAL_PREAMBLE_LEN; // 32 + 8 = 40
            rebuilt[PAGE_HEADER_LEN..preamble_end].copy_from_slice(&self.bytes.as_ref()[PAGE_HEADER_LEN..preamble_end]);
        }

        let mut count = 0u16;
        let mut cell_start = PAGE_SIZE;
        for cell in cells {
            if cell.is_empty() {
                return Err(invalid_input("cells must not be empty"));
            }
            let length = u16::try_from(cell.len()).map_err(|_| invalid_input("cell length exceeds u16"))?;
            let next_count = count.checked_add(1).ok_or_else(|| invalid_input("cell count exceeds u16"))?;
            // SLOT_LEN=4, count<=u16::MAX, base<=40 — overflow is impossible within PAGE_SIZE.
            let slot_offset = base + usize::from(count) * SLOT_LEN;
            let slot_end = slot_offset + SLOT_LEN;
            let next_cell_start =
                cell_start.checked_sub(cell.len()).ok_or_else(|| invalid_input("cells exceed page capacity"))?;
            if next_cell_start < slot_end {
                return Err(invalid_input("cells exceed page capacity"));
            }
            // Both ranges are within [0..PAGE_SIZE] — validated by next_cell_start < slot_end check.
            rebuilt[next_cell_start..cell_start].copy_from_slice(cell);
            let slot = Slot {
                offset: u16::try_from(next_cell_start).map_err(|_| invalid_input("cell offset exceeds u16"))?,
                length,
            };
            rebuilt[slot_offset..slot_end].copy_from_slice(&slot.encode());
            count = next_count;
            cell_start = next_cell_start;
        }

        if self.header.page_type == PageType::Internal && count == 0 {
            return Err(invalid_input("internal pages must not be empty"));
        }
        let slot_bytes =
            usize::from(count).checked_mul(SLOT_LEN).ok_or_else(|| invalid_input("slot directory size overflow"))?;
        let free_start = base.checked_add(slot_bytes).ok_or_else(|| invalid_input("slot directory overflow"))?;
        let header = PageHeader {
            page_type: self.header.page_type,
            cell_count: count,
            free_start: u16::try_from(free_start).map_err(|_| invalid_input("slot directory exceeds u16"))?,
            free_end: u16::try_from(cell_start).map_err(|_| invalid_input("cell offset exceeds u16"))?,
            left: self.header.left,
            right: self.header.right,
        };
        header.encode_into(&mut rebuilt, self.page_id, self.next_page_id)?;
        self.bytes
            .as_mut()
            .get_mut(..PAGE_SIZE)
            .ok_or_else(|| invalid_data("page destination is truncated"))?
            .copy_from_slice(&rebuilt);
        self.header = header;
        Ok(())
    }

    pub(crate) fn replace_same_len(&mut self, index: usize, replacement: &[u8]) -> io::Result<()> {
        let slot = self.slot(index)?;
        if replacement.len() != usize::from(slot.length) {
            return Err(invalid_input("replacement length differs from cell length"));
        }
        let offset = usize::from(slot.offset);
        let end = checked_end(offset, replacement.len(), io::ErrorKind::InvalidData)?;
        let page = self.bytes.as_mut();
        if page.len() != PAGE_SIZE {
            return Err(invalid_data("page must be exactly 4096 bytes"));
        }
        page.get_mut(offset..end)
            .ok_or_else(|| invalid_data("cell destination is outside page"))?
            .copy_from_slice(replacement);
        write_page_checksum(page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v3::format::{
        encode_free_body, page_checksum, write_page_checksum, InternalPreamble, OverflowHeader, PageHeader, PageType,
        Slot, PAGE_HEADER_LEN, PAGE_SIZE, SLOT_LEN,
    };
    use std::io;

    const PAGE_ID: u64 = 1;
    const NEXT_PAGE_ID: u64 = 8;

    struct ChangingMutView {
        bytes: [u8; PAGE_SIZE],
        calls: usize,
    }

    impl AsRef<[u8]> for ChangingMutView {
        fn as_ref(&self) -> &[u8] { &self.bytes }
    }

    impl AsMut<[u8]> for ChangingMutView {
        fn as_mut(&mut self) -> &mut [u8] {
            self.calls += 1;
            if self.calls == 1 {
                &mut self.bytes[..200]
            } else {
                &mut self.bytes[..32]
            }
        }
    }

    fn invalid_data<T>(result: io::Result<T>) -> io::Result<()> {
        match result {
            Err(err) if err.kind() == io::ErrorKind::InvalidData => Ok(()),
            Err(err) => Err(io::Error::other(format!("expected InvalidData, got {err}"))),
            Ok(_) => Err(io::Error::other("expected InvalidData")),
        }
    }

    fn invalid_input<T>(result: io::Result<T>) -> io::Result<()> {
        match result {
            Err(err) if err.kind() == io::ErrorKind::InvalidInput => Ok(()),
            Err(err) => Err(io::Error::other(format!("expected InvalidInput, got {err}"))),
            Ok(_) => Err(io::Error::other("expected InvalidInput")),
        }
    }

    fn empty_leaf() -> io::Result<[u8; PAGE_SIZE]> {
        let mut page = [0; PAGE_SIZE];
        PageHeader {
            page_type: PageType::Leaf,
            cell_count: 0,
            free_start: u16::try_from(PAGE_HEADER_LEN).map_err(io::Error::other)?,
            free_end: u16::try_from(PAGE_SIZE).map_err(io::Error::other)?,
            left: 0,
            right: 0,
        }
        .encode_into(&mut page, PAGE_ID, NEXT_PAGE_ID)?;
        Ok(page)
    }

    fn leaf_with_slots(slots: &[Slot], free_end: u16) -> io::Result<[u8; PAGE_SIZE]> {
        let mut page = [0; PAGE_SIZE];
        for (index, slot) in slots.iter().copied().enumerate() {
            let offset = PAGE_HEADER_LEN
                .checked_add(index.checked_mul(SLOT_LEN).ok_or_else(|| io::Error::other("test slot overflow"))?)
                .ok_or_else(|| io::Error::other("test slot overflow"))?;
            let end = offset.checked_add(SLOT_LEN).ok_or_else(|| io::Error::other("test slot overflow"))?;
            page.get_mut(offset..end)
                .ok_or_else(|| io::Error::other("test slot outside page"))?
                .copy_from_slice(&slot.encode());
        }
        let cell_count = u16::try_from(slots.len()).map_err(io::Error::other)?;
        let free_start = PAGE_HEADER_LEN
            .checked_add(slots.len().checked_mul(SLOT_LEN).ok_or_else(|| io::Error::other("test slot overflow"))?)
            .and_then(|value| u16::try_from(value).ok())
            .ok_or_else(|| io::Error::other("test slot overflow"))?;
        PageHeader { page_type: PageType::Leaf, cell_count, free_start, free_end, left: 0, right: 0 }.encode_into(
            &mut page,
            PAGE_ID,
            NEXT_PAGE_ID,
        )?;
        Ok(page)
    }

    fn chain_page(page_type: PageType) -> io::Result<[u8; PAGE_SIZE]> {
        let mut page = [0; PAGE_SIZE];
        match page_type {
            PageType::Overflow => OverflowHeader { payload_length: 0 }.encode_into(&mut page)?,
            PageType::Free => encode_free_body(&mut page)?,
            PageType::Leaf | PageType::Internal => return Err(io::Error::other("test requires a chain page")),
        }
        PageHeader { page_type, cell_count: 0, free_start: 0, free_end: 0, left: 0, right: 0 }.encode_into(
            &mut page,
            PAGE_ID,
            NEXT_PAGE_ID,
        )?;
        Ok(page)
    }

    fn set_right_reference(page: &mut [u8], reference: u64) -> io::Result<()> {
        page.get_mut(16..24)
            .ok_or_else(|| io::Error::other("missing right reference"))?
            .copy_from_slice(&reference.to_le_bytes());
        write_page_checksum(page)
    }

    #[test]
    fn rebuild_ordered_slot_bytes_match_slot_encode() -> io::Result<()> {
        let mut page = empty_leaf()?;
        let mut leaf = SlottedPage::open(page.as_mut_slice(), PAGE_ID, NEXT_PAGE_ID)?;
        leaf.rebuild_ordered([b"a".as_slice(), b"bc".as_slice()])?;
        let expected = [
            Slot { offset: u16::try_from(PAGE_SIZE - 1).map_err(io::Error::other)?, length: 1 },
            Slot { offset: u16::try_from(PAGE_SIZE - 3).map_err(io::Error::other)?, length: 2 },
        ];
        for (index, slot) in expected.into_iter().enumerate() {
            let offset = PAGE_HEADER_LEN
                .checked_add(index.checked_mul(SLOT_LEN).ok_or_else(|| io::Error::other("test slot overflow"))?)
                .ok_or_else(|| io::Error::other("test slot overflow"))?;
            let end = offset.checked_add(SLOT_LEN).ok_or_else(|| io::Error::other("test slot overflow"))?;
            let written = page.get(offset..end).ok_or_else(|| io::Error::other("test slot outside page"))?;
            assert_eq!(written, slot.encode(), "inline slot encoding drifted from Slot::encode at {index}");
        }
        Ok(())
    }

    #[test]
    fn opens_empty_leaf_from_immutable_and_mutable_buffers() -> io::Result<()> {
        let page = empty_leaf()?;
        let immutable = SlottedPage::open(page.as_slice(), PAGE_ID, NEXT_PAGE_ID)?;
        assert_eq!(immutable.header().page_type, PageType::Leaf);
        assert_eq!(immutable.cells().len(), 0);
        invalid_input(immutable.cell(0))?;

        let mut page = page;
        let mut mutable = SlottedPage::open(page.as_mut_slice(), PAGE_ID, NEXT_PAGE_ID)?;
        mutable.rebuild_ordered([b"a".as_slice(), b"bc".as_slice()])?;
        assert_eq!(mutable.cells().collect::<io::Result<Vec<_>>>()?, [b"a".as_slice(), b"bc".as_slice()]);
        Ok(())
    }

    #[test]
    fn rebuild_packs_slots_in_descending_offset_order_and_exactly_fits() -> io::Result<()> {
        let mut page = empty_leaf()?;
        let mut slotted = SlottedPage::open(page.as_mut_slice(), PAGE_ID, NEXT_PAGE_ID)?;
        let first = [1; 2028];
        let second = [2; 2028];
        slotted.rebuild_ordered([first.as_slice(), second.as_slice()])?;

        assert_eq!(slotted.header().free_start, 40);
        assert_eq!(slotted.header().free_end, 40);
        assert_eq!(slotted.cell(0)?, first);
        assert_eq!(slotted.cell(1)?, second);
        assert_eq!(slotted.slot(0)?.offset, 2068);
        assert_eq!(slotted.slot(1)?.offset, 40);
        Ok(())
    }

    #[test]
    fn rebuild_rejects_insufficient_space_without_changing_page() -> io::Result<()> {
        let original = empty_leaf()?;
        let mut page = original;
        {
            let mut slotted = SlottedPage::open(page.as_mut_slice(), PAGE_ID, NEXT_PAGE_ID)?;
            let cell = [1; 2029];
            invalid_input(slotted.rebuild_ordered([cell.as_slice(), cell.as_slice()]))?;
        }
        assert_eq!(page, original);
        Ok(())
    }

    #[test]
    fn equal_size_replacement_refreshes_page_checksum() -> io::Result<()> {
        let mut page = empty_leaf()?;
        {
            let mut slotted = SlottedPage::open(page.as_mut_slice(), PAGE_ID, NEXT_PAGE_ID)?;
            slotted.rebuild_ordered([b"old".as_slice()])?;
        }
        let old_checksum = page_checksum(&page)?;
        {
            let mut slotted = SlottedPage::open(page.as_mut_slice(), PAGE_ID, NEXT_PAGE_ID)?;
            slotted.replace_same_len(0, b"new")?;
            assert_eq!(slotted.cell(0)?, b"new");
            invalid_input(slotted.replace_same_len(0, b"longer"))?;
        }
        let new_checksum = page_checksum(&page)?;
        assert_ne!(old_checksum, new_checksum);
        SlottedPage::open(page.as_slice(), PAGE_ID, NEXT_PAGE_ID)?;
        Ok(())
    }

    #[test]
    fn replacement_rejects_a_short_mutable_view_without_partial_mutation() -> io::Result<()> {
        let slots = [Slot { offset: 100, length: 3 }];
        let mut bytes = leaf_with_slots(&slots, 100)?;
        bytes[100..103].copy_from_slice(b"old");
        write_page_checksum(&mut bytes)?;
        let original = bytes;
        let mut changing = ChangingMutView { bytes, calls: 0 };

        {
            let mut slotted = SlottedPage::open(&mut changing, PAGE_ID, NEXT_PAGE_ID)?;
            invalid_data(slotted.replace_same_len(0, b"new"))?;
        }

        assert_eq!(changing.calls, 1);
        assert_eq!(changing.bytes, original);
        Ok(())
    }

    #[test]
    fn rebuild_rejects_a_short_page_without_panicking() -> io::Result<()> {
        let mut page = [0; PAGE_SIZE];
        InternalPreamble { leftmost_child: 3 }.encode_into(&mut page, 2, NEXT_PAGE_ID)?;
        page[40..44].copy_from_slice(&Slot { offset: 4095, length: 1 }.encode());
        page[4095] = b'x';
        PageHeader { page_type: PageType::Internal, cell_count: 1, free_start: 44, free_end: 4095, left: 0, right: 0 }
            .encode_into(&mut page, 2, NEXT_PAGE_ID)?;

        let mut changing = ChangingMutView { bytes: page, calls: 0 };
        let mut slotted = SlottedPage::open(&mut changing, 2, NEXT_PAGE_ID)?;
        invalid_data(slotted.rebuild_ordered([b"a".as_slice()]))
    }

    #[test]
    fn rebuild_compacts_gapped_cells() -> io::Result<()> {
        let slots = [Slot { offset: 4080, length: 4 }, Slot { offset: 4000, length: 4 }];
        let mut page = leaf_with_slots(&slots, 4000)?;
        page[4080..4084].copy_from_slice(b"left");
        page[4000..4004].copy_from_slice(b"rght");
        write_page_checksum(&mut page)?;

        let mut slotted = SlottedPage::open(page.as_mut_slice(), PAGE_ID, NEXT_PAGE_ID)?;
        slotted.rebuild_ordered([b"left".as_slice(), b"rght".as_slice()])?;
        assert_eq!(slotted.header().free_end, 4088);
        assert_eq!(slotted.slot(0)?.offset, 4092);
        assert_eq!(slotted.slot(1)?.offset, 4088);
        Ok(())
    }

    #[test]
    fn opens_a_large_densely_packed_page() -> io::Result<()> {
        let mut page = empty_leaf()?;
        let cell = [7];
        {
            let mut slotted = SlottedPage::open(page.as_mut_slice(), PAGE_ID, NEXT_PAGE_ID)?;
            slotted.rebuild_ordered(std::iter::repeat_n(cell.as_slice(), 800))?;
        }
        let slotted = SlottedPage::open(page.as_slice(), PAGE_ID, NEXT_PAGE_ID)?;
        assert_eq!(slotted.cells().len(), 800);
        assert_eq!(slotted.cells().count(), 800);
        Ok(())
    }

    #[test]
    fn open_rejects_malformed_slot_ranges() -> io::Result<()> {
        let cases = [
            (vec![Slot { offset: 4090, length: 0 }], 4090),
            (vec![Slot { offset: 35, length: 1 }], 36),
            (vec![Slot { offset: 4090, length: 10 }], 4090),
            (vec![Slot { offset: 4080, length: 8 }, Slot { offset: 4080, length: 8 }], 4080),
            (vec![Slot { offset: 4080, length: 8 }, Slot { offset: 4090, length: 6 }], 4090),
            (vec![Slot { offset: 4080, length: 16 }, Slot { offset: 4070, length: 16 }], 4070),
            (vec![Slot { offset: 4080, length: 16 }, Slot { offset: 4064, length: 16 }], 4060),
        ];

        for (slots, free_end) in cases {
            invalid_data(SlottedPage::open(leaf_with_slots(&slots, free_end)?.as_slice(), PAGE_ID, NEXT_PAGE_ID))?;
        }
        Ok(())
    }

    #[test]
    fn open_rejects_leaf_self_reference() -> io::Result<()> {
        let mut page = empty_leaf()?;
        page[8..16].copy_from_slice(&PAGE_ID.to_le_bytes());
        write_page_checksum(&mut page)?;
        invalid_data(SlottedPage::open(page.as_slice(), PAGE_ID, NEXT_PAGE_ID))?;

        let mut page = empty_leaf()?;
        set_right_reference(&mut page, PAGE_ID)?;
        invalid_data(SlottedPage::open(page.as_slice(), PAGE_ID, NEXT_PAGE_ID))
    }

    #[test]
    fn open_rejects_overflow_self_reference() -> io::Result<()> {
        let mut page = chain_page(PageType::Overflow)?;
        set_right_reference(&mut page, PAGE_ID)?;
        invalid_data(SlottedPage::open(page.as_slice(), PAGE_ID, NEXT_PAGE_ID))
    }

    #[test]
    fn open_rejects_free_self_reference() -> io::Result<()> {
        let mut page = chain_page(PageType::Free)?;
        set_right_reference(&mut page, PAGE_ID)?;
        invalid_data(SlottedPage::open(page.as_slice(), PAGE_ID, NEXT_PAGE_ID))
    }

    #[test]
    fn open_validates_internal_preamble() -> io::Result<()> {
        let mut page = [0; PAGE_SIZE];
        InternalPreamble { leftmost_child: PAGE_ID }.encode_into(&mut page, 2, NEXT_PAGE_ID)?;
        PageHeader { page_type: PageType::Internal, cell_count: 1, free_start: 44, free_end: 4095, left: 0, right: 0 }
            .encode_into(&mut page, 2, NEXT_PAGE_ID)?;
        page[40..44].copy_from_slice(&Slot { offset: 4095, length: 1 }.encode());
        write_page_checksum(&mut page)?;
        SlottedPage::open(page.as_slice(), 2, NEXT_PAGE_ID)?;

        page[32..40].copy_from_slice(&2u64.to_le_bytes());
        write_page_checksum(&mut page)?;
        invalid_data(SlottedPage::open(page.as_slice(), 2, NEXT_PAGE_ID))
    }

    #[test]
    fn internal_rebuild_preserves_preamble_cells_and_checksum() -> io::Result<()> {
        let mut page = [0; PAGE_SIZE];
        InternalPreamble { leftmost_child: 3 }.encode_into(&mut page, 2, NEXT_PAGE_ID)?;
        page[40..44].copy_from_slice(&Slot { offset: 4095, length: 1 }.encode());
        page[4095] = b'x';
        PageHeader { page_type: PageType::Internal, cell_count: 1, free_start: 44, free_end: 4095, left: 0, right: 0 }
            .encode_into(&mut page, 2, NEXT_PAGE_ID)?;

        {
            let mut slotted = SlottedPage::open(page.as_mut_slice(), 2, NEXT_PAGE_ID)?;
            slotted.rebuild_ordered([b"a".as_slice(), b"bc".as_slice()])?;
            assert_eq!(slotted.cells().collect::<io::Result<Vec<_>>>()?, [b"a".as_slice(), b"bc".as_slice()]);
        }

        let reopened = SlottedPage::open(page.as_slice(), 2, NEXT_PAGE_ID)?;
        assert_eq!(InternalPreamble::decode(&page, 2, NEXT_PAGE_ID)?.leftmost_child, 3);
        assert_eq!(reopened.cells().collect::<io::Result<Vec<_>>>()?, [b"a".as_slice(), b"bc".as_slice()]);
        Ok(())
    }

    #[test]
    fn rebuild_ordered_roundtrip() -> io::Result<()> {
        let mut page = empty_leaf()?;
        let mut slotted = SlottedPage::open(page.as_mut_slice(), PAGE_ID, NEXT_PAGE_ID)?;
        let cells: Vec<&[u8]> = vec![b"alpha", b"beta", b"gamma"];
        slotted.rebuild_ordered(cells.iter().copied())?;
        let read_back: Vec<&[u8]> = slotted.cells().collect::<io::Result<_>>()?;
        assert_eq!(read_back, cells);
        Ok(())
    }
}

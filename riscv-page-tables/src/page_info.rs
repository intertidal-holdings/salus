// Copyright (c) 2022 by Rivos Inc.
// Licensed under the Apache License, Version 2.0, see LICENSE for details.
// SPDX-License-Identifier: Apache-2.0

use arrayvec::ArrayVec;
use page_collections::page_vec::PageVec;
use riscv_pages::{PageOwnerId, PageSize, RawAddr, SequentialPages, SupervisorPageAddr};

use crate::{HwMemMap, HwMemType, HwReservedMemType, PageTrackingError, PageTrackingResult};

/// Tracks the owners of a page. Ownership is nested in order to establish a "chain-of-custody"
/// for the page.
pub type PageOwnerVec = ArrayVec<PageOwnerId, MAX_PAGE_OWNERS>;

/// `PageInfo` holds the current ownership status of a page.
#[derive(Clone, Debug)]
pub enum PageInfo {
    /// Not present, reserved, or otherwise not usable.
    Reserved,

    /// Page is unowned. No pages should be in this state after startup: they must either
    /// be reserved, or owned by the hypervisor / VMs.
    Free,

    /// Page is owned by the hypervisor or a VM. Does not necessarily imply the page is mapped
    /// by the owning VM (e.g. may be used to build the VM's G-stage page-tables).
    Owned(PageOwnerVec),
}

/// The maximum length for an ownership chain. Enough for the host VM to assign to a guest VM
/// without further nesting.
///
/// TODO: Could save a u64 here by having hypervisor-owned pages be a separate `PageInfo` state
/// since pages can't transition between hypervisor-owned and VM-owned post-startup.
const MAX_PAGE_OWNERS: usize = 3;

impl PageInfo {
    /// Creates a new `PageInfo` that is free.
    pub fn new() -> Self {
        PageInfo::Free
    }

    /// Creates a new `PageInfo` that is initially owned by the hypervisor.
    pub fn new_hypervisor_owned() -> Self {
        let mut owners = PageOwnerVec::new();
        owners.push(PageOwnerId::hypervisor());
        PageInfo::Owned(owners)
    }

    /// Creates a new `PageInfo` that is forever reserved.
    pub fn new_reserved() -> Self {
        PageInfo::Reserved
    }

    /// Returns the current owner, if it exists.
    pub fn owner(&self) -> Option<PageOwnerId> {
        match self {
            PageInfo::Owned(ref owners) => Some(owners[owners.len() - 1]),
            _ => None,
        }
    }

    /// Returns if the page is free.
    pub fn is_free(&self) -> bool {
        matches!(self, PageInfo::Free)
    }

    /// Returns if the page is marked reserved.
    pub fn is_reserved(&self) -> bool {
        matches!(self, PageInfo::Reserved)
    }

    /// Pops the current owner if there is one, returning the page to the previous owner.
    pub fn pop_owner(&mut self) -> PageTrackingResult<PageOwnerId> {
        match self {
            PageInfo::Owned(ref mut owners) => {
                if owners.len() == 1 {
                    Err(PageTrackingError::OwnerOverflow) // Can't pop the last owner.
                } else {
                    Ok(owners.pop().expect("PageOwnerVec can't be empty"))
                }
            }
            PageInfo::Reserved => Err(PageTrackingError::ReservedPage),
            PageInfo::Free => Err(PageTrackingError::UnownedPage),
        }
    }

    /// Pops owners while the provided `check` function returns true or there are no more owners.
    pub fn pop_owners_while<F>(&mut self, check: F)
    where
        F: Fn(&PageOwnerId) -> bool,
    {
        while let Some(o) = self.owner() {
            if !check(&o) || self.pop_owner().is_err() {
                break;
            }
        }
    }

    /// Finds the first owner for which `check` returns true.
    pub fn find_owner<F>(&self, check: F) -> Option<PageOwnerId>
    where
        F: Fn(&PageOwnerId) -> bool,
    {
        match self {
            PageInfo::Owned(ref owners) => {
                // We go in reverse to start at the top of the ownership stack.
                owners.iter().rev().find(|&o| check(o)).copied()
            }
            _ => None,
        }
    }

    /// Sets the current owner of the page while maintaining a "chain of custody" so the previous
    /// owner is known when the new owner abandons the page.
    pub fn push_owner(&mut self, owner: PageOwnerId) -> PageTrackingResult<()> {
        match self {
            PageInfo::Owned(ref mut owners) => owners
                .try_push(owner)
                .map_err(|_| PageTrackingError::OwnerOverflow),
            PageInfo::Free => {
                let mut owners = PageOwnerVec::new();
                owners.push(owner);
                *self = PageInfo::Owned(owners);
                Ok(())
            }
            PageInfo::Reserved => Err(PageTrackingError::ReservedPage),
        }
    }
}

impl Default for PageInfo {
    fn default() -> Self {
        PageInfo::Free
    }
}

const MAX_SPARSE_MAP_ENTRIES: usize = 16;

/// Maps a contiguous range of memory to a subset of the `PageMap`.
#[derive(Clone, Copy, Debug)]
struct SparseMapEntry {
    base_pfn: usize,
    num_pages: usize,
    page_map_index: usize,
}

/// Keeps information for all physical pages in the system.
pub struct PageMap {
    pages: PageVec<PageInfo>,
    sparse_map: ArrayVec<SparseMapEntry, MAX_SPARSE_MAP_ENTRIES>,
}

impl PageMap {
    /// Builds a new `PageMap` from a populated `HwMemMap`. It will track ownership information
    /// for each page in the system.
    pub fn build_from(mut mem_map: HwMemMap) -> Self {
        // Determine how many pages we'll need for the page map.
        let total_pages = mem_map
            .regions()
            .fold(0, |pages, r| pages + r.size() / PageSize::Size4k as u64);
        let page_map_size =
            PageSize::Size4k.round_up(total_pages * core::mem::size_of::<PageInfo>() as u64);
        let page_map_pages = page_map_size / PageSize::Size4k as u64;

        // Find a space for the page map.
        let page_map_region = mem_map
            .regions()
            .find(|r| r.mem_type() == HwMemType::Available && r.size() >= page_map_size)
            .expect("No free space for PageMap");
        let page_map_base = page_map_region.base().get_4k_addr();

        // Safe to create pages from this memory as `HwMemMap` guarantees that this range is
        // valid and free to use.
        let seq_pages = unsafe { SequentialPages::from_mem_range(page_map_base, page_map_pages) };
        let struct_pages = PageVec::from(seq_pages);

        // Reserve the memory consumed by the pagemap itself.
        mem_map
            .reserve_region(
                HwReservedMemType::PageMap,
                RawAddr::from(page_map_base),
                page_map_size,
            )
            .expect("Failed to reserve page map");

        let mut page_map = Self::new(struct_pages);
        page_map.populate_from(mem_map);
        page_map
    }

    /// Constructs an empty `PageMap` from an existing vector of `PageInfo` structs.
    fn new(pages: PageVec<PageInfo>) -> Self {
        Self {
            pages,
            sparse_map: ArrayVec::new(),
        }
    }

    /// Populates an already-constructed `PageMap` with the memory map information from the given
    /// `HwMemMap`. This `PageMap` must be empty and must have been constructed with enough space
    /// for all the pages in the `HwMemMap`.
    fn populate_from(&mut self, mem_map: HwMemMap) {
        // Populate the page map with the regions in the memory map.
        //
        // All pages in available memory regions are initially free and will later become
        // allocated by the hypervisor (and for most pages, further deligated to the host VM).
        //
        // Pages in reserved regions are marked reserved, except for those containing the
        // host VM images, which are considered to be initially hypervisor-owned.
        let mut current_entry = SparseMapEntry {
            base_pfn: mem_map.regions().next().unwrap().base().index(),
            num_pages: 0,
            page_map_index: 0,
        };
        for r in mem_map.regions() {
            let base = r.base().get_4k_addr();
            if current_entry.base_pfn + current_entry.num_pages != base.index() {
                let next_entry = SparseMapEntry {
                    base_pfn: base.index(),
                    num_pages: 0,
                    page_map_index: current_entry.page_map_index + current_entry.num_pages,
                };
                self.sparse_map.push(current_entry);
                current_entry = next_entry;
            }

            let end = r.end().get_4k_addr();
            for _ in base.iter_from().take_while(|&a| a != end) {
                match r.mem_type() {
                    HwMemType::Available => {
                        self.pages.push(PageInfo::new());
                    }
                    HwMemType::Reserved(HwReservedMemType::HostKernelImage)
                    | HwMemType::Reserved(HwReservedMemType::HostInitramfsImage) => {
                        self.pages.push(PageInfo::new_hypervisor_owned());
                    }
                    _ => {
                        self.pages.push(PageInfo::new_reserved());
                    }
                }
                current_entry.num_pages += 1;
            }
            // Make sure we won't overflow later.
            assert!(current_entry
                .base_pfn
                .checked_add(current_entry.num_pages)
                .is_some());
        }
        self.sparse_map.push(current_entry);
    }

    /// Returns a reference to the `PageInfo` struct for the 4k page at `addr`.
    pub fn get(&self, addr: SupervisorPageAddr) -> Option<&PageInfo> {
        // TODO: Support ownership tracking of huge-pages.
        if addr.size().is_huge() {
            return None;
        }
        let index = self.get_map_index(addr)?;
        self.pages.get(index)
    }

    /// Returns a mutable reference to the `PageInfo` struct for the 4k page at `addr`.
    pub fn get_mut(&mut self, addr: SupervisorPageAddr) -> Option<&mut PageInfo> {
        if addr.size().is_huge() {
            return None;
        }
        let index = self.get_map_index(addr)?;
        self.pages.get_mut(index)
    }

    /// Returns the number of pages after the page at `addr`
    pub fn num_after(&self, addr: SupervisorPageAddr) -> Option<usize> {
        if addr.size().is_huge() {
            return None;
        }
        let index = self.get_map_index(addr)?;
        self.pages.len().checked_sub(index)
    }

    /// Returns the index in the `PageMap` for the given address.
    fn get_map_index(&self, addr: SupervisorPageAddr) -> Option<usize> {
        self.sparse_map
            .iter()
            .find(|s| s.base_pfn <= addr.index() && addr.index() < s.base_pfn + s.num_pages)
            .map(|entry| entry.page_map_index + addr.index() - entry.base_pfn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::HwMemMapBuilder;
    use riscv_pages::{Page, PageAddr, RawAddr, SequentialPages};

    fn stub_page_vec() -> PageVec<PageInfo> {
        let backing_mem = vec![0u8; 8192];
        let aligned_pointer = unsafe {
            // Not safe - just a test
            backing_mem
                .as_ptr()
                .add(backing_mem.as_ptr().align_offset(4096))
        };
        let addr = PageAddr::new(RawAddr::supervisor(aligned_pointer as u64)).unwrap();
        let page = unsafe {
            // Test-only: safe because the backing memory is leaked so the memory used for this page
            // will live until the test exits.
            Page::new(addr)
        };
        PageVec::from(SequentialPages::from(page))
    }

    #[test]
    fn indexing() {
        let pages = stub_page_vec();
        let num_pages = 10;
        let base_addr = PageAddr::new(RawAddr::supervisor(0x1000_0000)).unwrap();
        let mem_map = unsafe {
            // Not safe - just a test.
            HwMemMapBuilder::new(PageSize::Size4k as u64)
                .add_memory_region(
                    RawAddr::from(base_addr),
                    num_pages * PageSize::Size4k as u64,
                )
                .unwrap()
                .build()
        };
        let mut pages = PageMap::new(pages);
        pages.populate_from(mem_map);

        let before_addr = PageAddr::new(RawAddr::supervisor(base_addr.bits() - 4096)).unwrap();
        let last_addr = base_addr.checked_add_pages(num_pages - 1).unwrap();
        let after_addr = last_addr.checked_add_pages(1).unwrap();

        assert!(pages.get(before_addr).is_none());
        assert!(pages.get(base_addr).is_some());
        assert!(pages.get(last_addr).is_some());
        assert!(pages.get(after_addr).is_none());
    }

    #[test]
    fn page_map_building() {
        let pages = stub_page_vec();
        let mut mem_map = unsafe {
            // Not safe - just a test.
            HwMemMapBuilder::new(PageSize::Size4k as u64)
                .add_memory_region(RawAddr::supervisor(0x1000_0000), 0x2_0000)
                .unwrap()
                .build()
        };
        mem_map
            .reserve_region(
                HwReservedMemType::FirmwareReserved,
                RawAddr::supervisor(0x1000_4000),
                0x1000,
            )
            .unwrap();
        mem_map
            .reserve_region(
                HwReservedMemType::HostKernelImage,
                RawAddr::supervisor(0x1001_0000),
                0x2000,
            )
            .unwrap();
        let mut pages = PageMap::new(pages);
        pages.populate_from(mem_map);

        let free_addr = PageAddr::new(RawAddr::supervisor(0x1000_1000)).unwrap();
        let reserved_addr = PageAddr::new(RawAddr::supervisor(0x1000_4000)).unwrap();
        let used_addr = PageAddr::new(RawAddr::supervisor(0x1001_1000)).unwrap();

        assert!(pages.get(free_addr).unwrap().is_free());
        assert!(pages.get(reserved_addr).unwrap().is_reserved());
        assert!(pages.get(used_addr).unwrap().owner().is_some());
    }

    #[test]
    fn sparse_map() {
        let pages = stub_page_vec();
        const TOTAL_SIZE: u64 = 0x4_0000;
        let mem_map = unsafe {
            // Not safe - just a test.
            HwMemMapBuilder::new(PageSize::Size4k as u64)
                .add_memory_region(RawAddr::supervisor(0x1000_0000), TOTAL_SIZE / 2)
                .unwrap()
                .add_memory_region(RawAddr::supervisor(0x2000_0000), TOTAL_SIZE / 2)
                .unwrap()
                .build()
        };
        let mut pages = PageMap::new(pages);
        pages.populate_from(mem_map);

        let base_addr = PageAddr::new(RawAddr::supervisor(0x1000_0000)).unwrap();
        let r0_addr = PageAddr::new(RawAddr::supervisor(0x1000_8000)).unwrap();
        let r1_addr = PageAddr::new(RawAddr::supervisor(0x2000_3000)).unwrap();

        assert!(pages.get(base_addr).unwrap().is_free());
        assert!(pages.get(r0_addr).unwrap().is_free());
        assert!(pages.get(r1_addr).unwrap().is_free());
        assert_eq!(
            pages.num_after(base_addr).unwrap(),
            (TOTAL_SIZE / PageSize::Size4k as u64) as usize
        );
    }

    #[test]
    fn page_ownership() {
        let mut page = PageInfo::new();
        assert!(page.is_free());
        assert!(page.push_owner(PageOwnerId::hypervisor()).is_ok());
        assert!(page.push_owner(PageOwnerId::host()).is_ok());
        assert_eq!(page.owner().unwrap(), PageOwnerId::host());
        assert_eq!(page.pop_owner().unwrap(), PageOwnerId::host());
        assert!(page.pop_owner().is_err());
        assert!(!page.is_free());

        let mut page = PageInfo::new_reserved();
        assert!(!page.is_free());
        assert!(page.push_owner(PageOwnerId::hypervisor()).is_err());
    }
}

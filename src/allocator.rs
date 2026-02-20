use crate::free_stack::FreeStack;
use crate::global_free_list::GlobalFreeList;
use crate::header::{self, WorkerLocalListHeads, WorkerLocalListPartialFullHeads};
use crate::linked_list_node::LinkedListNode;
use crate::remote_free_list::RemoteFreeList;
use crate::size_classes::{size_class, NUM_SIZE_CLASSES};
use crate::slab_meta::SlabMeta;
use crate::worker_local_list::WorkerLocalList;
use crate::{error::Error, header::Header, size_classes::size_class_index};
use core::mem::offset_of;
use core::ptr::NonNull;
use core::sync::atomic::Ordering;
use std::fs::File;

pub struct Allocator {
    base: AllocatorBase,
    worker_index: u32,
}

pub struct FreeOnlyAllocator {
    base: AllocatorBase,
}

struct AllocatorBase {
    header: NonNull<Header>,
    file_size: usize,
}

impl Allocator {
    /// Create a new `Allocator` in the provided file with the given parameters.
    /// `min_workers` is the minimum number of workers to support.
    ///
    /// # Safety
    /// - `create` must only be called once for a given file. Subsequent calls
    ///   with the same file must use `join`.
    pub unsafe fn create(
        file: &File,
        file_size: usize,
        min_workers: u32,
        slab_size: u32,
    ) -> Result<Self, Error> {
        let header = crate::init::create(file, file_size, min_workers, slab_size)?;
        let worker_index = match unsafe { claim_any_worker_index(header) } {
            Some(worker_index) => worker_index,
            None => {
                let _ = crate::memory_map::unmap_file(header.as_ptr().cast(), file_size);
                return Err(Error::NoAvailableWorkers);
            }
        };

        // SAFETY: The header is guaranteed to be valid and initialized.
        Allocator::new(header, file_size, worker_index)
    }

    /// Join an existing allocator in the provided file.
    /// Picks the first available worker slot.
    pub fn join(file: &File) -> Result<Self, Error> {
        let (header, file_size) = crate::init::join(file)?;
        let worker_index = match unsafe { claim_any_worker_index(header) } {
            Some(worker_index) => worker_index,
            None => {
                let _ = crate::memory_map::unmap_file(header.as_ptr().cast(), file_size);
                return Err(Error::NoAvailableWorkers);
            }
        };

        // SAFETY: The header is guaranteed to be valid and initialized.
        Allocator::new(header, file_size, worker_index)
    }

    /// Creates a new `Allocator` for the given worker index.
    ///
    /// # Safety
    /// - The `header` must point to a valid header of an initialized allocator.
    fn new(header: NonNull<Header>, file_size: usize, worker_index: u32) -> Result<Self, Error> {
        // SAFETY: The header is assumed to be valid and initialized.
        if worker_index >= unsafe { header.as_ref() }.num_workers {
            return Err(Error::InvalidWorkerIndex);
        }
        Ok(Allocator {
            base: AllocatorBase::new(header, file_size),
            worker_index,
        })
    }
}

unsafe impl Send for Allocator {}
unsafe impl Send for FreeOnlyAllocator {}

impl Drop for Allocator {
    fn drop(&mut self) {
        self.release_worker();
        self.base.unmap();
    }
}

impl Drop for FreeOnlyAllocator {
    fn drop(&mut self) {
        self.base.unmap();
    }
}

impl FreeOnlyAllocator {
    /// Join an existing allocator in the provided file.
    pub fn join(file: &File) -> Result<Self, Error> {
        let (header, file_size) = crate::init::join(file)?;
        Ok(FreeOnlyAllocator {
            base: AllocatorBase::new(header, file_size),
        })
    }
}

impl Allocator {
    fn release_worker(&self) {
        self.worker_meta().claimed.store(0, Ordering::Release);
    }

    /// Allocates a block of memory of the given size.
    /// If the size is larger than the maximum size class, returns `None`.
    /// If the allocation fails, returns `None`.
    pub fn allocate(&self, size: u32) -> Option<NonNull<u8>> {
        let size_index = size_class_index(size)?;

        // SAFETY: `size_index` is guaranteed to be valid by `size_class_index`.
        let slab_index = unsafe { self.find_allocatable_slab_index(size_index) }?;
        // SAFETY:
        // - `slab_index` is guaranteed to be valid by `find_allocatable_slab_index`.
        // - `size_index` is guaranteed to be valid by `size_class_index`.
        unsafe { self.allocate_within_slab(slab_index, size_index) }
    }

    /// Try to find a suitable slab for allocation.
    /// If a partial slab assigned to the worker is not found, then try to find
    /// a slab from the global free list.
    ///
    /// # Safety
    /// - The `size_index` must be a valid index for the size classes.
    unsafe fn find_allocatable_slab_index(&self, size_index: usize) -> Option<u32> {
        // SAFETY: `size_index` is guaranteed to be valid by the caller.
        unsafe { self.worker_local_list_partial(size_index) }
            .head()
            .or_else(|| self.take_slab(size_index))
    }

    /// Attempt to allocate meomry within a slab.
    /// If the slab is full or the allocation otherwise fails, returns `None`.
    ///
    /// # Safety
    /// - The `slab_index` must be a valid index for the slabs
    /// - The `size_index` must be a valid index for the size classes.
    unsafe fn allocate_within_slab(
        &self,
        slab_index: u32,
        size_index: usize,
    ) -> Option<NonNull<u8>> {
        // SAFETY: The slab index is guaranteed to be valid by the caller.
        let mut free_stack = unsafe { self.slab_free_stack(slab_index) };
        let maybe_index_within_slab = free_stack.pop();

        // If the slab is empty - remove it from the worker's partial list,
        // and move it to the worker's full list.
        if free_stack.is_empty() {
            // SAFETY:
            // - The `slab_index` is guaranteed to be valid by the caller.
            // - The `size_index` is guaranteed to be valid by the caller.
            unsafe {
                self.worker_local_list_partial(size_index)
                    .remove(slab_index);
            }
            // SAFETY:
            // - The `slab_index` is guaranteed to be valid by the caller.
            // - The `size_index` is guaranteed to be valid by the caller.
            unsafe {
                self.worker_local_list_full(size_index).push(slab_index);
            }
        }

        maybe_index_within_slab.map(|index_within_slab| {
            // SAFETY: The `slab_index` is guaranteed to be valid by the caller.
            let slab = unsafe { self.slab(slab_index) };
            // SAFETY: The `size_index` is guaranteed to be valid by the caller.
            let size = unsafe { size_class(size_index) };
            self.worker_meta()
                .outstanding_allocation_bytes
                .fetch_add(size as u64, Ordering::Relaxed);
            slab.byte_add(index_within_slab as usize * size as usize)
        })
    }

    /// Attempt to take a slab from the global free list.
    /// If the global free list is empty, returns `None`.
    /// If the slab is successfully taken, it will be marked as assigned to the worker.
    ///
    /// # Safety
    /// - The `size_index` must be a valid index for the size claasses.
    unsafe fn take_slab(&self, size_index: usize) -> Option<u32> {
        let slab_index = self.global_free_list().pop()?;

        // SAFETY: The slab index is guaranteed to be valid by `pop`.
        unsafe { self.slab_meta(slab_index).as_ref() }.assign(self.worker_index, size_index);
        // SAFETY:
        // - The slab index is guaranteed to be valid by `pop`.
        // - The size index is guaranteed to be valid by the caller.
        unsafe {
            let slab_capacity = self.base.header.as_ref().slab_size / size_class(size_index);
            self.slab_free_stack(slab_index).reset(slab_capacity as u16);
        };
        // SAFETY: The size index is guaranteed to be valid by caller.
        let mut worker_local_list = unsafe { self.worker_local_list_partial(size_index) };
        // SAFETY: The slab index is guaranteed to be valid by `pop`.
        unsafe { worker_local_list.push(slab_index) };
        Some(slab_index)
    }
}

impl Allocator {
    /// Free a block of memory previously allocated by this allocator.
    ///
    /// # Safety
    /// - The `ptr` must be a valid pointer to a block of memory allocated by this allocator.
    /// - The `ptr` must not have been freed before.
    pub unsafe fn free(&self, ptr: NonNull<u8>) {
        // SAFETY: The pointer is assumed to be valid and allocated by this allocator.
        let offset = unsafe { self.offset(ptr) };
        self.free_offset(offset);
    }

    /// Free a block of memory previously allocated by this allocator.
    ///
    /// # Safety
    /// - The `offset` must be a valid offset within the memory allocated by this allocator.
    /// - The `offset` must not have been freed before.
    pub unsafe fn free_offset(&self, offset: usize) {
        let allocation_indexes = self.find_allocation_indexes(offset);

        // Check if the slab is assigned to this worker.
        if self.worker_index
            == unsafe { self.slab_meta(allocation_indexes.slab_index).as_ref() }
                .assigned_worker
                .load(Ordering::Acquire)
        {
            // SAFETY: The allocation indexes are valid and come from allocator-owned memory.
            let (size_index, size) = unsafe { self.slab_size_class(allocation_indexes.slab_index) };
            self.worker_meta()
                .outstanding_allocation_bytes
                .fetch_sub(size as u64, Ordering::Relaxed);
            self.local_free_with_size_index(allocation_indexes, size_index);
        } else {
            self.remote_free(allocation_indexes);
        }
    }

    fn local_free_with_size_index(&self, allocation_indexes: AllocationIndexes, size_index: usize) {
        // SAFETY: The allocation indexes are guaranteed to be valid by the caller.
        let (was_full, is_empty) = unsafe {
            let mut free_stack = self.slab_free_stack(allocation_indexes.slab_index);
            let was_full = free_stack.is_empty();
            free_stack.push(allocation_indexes.index_within_slab);
            // Names confusing:
            // - When the **free** stack is empty, the slab is full of allocations.
            // - When the **free** stack is full, the slab has no allocations available.
            (was_full, free_stack.is_full())
        };

        match (was_full, is_empty) {
            (true, true) => {
                // The slab was full and is now empty - this cannot happen unless the slab
                // size is equal to the size class.
                unreachable!("slab can only contain one allocation - this is not allowed");
            }
            (true, false) => {
                // The slab was full and is now partially full. It must be moved
                // from the worker's full list to the worker's partial list.
                // SAFETY: The allocation indexes are guaranteed to be valid by the caller.
                unsafe {
                    self.worker_local_list_full(size_index)
                        .remove(allocation_indexes.slab_index);
                }
                // SAFETY: The allocation indexes are guaranteed to be valid by the caller.
                unsafe {
                    self.worker_local_list_partial(size_index)
                        .push(allocation_indexes.slab_index);
                }
            }
            (false, true) => {
                // The slab was partially full and is now empty.
                // It must be moved from the worker's partial list to the global free list.
                // SAFETY: The allocation indexes are guaranteed to be valid by the caller.
                unsafe {
                    self.worker_local_list_partial(size_index)
                        .remove(allocation_indexes.slab_index);
                }
                // SAFETY: The allocation indexes are guaranteed to be valid by the caller.
                unsafe {
                    self.global_free_list().push(allocation_indexes.slab_index);
                }
            }
            (false, false) => {
                // The slab was partially full and is still partially full.
                // No action is needed, just return.
            }
        }
    }

    fn remote_free(&self, allocation_indexes: AllocationIndexes) {
        // SAFETY: The allocation indexes are guaranteed to be valid by the caller.
        unsafe {
            self.base
                .remote_free_list(allocation_indexes.slab_index)
                .push(allocation_indexes.index_within_slab);
        }
    }

    /// Find the offset given a pointer.
    ///
    /// # Safety
    /// - The `ptr` must be a valid pointer in the allocator's address space.
    pub unsafe fn offset(&self, ptr: NonNull<u8>) -> usize {
        self.base.offset(ptr)
    }

    /// Return a ptr given a shareable offset - calculated by `offset`.
    ///
    /// # Safety
    ///
    /// - Caller must ensure the offset is valid for this allocator.
    pub unsafe fn ptr_from_offset(&self, offset: usize) -> NonNull<u8> {
        self.base.ptr_from_offset(offset)
    }

    /// Find the slab index and index within the slab for a given offset.
    fn find_allocation_indexes(&self, offset: usize) -> AllocationIndexes {
        self.base.find_allocation_indexes(offset)
    }
}

impl FreeOnlyAllocator {
    /// Free a block of memory previously allocated by this allocator.
    ///
    /// # Safety
    /// - The `ptr` must be a valid pointer to a block of memory allocated by this allocator.
    /// - The `ptr` must not have been freed before.
    pub unsafe fn free(&self, ptr: NonNull<u8>) {
        // SAFETY: The pointer is assumed to be valid and allocated by this allocator.
        let offset = unsafe { self.offset(ptr) };
        self.free_offset(offset);
    }

    /// Free a block of memory previously allocated by this allocator.
    ///
    /// # Safety
    /// - The `offset` must be a valid offset within the memory allocated by this allocator.
    /// - The `offset` must not have been freed before.
    pub unsafe fn free_offset(&self, offset: usize) {
        let allocation_indexes = self.find_allocation_indexes(offset);
        // SAFETY: The allocation indexes are guaranteed to be valid by the caller.
        unsafe {
            self.base
                .remote_free_list(allocation_indexes.slab_index)
                .push(allocation_indexes.index_within_slab);
        }
    }

    /// Find the offset given a pointer.
    ///
    /// # Safety
    /// - The `ptr` must be a valid pointer in the allocator's address space.
    pub unsafe fn offset(&self, ptr: NonNull<u8>) -> usize {
        self.base.offset(ptr)
    }

    /// Return a ptr given a shareable offset - calculated by `offset`.
    ///
    /// # Safety
    ///
    /// - Caller must ensure the offset is valid for this allocator.
    pub unsafe fn ptr_from_offset(&self, offset: usize) -> NonNull<u8> {
        self.base.ptr_from_offset(offset)
    }

    /// Find the slab index and index within the slab for a given offset.
    fn find_allocation_indexes(&self, offset: usize) -> AllocationIndexes {
        self.base.find_allocation_indexes(offset)
    }
}

impl AllocatorBase {
    fn new(header: NonNull<Header>, file_size: usize) -> Self {
        Self { header, file_size }
    }

    fn unmap(&self) {
        // SAFETY: The header is guaranteed to be valid and initialized.
        let _ = crate::memory_map::unmap_file(self.header.as_ptr().cast(), self.file_size);
    }

    /// Find the offset given a pointer.
    ///
    /// # Safety
    /// - The `ptr` must be a valid pointer in the allocator's address space.
    unsafe fn offset(&self, ptr: NonNull<u8>) -> usize {
        ptr.byte_offset_from(self.header) as usize
    }

    /// Return a ptr given a shareable offset - calculated by `offset`.
    ///
    /// # Safety
    ///
    /// - Caller must ensure the offset is valid for this allocator.
    unsafe fn ptr_from_offset(&self, offset: usize) -> NonNull<u8> {
        unsafe { self.header.byte_add(offset) }.cast()
    }

    /// Find the slab index and index within the slab for a given offset.
    fn find_allocation_indexes(&self, offset: usize) -> AllocationIndexes {
        let (slab_index, offset_within_slab) = {
            // SAFETY: The header is assumed to be valid and initialized.
            let header = unsafe { self.header.as_ref() };
            debug_assert!(offset >= header.slabs_offset as usize);
            let offset_from_slab_start = offset.wrapping_sub(header.slabs_offset as usize);
            let slab_index = (offset_from_slab_start / header.slab_size as usize) as u32;
            debug_assert!(slab_index < header.num_slabs, "slab index out of bounds");

            // SAFETY: The slab size is guaranteed to be a power of 2, for a valid header.
            let offset_within_slab =
                unsafe { Self::offset_within_slab(header.slab_size, offset_from_slab_start) };

            (slab_index, offset_within_slab)
        };

        let index_within_slab = {
            // SAFETY: The slab index is guaranteed to be valid by the above calculations.
            let size_class_index = unsafe { self.slab_meta(slab_index).as_ref() }
                .size_class_index
                .load(Ordering::Acquire);
            // SAFETY: The size class index is guaranteed to be valid by valid slab meta.
            let size_class = unsafe { size_class(size_class_index) };
            (offset_within_slab / size_class) as u16
        };

        AllocationIndexes {
            slab_index,
            index_within_slab,
        }
    }

    /// Return offset within a slab.
    ///
    /// # Safety
    /// - The `slab_size` must be a power of 2.
    const unsafe fn offset_within_slab(slab_size: u32, offset_from_slab_start: usize) -> u32 {
        debug_assert!(slab_size.is_power_of_two());
        (offset_from_slab_start & (slab_size as usize - 1)) as u32
    }

    /// Returns an instance of `RemoteFreeList` for the given slab.
    ///
    /// # Safety
    /// - `slab_index` must be a valid slab index.
    unsafe fn remote_free_list<'a>(&'a self, slab_index: u32) -> RemoteFreeList<'a> {
        let (head, slab_item_size) = {
            // SAFETY: The slab index is guaranteed to be valid by the caller.
            let slab_meta = unsafe { self.slab_meta(slab_index).as_ref() };
            // SAFETY: The slab meta is guaranteed to be valid by the caller.
            let size_class =
                unsafe { size_class(slab_meta.size_class_index.load(Ordering::Acquire)) };
            (&slab_meta.remote_free_stack_head, size_class)
        };
        // SAFETY: The slab index is guaranteed to be valid by the caller.
        let slab = unsafe { self.slab(slab_index) };

        // SAFETY:
        // - `slab_item_size` must be a valid size AND currently assigned to the slab.
        // - `head` must be a valid reference to a `CacheAlignedU16
        //   that is the head of the remote free list.
        // - `slab` must be a valid pointer to the beginning of the slab.
        unsafe { RemoteFreeList::new(slab_item_size, head, slab) }
    }

    /// Returns a pointer to the slab meta for the given slab index.
    ///
    /// # Safety
    /// - The `slab_index` must be a valid index for the slabs.
    unsafe fn slab_meta(&self, slab_index: u32) -> NonNull<SlabMeta> {
        // SAFETY: The header is assumed to be valid and initialized.
        let offset = unsafe { self.header.as_ref() }.slab_shared_meta_offset;
        // SAFETY: The header is guaranteed to be valid and initialized.
        let slab_metas = unsafe { self.header.byte_add(offset as usize).cast::<SlabMeta>() };
        // SAFETY: The `slab_index` is guaranteed to be valid by the caller.
        unsafe { slab_metas.add(slab_index as usize) }
    }

    /// Return a pointer to a slab.
    ///
    /// # Safety
    /// - The `slab_index` must be a valid index for the slabs.
    unsafe fn slab(&self, slab_index: u32) -> NonNull<u8> {
        let (slab_size, offset) = {
            // SAFETY: The header is assumed to be valid and initialized.
            let header = unsafe { self.header.as_ref() };
            (header.slab_size, header.slabs_offset)
        };
        // SAFETY: The header is guaranteed to be valid and initialized.
        // The slabs are laid out sequentially after the free stacks.
        unsafe {
            self.header
                .byte_add(offset as usize)
                .byte_add(slab_index as usize * slab_size as usize)
                .cast()
        }
    }

    fn free_list_elements(&self) -> NonNull<LinkedListNode> {
        // SAFETY: The header is assumed to be valid and initialized.
        let offset = unsafe { self.header.as_ref() }.free_list_elements_offset;
        // SAFETY: The header is guaranteed to be valid and initialized.
        unsafe { self.header.byte_add(offset as usize) }.cast()
    }
}

impl Allocator {
    pub fn outstanding_allocation_bytes(&self) -> u64 {
        self.worker_meta()
            .outstanding_allocation_bytes
            .load(Ordering::Relaxed)
    }

    /// Frees all items in remote free lists.
    pub fn clean_remote_free_lists(&self) {
        // Do partial slabs before full slabs, because the act of freeing within
        // the full slabs may move them to partial slabs list, which would lead
        // to us double-iterating.
        for size_index in 0..NUM_SIZE_CLASSES {
            // SAFETY: The size index is guaranteed to be valid by the loop.
            let worker_local_list = unsafe { self.worker_local_list_partial(size_index) };
            self.clean_remote_free_lists_for_list(worker_local_list);

            // SAFETY: The size index is guaranteed to be valid by the loop.
            let worker_local_list = unsafe { self.worker_local_list_full(size_index) };
            self.clean_remote_free_lists_for_list(worker_local_list);
        }
    }

    /// Frees all items in the remote free list for the given worker local list.
    fn clean_remote_free_lists_for_list(&self, worker_local_list: WorkerLocalList) {
        for slab_index in worker_local_list.iterate() {
            // SAFETY: Slab indices come from worker-local lists and are valid.
            let (size_index, size) = unsafe { self.slab_size_class(slab_index) };
            let mut drained_items = 0u64;
            // SAFETY: The slab index is guaranteed to be valid by the iterator.
            let remote_free_list = unsafe { self.remote_free_list(slab_index) };
            for index_within_slab in remote_free_list.drain() {
                self.local_free_with_size_index(
                    AllocationIndexes {
                        slab_index,
                        index_within_slab,
                    },
                    size_index,
                );
                drained_items += 1;
            }
            if drained_items != 0 {
                self.worker_meta()
                    .outstanding_allocation_bytes
                    .fetch_sub(drained_items * size as u64, Ordering::Relaxed);
            }
        }
    }
}

impl Allocator {
    /// Returns a pointer to the free list elements in allocator.
    fn free_list_elements(&self) -> NonNull<LinkedListNode> {
        self.base.free_list_elements()
    }

    /// Returns a `GlobalFreeList` to interact with the global free list.
    fn global_free_list<'a>(&'a self) -> GlobalFreeList<'a> {
        // SAFETY: The header is assumed to be valid and initialized.
        let head = &unsafe { self.base.header.as_ref() }.global_free_list_head;
        let list = self.free_list_elements();
        // SAFETY:
        // - `head` is a valid reference to the global free list head.
        // - `list` is guaranteed to be valid wtih sufficient capacity.
        unsafe { GlobalFreeList::new(head, list) }
    }

    /// Returns a `WorkerLocalList` for the current worker to interact with its
    /// local free list of partially full slabs.
    ///
    /// # Safety
    /// - The `size_index` must be a valid index for the size classes.
    unsafe fn worker_local_list_partial<'a>(&'a self, size_index: usize) -> WorkerLocalList<'a> {
        let head = &self.worker_head(size_index).partial;
        let list = self.free_list_elements();

        // SAFETY:
        // - `head` is a valid reference to the worker's local list head.
        // - `list` is guaranteed to be valid with sufficient capacity.
        unsafe { WorkerLocalList::new(head, list) }
    }

    /// Returns a `WorkerLocalList` for the current worker to interact with its
    /// local free list of full slabs.
    ///
    /// # Safety
    /// - The `size_index` must be a valid index for the size classes.
    unsafe fn worker_local_list_full<'a>(&'a self, size_index: usize) -> WorkerLocalList<'a> {
        let head = &self.worker_head(size_index).full;
        let list = self.free_list_elements();

        // SAFETY:
        // - `head` is a valid reference to the worker's local list head.
        // - `list` is guaranteed to be valid with sufficient capacity.
        unsafe { WorkerLocalList::new(head, list) }
    }

    fn worker_meta(&self) -> &WorkerLocalListHeads {
        // SAFETY: The worker index is guaranteed to be valid by the constructor.
        unsafe { worker_meta_ptr(self.base.header, self.worker_index).as_ref() }
    }

    fn worker_head(&self, size_index: usize) -> &WorkerLocalListPartialFullHeads {
        // SAFETY: The size index is guaranteed to be valid by the caller.
        &self.worker_meta().heads[size_index]
    }

    /// Returns the slab's assigned size class index and class size in bytes.
    ///
    /// # Safety
    /// - `slab_index` must be a valid slab index.
    unsafe fn slab_size_class(&self, slab_index: u32) -> (usize, u32) {
        let size_index = unsafe { self.slab_meta(slab_index).as_ref() }
            .size_class_index
            .load(Ordering::Relaxed);
        // SAFETY: The slab meta stores a valid size class index while assigned.
        let size = unsafe { size_class(size_index) };
        (size_index, size)
    }

    /// Returns an instance of `RemoteFreeList` for the given slab.
    ///
    /// # Safety
    /// - `slab_index` must be a valid slab index.
    unsafe fn remote_free_list<'a>(&'a self, slab_index: u32) -> RemoteFreeList<'a> {
        self.base.remote_free_list(slab_index)
    }

    /// Returns a pointer to the slab meta for the given slab index.
    ///
    /// # Safety
    /// - The `slab_index` must be a valid index for the slabs.
    unsafe fn slab_meta(&self, slab_index: u32) -> NonNull<SlabMeta> {
        self.base.slab_meta(slab_index)
    }

    /// Return a mutable reference to a free stack for the given slab index.
    ///
    /// # Safety
    /// - The `slab_index` must be a valid index for the slabs.
    unsafe fn slab_free_stack<'a>(&'a self, slab_index: u32) -> FreeStack<'a> {
        let (slab_size, offset) = {
            // SAFETY: The header is assumed to be valid and initialized.
            let header = unsafe { self.base.header.as_ref() };
            (header.slab_size, header.slab_free_stacks_offset)
        };
        let free_stack_size = header::layout::single_free_stack_size(slab_size);

        // SAFETY: The `FreeStack` layout is guaranteed to have enough room
        // for top, capacity, and the trailing stack.
        let mut top = unsafe {
            self.base
                .header
                .byte_add(offset as usize)
                .byte_add(slab_index as usize * free_stack_size)
                .cast()
        };
        let mut capacity = unsafe { top.add(1) };
        let trailing_stack = unsafe { capacity.add(1) };
        unsafe { FreeStack::new(top.as_mut(), capacity.as_mut(), trailing_stack) }
    }

    /// Return a pointer to a slab.
    ///
    /// # Safety
    /// - The `slab_index` must be a valid index for the slabs.
    unsafe fn slab(&self, slab_index: u32) -> NonNull<u8> {
        self.base.slab(slab_index)
    }
}

unsafe fn worker_meta_ptr(
    header: NonNull<Header>,
    worker_index: u32,
) -> NonNull<WorkerLocalListHeads> {
    let all_workers_heads = unsafe {
        header
            .byte_add(offset_of!(Header, worker_local_list_heads))
            .cast::<WorkerLocalListHeads>()
    };
    // SAFETY: The caller guarantees the worker index is in range.
    unsafe { all_workers_heads.add(worker_index as usize) }
}

unsafe fn claim_any_worker_index(header: NonNull<Header>) -> Option<u32> {
    let num_workers = unsafe { header.as_ref() }.num_workers;
    for worker_index in 0..num_workers {
        let claimed = unsafe { &worker_meta_ptr(header, worker_index).as_ref().claimed };
        if claimed
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Some(worker_index);
        }
    }
    None
}

struct AllocationIndexes {
    slab_index: u32,
    index_within_slab: u16,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::size_classes::{MAX_SIZE, SIZE_CLASSES};

    const TEST_BUFFER_SIZE: usize = 64 * 1024 * 1024; // 64 MiB

    fn create_temp_shmem_file() -> Result<File, Error> {
        use std::fs::OpenOptions;
        use std::sync::atomic::{AtomicU64, Ordering};

        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let temp_dir = std::env::temp_dir();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = temp_dir.join(format!("rts-alloc-{n}.tmp"));

        let mut open_options = OpenOptions::new();
        open_options.read(true).write(true).create_new(true);

        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_ATTRIBUTE_TEMPORARY, FILE_FLAG_DELETE_ON_CLOSE,
            };

            open_options
                .attributes(FILE_ATTRIBUTE_TEMPORARY)
                .custom_flags(FILE_FLAG_DELETE_ON_CLOSE);
        }

        let open_result = open_options.open(&path);

        match open_result {
            Ok(file) => {
                #[cfg(unix)]
                {
                    std::fs::remove_file(&path)?;
                }
                Ok(file)
            }
            Err(err) => Err(Error::IoError(err)),
        }
    }

    fn initialize_for_test(slab_size: u32, num_workers: u32) -> (File, Allocator) {
        let file = create_temp_shmem_file().unwrap();
        // SAFETY: Test helper creates allocator from a fresh temp shared-memory file.
        let allocator =
            unsafe { Allocator::create(&file, TEST_BUFFER_SIZE, num_workers, slab_size).unwrap() };
        (file, allocator)
    }

    #[test]
    fn test_allocator() {
        let slab_size = 65536; // 64 KiB
        let num_workers = 4;
        let (_file, allocator) = initialize_for_test(slab_size, num_workers);
        assert_eq!(allocator.outstanding_allocation_bytes(), 0);

        let mut allocations = vec![];
        let mut total_allocated_bytes = 0u64;

        for class_size in SIZE_CLASSES[..NUM_SIZE_CLASSES - 1].iter() {
            for size in [class_size - 1, *class_size, class_size + 1] {
                allocations.push(allocator.allocate(size).unwrap());
                total_allocated_bytes += size_class_index(size)
                    .map(|i| unsafe { size_class(i) as u64 })
                    .unwrap();
            }
        }
        for size in [MAX_SIZE - 1, MAX_SIZE] {
            allocations.push(allocator.allocate(size).unwrap());
            total_allocated_bytes += size_class_index(size)
                .map(|i| unsafe { size_class(i) as u64 })
                .unwrap();
        }
        assert_eq!(
            allocator.outstanding_allocation_bytes(),
            total_allocated_bytes
        );
        assert!(allocator.allocate(MAX_SIZE + 1).is_none());

        // The worker should have local lists for all size classes.
        for size_index in 0..NUM_SIZE_CLASSES {
            // SAFETY: The size index is guaranteed to be valid by the loop.
            let worker_local_list = unsafe { allocator.worker_local_list_partial(size_index) };
            assert!(worker_local_list.head().is_some());
        }

        for ptr in allocations {
            // SAFETY: ptr is valid allocation from the allocator.
            unsafe {
                allocator.free(ptr);
            }
        }
        assert_eq!(allocator.outstanding_allocation_bytes(), 0);

        // The worker local lists should be empty after freeing.
        for size_index in 0..NUM_SIZE_CLASSES {
            // SAFETY: The size index is guaranteed to be valid by the loop.
            let worker_local_list = unsafe { allocator.worker_local_list_partial(size_index) };
            assert_eq!(worker_local_list.head(), None);
        }
    }

    #[test]
    fn test_slab_list_transitions() {
        let slab_size = 65536; // 64 KiB
        let num_workers = 4;
        let (_file, allocator) = initialize_for_test(slab_size, num_workers);

        let allocation_size = 2048;
        let size_index = size_class_index(allocation_size).unwrap();
        let allocations_per_slab = slab_size / allocation_size;

        fn check_worker_list_expectations(
            allocator: &Allocator,
            size_index: usize,
            expect_partial: bool,
            expect_full: bool,
        ) {
            unsafe {
                let partial_list = allocator.worker_local_list_partial(size_index);
                assert_eq!(
                    partial_list.head().is_some(),
                    expect_partial,
                    "{:?}",
                    partial_list.head()
                );

                let full_list = allocator.worker_local_list_full(size_index);
                assert_eq!(
                    full_list.head().is_some(),
                    expect_full,
                    "{:?}",
                    full_list.head()
                );
            }
        }

        // The parital list and full list should begin empty.
        check_worker_list_expectations(&allocator, size_index, false, false);

        let mut first_slab_allocations = vec![];
        for _ in 0..allocations_per_slab - 1 {
            first_slab_allocations.push(allocator.allocate(allocation_size).unwrap());
        }

        // The first slab should be partially full and the full list empty.
        check_worker_list_expectations(&allocator, size_index, true, false);

        // Allocate one more to fill the slab.
        first_slab_allocations.push(allocator.allocate(allocation_size).unwrap());

        // The first slab should now be full and moved to the full list.
        check_worker_list_expectations(&allocator, size_index, false, true);

        // Allocating again will give a new slab, which will be partially full.
        let second_slab_allocation = allocator.allocate(allocation_size).unwrap();

        // The second slab should be partially full and the first slab in the full list.
        check_worker_list_expectations(&allocator, size_index, true, true);

        let mut first_slab_allocations = first_slab_allocations.drain(..);
        unsafe {
            allocator.free(first_slab_allocations.next().unwrap());
        }
        // Both slabs should be partially full, and none are full.
        check_worker_list_expectations(&allocator, size_index, true, false);

        // Free the first slab allocation.
        for ptr in first_slab_allocations {
            unsafe {
                allocator.free(ptr);
            }
        }
        // The first slab is now empty and should be moved to the global free list,
        // but the second slab is still partially full.
        check_worker_list_expectations(&allocator, size_index, true, false);

        // Free the second slab allocation.
        unsafe {
            allocator.free(second_slab_allocation);
        }
        // Both slabs should now be empty and moved to the global free list.
        check_worker_list_expectations(&allocator, size_index, false, false);
    }

    #[test]
    fn test_out_of_slabs() {
        let slab_size = 65536; // 64 KiB
        let num_workers = 4;
        let (_file, allocator) = initialize_for_test(slab_size, num_workers);

        let num_slabs = unsafe { allocator.base.header.as_ref() }.num_slabs;
        for index in 0..num_slabs {
            let slab_index = unsafe { allocator.take_slab(0) }.unwrap();
            assert_eq!(slab_index, index);
        }
        // The next slab allocation should fail, as all slabs are taken.
        assert!(unsafe { allocator.take_slab(0) }.is_none());
    }

    #[test]
    fn test_remote_free_lists() {
        let slab_size = 65536; // 64 KiB
        let num_workers = 4;
        let (file, allocator_0) = initialize_for_test(slab_size, num_workers);
        let file_for_join = file.try_clone().unwrap();
        let allocator_1 = Allocator::join(&file_for_join).unwrap();

        let allocation_size = 2048;
        let size_index = size_class_index(allocation_size).unwrap();
        let allocations_per_slab = slab_size / allocation_size;

        // Allocate enough to fill the first slab.
        let mut allocations = vec![];
        for _ in 0..allocations_per_slab {
            allocations.push(allocator_0.allocate(allocation_size).unwrap());
        }

        // The first slab should be full.
        let slab_index = unsafe {
            let worker_local_list = allocator_0.worker_local_list_partial(size_index);
            assert!(worker_local_list.head().is_none());
            let worker_local_list = allocator_0.worker_local_list_full(size_index);
            assert!(worker_local_list.head().is_some());
            worker_local_list.head().unwrap()
        };

        // The slab's remote list should be empty.
        let remote_free_list = unsafe { allocator_0.remote_free_list(slab_index) };
        assert!(remote_free_list.iterate().next().is_none());

        // Free the allocations to the remote free list.
        for ptr in allocations {
            unsafe {
                let offset = allocator_0.offset(ptr);
                allocator_1.free_offset(offset);
            }
        }
        assert_eq!(
            allocator_0.outstanding_allocation_bytes(),
            allocations_per_slab as u64 * allocation_size as u64
        );

        // Allocator 0 can NOT allocate in the same slab.
        let different_slab_allocation = allocator_0.allocate(allocation_size).unwrap();
        let allocation_indexes = unsafe {
            allocator_0.find_allocation_indexes(allocator_0.offset(different_slab_allocation))
        };
        assert_ne!(allocation_indexes.slab_index, slab_index);
        unsafe { allocator_0.free(different_slab_allocation) };

        // If we clean the remote free lists, the next allocation should succeed in the same slab.
        allocator_0.clean_remote_free_lists();
        assert_eq!(allocator_0.outstanding_allocation_bytes(), 0);
        let same_slab_allocation = allocator_0.allocate(allocation_size).unwrap();
        let allocation_indexes = unsafe {
            allocator_0.find_allocation_indexes(allocator_0.offset(same_slab_allocation))
        };
        assert_eq!(allocation_indexes.slab_index, slab_index);
    }

    #[test]
    fn test_free_only_allocator() {
        let slab_size = 65536; // 64 KiB
        let num_workers = 4;
        let (file, allocator) = initialize_for_test(slab_size, num_workers);
        let file_for_join = file.try_clone().unwrap();
        let free_only_allocator = FreeOnlyAllocator::join(&file_for_join).unwrap();

        let allocation_size = 2048;
        let allocation = allocator.allocate(allocation_size).unwrap();

        let allocation_indexes =
            unsafe { allocator.find_allocation_indexes(allocator.offset(allocation)) };

        // SAFETY: allocation is a valid pointer allocated by the allocator.
        unsafe {
            let offset = allocator.offset(allocation);
            free_only_allocator.free_offset(offset);
        }

        // Index should be in the remote free list for the slab.
        assert_eq!(
            unsafe { allocator.remote_free_list(allocation_indexes.slab_index) }
                .iterate()
                .next()
                .unwrap(),
            allocation_indexes.index_within_slab
        );
    }
}

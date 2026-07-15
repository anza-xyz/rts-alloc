use crate::allocator::{Allocator, AllocatorBase, FreeOnlyAllocator};
use core::ptr::NonNull;

/// Collects frees and publishes remote frees in batches grouped by owning worker.
///
/// Frees owned by the worker represented by the [`Allocator`] handle that created
/// the batch are reclaimed immediately. All other frees are linked into intrusive
/// chains and published when [`Self::flush`] is called or the batch is dropped.
pub struct RemoteFreeBatch<'a> {
    source: RemoteFreeBatchSource<'a>,
    chains: Vec<WorkerRemoteFreeChain>,
}

enum RemoteFreeBatchSource<'a> {
    Allocator(&'a Allocator),
    FreeOnly(&'a FreeOnlyAllocator),
}

struct WorkerRemoteFreeChain {
    // INVARIANT: `head` and `tail` are valid, uniquely freed allocation offsets
    // whose intrusive links form a private chain owned by the batch.
    worker_index: u32,
    head: usize,
    tail: usize,
}

impl RemoteFreeBatchSource<'_> {
    fn base(&self) -> &AllocatorBase {
        match self {
            Self::Allocator(allocator) => allocator.base(),
            Self::FreeOnly(allocator) => allocator.base(),
        }
    }

    /// Find the offset for an allocation pointer.
    ///
    /// # Safety
    /// - `ptr` must be a valid pointer into this allocator region.
    unsafe fn offset(&self, ptr: NonNull<u8>) -> usize {
        match self {
            Self::Allocator(allocator) => unsafe { allocator.offset(ptr) },
            Self::FreeOnly(allocator) => unsafe { allocator.offset(ptr) },
        }
    }
}

impl<'a> RemoteFreeBatch<'a> {
    fn new(source: RemoteFreeBatchSource<'a>) -> Self {
        Self {
            source,
            chains: Vec::new(),
        }
    }

    /// Free a block of memory from this allocator region.
    ///
    /// Locally owned allocations are reclaimed immediately. Remote frees remain
    /// private to this batch until it is flushed or dropped.
    ///
    /// # Safety
    /// - `ptr` must point to a valid allocation in this allocator region.
    /// - The `ptr` must not have been freed before or added to another free batch.
    pub unsafe fn free(&mut self, ptr: NonNull<u8>) {
        // SAFETY: The caller guarantees that the pointer refers to a valid allocation
        // in this allocator region.
        let offset = unsafe { self.source.offset(ptr) };
        // SAFETY: The offset was derived from the caller-provided allocation pointer.
        unsafe { self.free_offset(offset) };
    }

    /// Free a block of memory from this allocator region.
    ///
    /// Locally owned allocations are reclaimed immediately. Remote frees remain
    /// private to this batch until it is flushed or dropped.
    ///
    /// # Safety
    /// - `offset` must identify a valid allocation in this allocator region.
    /// - The `offset` must not have been freed before or added to another free batch.
    pub unsafe fn free_offset(&mut self, offset: usize) {
        // SAFETY: The caller guarantees that `offset` refers to a valid allocation.
        let Some((allocation_indexes, worker_index)) = (unsafe {
            self.source
                .base()
                .allocation_indexes_and_assigned_worker(offset)
        }) else {
            return;
        };

        if let RemoteFreeBatchSource::Allocator(allocator) = &self.source {
            if allocator.worker_index() == worker_index {
                // SAFETY: The indexes came from the caller's valid offset and the
                // ownership check above confirms its slab is local.
                unsafe { allocator.free_local(allocation_indexes) };
                return;
            }
        }

        // SAFETY: The caller guarantees that `offset` refers to a valid, uniquely
        // freed allocation and `worker_index` was read from its slab metadata.
        unsafe { self.push_remote(worker_index, offset) };
    }

    /// Publish all remote frees currently held by this batch.
    ///
    /// The batch may be reused after flushing. Capacity allocated for destination
    /// workers is retained.
    pub fn flush(&mut self) {
        while let Some(chain) = self.chains.pop() {
            // SAFETY: Chains can only be created from offsets accepted by the unsafe
            // `free` methods and remain private until they are removed here.
            unsafe {
                self.source.base().publish_remote_free_chain(
                    chain.worker_index,
                    chain.head,
                    chain.tail,
                )
            };
        }
    }

    /// Add an allocation to its worker's private remote-free chain.
    ///
    /// # Safety
    /// - `offset` must refer to a valid, uniquely freed allocation.
    /// - `worker_index` must be the worker currently assigned to its slab.
    unsafe fn push_remote(&mut self, worker_index: u32, offset: usize) {
        if let Some(chain) = self
            .chains
            .iter_mut()
            .find(|chain| chain.worker_index == worker_index)
        {
            // SAFETY: Guaranteed by the caller; `chain.head` is another valid
            // offset in the same private chain.
            unsafe { self.source.base().set_remote_free_next(offset, chain.head) };
            chain.head = offset;
            return;
        }

        // Register the destination before modifying the allocation. If allocation
        // fails, dropping the batch can still publish every previously linked chain.
        self.chains.push(WorkerRemoteFreeChain {
            worker_index,
            head: offset,
            tail: offset,
        });
    }
}

impl Drop for RemoteFreeBatch<'_> {
    fn drop(&mut self) {
        self.flush();
    }
}

impl Allocator {
    /// Create a batch that groups remote frees by owning worker.
    ///
    /// Locally owned allocations are reclaimed immediately. Remote frees are
    /// published by [`RemoteFreeBatch::flush`] or when the batch is dropped.
    pub fn remote_free_batch(&self) -> RemoteFreeBatch<'_> {
        RemoteFreeBatch::new(RemoteFreeBatchSource::Allocator(self))
    }
}

impl FreeOnlyAllocator {
    /// Create a batch that groups remote frees by owning worker.
    ///
    /// Remote frees are published by [`RemoteFreeBatch::flush`] or when the
    /// batch is dropped.
    pub fn remote_free_batch(&self) -> RemoteFreeBatch<'_> {
        RemoteFreeBatch::new(RemoteFreeBatchSource::FreeOnly(self))
    }
}

use crate::sync::{AtomicU32, AtomicUsize, Ordering};
use crate::{cache_aligned::CacheAlignedU16, index::NULL_U16};

#[repr(C)]
pub struct SlabMeta {
    pub assigned_worker: AtomicU32,
    pub size_class_index: AtomicUsize,
    pub remote_free_stack_head: CacheAlignedU16,
}

impl SlabMeta {
    pub fn assign(&self, worker_index: u32, size_class_index: usize) {
        self.assigned_worker.store(worker_index, Ordering::Release);
        self.size_class_index
            .store(size_class_index, Ordering::Release);
        self.remote_free_stack_head
            .store(NULL_U16, Ordering::Release);
    }
}

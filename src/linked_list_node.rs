use crate::sync::AtomicU32;

/// A node in various linked lists used in the allocator.
/// Each node is associated with a slab index.
/// Since a node is only part of a single linked list at a time,
/// we could have re-used the same memory. However, doing this
/// structure is used for convenience and clarity - without too much
/// additional memory overhead.
#[repr(C)]
pub struct LinkedListNode {
    /// The index of the next node in the global free list.
    pub global_next: AtomicU32,
    /// The index of the previous node in a worker's local list.
    pub worker_local_prev: AtomicU32,
    /// The index of the next node in a worker's local list.
    pub worker_local_next: AtomicU32,
}

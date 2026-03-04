use crate::cache_aligned::CacheAlignedU64;
use crate::index::NULL_U32;
use crate::linked_list_node::LinkedListNode;
use crate::sync::Ordering;
use core::ptr::NonNull;

pub(crate) const fn pack_index(gen: u32, index: u32) -> u64 {
    ((gen as u64) << 32) | (index as u64)
}

const fn unpack_index(tagged: u64) -> (u32, u32) {
    ((tagged >> 32) as u32, tagged as u32)
}

/// A singly linked-list that tracks slabs not assigned to any worker.
/// This list is safe to use concurrently across processes.
///
/// The head uses a tagged pointer (generation counter + index) packed into
/// a u64 to prevent ABA races on the lock-free stack.
pub struct GlobalFreeList<'a> {
    head: &'a CacheAlignedU64,
    list: NonNull<LinkedListNode>,
}

impl<'a> GlobalFreeList<'a> {
    /// Creates a new `GlobalFreeList` with the given `head` and `list`.
    ///
    /// # Safety
    /// - The index portion of `head` must be a valid index into the `list` or NULL_U32.
    /// - `list` must be a valid pointer to an array of `FreeListElement` with sufficient capacity.
    pub unsafe fn new(
        head: &'a CacheAlignedU64,
        list: NonNull<LinkedListNode>,
    ) -> GlobalFreeList<'a> {
        GlobalFreeList { head, list }
    }

    /// Pushes `slab_index` onto the head of the global free list.
    ///
    /// # Safety
    /// - `slab_index` must be a valid index into the `list`.
    pub unsafe fn push(&self, slab_index: u32) {
        // SAFETY: The `slab_index` is assumed to be a valid index into the `list`.
        let next_head_ref = unsafe { self.get_unchecked(slab_index) };
        loop {
            let current_head = self.head.load(Ordering::Acquire);

            // Optimistically update the `global_next` of our owned slab.
            let (head_gen, head_index) = unpack_index(current_head);
            next_head_ref
                .global_next
                .store(head_index, Ordering::Release);

            // NB: Generation is a global counter that gets incremented on every
            // successful push/pop. This ensures if we pop & push the same index it will
            // not show up as the same `self.head`.
            if self
                .head
                .compare_exchange(
                    current_head,
                    pack_index(head_gen.wrapping_add(1), slab_index),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                return; // Successfully pushed the slab index onto the list
            }
        }
    }

    /// Pops a slab index from the head of the global free list.
    /// Returns `None` if the list is empty.
    pub fn pop(&self) -> Option<u32> {
        loop {
            let current_head = self.head.load(Ordering::Acquire);
            let (head_gen, head_index) = unpack_index(current_head);
            if head_index == NULL_U32 {
                return None; // The list is empty
            }

            let current_head_ref = unsafe { self.get_unchecked(head_index) };
            let next_slab_index = current_head_ref.global_next.load(Ordering::Acquire);

            // NB: Generation is a global counter that gets incremented on every
            // successful push/pop. This ensures if we pop & push the same index it will
            // not show up as the same `self.head`.
            if self
                .head
                .compare_exchange(
                    current_head,
                    pack_index(head_gen.wrapping_add(1), next_slab_index),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                current_head_ref
                    .global_next
                    .store(NULL_U32, Ordering::Release);
                return Some(head_index); // Successfully popped the slab index
            }
        }
    }

    /// Get reference to a specific slab indexes `FreeListElement`.
    ///
    /// # Safety
    /// - The `slab_index` must be a valid index into the `list`.
    pub unsafe fn get_unchecked(&self, slab_index: u32) -> &LinkedListNode {
        self.list.add(slab_index as usize).as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache_aligned::CacheAligned;
    use crate::sync::{AtomicU32, AtomicU64};

    struct SharedList {
        head: CacheAligned<AtomicU64>,
        buffer: Vec<LinkedListNode>,
    }

    impl SharedList {
        fn new(len: usize) -> Self {
            let head = CacheAligned(AtomicU64::new(pack_index(0, NULL_U32)));
            let buffer = (0..len)
                .map(|_| LinkedListNode {
                    global_next: AtomicU32::new(NULL_U32),
                    worker_local_prev: AtomicU32::new(NULL_U32),
                    worker_local_next: AtomicU32::new(NULL_U32),
                })
                .collect();

            Self { head, buffer }
        }

        fn list(&self) -> GlobalFreeList<'_> {
            unsafe {
                GlobalFreeList::new(
                    &self.head,
                    NonNull::new(self.buffer.as_ptr() as *mut LinkedListNode).unwrap(),
                )
            }
        }
    }

    // SAFETY:
    //
    // - The GlobalFreeList is designed for concurrent use across threads/processes.
    // - All mutations go through atomics.
    // - The backing Vec is never reallocated after construction.
    unsafe impl Send for SharedList {}
    unsafe impl Sync for SharedList {}

    #[test]
    fn test_global_free_list() {
        const LIST_CAPACITY: usize = 1024;

        let shared = SharedList::new(LIST_CAPACITY);
        let global_free_list = shared.list();

        // SAFETY: Pushing and popping within the capacity.
        unsafe {
            let range = 0..3;
            for index in range.clone() {
                global_free_list.push(index);
            }

            for index in range.clone().rev() {
                assert_eq!(global_free_list.pop(), Some(index));
            }
            assert_eq!(global_free_list.pop(), None);

            // check that the links have been cleared
            for index in range {
                assert_eq!(
                    global_free_list
                        .get_unchecked(index)
                        .global_next
                        .load(Ordering::Acquire),
                    NULL_U32
                );
            }
        }
    }

    #[cfg(feature = "shuttle")]
    mod shuttle_tests {
        use super::*;
        use shuttle::sync::Arc;

        const NUM_NODES: usize = 8;

        fn collect_free(shared: &SharedList) -> Vec<u32> {
            let list = shared.list();
            let mut result = Vec::new();
            let (_, mut current) = unpack_index(shared.head.load(Ordering::SeqCst));
            let mut visited = std::collections::HashSet::new();
            while current != NULL_U32 {
                assert!(visited.insert(current), "LinkedList cycle detected");
                assert!((current as usize) < NUM_NODES, "LinkedList out of bounds");

                // Push current node.
                result.push(current);

                // Progress to next node (may be terminal).
                current = unsafe { list.get_unchecked(current) }
                    .global_next
                    .load(Ordering::SeqCst);
            }

            result
        }

        #[test]
        fn test_global_free_list_aba() {
            let run = || {
                let shared = Arc::new(SharedList::new(NUM_NODES));
                let list = shared.list();

                // Push 3 nodes into our global free list.
                unsafe {
                    list.push(0);
                    list.push(1);
                    list.push(2);
                }

                // Thread A pops one element.
                let shared_a = shared.clone();
                let thread_a = shuttle::thread::spawn(move || {
                    let list = shared_a.list();

                    list.pop()
                });

                // Thread B pops an element and immediately pushes it back, creating a
                // window where an ABA race can occur.
                let shared_b = shared.clone();
                let thread_b = shuttle::thread::spawn(move || {
                    let list = shared_b.list();
                    if let Some(idx) = list.pop() {
                        unsafe { list.push(idx) };
                    }

                    list.pop()
                });

                // Wait for the results from both threads.
                let a_claimed = thread_a.join().unwrap();
                let b_claimed = thread_b.join().unwrap();

                // All popped indices + remaining indices should account for exactly the 3
                // nodes we pushed, with no duplicates.
                let all_indices: Vec<u32> = collect_free(&shared)
                    .into_iter()
                    .chain(a_claimed)
                    .chain(b_claimed)
                    .collect();
                let mut all_unique = all_indices.clone();
                all_unique.sort();
                all_unique.dedup();
                assert!(
                    all_indices.len() == 3 && all_unique.len() == 3,
                    "node lost or duplicated! popped=({:?}, {:?}), remaining={:?}",
                    a_claimed,
                    b_claimed,
                    collect_free(&shared),
                );
            };

            shuttle::check_random(run, 10_000);
        }
    }
}

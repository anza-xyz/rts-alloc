use crate::linked_list_node::LinkedListNode;
use crate::sync::Ordering;
use crate::{cache_aligned::CacheAlignedU32, index::NULL_U32};
use core::ptr::NonNull;

/// A singly linked-list that tracks slabs not assigned to any worker.
/// This list is safe to use concurrently across processes.
pub struct GlobalFreeList<'a> {
    head: &'a CacheAlignedU32,
    list: NonNull<LinkedListNode>,
}

impl<'a> GlobalFreeList<'a> {
    /// Creates a new `GlobalFreeList` with the given `head` and `list`.
    ///
    /// # Safety
    /// - `head` must be a valid index into the `list` or NULL_U32.
    /// - `list` must be a valid pointer to an array of `FreeListElement` with sufficient capacity.
    pub unsafe fn new(
        head: &'a CacheAlignedU32,
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
            next_head_ref
                .global_next
                .store(current_head, Ordering::Release);
            if self
                .head
                .compare_exchange(
                    current_head,
                    slab_index,
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
            if current_head == NULL_U32 {
                return None; // The list is empty
            }

            let current_head_ref = unsafe { self.get_unchecked(current_head) };
            let next_slab_index = current_head_ref.global_next.load(Ordering::Acquire);

            if self
                .head
                .compare_exchange(
                    current_head,
                    next_slab_index,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                current_head_ref
                    .global_next
                    .store(NULL_U32, Ordering::Release);
                return Some(current_head); // Successfully popped the slab index
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
    use core::sync::atomic::AtomicU32;

    #[test]
    fn test_global_free_list() {
        const LIST_CAPACITY: usize = 1024;

        let head = CacheAligned(AtomicU32::new(NULL_U32));
        let mut buffer = (0..LIST_CAPACITY)
            .map(|_| LinkedListNode {
                global_next: AtomicU32::new(NULL_U32),
                worker_local_prev: AtomicU32::new(NULL_U32),
                worker_local_next: AtomicU32::new(NULL_U32),
            })
            .collect::<Vec<_>>();

        let global_free_list =
            unsafe { GlobalFreeList::new(&head, NonNull::new(buffer.as_mut_ptr()).unwrap()) };

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
}

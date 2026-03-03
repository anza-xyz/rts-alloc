use crate::sync::{AtomicU32, Ordering};
use crate::{index::NULL_U32, linked_list_node::LinkedListNode};
use core::ptr::NonNull;

/// A doubly linked-list that tracks slabs assigned to a worker.
/// This list is not safe to use concurrently across processes,
/// and should only be accessed by the associated worker.
pub struct WorkerLocalList<'a> {
    head: &'a AtomicU32,
    list: NonNull<LinkedListNode>,
}

impl<'a> WorkerLocalList<'a> {
    /// Creates a new `WorkerLocalList` with the given `head` and `list`.
    ///
    /// # Safety
    /// - `head` must be a valid index into the `list` or NULL_U32.
    /// - `list` must be a valid pointer to an array of `FreeListElement` with sufficient capacity.
    pub unsafe fn new(head: &'a AtomicU32, list: NonNull<LinkedListNode>) -> Self {
        Self { head, list }
    }

    /// Returns the head of the worker local list.
    pub fn head(&self) -> Option<u32> {
        let head = self.head.load(Ordering::Acquire);
        if head != NULL_U32 {
            Some(head)
        } else {
            None
        }
    }

    /// Pushes `slab_index` onto the head of the worker local list.
    ///
    /// # Safety
    /// - `slab_index` must be a valid index into the `list`.
    pub unsafe fn push(&mut self, slab_index: u32) {
        let current_head = self.head.load(Ordering::Acquire);

        {
            // SAFETY: The `slab_index` is assumed to be a valid index into the `list`.
            let pushed_node = unsafe { self.get_unchecked(slab_index) };
            pushed_node
                .worker_local_prev
                .store(NULL_U32, Ordering::Release);
            pushed_node
                .worker_local_next
                .store(current_head, Ordering::Release);
        }

        if current_head != NULL_U32 {
            // SAFETY: The `current_head` is assumed to be a valid index into the `list`.
            let current_head_ref = unsafe { self.get_unchecked(current_head) };
            current_head_ref
                .worker_local_prev
                .store(slab_index, Ordering::Release);
        }

        self.head.store(slab_index, Ordering::Release);
    }

    /// Remove a slab from the list.
    ///
    /// # Safety
    /// - `slab_index` must be a valid index into the `list`.
    /// - `slab_index` must be present in the list.
    pub unsafe fn remove(&mut self, slab_index: u32) {
        let (prev, next) = {
            // SAFETY: The `slab_index` is assumed to be a valid index into the `list`.
            let current_node = unsafe { self.get_unchecked(slab_index) };
            (
                current_node
                    .worker_local_prev
                    .swap(NULL_U32, Ordering::AcqRel),
                current_node
                    .worker_local_next
                    .swap(NULL_U32, Ordering::AcqRel),
            )
        };

        if prev != NULL_U32 {
            // SAFETY: The `prev` is assumed to be a valid index into the `list`.
            let prev_node = unsafe { self.get_unchecked(prev) };
            prev_node.worker_local_next.store(next, Ordering::Release);
        } else {
            // If there is no previous node, we are at the head.
            self.head.store(next, Ordering::Release);
        };

        if next != NULL_U32 {
            // SAFETY: The `next` is assumed to be a valid index into the `list`.
            let next_node = unsafe { self.get_unchecked(next) };
            next_node.worker_local_prev.store(prev, Ordering::Release);
        }
    }

    /// Iterates over the worker local list.
    pub fn iterate(&self) -> impl Iterator<Item = u32> + '_ {
        let mut current_head = self.head.load(Ordering::Acquire);
        core::iter::from_fn(move || {
            if current_head == NULL_U32 {
                return None; // End of the list
            }
            let ret = Some(current_head);
            // SAFETY: The `current_head` is assumed to be a valid index into the `list`.
            let current_node = unsafe { self.get_unchecked(current_head) };
            let next_index = current_node.worker_local_next.load(Ordering::Acquire);
            current_head = next_index;
            ret
        })
    }

    /// Get reference to a specific slab indexes `FreeListElement`.
    ///
    /// # Safety
    /// - The `slab_index` must be a valid index into the `list`.
    unsafe fn get_unchecked(&self, slab_index: u32) -> &LinkedListNode {
        self.list.add(slab_index as usize).as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::AtomicU32;

    #[test]
    fn test_worker_local_list() {
        const LIST_CAPACITY: u32 = 1024;
        let head = AtomicU32::new(NULL_U32);
        let mut buffer = (0..LIST_CAPACITY)
            .map(|_| LinkedListNode {
                global_next: AtomicU32::new(NULL_U32),
                worker_local_prev: AtomicU32::new(NULL_U32),
                worker_local_next: AtomicU32::new(NULL_U32),
            })
            .collect::<Vec<_>>();
        let mut worker_local_list =
            unsafe { WorkerLocalList::new(&head, NonNull::new(buffer.as_mut_ptr()).unwrap()) };

        // Test removal orders:
        // - [head, head, head]
        // - [tail, tail, tail]
        // - [middle, head, head]
        for removal_order in [[2, 1, 0], [0, 1, 2], [1, 2, 0], [1, 0, 2]] {
            unsafe {
                worker_local_list.push(0);
                assert_eq!(worker_local_list.iterate().collect::<Vec<_>>(), vec![0]);

                worker_local_list.push(1);
                assert_eq!(worker_local_list.iterate().collect::<Vec<_>>(), vec![1, 0]);

                worker_local_list.push(2);
                assert_eq!(
                    worker_local_list.iterate().collect::<Vec<_>>(),
                    vec![2, 1, 0]
                );

                let mut expected_order = vec![2, 1, 0];
                for index in removal_order {
                    worker_local_list.remove(index);
                    expected_order.retain(|&x| x != index);
                    assert_eq!(
                        worker_local_list.iterate().collect::<Vec<_>>(),
                        expected_order,
                        "mismatch for order: {removal_order:?}"
                    );
                }
            }
        }
    }
}

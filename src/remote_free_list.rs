use crate::cache_aligned::CacheAlignedU16;
use crate::index::NULL_U16;
use crate::sync::AtomicU16;
use crate::sync::Ordering;
use core::ptr::NonNull;

/// A shared remote free list for a slab.
/// Intrusive linked list where each element is used as either a
/// element storing data or as a link to the next element.
/// Lock-free and thread/process safe.
pub struct RemoteFreeList<'a> {
    /// Size of each element in the slab.
    slab_item_size: u32,
    /// The head of the remote free list.
    head: &'a CacheAlignedU16,
    /// Pointer to the beginning of the slab.
    slab: NonNull<u8>,
}

impl<'a> RemoteFreeList<'a> {
    /// Creates a new remote free list.
    ///
    /// # Safety
    /// - `slab_item_size` must be a valid size AND currently assigned to the slab.
    /// - `head` must be a valid reference to a `CacheAlignedU32` that is the head
    ///   of the remote free list.
    /// - `slab` must be a valid pointer to the beginning of the slab.
    pub unsafe fn new(slab_item_size: u32, head: &'a CacheAlignedU16, slab: NonNull<u8>) -> Self {
        Self {
            slab_item_size,
            head,
            slab,
        }
    }

    /// Pushes an item to the remote free list.
    ///
    /// # Safety
    /// - The `index` must be a valid index within the slab.
    /// - The `index` must not already be in the remote free list.
    pub unsafe fn push(&self, index: u16) {
        // SAFETY: The index is guaranteed to be valid by the caller.
        let item: &AtomicU16 = unsafe {
            self.slab
                .byte_add(index as usize * self.slab_item_size as usize)
                .cast()
                .as_ref()
        };

        loop {
            // Read the current head of the remote free list.
            let current_head = self.head.load(Ordering::Acquire);
            // Set the next pointer of the item to the current head.
            item.store(current_head, Ordering::Release);
            // Attempt to set the head to the item.
            if self
                .head
                .compare_exchange(current_head, index, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                // Successfully pushed the item to the remote free list.
                break;
            }
        }
    }

    /// Drain the remote free list and return an iterator over the items.
    pub fn drain(&self) -> impl Iterator<Item = u16> + '_ {
        // Swap the head to NULL_U16 to mark the list as drained.
        let current = self.head.swap(NULL_U16, Ordering::AcqRel);
        self.iterate_from(current)
    }

    /// Iterate over the remote free list starting from a given index.
    fn iterate_from(&self, mut current: u16) -> impl Iterator<Item = u16> + '_ {
        core::iter::from_fn(move || {
            if current == NULL_U16 {
                return None; // End of the list
            }
            let ret = Some(current);

            // SAFETY: The `current` is assumed to be a valid index into the slab.
            let item: &AtomicU16 = unsafe {
                self.slab
                    .byte_add(current as usize * self.slab_item_size as usize)
                    .cast()
                    .as_ref()
            };
            // Read the next item in the list.
            current = item.load(Ordering::Acquire);
            ret
        })
    }
}

#[cfg(test)]
impl<'a> RemoteFreeList<'a> {
    /// Iterate over the remote free list.
    pub fn iterate(&self) -> impl Iterator<Item = u16> + '_ {
        // Start iterating from the head of the remote free list.
        self.iterate_from(self.head.load(Ordering::Acquire))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cache_aligned::CacheAligned, size_classes::MIN_SIZE};

    const SLAB_ITEM_SIZE: u32 = MIN_SIZE;
    #[repr(C, align(256))]
    struct DummyTypeForAlignment([u8; SLAB_ITEM_SIZE as usize]);

    #[test]
    fn test_remote_free_list() {
        const TEST_CAPACITY: usize = 128;
        let head = CacheAligned(AtomicU16::new(NULL_U16));
        let mut slab_backing = (0..TEST_CAPACITY)
            .map(|_| DummyTypeForAlignment([0; SLAB_ITEM_SIZE as usize]))
            .collect::<Vec<_>>();
        let slab = NonNull::new(slab_backing.as_mut_ptr())
            .unwrap()
            .cast::<u8>();

        let remote_free_list = unsafe { RemoteFreeList::new(SLAB_ITEM_SIZE, &head, slab) };

        // SAFETY: Pushing and iterating within bounds.
        unsafe {
            remote_free_list.push(7);
            remote_free_list.push(42);
            remote_free_list.push(63);
            assert_eq!(remote_free_list.iterate().collect::<Vec<_>>(), [63, 42, 7]);

            let drain_iter = remote_free_list.drain();
            assert_eq!(remote_free_list.iterate().collect::<Vec<_>>(), []);
            remote_free_list.push(99);
            remote_free_list.push(79);
            assert_eq!(drain_iter.collect::<Vec<_>>(), [63, 42, 7]);
            assert_eq!(remote_free_list.iterate().collect::<Vec<_>>(), [79, 99]);
            assert_eq!(remote_free_list.iterate().collect::<Vec<_>>(), [79, 99]);
        }
    }
}

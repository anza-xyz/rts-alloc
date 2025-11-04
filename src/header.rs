use crate::{
    cache_aligned::{CacheAligned, CacheAlignedU32},
    size_classes::NUM_SIZE_CLASSES,
};
use core::sync::atomic::AtomicU32;

pub const MAGIC: u64 = 0x727473616c6f63; // "rtsaloc"
pub const VERSION: u32 = 1;

pub type WorkerLocalListHeads = CacheAligned<[WorkerLocalListPartialFullHeads; NUM_SIZE_CLASSES]>;
pub struct WorkerLocalListPartialFullHeads {
    pub partial: AtomicU32,
    pub full: AtomicU32,
}

#[repr(C)]
pub struct Header {
    pub magic: u64,
    pub version: AtomicU32,
    /// Maximum number of workers that can use this allocator.
    pub num_workers: u32,
    /// Number of slabs in the allocator.
    pub num_slabs: u32,
    /// The size in bytes of each slab.
    pub slab_size: u32,

    /// The offset in bytes to the free list elements.
    pub free_list_elements_offset: u32,
    /// The offset in bytes to the slab shared metadata.
    pub slab_shared_meta_offset: u32,
    /// The offset in bytes to the slab free stacks.
    pub slab_free_stacks_offset: u32,
    /// The offset in bytes to the slabs.
    pub slabs_offset: u32,

    /// The head of the global free list.
    pub global_free_list_head: CacheAlignedU32,
    /// The heads of the per-worker local free lists.
    pub worker_local_list_heads: [WorkerLocalListHeads; 0],
}

// Layout of the allocator.
// Padding used to ensure proper alignment between components.
//
// [header]
// [worker_local_list_heads; num_workers]
// [free_list_elements; num_slabs]
// [slab_shared_meta]
// [slab_free_stacks]
// [slabs]
//
// header:
//     - contains metadata about the allocator as a whole.
//
// worker_local_list_heads:
//     - contains heads of the worker local lists.
//     - each worker has its own set of heads.
//     - each worker has a partial and full head for each size class.
//     - the heads store indexes into the free list elements.
//     - NULL_U32 is used to indicate a null pointer in the linked list.
//
// free_list_elements:
//     - list of free list elements, one per slab.
//     - the free list elements, in conjunction with the `global_free_list_head`
//       and `worker_local_list_heads`, form linked lists of slabs, in various states.
//     - it is NOT valid for a slab to be in multiple lists at the same time.
//     - NULL_U32 is used to indicate a null pointer in the linked list.
//
// slab_shared_meta:
//     - shared metadata for each slab.
//
// slab_free_stacks:
//     - each slab has its own free stack.
//     - the free stack is used to track free indices within the slab.
//
// slabs:
//     - the slabs themselves, containing chunks of `slab_size` bytes each.
//     - guaranteed to be offset a multiple of `slab_size` bytes from the
//       start of the file.
pub mod layout {
    use crate::{
        align::round_to_next_alignment_of,
        free_stack::FreeStack,
        header::{Header, WorkerLocalListHeads},
        linked_list_node::LinkedListNode,
        size_classes::MIN_SIZE,
        slab_meta::SlabMeta,
    };
    #[derive(Debug)]
    pub struct AllocatorLayout {
        /// The number of slabs in the allocator.
        pub num_slabs: u32,
        /// The offset in bytes to the free list elements.
        pub free_list_elements_offset: u32,
        /// The offset in bytes to the slab shared metadata.
        pub slab_shared_meta_offset: u32,
        /// The offset in bytes to the slab free stacks.
        pub slab_free_stacks_offset: u32,
        /// The offset in bytes to the slabs.
        pub slabs_offset: u32,
    }

    pub fn offsets(file_size: usize, slab_size: u32, num_workers: u32) -> AllocatorLayout {
        let mut offset = header_size();
        offset += worker_local_list_heads_size(num_workers);
        offset = pad_for_free_list_elements(offset);
        let free_list_elements_offset = offset as u32;

        // Get an upperbound on number of slabs:
        // At least 1 slab slot is taken for the header and meta information.
        let num_slabs_upperbound = ((file_size / slab_size as usize) - 1) as u32;

        offset += free_list_elements_size(num_slabs_upperbound);
        offset = pad_for_slab_meta(offset);
        let slab_shared_meta_offset = offset as u32;

        offset += slab_meta_size(num_slabs_upperbound);
        offset = pad_for_slab_free_stacks(offset);
        let slab_free_stacks_offset = offset as u32;

        offset += free_stacks_size(num_slabs_upperbound, slab_size);
        offset = pad_for_slabs(offset, slab_size);
        let slabs_offset = offset as u32;

        let num_slabs = (file_size as u32 - slabs_offset) / slab_size;

        AllocatorLayout {
            num_slabs,
            free_list_elements_offset,
            slab_shared_meta_offset,
            slab_free_stacks_offset,
            slabs_offset,
        }
    }

    /// The size of the header in bytes.
    pub const fn header_size() -> usize {
        core::mem::size_of::<Header>()
    }

    /// The size of the worker local list heads in bytes.
    pub const fn worker_local_list_heads_size(num_workers: u32) -> usize {
        core::mem::size_of::<WorkerLocalListHeads>() * num_workers as usize
    }

    /// Update offset to padd for free list elements.
    pub const fn pad_for_free_list_elements(offset: usize) -> usize {
        const FREE_LIST_ELEMENT_ALIGNMENT: usize = core::mem::align_of::<LinkedListNode>();
        round_to_next_alignment_of::<FREE_LIST_ELEMENT_ALIGNMENT>(offset)
    }

    /// The size of the free list elements in bytes.
    pub const fn free_list_elements_size(num_slabs: u32) -> usize {
        core::mem::size_of::<LinkedListNode>() * num_slabs as usize
    }

    /// Update offset to pad for slab shared metadata.
    pub const fn pad_for_slab_meta(offset: usize) -> usize {
        const SLAB_META_ALIGNMENT: usize = core::mem::align_of::<SlabMeta>();
        round_to_next_alignment_of::<SLAB_META_ALIGNMENT>(offset)
    }

    /// The size of the slab meta in bytes with trailing padding.
    pub const fn slab_meta_size(num_slabs: u32) -> usize {
        core::mem::size_of::<SlabMeta>() * num_slabs as usize
    }

    /// Update offset to pad for slab free stacks.
    pub const fn pad_for_slab_free_stacks(offset: usize) -> usize {
        const FREE_STACK_ALIGNMENT: usize = core::mem::align_of::<FreeStack>();
        round_to_next_alignment_of::<FREE_STACK_ALIGNMENT>(offset)
    }

    /// The size of an individual free stack in bytes.
    pub const fn single_free_stack_size(slab_size: u32) -> usize {
        let max_capacity = slab_size / MIN_SIZE;
        FreeStack::byte_size(max_capacity as u16)
    }

    /// The size of the free stacks in bytes WITHOUT trailing padding.
    pub const fn free_stacks_size(num_slabs: u32, slab_size: u32) -> usize {
        single_free_stack_size(slab_size) * num_slabs as usize
    }

    /// Update offset to the next multiple of `slab_size`.
    pub const fn pad_for_slabs(offset: usize, slab_size: u32) -> usize {
        debug_assert!(slab_size.is_power_of_two());
        let slab_size = slab_size as usize;
        (offset + slab_size - 1) & !(slab_size - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_layout() {
        let num_workers = 4;
        let num_slabs = 8;
        let slab_size = 4096;

        let mut offset = layout::header_size();
        assert_eq!(offset, core::mem::size_of::<Header>());

        offset += layout::worker_local_list_heads_size(num_workers);
        assert_eq!(offset, 384);

        offset = layout::pad_for_free_list_elements(offset);
        assert_eq!(offset, 384);

        offset += layout::free_list_elements_size(num_slabs);
        assert_eq!(offset, 480);

        offset = layout::pad_for_slab_meta(offset);
        assert_eq!(offset, 512);

        offset += layout::slab_meta_size(num_slabs);
        assert_eq!(offset, 1536);

        offset = layout::pad_for_slab_free_stacks(offset);
        assert_eq!(offset, 1536);

        offset += layout::free_stacks_size(num_slabs, slab_size);
        assert_eq!(offset, 1824);

        offset = layout::pad_for_slabs(offset, slab_size);
        assert_eq!(offset, 4096);
    }
}

use crate::{
    error::Error,
    header::{
        layout::{self, AllocatorLayout},
        Header, WorkerLocalListHeads,
    },
    index::NULL_U32,
    linked_list_node::LinkedListNode,
    size_classes::{MAX_SIZE, MIN_SIZE},
};
use std::{
    fs::File,
    mem::offset_of,
    os::{fd::AsRawFd, raw::c_void},
    ptr::NonNull,
    sync::atomic::Ordering,
};

/// Create and initialize the allocator's backing file.
/// Returns pointer to header.
pub fn create(
    file: &File,
    file_size: usize,
    num_workers: u32,
    slab_size: u32,
) -> Result<NonNull<Header>, Error> {
    if num_workers == 0 {
        return Err(Error::InvalidNumWorkers);
    }
    verify_slab_size(slab_size)?;
    verify_allocator_size(file_size, slab_size, num_workers)?;

    // Given parameters, calculate layout.
    let layout = layout::offsets(file_size, slab_size, num_workers);
    if layout.num_slabs == 0 {
        return Err(Error::InvalidFileSize);
    }

    // Resize the file if it's currently 0 sized, else error.
    if file.metadata()?.len() != 0 {
        return Err(Error::AlreadyInitialized);
    }
    file.set_len(file_size as u64)?;

    // Map the file into memory.
    let mmap = open_mmap(file, file_size)?;

    // Initialize the header.
    // SAFETY: The header is valid for any byte pattern.
    //         There is sufficient space for a `Header` and trailing data.
    let header = NonNull::new(mmap.cast::<Header>()).expect("mmap already checked for null");
    unsafe {
        initialize::allocator(header, slab_size, num_workers, layout);
    }

    Ok(header)
}

/// Join an existing allocator, returning a pointer to the header and size.
pub fn join(file: &File) -> Result<(NonNull<Header>, usize), Error> {
    let file_size = file.metadata()?.len() as usize;
    let mmap = open_mmap(file, file_size)?;
    let header = NonNull::new(mmap.cast::<Header>()).expect("mmap already checked for null");

    // Verify header
    {
        // SAFETY: The header is assumed to be valid and initialized.
        let header = unsafe { header.as_ref() };
        if header.version.load(Ordering::SeqCst) != crate::header::VERSION
            || header.magic != crate::header::MAGIC
            || header.num_workers == 0
        {
            return Err(Error::InvalidHeader);
        }
        verify_slab_size(header.slab_size)?;
        verify_allocator_size(file_size, header.slab_size, header.num_workers)?;
        let expected_layout = layout::offsets(file_size, header.slab_size, header.num_workers);

        if header.num_slabs != expected_layout.num_slabs
            || header.free_list_elements_offset != expected_layout.free_list_elements_offset
            || header.slab_shared_meta_offset != expected_layout.slab_shared_meta_offset
            || header.slab_free_stacks_offset != expected_layout.slab_free_stacks_offset
            || header.slabs_offset != expected_layout.slabs_offset
        {
            return Err(Error::InvalidHeader);
        }
    }

    Ok((header, file_size))
}

fn verify_slab_size(slab_size: u32) -> Result<(), Error> {
    if !slab_size.is_power_of_two() {
        return Err(Error::InvalidSlabSize);
    }

    // If the slab size is not large enough to hold at least 4 allocations,
    // then there's really no point to having a slab allocator.
    if slab_size < 4 * MAX_SIZE {
        return Err(Error::InvalidSlabSize);
    }

    if slab_size / MIN_SIZE > u16::MAX as u32 {
        return Err(Error::InvalidSlabSize);
    }

    Ok(())
}

fn verify_allocator_size(file_size: usize, slab_size: u32, num_workers: u32) -> Result<(), Error> {
    let min_file_size = layout::min_file_size(num_workers, slab_size);
    if file_size < min_file_size {
        return Err(Error::InvalidFileSize);
    }

    Ok(())
}

fn open_mmap(file: &File, size: usize) -> Result<*mut c_void, Error> {
    let mmap = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };

    if mmap == libc::MAP_FAILED {
        return Err(Error::MmapError(std::io::Error::last_os_error()));
    }

    Ok(mmap)
}

pub mod initialize {
    use super::*;
    use crate::{
        header::WorkerLocalListPartialFullHeads, size_classes::NUM_SIZE_CLASSES,
        slab_meta::SlabMeta,
    };

    /// Initialize the allocator's backing memory.
    ///
    /// # Safety
    /// - `header` must be a valid pointer with sufficient space for a `Header`.
    /// - `slab_size` must be a valid power of two, used to calculate the `layout`.
    pub unsafe fn allocator(
        header: NonNull<Header>,
        slab_size: u32,
        num_workers: u32,
        layout: AllocatorLayout,
    ) {
        // SAFETY: The header is valid for any byte pattern, and we are initializing it.
        //         There is sufficient space for a `Header` and trailing data.
        unsafe {
            init_header(header, slab_size, num_workers, layout);
        }

        // SAFETY: The header is assumed to be valid and initialized.
        unsafe {
            worker_local_lists(header);
            free_list_elements(header);
            slab_shared_meta(header);
        }
    }

    /// # Safety
    /// - `header` must be a valid pointer with sufficient space for a `Header`.
    /// - Other parameters must be verified or calculated correctly.
    unsafe fn init_header(
        mut header: NonNull<Header>,
        slab_size: u32,
        num_workers: u32,
        layout: AllocatorLayout,
    ) {
        // SAFETY: The header is valid for any byte pattern.
        let header = unsafe { header.as_mut() };
        header.num_workers = num_workers;
        header.num_slabs = layout.num_slabs;
        header.slab_size = slab_size;
        header.free_list_elements_offset = layout.free_list_elements_offset;
        header.slab_shared_meta_offset = layout.slab_shared_meta_offset;
        header.slab_free_stacks_offset = layout.slab_free_stacks_offset;
        header.slabs_offset = layout.slabs_offset;
        header
            .global_free_list_head
            .store(NULL_U32, Ordering::Release);
        header.magic = crate::header::MAGIC;
        header
            .version
            .store(crate::header::VERSION, Ordering::SeqCst);
    }

    /// # Safety
    /// - `header` must be a valid pointer to an initialized `Header`
    ///   with sufficient trailing space for an `Allocator`.
    fn worker_local_lists(header: NonNull<Header>) {
        let num_workers = {
            // SAFETY: The header is assumed to be valid and initialized.
            let header = unsafe { header.as_ref() };
            header.num_workers
        };

        // SAFETY: The header is assumed to be valid and initialized.
        let all_workers_heads = unsafe {
            header
                .byte_add(offset_of!(Header, worker_local_list_heads))
                .cast::<WorkerLocalListHeads>()
        };
        for i in 0..num_workers {
            let worker_head = unsafe {
                all_workers_heads
                    .add(i as usize)
                    .cast::<[WorkerLocalListPartialFullHeads; NUM_SIZE_CLASSES]>()
                    .as_mut()
            };

            for worker_partial_full in worker_head.iter_mut() {
                worker_partial_full
                    .partial
                    .store(NULL_U32, Ordering::Release);
                worker_partial_full.full.store(NULL_U32, Ordering::Release);
            }
        }
    }

    /// # Safety
    /// - `header` must be a valid pointer to an initialized `Header`
    ///   with sufficient trailing space for a an `Allocator`.
    unsafe fn free_list_elements(header: NonNull<Header>) {
        // SAFETY: The header is assumed to be valid and initialized.
        let (num_slabs, free_list_elements_offset) = {
            let header = unsafe { header.as_ref() };
            (header.num_slabs, header.free_list_elements_offset)
        };

        // SAFETY: The header has enough trailing space for free list elements.
        let free_list_element_ptr =
            unsafe { header.byte_add(free_list_elements_offset as usize) }.cast::<LinkedListNode>();

        for slab_index in 0..num_slabs {
            let global_next = if slab_index == num_slabs - 1 {
                NULL_U32
            } else {
                slab_index + 1
            };

            // SAFETY: The mmap must be large enough to hold all free list elements.
            let free_list_element =
                unsafe { free_list_element_ptr.add(slab_index as usize).as_mut() };
            free_list_element
                .global_next
                .store(global_next, Ordering::Release);
            free_list_element
                .worker_local_prev
                .store(NULL_U32, Ordering::Release);
            free_list_element
                .worker_local_next
                .store(NULL_U32, Ordering::Release);
        }

        // Now that the list has been initialized, set the global free list head.
        // SAFETY: The header is assumed to be valid and initialized.
        unsafe { header.as_ref() }
            .global_free_list_head
            .store(0, Ordering::Release);
    }

    fn slab_shared_meta(header: NonNull<Header>) {
        let (num_slabs, slab_shared_meta_offset) = {
            // SAFETY: The header is assumed to be valid and initialized.
            let header = unsafe { header.as_ref() };
            (header.num_slabs, header.slab_shared_meta_offset)
        };

        for slab_index in 0..num_slabs {
            // SAFETY: The header has enough trailing space for slab meta.
            let slab_meta = unsafe {
                header
                    .byte_add(slab_shared_meta_offset as usize)
                    .cast::<SlabMeta>()
                    .add(slab_index as usize)
                    .as_mut()
            };
            slab_meta.assign(NULL_U32, 0);
        }
    }
}

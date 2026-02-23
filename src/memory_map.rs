use crate::error::Error;
use core::ffi::c_void;
use std::fs::File;

#[cfg(unix)]
pub(crate) fn map_file(file: &File, size: usize) -> Result<*mut c_void, Error> {
    use std::os::fd::AsRawFd;

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

#[cfg(windows)]
pub(crate) fn map_file(file: &File, size: usize) -> Result<*mut c_void, Error> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, MapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
    };

    let size_u64 = u64::try_from(size).map_err(|_| Error::InvalidFileSize)?;
    let size_high = (size_u64 >> 32) as u32;
    let size_low = size_u64 as u32;

    let mapping = unsafe {
        CreateFileMappingW(
            file.as_raw_handle() as HANDLE,
            core::ptr::null(),
            PAGE_READWRITE,
            size_high,
            size_low,
            core::ptr::null(),
        )
    };

    if mapping.is_null() {
        return Err(Error::MmapError(std::io::Error::last_os_error()));
    }

    let mmap = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, size) };

    if mmap.Value.is_null() {
        return Err(Error::MmapError(std::io::Error::last_os_error()));
    }

    unsafe {
        CloseHandle(mapping);
    }

    Ok(mmap.Value.cast())
}

#[cfg(unix)]
pub(crate) fn unmap_file(ptr: *mut c_void, size: usize) -> Result<(), Error> {
    let rc = unsafe { libc::munmap(ptr, size) };
    if rc != 0 {
        return Err(Error::MmapError(std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn unmap_file(ptr: *mut c_void, _size: usize) -> Result<(), Error> {
    use windows_sys::Win32::System::Memory::{UnmapViewOfFile, MEMORY_MAPPED_VIEW_ADDRESS};

    let rc = unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: ptr.cast() }) };
    if rc == 0 {
        return Err(Error::MmapError(std::io::Error::last_os_error()));
    }
    Ok(())
}

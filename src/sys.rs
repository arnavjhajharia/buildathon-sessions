//! Read-only memory maps through libc, so the binary has no dependencies.
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::Path;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
}

const PROT_READ: i32 = 1;
const MAP_PRIVATE: i32 = 2;

/// Maps a whole file read-only and leaks the mapping: it lives until exit.
pub fn map_file(path: &Path) -> std::io::Result<&'static [u8]> {
    let file = File::open(path)?;
    let len = file.metadata()?.len() as usize;
    if len == 0 {
        return Ok(&[]);
    }
    let ptr = unsafe { mmap(std::ptr::null_mut(), len, PROT_READ, MAP_PRIVATE, file.as_raw_fd(), 0) };
    if ptr as isize == -1 {
        // Fall back to reading the file.
        let bytes = std::fs::read(path)?;
        return Ok(Box::leak(bytes.into_boxed_slice()));
    }
    unsafe {
        // MADV_WILLNEED = 3 on Linux and macOS.
        madvise(ptr, len, 3);
        Ok(std::slice::from_raw_parts(ptr, len))
    }
}

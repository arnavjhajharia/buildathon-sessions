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

// ------------------------------------------------------------ allocator
//
// Large blocks go straight to mmap and grow with mremap on Linux (pages move,
// bytes are not copied); small ones go to the system allocator. musl's
// allocator is slow under threads, and our big vectors grow a lot.

use std::alloc::{GlobalAlloc, Layout, System};

extern "C" {
    fn munmap(addr: *mut u8, len: usize) -> i32;
    #[cfg(target_os = "linux")]
    fn mremap(old: *mut u8, old_len: usize, new_len: usize, flags: i32, ...) -> *mut u8;
}

const BIG: usize = 1 << 18;
const PROT_RW: i32 = 1 | 2;
#[cfg(target_os = "linux")]
const MAP_ANON: i32 = 0x20;
#[cfg(not(target_os = "linux"))]
const MAP_ANON: i32 = 0x1000;

pub struct Alloc;

#[inline]
fn round(n: usize) -> usize {
    (n + 4095) & !4095
}

unsafe fn big_alloc(size: usize) -> *mut u8 {
    let p = mmap(std::ptr::null_mut(), round(size), PROT_RW, MAP_PRIVATE | MAP_ANON, -1, 0);
    if p as isize == -1 {
        return std::ptr::null_mut();
    }
    #[cfg(target_os = "linux")]
    madvise(p, round(size), 14); // MADV_HUGEPAGE
    p
}

unsafe impl GlobalAlloc for Alloc {
    #[inline]
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if l.size() >= BIG && l.align() <= 4096 {
            big_alloc(l.size())
        } else {
            System.alloc(l)
        }
    }
    #[inline]
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        if l.size() >= BIG && l.align() <= 4096 {
            big_alloc(l.size()) // anonymous pages are zero
        } else {
            System.alloc_zeroed(l)
        }
    }
    #[inline]
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        if l.size() >= BIG && l.align() <= 4096 {
            munmap(p, round(l.size()));
        } else {
            System.dealloc(p, l)
        }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let old_big = l.size() >= BIG && l.align() <= 4096;
        let new_big = new >= BIG && l.align() <= 4096;
        if !old_big && !new_big {
            return System.realloc(p, l, new);
        }
        #[cfg(target_os = "linux")]
        if old_big && new_big {
            if round(new) == round(l.size()) {
                return p;
            }
            let q = mremap(p, round(l.size()), round(new), 1 /* MREMAP_MAYMOVE */);
            return if q as isize == -1 { std::ptr::null_mut() } else { q };
        }
        let q = self.alloc(Layout::from_size_align_unchecked(new, l.align()));
        if !q.is_null() {
            std::ptr::copy_nonoverlapping(p, q, l.size().min(new));
            self.dealloc(p, l);
        }
        q
    }
}

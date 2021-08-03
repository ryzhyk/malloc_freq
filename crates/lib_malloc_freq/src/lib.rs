//! Companion crate to [`malloc_freq`].  This crate compiles into a dynamic library that can be
//! loaded via `LD_PRELOAD` to intercept `malloc` calls issued by the program and redirect them
//! to the `malloc_freq` profiler.

use libc::c_void;
use malloc_freq::ProfAllocator;

/// When this library is loaded with `LD_PRELOAD`, this `malloc` implementation
/// catches `malloc` calls performed by the program and records them in the `malloc_freq`
/// profile before invoking the original `libc` malloc.
///
/// # Safety
///
/// This method internally uses [`libc::malloc`], which is `unsafe extern "C"`.
#[no_mangle]
pub unsafe extern "C" fn malloc(size: libc::size_t) -> *mut c_void {
    ProfAllocator::malloc(size)
}

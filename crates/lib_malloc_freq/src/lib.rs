use libc::c_void;
use malloc_freq::ProfAllocator;

#[no_mangle]
pub unsafe extern "C" fn malloc(size: libc::size_t) -> *mut c_void {
    ProfAllocator::malloc(size)
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}

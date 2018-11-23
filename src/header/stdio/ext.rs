use header::stdio::{FILE, F_NORD, F_NOWR};
use platform::types::*;

#[no_mangle]
pub extern "C" fn __freadable(stream: *mut FILE) -> c_int {
    let mut stream = unsafe { &mut *stream }.lock();

    (stream.flags & F_NORD == 0) as c_int
}

#[no_mangle]
pub extern "C" fn __fwritable(stream: *mut FILE) -> c_int {
    let mut stream = unsafe { &mut *stream }.lock();

    (stream.flags & F_NOWR == 0) as c_int
}

#[no_mangle]
pub extern "C" fn __fpending(stream: *mut FILE) -> size_t {
    let mut stream = unsafe { &mut *stream }.lock();

    stream.writer.inner.buf.len() as size_t
}
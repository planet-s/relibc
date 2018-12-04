//! stdio implementation for Redox, following http://pubs.opengroup.org/onlinepubs/7908799/xsh/stdio.h.html

use alloc::borrow::{Borrow, BorrowMut};
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::fmt::Write as WriteFmt;
use core::ops::{Deref, DerefMut};
use core::{fmt, mem, ptr, slice, str};
use core::ffi::VaList as va_list;

use c_str::CStr;
use fs::File;
use header::errno::{self, STR_ERROR};
use header::string::strlen;
use header::{fcntl, stdlib, unistd};
use io::{self, BufRead, LineWriter, Read, Write};
use mutex::Mutex;
use platform;
use platform::types::*;
use platform::{errno, WriteByte};
use platform::{Pal, Sys};

pub use self::constants::*;
mod constants;

pub use self::default::*;
mod default;

pub use self::getdelim::*;
mod getdelim;

mod ext;
mod helpers;
mod printf;
mod scanf;

enum Buffer<'a> {
    Borrowed(&'a mut [u8]),
    Owned(Vec<u8>),
}
impl<'a> Deref for Buffer<'a> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        match self {
            Buffer::Borrowed(inner) => inner,
            Buffer::Owned(inner) => inner.borrow(),
        }
    }
}
impl<'a> DerefMut for Buffer<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Buffer::Borrowed(inner) => inner,
            Buffer::Owned(inner) => inner.borrow_mut(),
        }
    }
}

/// This struct gets exposed to the C API.
pub struct FILE {
    // Can't use spin crate because *_unlocked functions are things in C :(
    lock: Mutex<()>,

    file: File,
    // pub for stdio_ext
    pub(crate) flags: c_int,
    read_buf: Buffer<'static>,
    read_pos: usize,
    read_size: usize,
    unget: Option<u8>,
    // pub for stdio_ext
    pub(crate) writer: LineWriter<File>,

    // Optional pid for use with popen/pclose
    pid: Option<c_int>,
}

impl Read for FILE {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if !out.is_empty() {
            if let Some(c) = self.unget.take() {
                out[0] = c;
                return Ok(1);
            }
        }

        let len = {
            let buf = self.fill_buf()?;
            let len = buf.len().min(out.len());

            out[..len].copy_from_slice(&buf[..len]);
            len
        };
        self.consume(len);
        Ok(len)
    }
}
impl BufRead for FILE {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.read_pos == self.read_size {
            self.read_size = match self.file.read(&mut self.read_buf) {
                Ok(0) => {
                    self.flags |= F_EOF;
                    0
                }
                Ok(n) => n,
                Err(err) => {
                    self.flags |= F_ERR;
                    return Err(err);
                }
            };
            self.read_pos = 0;
        }
        Ok(&self.read_buf[self.read_pos..self.read_size])
    }
    fn consume(&mut self, i: usize) {
        self.read_pos = (self.read_pos + i).min(self.read_size);
    }
}
impl Write for FILE {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.writer.write(buf) {
            Ok(n) => Ok(n),
            Err(err) => {
                self.flags |= F_ERR;
                Err(err)
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self.writer.flush() {
            Ok(()) => Ok(()),
            Err(err) => {
                self.flags |= F_ERR;
                Err(err)
            }
        }
    }
}
impl WriteFmt for FILE {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_all(s.as_bytes())
            .map(|_| ())
            .map_err(|_| fmt::Error)
    }
}
impl WriteByte for FILE {
    fn write_u8(&mut self, c: u8) -> fmt::Result {
        self.write_all(&[c]).map(|_| ()).map_err(|_| fmt::Error)
    }
}
impl FILE {
    pub fn lock(&mut self) -> LockGuard {
        unsafe {
            flockfile(self);
        }
        LockGuard(self)
    }
}

pub struct LockGuard<'a>(&'a mut FILE);
impl<'a> Deref for LockGuard<'a> {
    type Target = FILE;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<'a> DerefMut for LockGuard<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0
    }
}
impl<'a> Drop for LockGuard<'a> {
    fn drop(&mut self) {
        unsafe {
            funlockfile(self.0);
        }
    }
}

/// Clears EOF and ERR indicators on a stream
#[no_mangle]
pub unsafe extern "C" fn clearerr(stream: *mut FILE) {
    let mut stream = (*stream).lock();
    stream.flags &= !(F_EOF | F_ERR);
}

// #[no_mangle]
pub extern "C" fn ctermid(_s: *mut c_char) -> *mut c_char {
    unimplemented!();
}

// #[no_mangle]
pub extern "C" fn cuserid(_s: *mut c_char) -> *mut c_char {
    unimplemented!();
}

/// Close a file
/// This function does not guarentee that the file buffer will be flushed or that the file
/// descriptor will be closed, so if it is important that the file be written to, use `fflush()`
/// prior to using this function.
#[no_mangle]
pub unsafe extern "C" fn fclose(stream: *mut FILE) -> c_int {
    let stream = &mut *stream;
    flockfile(stream);

    let mut r = stream.flush().is_err();
    let close = Sys::close(*stream.file) < 0;
    r = r || close;

    if stream.flags & constants::F_PERM == 0 {
        // Not one of stdin, stdout or stderr
        let mut stream = Box::from_raw(stream);
        // Reference files aren't closed on drop, so pretend to be a reference
        stream.file.reference = true;
    } else {
        funlockfile(stream);
    }

    r as c_int
}

/// Open a file from a file descriptor
#[no_mangle]
pub unsafe extern "C" fn fdopen(fildes: c_int, mode: *const c_char) -> *mut FILE {
    if let Some(f) = helpers::_fdopen(fildes, mode) {
        f
    } else {
        ptr::null_mut()
    }
}

/// Check for EOF
#[no_mangle]
pub unsafe extern "C" fn feof(stream: *mut FILE) -> c_int {
    let stream = (*stream).lock();
    stream.flags & F_EOF
}

/// Check for ERR
#[no_mangle]
pub unsafe extern "C" fn ferror(stream: *mut FILE) -> c_int {
    let stream = (*stream).lock();
    stream.flags & F_ERR
}

/// Flush output to stream, or sync read position
/// Ensure the file is unlocked before calling this function, as it will attempt to lock the file
/// itself.
#[no_mangle]
pub unsafe extern "C" fn fflush(stream: *mut FILE) -> c_int {
    if stream.is_null() {
        //TODO: flush all files!

        if fflush(stdout) != 0 {
            return EOF;
        }

        if fflush(stderr) != 0 {
            return EOF;
        }
    } else {
        let mut stream = (*stream).lock();
        if stream.flush().is_err() {
            return EOF;
        }
    }

    0
}

/// Get a single char from a stream
#[no_mangle]
pub unsafe extern "C" fn fgetc(stream: *mut FILE) -> c_int {
    let mut stream = (*stream).lock();
    getc_unlocked(&mut *stream)
}

/// Get the position of the stream and store it in pos
#[no_mangle]
pub unsafe extern "C" fn fgetpos(stream: *mut FILE, pos: *mut fpos_t) -> c_int {
    let off = ftello(stream);
    if off < 0 {
        return -1;
    }
    *pos = off;
    0
}

/// Get a string from the stream
#[no_mangle]
pub unsafe extern "C" fn fgets(
    original: *mut c_char,
    max: c_int,
    stream: *mut FILE,
) -> *mut c_char {
    let mut stream = (*stream).lock();
    let mut out = original;
    let max = max as usize;
    let mut left = max.saturating_sub(1); // Make space for the terminating NUL-byte
    let mut wrote = false;

    if left >= 1 {
        if let Some(c) = stream.unget.take() {
            *out = c as c_char;
            out = out.offset(1);
            left -= 1;
        }
    }

    loop {
        if left == 0 {
            break;
        }

        // TODO: When NLL is a thing, this block can be flattened out
        let (read, exit) = {
            let mut buf = match stream.fill_buf() {
                Ok(buf) => buf,
                Err(_) => return ptr::null_mut(),
            };
            if buf.is_empty() {
                break;
            }
            wrote = true;
            let len = buf.len().min(left);

            let newline = buf[..len].iter().position(|&c| c == b'\n');
            let len = newline.map(|i| i + 1).unwrap_or(len);

            ptr::copy_nonoverlapping(buf.as_ptr(), out as *mut u8, len);

            (len, newline.is_some())
        };

        stream.consume(read);

        out = out.add(read);
        left -= read;

        if exit {
            break;
        }
    }

    if max >= 1 {
        // Write the NUL byte
        *out = 0;
    }
    if wrote {
        original
    } else {
        ptr::null_mut()
    }
}

/// Get the underlying file descriptor
#[no_mangle]
pub unsafe extern "C" fn fileno(stream: *mut FILE) -> c_int {
    let stream = (*stream).lock();
    *stream.file
}

/// Lock the file
/// Do not call any functions other than those with the `_unlocked` postfix while the file is
/// locked
#[no_mangle]
pub unsafe extern "C" fn flockfile(file: *mut FILE) {
    (*file).lock.manual_lock();
}

/// Open the file in mode `mode`
#[no_mangle]
pub unsafe extern "C" fn fopen(filename: *const c_char, mode: *const c_char) -> *mut FILE {
    let initial_mode = *mode;
    if initial_mode != b'r' as i8 && initial_mode != b'w' as i8 && initial_mode != b'a' as i8 {
        platform::errno = errno::EINVAL;
        return ptr::null_mut();
    }

    let flags = helpers::parse_mode_flags(mode);

    let new_mode = if flags & fcntl::O_CREAT == fcntl::O_CREAT {
        0o666
    } else {
        0
    };

    let fd = fcntl::sys_open(filename, flags, new_mode);
    if fd < 0 {
        return ptr::null_mut();
    }

    if flags & fcntl::O_CLOEXEC > 0 {
        fcntl::sys_fcntl(fd, fcntl::F_SETFD, fcntl::FD_CLOEXEC);
    }

    if let Some(f) = helpers::_fdopen(fd, mode) {
        f
    } else {
        Sys::close(fd);
        ptr::null_mut()
    }
}

/// Insert a character into the stream
#[no_mangle]
pub unsafe extern "C" fn fputc(c: c_int, stream: *mut FILE) -> c_int {
    let mut stream = (*stream).lock();
    putc_unlocked(c, &mut *stream)
}

/// Insert a string into a stream
#[no_mangle]
pub unsafe extern "C" fn fputs(s: *const c_char, stream: *mut FILE) -> c_int {
    let len = strlen(s);
    (fwrite(s as *const c_void, 1, len, stream) == len) as c_int - 1
}

/// Read `nitems` of size `size` into `ptr` from `stream`
#[no_mangle]
pub unsafe extern "C" fn fread(
    ptr: *mut c_void,
    size: size_t,
    nitems: size_t,
    stream: *mut FILE,
) -> size_t {
    if size == 0 || nitems == 0 {
        return 0;
    }

    let mut stream = (*stream).lock();
    let buf = slice::from_raw_parts_mut(ptr as *mut u8, size as usize * nitems as usize);
    let mut read = 0;
    while read < buf.len() {
        match stream.read(&mut buf[read..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => read += n,
        }
    }
    (read / size as usize) as size_t
}

#[no_mangle]
pub unsafe extern "C" fn freopen(
    filename: *const c_char,
    mode: *const c_char,
    stream: &mut FILE,
) -> *mut FILE {
    let mut flags = helpers::parse_mode_flags(mode);
    flockfile(stream);

    let _ = stream.flush();
    if filename.is_null() {
        // Reopen stream in new mode
        if flags & fcntl::O_CLOEXEC > 0 {
            fcntl::sys_fcntl(*stream.file, fcntl::F_SETFD, fcntl::FD_CLOEXEC);
        }
        flags &= !(fcntl::O_CREAT | fcntl::O_EXCL | fcntl::O_CLOEXEC);
        if fcntl::sys_fcntl(*stream.file, fcntl::F_SETFL, flags) < 0 {
            funlockfile(stream);
            fclose(stream);
            return ptr::null_mut();
        }
    } else {
        let new = fopen(filename, mode);
        if new.is_null() {
            funlockfile(stream);
            fclose(stream);
            return ptr::null_mut();
        }
        let new = &mut *new; // Should be safe, new is not null
        if *new.file == *stream.file {
            new.file.fd = -1;
        } else if Sys::dup2(*new.file, *stream.file) < 0
            || fcntl::sys_fcntl(*stream.file, fcntl::F_SETFL, flags & fcntl::O_CLOEXEC) < 0
        {
            funlockfile(stream);
            fclose(new);
            fclose(stream);
            return ptr::null_mut();
        }
        stream.flags = (stream.flags & constants::F_PERM) | new.flags;
        fclose(new);
    }
    funlockfile(stream);
    stream
}

/// Seek to an offset `offset` from `whence`
#[no_mangle]
pub unsafe extern "C" fn fseek(stream: *mut FILE, offset: c_long, whence: c_int) -> c_int {
    fseeko(stream, offset as off_t, whence)
}

/// Seek to an offset `offset` from `whence`
#[no_mangle]
pub unsafe extern "C" fn fseeko(stream: *mut FILE, mut off: off_t, whence: c_int) -> c_int {
    let mut stream = (*stream).lock();

    if whence == SEEK_CUR {
        // Since it's a buffered writer, our actual cursor isn't where the user
        // thinks
        off -= (stream.read_size - stream.read_pos) as off_t;
    }

    // Flush write buffer before seek
    if stream.flush().is_err() {
        return -1;
    }

    let err = Sys::lseek(*stream.file, off, whence);
    if err < 0 {
        return err as c_int;
    }

    stream.flags &= !(F_EOF | F_ERR);
    stream.read_pos = 0;
    stream.read_size = 0;
    stream.unget = None;
    0
}

/// Seek to a position `pos` in the file from the beginning of the file
#[no_mangle]
pub unsafe extern "C" fn fsetpos(stream: *mut FILE, pos: *const fpos_t) -> c_int {
    fseek(stream, *pos, SEEK_SET)
}

/// Get the current position of the cursor in the file
#[no_mangle]
pub unsafe extern "C" fn ftell(stream: *mut FILE) -> c_long {
    ftello(stream) as c_long
}

/// Get the current position of the cursor in the file
#[no_mangle]
pub unsafe extern "C" fn ftello(stream: *mut FILE) -> off_t {
    let stream = (*stream).lock();
    let pos = Sys::lseek(*stream.file, 0, SEEK_CUR);
    if pos < 0 {
        return -1;
    }

    pos - (stream.read_size - stream.read_pos) as off_t
}

/// Try to lock the file. Returns 0 for success, 1 for failure
#[no_mangle]
pub unsafe extern "C" fn ftrylockfile(file: *mut FILE) -> c_int {
    if (*file).lock.manual_try_lock().is_ok() {
        0
    } else {
        1
    }
}

/// Unlock the file
#[no_mangle]
pub unsafe extern "C" fn funlockfile(file: *mut FILE) {
    (*file).lock.manual_unlock();
}

/// Write `nitems` of size `size` from `ptr` to `stream`
#[no_mangle]
pub unsafe extern "C" fn fwrite(
    ptr: *const c_void,
    size: size_t,
    nitems: size_t,
    stream: *mut FILE,
) -> size_t {
    if size == 0 || nitems == 0 {
        return 0;
    }
    let mut stream = (*stream).lock();
    let buf = slice::from_raw_parts_mut(ptr as *mut u8, size as usize * nitems as usize);
    let mut written = 0;
    while written < buf.len() {
        match stream.write(&mut buf[written..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => written += n,
        }
    }
    (written / size as usize) as size_t
}

/// Get a single char from a stream
#[no_mangle]
pub unsafe extern "C" fn getc(stream: *mut FILE) -> c_int {
    let mut stream = (*stream).lock();
    getc_unlocked(&mut *stream)
}

/// Get a single char from `stdin`
#[no_mangle]
pub unsafe extern "C" fn getchar() -> c_int {
    fgetc(&mut *stdin)
}

/// Get a char from a stream without locking the stream
#[no_mangle]
pub unsafe extern "C" fn getc_unlocked(stream: *mut FILE) -> c_int {
    let mut buf = [0];

    match (*stream).read(&mut buf) {
        Ok(0) | Err(_) => EOF,
        Ok(_) => buf[0] as c_int,
    }
}

/// Get a char from `stdin` without locking `stdin`
#[no_mangle]
pub unsafe extern "C" fn getchar_unlocked() -> c_int {
    getc_unlocked(&mut *stdin)
}

/// Get a string from `stdin`
#[no_mangle]
pub unsafe extern "C" fn gets(s: *mut c_char) -> *mut c_char {
    fgets(s, c_int::max_value(), &mut *stdin)
}

/// Get an integer from `stream`
#[no_mangle]
pub unsafe extern "C" fn getw(stream: *mut FILE) -> c_int {
    let mut ret: c_int = 0;
    if fread(
        &mut ret as *mut _ as *mut c_void,
        mem::size_of_val(&ret),
        1,
        stream,
    ) > 0
    {
        ret
    } else {
        -1
    }
}

#[no_mangle]
pub unsafe extern "C" fn pclose(stream: *mut FILE) -> c_int {
    let pid = {
        let mut stream = (*stream).lock();

        if let Some(pid) = stream.pid.take() {
            pid
        } else {
            errno = errno::ECHILD;
            return -1;
        }
    };

    fclose(stream);

    let mut wstatus = 0;
    if Sys::waitpid(pid, &mut wstatus, 0) < 0 {
        return -1;
    }

    wstatus
}

#[no_mangle]
pub unsafe extern "C" fn perror(s: *const c_char) {
    let s_cstr = CStr::from_ptr(s);
    let s_str = str::from_utf8_unchecked(s_cstr.to_bytes());

    let mut w = platform::FileWriter(2);
    if errno >= 0 && errno < STR_ERROR.len() as c_int {
        w.write_fmt(format_args!("{}: {}\n", s_str, STR_ERROR[errno as usize]))
            .unwrap();
    } else {
        w.write_fmt(format_args!("{}: Unknown error {}\n", s_str, errno))
            .unwrap();
    }
}

#[no_mangle]
pub unsafe extern "C" fn popen(command: *const c_char, mode: *const c_char) -> *mut FILE {
    //TODO: share code with system

    let mode = CStr::from_ptr(mode);

    let mut cloexec = false;
    let mut write_opt = None;
    for b in mode.to_bytes().iter() {
        match b {
            b'e' => cloexec = true,
            b'r' if write_opt.is_none() => write_opt = Some(false),
            b'w' if write_opt.is_none() => write_opt = Some(true),
            _ => {
                errno = errno::EINVAL;
                return ptr::null_mut();
            }
        }
    }

    let write = match write_opt {
        Some(some) => some,
        None => {
            errno = errno::EINVAL;
            return ptr::null_mut();
        }
    };

    let mut pipes = [-1, -1];
    if unistd::pipe(pipes.as_mut_ptr()) != 0 {
        return ptr::null_mut();
    }

    let child_pid = unistd::fork();
    if child_pid == 0 {
        let command_nonnull = if command.is_null() {
            "exit 0\0".as_ptr()
        } else {
            command as *const u8
        };

        let shell = "/bin/sh\0".as_ptr();

        let args = [
            "sh\0".as_ptr(),
            "-c\0".as_ptr(),
            command_nonnull,
            ptr::null(),
        ];

        // Setup up stdin or stdout
        //TODO: dup errors are ignored, should they be?
        {
            if write {
                unistd::dup2(0, pipes[0]);
            } else {
                unistd::dup2(1, pipes[1]);
            }

            unistd::close(pipes[0]);
            unistd::close(pipes[1]);
        }

        unistd::execv(shell as *const c_char, args.as_ptr() as *const *mut c_char);

        stdlib::exit(127);

        unreachable!();
    } else if child_pid > 0 {
        let (fd, fd_mode) = if write {
            unistd::close(pipes[0]);
            (pipes[1], if cloexec { c_str!("we") } else { c_str!("w") })
        } else {
            unistd::close(pipes[1]);
            (pipes[0], if cloexec { c_str!("re") } else { c_str!("r") })
        };

        if let Some(f) = helpers::_fdopen(fd, fd_mode.as_ptr()) {
            (*f).pid = Some(child_pid);
            f
        } else {
            ptr::null_mut()
        }
    } else {
        ptr::null_mut()
    }
}

/// Put a character `c` into `stream`
#[no_mangle]
pub unsafe extern "C" fn putc(c: c_int, stream: *mut FILE) -> c_int {
    let mut stream = (*stream).lock();
    putc_unlocked(c, &mut *stream)
}

/// Put a character `c` into `stdout`
#[no_mangle]
pub unsafe extern "C" fn putchar(c: c_int) -> c_int {
    fputc(c, &mut *stdout)
}

/// Put a character `c` into `stream` without locking `stream`
#[no_mangle]
pub unsafe extern "C" fn putc_unlocked(c: c_int, stream: *mut FILE) -> c_int {
    match (*stream).write(&[c as u8]) {
        Ok(0) | Err(_) => EOF,
        Ok(_) => c,
    }
}

/// Put a character `c` into `stdout` without locking `stdout`
#[no_mangle]
pub unsafe extern "C" fn putchar_unlocked(c: c_int) -> c_int {
    putc_unlocked(c, stdout)
}

/// Put a string `s` into `stdout`
#[no_mangle]
pub unsafe extern "C" fn puts(s: *const c_char) -> c_int {
    let ret = (fputs(s, stdout) > 0) || (putchar_unlocked(b'\n' as c_int) > 0);
    if ret {
        0
    } else {
        -1
    }
}

/// Put an integer `w` into `stream`
#[no_mangle]
pub unsafe extern "C" fn putw(w: c_int, stream: *mut FILE) -> c_int {
    fwrite(&w as *const c_int as _, mem::size_of_val(&w), 1, stream) as i32 - 1
}

/// Delete file or directory `path`
#[no_mangle]
pub unsafe extern "C" fn remove(path: *const c_char) -> c_int {
    let path = CStr::from_ptr(path);
    let r = Sys::unlink(path);
    if r == -errno::EISDIR {
        Sys::rmdir(path)
    } else {
        r
    }
}

#[no_mangle]
pub unsafe extern "C" fn rename(oldpath: *const c_char, newpath: *const c_char) -> c_int {
    let oldpath = CStr::from_ptr(oldpath);
    let newpath = CStr::from_ptr(newpath);
    Sys::rename(oldpath, newpath)
}

/// Rewind `stream` back to the beginning of it
#[no_mangle]
pub unsafe extern "C" fn rewind(stream: *mut FILE) {
    fseeko(stream, 0, SEEK_SET);
}

/// Reset `stream` to use buffer `buf`. Buffer must be `BUFSIZ` in length
#[no_mangle]
pub unsafe extern "C" fn setbuf(stream: *mut FILE, buf: *mut c_char) {
    setvbuf(
        stream,
        buf,
        if buf.is_null() { _IONBF } else { _IOFBF },
        BUFSIZ as usize,
    );
}

/// Reset `stream` to use buffer `buf` of size `size`
/// If this isn't the meaning of unsafe, idk what is
#[no_mangle]
pub unsafe extern "C" fn setvbuf(
    stream: *mut FILE,
    buf: *mut c_char,
    mode: c_int,
    mut size: size_t,
) -> c_int {
    let mut stream = (*stream).lock();
    // Set a buffer of size `size` if no buffer is given
    stream.read_buf = if buf.is_null() || size == 0 {
        if size == 0 {
            size = BUFSIZ as usize;
        }
        // TODO: Make it unbuffered if _IONBF
        // if mode == _IONBF {
        // } else {
        Buffer::Owned(vec![0; size as usize])
    // }
    } else {
        Buffer::Borrowed(slice::from_raw_parts_mut(buf as *mut u8, size))
    };
    stream.flags |= F_SVB;
    0
}

// #[no_mangle]
pub extern "C" fn tempnam(_dir: *const c_char, _pfx: *const c_char) -> *mut c_char {
    unimplemented!();
}

#[no_mangle]
pub unsafe extern "C" fn tmpfile() -> *mut FILE {
    let mut file_name = *b"/tmp/tmpfileXXXXXX\0";
    let file_name = file_name.as_mut_ptr() as *mut c_char;
    let fd = stdlib::mkstemp(file_name);

    if fd < 0 {
        return ptr::null_mut();
    }

    let fp = fdopen(fd, c_str!("w+").as_ptr());
    {
        let file_name = CStr::from_ptr(file_name);
        Sys::unlink(file_name);
    }

    if fp == ptr::null_mut() {
        Sys::close(fd);
    }

    fp
}

// #[no_mangle]
pub extern "C" fn tmpnam(_s: *mut c_char) -> *mut c_char {
    unimplemented!();
}

/// Push character `c` back onto `stream` so it'll be read next
#[no_mangle]
pub unsafe extern "C" fn ungetc(c: c_int, stream: *mut FILE) -> c_int {
    let mut stream = (*stream).lock();
    if stream.unget.is_some() {
        platform::errno = errno::EIO;
        return EOF;
    }
    stream.unget = Some(c as u8);
    c
}

#[no_mangle]
pub unsafe extern "C" fn vfprintf(file: *mut FILE, format: *const c_char, ap: va_list) -> c_int {
    let mut file = (*file).lock();
    printf::printf(&mut *file, format, ap)
}

#[no_mangle]
pub unsafe extern "C" fn vprintf(format: *const c_char, ap: va_list) -> c_int {
    vfprintf(&mut *stdout, format, ap)
}

#[no_mangle]
pub unsafe extern "C" fn vasprintf(
    strp: *mut *mut c_char,
    format: *const c_char,
    ap: va_list,
) -> c_int {
    let mut alloc_writer = platform::AllocStringWriter(ptr::null_mut(), 0);
    let ret = printf::printf(&mut alloc_writer, format, ap);
    *strp = alloc_writer.0 as *mut c_char;
    ret
}

#[no_mangle]
pub unsafe extern "C" fn vsnprintf(
    s: *mut c_char,
    n: size_t,
    format: *const c_char,
    ap: va_list,
) -> c_int {
    printf::printf(
        &mut platform::StringWriter(s as *mut u8, n as usize),
        format,
        ap,
    )
}

#[no_mangle]
pub unsafe extern "C" fn vsprintf(s: *mut c_char, format: *const c_char, ap: va_list) -> c_int {
    printf::printf(&mut platform::UnsafeStringWriter(s as *mut u8), format, ap)
}

#[no_mangle]
pub unsafe extern "C" fn vfscanf(file: *mut FILE, format: *const c_char, ap: va_list) -> c_int {
    let mut file = (*file).lock();
    scanf::scanf(&mut *file, format, ap)
}

#[no_mangle]
pub unsafe extern "C" fn vscanf(format: *const c_char, ap: va_list) -> c_int {
    vfscanf(&mut *stdin, format, ap)
}

#[no_mangle]
pub unsafe extern "C" fn vsscanf(s: *const c_char, format: *const c_char, ap: va_list) -> c_int {
    scanf::scanf(
        &mut platform::UnsafeStringReader(s as *const u8),
        format,
        ap,
    )
}

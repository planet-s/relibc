//! termios implementation, following http://pubs.opengroup.org/onlinepubs/7908799/xsh/termios.h.html

use crate::{
    header::{errno, sys_ioctl},
    platform::{self, types::*},
};

pub use self::sys::*;

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
pub mod sys;

#[cfg(target_os = "redox")]
#[path = "redox.rs"]
pub mod sys;

pub type cc_t = u8;
pub type speed_t = u32;
pub type tcflag_t = u32;

pub const TCOOFF: c_int = 0;
pub const TCOON: c_int = 1;
pub const TCIOFF: c_int = 2;
pub const TCION: c_int = 3;

pub const TCIFLUSH: c_int = 0;
pub const TCOFLUSH: c_int = 1;
pub const TCIOFLUSH: c_int = 2;

pub const TCSANOW: c_int = 0;
pub const TCSADRAIN: c_int = 1;
pub const TCSAFLUSH: c_int = 2;

#[repr(C)]
#[derive(Default)]
pub struct termios {
    c_iflag: tcflag_t,
    c_oflag: tcflag_t,
    c_cflag: tcflag_t,
    c_lflag: tcflag_t,
    c_line: cc_t,
    c_cc: [cc_t; NCCS],
    __c_ispeed: speed_t,
    __c_ospeed: speed_t,
}

#[no_mangle]
pub unsafe extern "C" fn tcgetattr(fd: c_int, out: *mut termios) -> c_int {
    sys_ioctl::ioctl(fd, sys_ioctl::TCGETS, out as *mut c_void)
}

#[no_mangle]
pub unsafe extern "C" fn tcsetattr(fd: c_int, act: c_int, value: *mut termios) -> c_int {
    if act < 0 || act > 2 {
        platform::errno = errno::EINVAL;
        return -1;
    }
    // This is safe because ioctl shouldn't modify the value
    sys_ioctl::ioctl(fd, sys_ioctl::TCSETS + act as c_ulong, value as *mut c_void)
}

#[no_mangle]
pub unsafe extern "C" fn cfgetispeed(termios_p: *const termios) -> speed_t {
    (*termios_p).__c_ispeed
}

#[no_mangle]
pub unsafe extern "C" fn cfgetospeed(termios_p: *const termios) -> speed_t {
    (*termios_p).__c_ospeed
}

#[no_mangle]
pub unsafe extern "C" fn cfsetispeed(termios_p: *mut termios, speed: speed_t) -> c_int {
    match speed {
        B0..=B38400 | B57600..=B4000000 => {
            (*termios_p).__c_ispeed = speed;
            0
        }
        _ => {
            platform::errno = errno::EINVAL;
            -1
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn cfsetospeed(termios_p: *mut termios, speed: speed_t) -> c_int {
    match speed {
        B0..=B38400 | B57600..=B4000000 => {
            (*termios_p).__c_ospeed = speed;
            0
        }
        _ => {
            platform::errno = errno::EINVAL;
            -1
        }
    }
}

// Based on glibc/termios/cfmakeraw.c
#[no_mangle]
pub unsafe extern "C" fn cfmakeraw(t: *mut termios) {
    (*t).c_iflag &= !(IGNBRK|BRKINT|PARMRK|ISTRIP|INLCR|IGNCR|ICRNL|IXON);
    (*t).c_oflag &= !OPOST;
    (*t).c_lflag &= !(ECHO|ECHONL|ICANON|ISIG|IEXTEN);
    (*t).c_cflag &= !(CSIZE|PARENB);
    (*t).c_cflag |= CS8;
    // Read returns after each char
    (*t).c_cc[sys::VMIN as usize] = 1;
    (*t).c_cc[sys::VTIME as usize] = 0;
}

#[no_mangle]
pub unsafe extern "C" fn tcflush(fd: c_int, queue: c_int) -> c_int {
    sys_ioctl::ioctl(fd, sys_ioctl::TCFLSH, queue as *mut c_void)
}

#[no_mangle]
pub unsafe extern "C" fn tcdrain(fd: c_int) -> c_int {
    sys_ioctl::ioctl(fd, sys_ioctl::TCSBRK, 1 as *mut _)
}

#[no_mangle]
pub unsafe extern "C" fn tcsendbreak(fd: c_int, _dur: c_int) -> c_int {
    // non-zero duration is ignored by musl due to it being
    // implementation-defined. we do the same.
    sys_ioctl::ioctl(fd, sys_ioctl::TCSBRK, 0 as *mut _)
}

#[no_mangle]
pub unsafe extern "C" fn tcflow(fd: c_int, action: c_int) -> c_int {
    // non-zero duration is ignored by musl due to it being
    // implementation-defined. we do the same.
    sys_ioctl::ioctl(fd, sys_ioctl::TCXONC, action as *mut _)
}

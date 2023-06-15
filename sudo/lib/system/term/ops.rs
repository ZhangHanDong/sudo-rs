//! This module is a port of ogsudo's `lib/util/term.c` with some minor changes to make it
//! rust-like.

use std::{
    ffi::c_int,
    io,
    mem::MaybeUninit,
    os::fd::AsRawFd,
    sync::atomic::{AtomicBool, Ordering},
};

use libc::{
    c_void, cfgetispeed, cfgetospeed, cfmakeraw, cfsetispeed, cfsetospeed, ioctl, sigaction,
    sigemptyset, sighandler_t, siginfo_t, sigset_t, tcflag_t, tcgetattr, tcsetattr, termios,
    winsize, CS7, CS8, ECHO, ECHOCTL, ECHOE, ECHOK, ECHOKE, ECHONL, ICANON, ICRNL, IEXTEN, IGNCR,
    IGNPAR, IMAXBEL, INLCR, INPCK, ISIG, ISTRIP, IUTF8, IXANY, IXOFF, IXON, NOFLSH, OCRNL, OLCUC,
    ONLCR, ONLRET, ONOCR, OPOST, PARENB, PARMRK, PARODD, PENDIN, SIGTTOU, TCSADRAIN, TCSAFLUSH,
    TIOCGWINSZ, TIOCSWINSZ, TOSTOP,
};

use crate::cutils::cerr;

const INPUT_FLAGS: tcflag_t = IGNPAR
    | PARMRK
    | INPCK
    | ISTRIP
    | INLCR
    | IGNCR
    | ICRNL
    // | IUCLC /* FIXME: not in libc */
    | IXON
    | IXANY
    | IXOFF
    | IMAXBEL
    | IUTF8;
const OUTPUT_FLAGS: tcflag_t = OPOST | OLCUC | ONLCR | OCRNL | ONOCR | ONLRET;
const CONTROL_FLAGS: tcflag_t = CS7 | CS8 | PARENB | PARODD;
const LOCAL_FLAGS: tcflag_t = ISIG
    | ICANON
    // | XCASE /* FIXME: not in libc */
    | ECHO
    | ECHOE
    | ECHOK
    | ECHONL
    | NOFLSH
    | TOSTOP
    | IEXTEN
    | ECHOCTL
    | ECHOKE
    | PENDIN;

// FIXME: me no like `static mut`.
static mut OTERM: MaybeUninit<termios> = MaybeUninit::uninit();
static CHANGED: AtomicBool = AtomicBool::new(false);
static GOT_SIGTTOU: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigttou(_signal: c_int, _info: *mut siginfo_t, _: *mut c_void) {
    GOT_SIGTTOU.store(true, Ordering::SeqCst);
}

/// This is like `tcsetattr` but it only suceeds if we are in the foreground process group.
fn tcsetattr_nobg(fd: c_int, flags: c_int, tp: *const termios) -> io::Result<()> {
    // This function is based around the fact that we receive `SIGTTOU` if we call `tcsetattr` and
    // we are not in the foreground process group.

    let mut original_action = MaybeUninit::<sigaction>::uninit();

    let action = sigaction {
        // Call `on_sigttou` if `SIGTTOU` arrives.
        sa_sigaction: on_sigttou as sighandler_t,
        // Exclude any other signals from the set
        sa_mask: {
            let mut sa_mask = MaybeUninit::<sigset_t>::uninit();
            unsafe { sigemptyset(sa_mask.as_mut_ptr()) };
            unsafe { sa_mask.assume_init() }
        },
        sa_flags: 0,
        sa_restorer: None,
    };
    // Reset `GOT_SIGTTOU`.
    GOT_SIGTTOU.store(false, Ordering::SeqCst);
    // Set `action` as the action for `SIGTTOU` and store the original action in `original_action`
    // to restore it later.
    unsafe { sigaction(SIGTTOU, &action, original_action.as_mut_ptr()) };
    // Call `tcsetattr` until it suceeds and ignore interruptions if we did not receive `SIGTTOU`.
    loop {
        match cerr(unsafe { tcsetattr(fd, flags, tp) }) {
            Ok(_) => break,
            Err(err) => {
                let got_sigttou = GOT_SIGTTOU.load(Ordering::SeqCst);
                if got_sigttou || err.kind() != io::ErrorKind::Interrupted {
                    return Err(err);
                }
            }
        }
    }
    // Restore the original action.
    unsafe { sigaction(SIGTTOU, original_action.as_ptr(), std::ptr::null_mut()) };

    Ok(())
}

/// Copy the settings of the `src` terminal to the `dst` terminal.
pub fn term_copy<S: AsRawFd, D: AsRawFd>(src: &S, dst: &D) -> io::Result<()> {
    let src = src.as_raw_fd();
    let dst = dst.as_raw_fd();

    let mut tt_src = MaybeUninit::<termios>::uninit();
    let mut tt_dst = MaybeUninit::<termios>::uninit();
    let mut wsize = MaybeUninit::<winsize>::uninit();

    cerr(unsafe { tcgetattr(src, tt_src.as_mut_ptr()) })?;
    cerr(unsafe { tcgetattr(dst, tt_dst.as_mut_ptr()) })?;

    let tt_src = unsafe { tt_src.assume_init() };
    let mut tt_dst = unsafe { tt_dst.assume_init() };

    // Clear select input, output, control and local flags.
    tt_dst.c_iflag &= !INPUT_FLAGS;
    tt_dst.c_oflag &= !OUTPUT_FLAGS;
    tt_dst.c_cflag &= !CONTROL_FLAGS;
    tt_dst.c_lflag &= !LOCAL_FLAGS;

    // Copy select input, output, control and local flags.
    tt_dst.c_iflag |= tt_src.c_iflag & INPUT_FLAGS;
    tt_dst.c_oflag |= tt_src.c_oflag & OUTPUT_FLAGS;
    tt_dst.c_cflag |= tt_src.c_cflag & CONTROL_FLAGS;
    tt_dst.c_lflag |= tt_src.c_lflag & LOCAL_FLAGS;

    // Copy special chars from src verbatim.
    tt_dst.c_cc.copy_from_slice(&tt_src.c_cc);

    // Copy speed from `src`.
    {
        let mut speed = unsafe { cfgetospeed(&tt_src) };
        // Zero output speed closes the connection.
        if speed == libc::B0 {
            speed = libc::B38400;
        }
        unsafe { cfsetospeed(&mut tt_dst, speed) };
        speed = unsafe { cfgetispeed(&tt_src) };
        unsafe { cfsetispeed(&mut tt_dst, speed) };
    }

    tcsetattr_nobg(dst, TCSAFLUSH, &tt_dst)?;

    cerr(unsafe { ioctl(src, TIOCGWINSZ, &mut wsize) })?;
    cerr(unsafe { ioctl(dst, TIOCSWINSZ, &wsize) })?;

    Ok(())
}

/// Set the `fd` terminal to raw mode. Enable terminal signals if `with_signals` is set to `true`.  
pub fn term_raw<F: AsRawFd>(fd: &F, with_signals: bool) -> io::Result<()> {
    let fd = fd.as_raw_fd();

    if !CHANGED.load(Ordering::Acquire) {
        cerr(unsafe { tcgetattr(fd, OTERM.as_mut_ptr()) })?;
    }
    // Retrieve the original terminal.
    let mut term = unsafe { OTERM.assume_init() };
    // Set terminal to raw mode.
    unsafe { cfmakeraw(&mut term) };
    // Enable terminal signals.
    if with_signals {
        term.c_cflag |= ISIG;
    }

    tcsetattr_nobg(fd, TCSADRAIN, &term)?;
    CHANGED.store(true, Ordering::Release);

    Ok(())
}

/// Restore the saved terminal settings if we are in the foreground process group.
///
/// This change is done after waiting for all the queued output to be written. To discard the
/// queued input `flush` must be set to `true`.
pub fn term_restore<F: AsRawFd>(fd: &F, flush: bool) -> io::Result<()> {
    if CHANGED.load(Ordering::Acquire) {
        let fd = fd.as_raw_fd();
        let flags = if flush { TCSAFLUSH } else { TCSADRAIN };
        tcsetattr_nobg(fd, flags, unsafe { OTERM.as_ptr() })?;
    }

    Ok(())
}

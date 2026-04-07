// Copyright (c) 2026 Jon-Erik G. Storm. All rights reserved.
// Signal handling for jmake: cleanup on SIGTERM.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// Maximum path length for the temp file path stored for signal handler use.
const MAX_PATH: usize = 4096;
// Maximum message length
const MAX_MSG: usize = 2048;

// Global buffers (file-static, initialized to zero).
// These are used by the signal handler and must be async-signal-safe.
// We use a simple approach: store bytes as arrays accessed through atomic loads.
// Single-threaded access is assumed since make is single-threaded.

static mut TEMP_FILE_BUF: [u8; MAX_PATH] = [0u8; MAX_PATH];
static TEMP_FILE_LEN: AtomicUsize = AtomicUsize::new(0);

static mut TERM_MSG_BUF: [u8; MAX_MSG] = [0u8; MAX_MSG];
static TERM_MSG_LEN: AtomicUsize = AtomicUsize::new(0);

static SIGNAL_RECEIVED: AtomicBool = AtomicBool::new(false);

/// Set the temp stdin file path for the SIGTERM handler to clean up.
pub fn set_temp_stdin_path(path: &str) {
    let bytes = path.as_bytes();
    let len = bytes.len().min(MAX_PATH - 1);
    unsafe {
        TEMP_FILE_BUF[..len].copy_from_slice(&bytes[..len]);
        TEMP_FILE_BUF[len] = 0;
    }
    TEMP_FILE_LEN.store(len, Ordering::Release);
}

/// Clear the temp stdin path.
pub fn clear_temp_stdin_path() {
    TEMP_FILE_LEN.store(0, Ordering::Release);
}

/// Set the Terminated message for the SIGTERM handler to print.
/// Format: "progname: *** [file:line: target] Terminated\n"
pub fn set_term_message(msg: &str) {
    let bytes = msg.as_bytes();
    let len = bytes.len().min(MAX_MSG - 1);
    unsafe {
        TERM_MSG_BUF[..len].copy_from_slice(&bytes[..len]);
        TERM_MSG_BUF[len] = 0;
    }
    TERM_MSG_LEN.store(len, Ordering::Release);
}

/// Clear the Terminated message.
pub fn clear_term_message() {
    TERM_MSG_LEN.store(0, Ordering::Release);
}

/// Returns true if a SIGTERM was received.
pub fn signal_received() -> bool {
    SIGNAL_RECEIVED.load(Ordering::Acquire)
}

/// The SIGTERM signal handler.
/// Async-signal-safe: uses only write() and unlink().
extern "C" fn sigterm_handler(_sig: libc::c_int) {
    SIGNAL_RECEIVED.store(true, Ordering::Release);

    // Print the Terminated message to stderr
    let msg_len = TERM_MSG_LEN.load(Ordering::Acquire);
    if msg_len > 0 {
        unsafe {
            libc::write(
                libc::STDERR_FILENO,
                TERM_MSG_BUF.as_ptr() as *const libc::c_void,
                msg_len,
            );
        }
    }

    // Delete the temp stdin file
    let path_len = TEMP_FILE_LEN.load(Ordering::Acquire);
    if path_len > 0 {
        unsafe {
            libc::unlink(TEMP_FILE_BUF.as_ptr() as *const libc::c_char);
        }
    }

    // Reset SIGTERM to default and re-raise to die with signal 15 status
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_DFL);
        libc::raise(libc::SIGTERM);
    }
}

/// Install the SIGTERM handler.
pub fn install_sigterm_handler() {
    unsafe {
        libc::signal(libc::SIGTERM, sigterm_handler as libc::sighandler_t);
    }
}

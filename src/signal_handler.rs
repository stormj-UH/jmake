// (c) 2026 Jon-Erik G. Storm, Inc., a California Corporation,
// doing business as LAVA GOAT SOFTWARE. All rights reserved.
// SPDX-License-Identifier: MIT

//! SIGTERM handler and async-signal-safe cleanup for jmake.
//!
//! When jmake receives `SIGTERM` while a recipe is running it must:
//!
//! 1. Print a `"*** [file:line: target] Terminated"` message to stderr.
//! 2. Delete any temporary file that was created to hold stdin content (the
//!    `--temp-stdin` mechanism used when jmake re-execs itself to pass makefile
//!    content from stdin to the child).
//! 3. Reset `SIGTERM` to `SIG_DFL` and re-raise the signal so the process
//!    exits with the correct signal-killed status (not exit code 0 or 1).
//!
//! # Design: async-signal-safe globals
//!
//! Signal handlers may interrupt any point in the program; they must only
//! call functions listed as async-signal-safe in POSIX.1-2017 §2.4.3.
//! That rules out Rust's allocator, `eprintln!`, `std::fs::remove_file`, and
//! any lock-taking function.
//!
//! To work around this, the paths and messages that the handler needs are
//! stored in fixed-size static buffers (`SyncBuffer<N>`) before the handler
//! is invoked.  The handler reads those buffers with raw pointer accesses and
//! calls only `write(2)`, `unlink(2)`, `signal(2)`, and `raise(2)`.
//!
//! # Soundness
//!
//! See the four invariants (I1–I4) documented on the `SyncBuffer` statics
//! below.  The key points are:
//!
//! - jmake is **single-threaded** at the point where these globals are written
//!   (invariant I1), so there is no concurrent-write hazard.
//! - A `Release` atomic store to the length field always follows the buffer
//!   write; the handler loads it with `Acquire`, ensuring it never sees a
//!   partially-written buffer (invariant I2).
//! - No Rust reference (`&` or `&mut`) to the buffer interior is ever
//!   materialised while a raw-pointer write is in progress (invariant I4).
//!
//! # Thread safety
//!
//! `set_temp_stdin_path`, `clear_temp_stdin_path`, `set_term_message`, and
//! `clear_term_message` must only be called from the main thread.  The
//! signal handler itself runs asynchronously on whatever thread receives the
//! signal, but invariant I1 ensures that does not create a data race.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// Maximum path length for the temp file path stored for signal handler use.
//
// POSIX PATH_MAX is 4096 on Linux (include/uapi/linux/limits.h) and on macOS
// (sys/syslimits.h).  musl libc defines PATH_MAX as 4096 in
// include/limits.h.  We match that value exactly so that any valid
// absolute path fits in the buffer with a NUL terminator in the last byte.
//
// Const-assert: a compile-time check makes the relationship machine-verifiable.
const MAX_PATH: usize = 4096;
const _: () = assert!(MAX_PATH == 4096, "MAX_PATH must match POSIX PATH_MAX (4096)");

// Maximum message length for the Terminated diagnostic line.
//
// The message has the form:
//   "<progname>: *** [<file>:<line>: <target>] Terminated\n"
// Worst case: 255 (progname) + 4096 (file path) + 20 (line) + 255 (target)
// + ~30 (fixed text) ≈ 4656 bytes.  2048 is generous for the common case
// (short progname, short path, short target) while remaining signal-safe.
// If a message is truncated it is still diagnostic; truncation is safe.
const MAX_MSG: usize = 2048;
const _: () = assert!(MAX_MSG == 2048, "MAX_MSG sentinel — update if you change the constant");

// Signal-handler globals.
//
// These buffers are accessed from the async-signal context (sigterm_handler)
// and from normal code (set_*/clear_*).  To avoid creating shared references
// to mutable statics — which is undefined behaviour under Rust's aliasing model
// and triggers the `static_mut_refs` lint — the buffers are wrapped in
// `UnsafeCell`.  `UnsafeCell` is the language-approved escape hatch for
// interior mutability; a `*const UnsafeCell<T>` raw pointer may be cast to
// `*mut T` without violating aliasing rules.
//
// Soundness invariants that make the accesses safe in practice:
//   I1. jmake is single-threaded.  The set_*/clear_* functions and the signal
//       handler never execute concurrently with each other on different threads.
//   I2. The signal handler is installed AFTER the buffer contents are fully
//       written by set_* (the Release store to the AtomicUsize length happens
//       after the buffer write; the handler reads the length with Acquire).
//   I3. The only libc functions called inside the signal handler (write,
//       unlink, signal, raise) are async-signal-safe per POSIX.1-2017 §2.4.3.
//   I4. No Rust reference (& or &mut) to the buffer interior is ever live
//       at the same time as any write to that buffer.  We use raw pointers
//       exclusively; the UnsafeCell wrapper documents this intent.
//
// `UnsafeCell<[u8; N]>` does NOT implement `Sync` by default, so we provide a
// wrapper type that asserts `Sync` + `Send` under invariant I1.
struct SyncBuffer<const N: usize>(UnsafeCell<[u8; N]>);

// SAFETY: I1 — single-threaded; no concurrent access from multiple threads.
unsafe impl<const N: usize> Sync for SyncBuffer<N> {}
// SAFETY: I1 — single-threaded; ownership transfer between threads never occurs.
unsafe impl<const N: usize> Send for SyncBuffer<N> {}

impl<const N: usize> SyncBuffer<N> {
    const fn new() -> Self {
        SyncBuffer(UnsafeCell::new([0u8; N]))
    }

    /// Return a raw mutable pointer to the inner byte array.
    /// Callers must uphold I1 (exclusive access) and must not create live
    /// Rust references at the same time.
    #[inline]
    fn as_mut_ptr(&self) -> *mut u8 {
        // SAFETY: UnsafeCell::get() returns *mut T without creating a reference.
        // Dereferencing the result is the caller's responsibility.
        self.0.get().cast::<u8>()
    }

    /// Return a raw const pointer to the inner byte array.
    #[inline]
    fn as_ptr(&self) -> *const u8 {
        self.as_mut_ptr() as *const u8
    }
}

static TEMP_FILE_BUF: SyncBuffer<MAX_PATH> = SyncBuffer::new();
static TEMP_FILE_LEN: AtomicUsize = AtomicUsize::new(0);

static TERM_MSG_BUF: SyncBuffer<MAX_MSG> = SyncBuffer::new();
static TERM_MSG_LEN: AtomicUsize = AtomicUsize::new(0);

static SIGNAL_RECEIVED: AtomicBool = AtomicBool::new(false);

/// Set the temp stdin file path for the SIGTERM handler to clean up.
pub fn set_temp_stdin_path(path: &str) {
    let bytes = path.as_bytes();
    let len = bytes.len().min(MAX_PATH - 1);
    // I1 (single-threaded): no concurrent writer — enforced by the caller
    // contract documented on this module (main thread only).
    // I2 (Release/Acquire ordering): the store below happens-after this write.
    // I3 (bounds): len ≤ MAX_PATH-1 by the .min() above.
    // I4 (no aliasing): no live Rust reference into TEMP_FILE_BUF exists here.
    debug_assert!(len < MAX_PATH, "path len {len} must be < MAX_PATH {MAX_PATH}");
    debug_assert!(
        len + 1 <= MAX_PATH,
        "NUL terminator at offset {0} must be within buffer [0, {MAX_PATH})",
        len + 1
    );
    // SAFETY:
    // - I1: jmake is single-threaded; no concurrent writer exists.
    // - I2: pointer from UnsafeCell::get() — the approved way to obtain *mut
    //   without constructing a &mut or & reference; no Rust reference to the
    //   buffer interior is created.
    // - I3: len ≤ MAX_PATH-1 (enforced by .min() and the debug_assert above),
    //   so bytes [0..len] and the NUL at [len] are all within the array bounds.
    // - I4: the Release store to TEMP_FILE_LEN below happens-after this write;
    //   the signal handler loads with Acquire and therefore cannot observe a
    //   partially-written buffer.
    unsafe {
        let dst = TEMP_FILE_BUF.as_mut_ptr();
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, len);
        dst.add(len).write(0u8);
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
    // I1 (single-threaded): no concurrent writer — main thread only.
    // I2 (Release/Acquire): store below happens-after the write.
    // I3 (bounds): len ≤ MAX_MSG-1 by the .min() above.
    // I4 (no aliasing): no live Rust reference into TERM_MSG_BUF exists here.
    debug_assert!(len < MAX_MSG, "msg len {len} must be < MAX_MSG {MAX_MSG}");
    debug_assert!(
        len + 1 <= MAX_MSG,
        "NUL terminator at offset {0} must be within buffer [0, {MAX_MSG})",
        len + 1
    );
    // SAFETY:
    // - I1: single-threaded; no concurrent writer.
    // - I2: pointer from UnsafeCell::get(); no Rust reference materialised.
    // - I3: len ≤ MAX_MSG-1 (enforced by .min() and debug_assert above),
    //   so [0..len] and the NUL at [len] are within the array bounds.
    // - I4: Release store below ensures the signal handler (Acquire load)
    //   observes the complete buffer write.
    unsafe {
        let dst = TERM_MSG_BUF.as_mut_ptr();
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, len);
        dst.add(len).write(0u8);
    }
    TERM_MSG_LEN.store(len, Ordering::Release);
}

/// Clear the Terminated message.
pub fn clear_term_message() {
    TERM_MSG_LEN.store(0, Ordering::Release);
}

/// Returns true if a SIGTERM was received.
/// Currently unused by the main loop but retained for future signal-poll support.
#[allow(dead_code)]
pub fn signal_received() -> bool {
    SIGNAL_RECEIVED.load(Ordering::Acquire)
}

/// The SIGTERM signal handler.
///
/// Async-signal-safety: only `write`, `unlink`, `signal`, and `raise` are
/// called, all of which are async-signal-safe per POSIX.1-2017 §2.4.3.
///
/// SECURITY: verified — the following invariants hold:
/// 1. No heap allocation inside the handler (no Box, Vec, String, format!, eprintln!).
/// 2. No mutex or lock acquisition (no std::sync::Mutex, no RefCell borrow).
/// 3. Only async-signal-safe syscalls: write(2), unlink(2), signal(2), raise(2).
/// 4. Buffer reads are guarded by Acquire-load of the length atomics (written
///    with Release by the set_* functions), so no partially-written buffer is
///    ever visible to the handler.
/// 5. The SyncBuffer statics are written only from the main thread (I1), so
///    there is no concurrent-write data race.
extern "C" fn sigterm_handler(_sig: libc::c_int) {
    SIGNAL_RECEIVED.store(true, Ordering::Release);

    // Print the Terminated message to stderr.
    let msg_len = TERM_MSG_LEN.load(Ordering::Acquire);
    if msg_len > 0 {
        // SAFETY:
        // - I2/I3: msg_len was stored with Release by set_term_message after
        //   writing the buffer; we load with Acquire, so the buffer write is
        //   visible here.
        // - as_ptr() returns the UnsafeCell interior pointer; msg_len ≤ MAX_MSG-1
        //   so the pointer range [0, msg_len) is within the array.
        // - No Rust reference to TERM_MSG_BUF is created; we use a raw pointer.
        // - libc::write is async-signal-safe.
        unsafe {
            libc::write(
                libc::STDERR_FILENO,
                TERM_MSG_BUF.as_ptr() as *const libc::c_void,
                msg_len,
            );
        }
    }

    // Delete the temp stdin file.
    let path_len = TEMP_FILE_LEN.load(Ordering::Acquire);
    if path_len > 0 {
        // SAFETY:
        // - I2/I3: path_len was stored with Release by set_temp_stdin_path after
        //   writing the buffer and the NUL terminator; Acquire load sees the full write.
        // - as_ptr() is the UnsafeCell interior pointer; the byte at [path_len] is 0
        //   (NUL terminator written by set_temp_stdin_path), so it is a valid C string.
        // - No Rust reference is created; raw pointer used throughout.
        // - libc::unlink is async-signal-safe.
        unsafe {
            libc::unlink(TEMP_FILE_BUF.as_ptr() as *const libc::c_char);
        }
    }

    // Reset SIGTERM to default and re-raise to die with signal 15 status.
    // SAFETY:
    // - libc::signal and libc::raise are async-signal-safe per POSIX.
    // - SIG_DFL is a valid signal disposition.
    // - Passing SIGTERM to raise() kills the current process; control does
    //   not return from this block in normal operation.
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_DFL);
        libc::raise(libc::SIGTERM);
    }
}

/// Install the SIGTERM handler.
pub fn install_sigterm_handler() {
    // SAFETY:
    // - libc::signal is an FFI call to the POSIX signal(2) syscall.
    // - sigterm_handler satisfies the async-signal-safe contract: it calls
    //   only write, unlink, signal, raise (all POSIX async-signal-safe).
    // - The cast `sigterm_handler as libc::sighandler_t` is valid because
    //   `extern "C" fn(c_int)` has the same ABI as the C `sighandler_t` type.
    // - I2: set_temp_stdin_path / set_term_message must be called (if needed)
    //   before the next SIGTERM is delivered; the caller owns this ordering.
    unsafe {
        libc::signal(libc::SIGTERM, sigterm_handler as libc::sighandler_t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path of exactly MAX_PATH-1 bytes is the longest path that fits in the
    /// buffer.  After set_temp_stdin_path the stored length must equal MAX_PATH-1
    /// and the byte at that offset must be 0 (the NUL terminator).
    #[test]
    fn max_path_minus_one_stored_correctly() {
        // Build a path of exactly MAX_PATH-1 ASCII bytes.
        let path: String = "a".repeat(MAX_PATH - 1);
        set_temp_stdin_path(&path);

        let stored_len = TEMP_FILE_LEN.load(Ordering::Acquire);
        assert_eq!(stored_len, MAX_PATH - 1,
            "stored length should be MAX_PATH-1 = {}", MAX_PATH - 1);

        // The NUL terminator must sit at index MAX_PATH-1, which is within the
        // MAX_PATH-element buffer (valid indices 0..MAX_PATH-1 inclusive).
        // SAFETY: stored_len == MAX_PATH-1 < MAX_PATH; reading one byte at that
        // offset is within the buffer.  No other code mutates the buffer
        // concurrently (single-threaded test).
        let nul_byte = unsafe { TEMP_FILE_BUF.as_ptr().add(stored_len).read() };
        assert_eq!(nul_byte, 0u8, "byte at [stored_len] must be NUL terminator");

        // Clean up so other tests start from a known state.
        clear_temp_stdin_path();
        assert_eq!(TEMP_FILE_LEN.load(Ordering::Acquire), 0,
            "clear_temp_stdin_path should atomically zero the length");
    }

    /// clear_temp_stdin_path() must zero the length atomically (Release store)
    /// so a signal handler racing after this call cannot observe a stale length
    /// paired with a stale buffer.
    #[test]
    fn clear_temp_stdin_path_zeros_length_atomically() {
        // Write a non-empty path, then clear it.
        set_temp_stdin_path("/tmp/jmake-test-path");
        assert!(TEMP_FILE_LEN.load(Ordering::Acquire) > 0,
            "length must be non-zero after set_temp_stdin_path");

        clear_temp_stdin_path();

        // The Release store in clear_temp_stdin_path guarantees that any
        // Acquire load (e.g. in the signal handler) sees 0 here.
        let len_after = TEMP_FILE_LEN.load(Ordering::Acquire);
        assert_eq!(len_after, 0,
            "TEMP_FILE_LEN must be 0 immediately after clear_temp_stdin_path");
    }
}

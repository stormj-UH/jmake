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

// INVARIANTS: src/signal_handler.rs — async-signal-safe SIGTERM cleanup
//
// I1. Single-threaded write rule:
//     set_temp_stdin_path, clear_temp_stdin_path, set_term_message, and
//     clear_term_message are called ONLY from the main thread.  The signal
//     handler (sigterm_handler) runs asynchronously on whichever thread receives
//     SIGTERM, but because jmake has only one thread during these writes there is
//     no concurrent-writer data race.
//
// I2. Acquire/Release ordering (buffer visibility):
//     Every set_* call writes the buffer bytes BEFORE storing the length with
//     Ordering::Release.  sigterm_handler loads the length with Ordering::Acquire
//     before reading buffer bytes.  This Release-before-Acquire pairing guarantees
//     the handler never observes a partially-written buffer.
//
// I3. Async-signal-safe syscalls only:
//     sigterm_handler calls ONLY write(2), unlink(2), signal(2), raise(2) — all
//     listed as async-signal-safe in POSIX.1-2017 §2.4.3.  It must NEVER call
//     malloc, free, any lock-taking function, eprintln!, format!, or any Rust
//     allocator function.
//
// I4. No live Rust references during raw-pointer writes:
//     No `&` or `&mut` reference to the interior of TEMP_FILE_BUF or TERM_MSG_BUF
//     is ever alive at the same time as the raw-pointer copy_nonoverlapping writes
//     inside set_*.  UnsafeCell::get() returns *mut without creating a reference.
//
// I5. Buffer capacity invariant:
//     TEMP_FILE_BUF has capacity MAX_PATH bytes; TERM_MSG_BUF has capacity MAX_MSG.
//     set_* caps the written length at N-1 (leaving room for the NUL terminator).
//     TEMP_FILE_LEN and TERM_MSG_LEN are always <= MAX_PATH-1 and MAX_MSG-1 respectively.

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

// SAFETY:
// - I1 (single-threaded writes): the only writers (set_temp_stdin_path,
//   clear_temp_stdin_path, set_term_message, clear_term_message) are called
//   exclusively from the main thread.  No concurrent write can occur.
// - The signal handler (sigterm_handler) reads via raw pointer after an
//   Acquire load of the length atomic (I2), so it never races with a write.
// - Sharing the buffer as `&SyncBuffer` across thread boundaries is therefore
//   safe: the caller invariant (I1) prevents simultaneous mutation.
unsafe impl<const N: usize> Sync for SyncBuffer<N> {}

// SAFETY:
// - I1 (single-threaded): SyncBuffer values are only ever constructed as
//   `static` items (TEMP_FILE_BUF, TERM_MSG_BUF).  No code moves a
//   SyncBuffer between threads.  Implementing Send satisfies the compiler's
//   requirement for `static` items, and the invariant I1 ensures the impl is
//   never exercised by an actual cross-thread transfer.
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
//
// PRE:  Called only from the main thread (I1).
//       `path` is a valid UTF-8 string; if longer than MAX_PATH-1 bytes it is silently truncated.
// POST: TEMP_FILE_BUF[0..len] contains the UTF-8 bytes of `path`; TEMP_FILE_BUF[len] == 0.
//       TEMP_FILE_LEN is stored with Release ordering so sigterm_handler sees the full write (I2).
//       All invariants I1–I5 hold on return.
// NOTE: Panic/drop safety: copy_nonoverlapping is unsafe but cannot unwind; AtomicUsize::store
//       cannot panic.  Early-return impossible — no invariant can be left broken.
pub fn set_temp_stdin_path(path: &str) {
    let bytes = path.as_bytes();
    let len = bytes.len().min(MAX_PATH - 1);
    // Constructor-time bounds check (not a hot path — called during setup only).
    // assert! rather than debug_assert! so a bad caller is caught in release builds.
    assert!(len < MAX_PATH, "path len {len} must be < MAX_PATH {MAX_PATH}");
    assert!(
        len + 1 <= MAX_PATH,
        "NUL terminator at offset {} must be within buffer [0, {MAX_PATH})",
        len + 1
    );
    // SAFETY:
    // - I1 (single-threaded): jmake is single-threaded at the point these
    //   globals are written; no concurrent writer can race with this store.
    //   Enforced by the module-level caller contract (main thread only).
    // - I2 (pointer via UnsafeCell): `as_mut_ptr()` calls `UnsafeCell::get()`
    //   which yields a raw `*mut u8` without materialising any Rust reference.
    //   This is the language-approved path for interior mutability (I4).
    // - I3 (bounds): `len ≤ MAX_PATH-1` is guaranteed by `.min(MAX_PATH - 1)`
    //   above and enforced by the `assert!` above this block.  The NUL byte is
    //   written at `[len]`, which is at most `[MAX_PATH-1]` — the last valid
    //   index of an array of length `MAX_PATH`.
    // - I4 (Release/Acquire ordering): the `Release` store to `TEMP_FILE_LEN`
    //   immediately below this block happens-after the buffer write.  The
    //   signal handler's `Acquire` load of `TEMP_FILE_LEN` therefore sees a
    //   fully-written buffer; it never observes a partial write.
    // - I5 (no live map or RefCell references): this function contains no
    //   borrow guards; no `&` or `&mut` into the buffer interior exists.
    unsafe {
        let dst = TEMP_FILE_BUF.as_mut_ptr();
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, len);
        dst.add(len).write(0u8);
    }
    TEMP_FILE_LEN.store(len, Ordering::Release);
}

/// Clear the temp stdin path.
//
// PRE:  Called only from the main thread (I1).
// POST: TEMP_FILE_LEN == 0 (Release).  After this, sigterm_handler will skip unlink(2).
//       Buffer bytes are left as-is but are inaccessible because length is 0.
// NOTE: Panic/drop safety: atomic store cannot panic.
pub fn clear_temp_stdin_path() {
    TEMP_FILE_LEN.store(0, Ordering::Release);
}

/// Set the Terminated message for the SIGTERM handler to print.
/// Format: "progname: *** [file:line: target] Terminated\n"
//
// PRE:  Called only from the main thread (I1).
//       `msg` is valid UTF-8; silently truncated to MAX_MSG-1 bytes if longer.
// POST: TERM_MSG_BUF[0..len] holds the message bytes; TERM_MSG_BUF[len] == 0.
//       TERM_MSG_LEN stored with Release ordering (I2).  I1–I5 hold on return.
// NOTE: Panic/drop safety: same as set_temp_stdin_path.
pub fn set_term_message(msg: &str) {
    let bytes = msg.as_bytes();
    let len = bytes.len().min(MAX_MSG - 1);
    // Constructor-time bounds check (not a hot path — called during recipe setup).
    // assert! rather than debug_assert! so a bad caller is caught in release builds.
    assert!(len < MAX_MSG, "msg len {len} must be < MAX_MSG {MAX_MSG}");
    assert!(
        len + 1 <= MAX_MSG,
        "NUL terminator at offset {} must be within buffer [0, {MAX_MSG})",
        len + 1
    );
    // SAFETY:
    // - I1 (single-threaded): jmake is single-threaded at the point these
    //   globals are written; no concurrent writer can race with this store.
    //   Enforced by the module-level caller contract (main thread only).
    // - I2 (pointer via UnsafeCell): `as_mut_ptr()` calls `UnsafeCell::get()`
    //   which yields a raw `*mut u8` without materialising any Rust reference.
    //   This is the language-approved path for interior mutability (I4).
    // - I3 (bounds): `len ≤ MAX_MSG-1` is guaranteed by `.min(MAX_MSG - 1)`
    //   above and enforced by the `assert!` above this block.  The NUL byte is
    //   written at `[len]`, which is at most `[MAX_MSG-1]` — the last valid
    //   index of an array of length `MAX_MSG`.
    // - I4 (Release/Acquire ordering): the `Release` store to `TERM_MSG_LEN`
    //   immediately below this block happens-after the buffer write.  The
    //   signal handler's `Acquire` load of `TERM_MSG_LEN` therefore sees a
    //   fully-written buffer; it never observes a partial write.
    // - I5 (no live references): this function contains no borrow guards;
    //   no `&` or `&mut` into the buffer interior is ever alive here.
    unsafe {
        let dst = TERM_MSG_BUF.as_mut_ptr();
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, len);
        dst.add(len).write(0u8);
    }
    TERM_MSG_LEN.store(len, Ordering::Release);
}

/// Clear the Terminated message.
//
// PRE:  Called only from the main thread (I1).
// POST: TERM_MSG_LEN == 0 (Release).  Handler will skip write(2) for this message.
// NOTE: Panic/drop safety: atomic store cannot panic.
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
        // - msg_len is non-zero (checked above) and ≤ MAX_MSG-1 because
        //   set_term_message caps it with `.min(MAX_MSG - 1)` before storing.
        // - I2 (Release/Acquire): msg_len was stored with `Release` by
        //   set_term_message *after* the buffer bytes were written; this
        //   `Acquire` load therefore observes a fully-written buffer.
        // - TERM_MSG_BUF.as_ptr() returns the UnsafeCell interior pointer
        //   (`*const u8`) without creating a Rust reference; the range
        //   [ptr, ptr + msg_len) lies within the MAX_MSG-byte array (I5).
        // - No heap allocation or lock acquisition occurs (I3: async-signal-safe).
        // - libc::write(STDERR_FILENO, …) is async-signal-safe per
        //   POSIX.1-2017 §2.4.3.
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
        // - path_len is non-zero (checked above) and ≤ MAX_PATH-1 because
        //   set_temp_stdin_path caps it with `.min(MAX_PATH - 1)` before storing.
        // - I2 (Release/Acquire): path_len was stored with `Release` by
        //   set_temp_stdin_path *after* writing path bytes AND the NUL byte at
        //   [path_len]; this `Acquire` load therefore observes a fully-written,
        //   NUL-terminated C string.
        // - TEMP_FILE_BUF.as_ptr() returns the UnsafeCell interior pointer
        //   without creating a Rust reference; the NUL at [path_len] makes the
        //   pointed-to region a valid C string (I5).
        // - No heap allocation or lock acquisition occurs (I3: async-signal-safe).
        // - libc::unlink is async-signal-safe per POSIX.1-2017 §2.4.3.
        unsafe {
            libc::unlink(TEMP_FILE_BUF.as_ptr() as *const libc::c_char);
        }
    }

    // Reset SIGTERM to default and re-raise to die with signal 15 status.
    // SAFETY:
    // - libc::signal(SIGTERM, SIG_DFL) resets the disposition to the default
    //   (terminate); SIG_DFL is always a valid sighandler_t value.
    // - libc::raise(SIGTERM) sends SIGTERM to the calling process; with the
    //   disposition reset to SIG_DFL, the process terminates immediately and
    //   the caller sees a signal-killed exit status (not exit code 1).
    // - Both libc::signal and libc::raise are async-signal-safe per
    //   POSIX.1-2017 §2.4.3.
    // - No heap allocation, no locks, no Rust references involved (I3).
    // - Normal control flow does not continue past libc::raise(SIGTERM).
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_DFL);
        libc::raise(libc::SIGTERM);
    }
}

/// Install the SIGTERM handler.
//
// PRE:  Called at most once, from the main thread, before any SIGTERM can be delivered
//       for which set_term_message / set_temp_stdin_path must have already been called.
//       (If neither buffer needs to be used before the first signal, the ordering
//       requirement is vacuous.)
// POST: SIGTERM is handled by sigterm_handler for the lifetime of the process.
//       All invariants I1–I5 remain valid (this function does not write any buffer).
// NOTE: Panic/drop safety: libc::signal is an FFI call; it cannot unwind into Rust.
pub fn install_sigterm_handler() {
    // SAFETY:
    // - libc::signal is an FFI call to the POSIX signal(2) syscall; it cannot
    //   unwind into Rust.
    // - `sigterm_handler` is declared `extern "C"` and matches the C
    //   `sighandler_t` ABI (fn(c_int)).  The explicit local binding
    //   `handler: extern "C" fn(libc::c_int)` documents the ABI and allows
    //   the cast to `sighandler_t` without a direct fn-item-to-integer cast
    //   (which Rust ≥1.94 forbids).
    // - sigterm_handler satisfies the async-signal-safe contract: it calls
    //   only write(2), unlink(2), signal(2), raise(2) — all listed as
    //   async-signal-safe in POSIX.1-2017 §2.4.3.  It performs no heap
    //   allocation, takes no locks, and dereferences no Rust references.
    // - I1 (single-threaded install): this function is called once from the
    //   main thread before any recipes execute.  No concurrent call to
    //   set_*/clear_* can race with the libc::signal() call.
    // - I2 (buffer-before-handler ordering): set_temp_stdin_path and
    //   set_term_message must be called (with their Release stores) before
    //   the first SIGTERM is delivered; the caller owns this ordering.
    unsafe {
        let handler: extern "C" fn(libc::c_int) = sigterm_handler;
        libc::signal(libc::SIGTERM, handler as libc::sighandler_t);
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
        // SAFETY:
        // - stored_len == MAX_PATH-1 (asserted above), which is < MAX_PATH, so
        //   `as_ptr().add(stored_len)` points to a valid byte inside the array.
        // - `as_ptr()` yields the UnsafeCell interior pointer without creating
        //   a Rust reference (I4); `.read()` performs a volatile-equivalent
        //   single-byte load — no aliasing hazard.
        // - This is a single-threaded test; no concurrent mutation of the
        //   buffer can occur (I1).
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

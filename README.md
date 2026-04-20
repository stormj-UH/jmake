# jmake

Clean-room drop-in replacement for [GNU Make](https://www.gnu.org/software/make/), written in Rust.

<!-- test-badge-start -->
**GNU Make 4.4.1 test suite: 1340 / 1340 (100.0%)**
<!-- test-badge-end -->

## Overview

jmake is a complete reimplementation of GNU Make built from scratch in safe Rust, **without referencing the GNU Make source code** (clean-room design). The 4.4.1 upstream test suite passes **1369 out of 1369 tests** as of 1.1.0, and jmake drives the real builds for [jonerix](https://github.com/stormj-UH/jonerix): Linux kernel modules, musl, LLVM, CPython, Node.js, hostapd, wpa_supplicant, dhcpcd, OpenRC, and ~80 other packages that predate Rust by decades.

## Features

- **Drop-in compatible** with GNU Make 4.4.1 — `ln -s jmake make` and walk away
- **Parallel builds** (`-j N`) with a thread-pool scheduler and a proper dependency graph (no `select()` loop, no `WNOHANG` polling)
- **Every user-facing GNU Make feature**: pattern rules, suffix rules, static-pattern rules, double-colon rules, grouped targets (`&:`), order-only prerequisites, target-specific variables, `.SECONDEXPANSION`, VPATH / vpath, `.ONESHELL`, `.POSIX`, `.DELETE_ON_ERROR`, `.NOTPARALLEL`, `.EXTRA_PREREQS`, private variables, `override`, `export` / `unexport`, `-include`, `sinclude`, `include`, recursive makes (`$(MAKE)` / `MAKELEVEL`), `MAKEFLAGS` / `GNUMAKEFLAGS` plumbing, makefile re-exec on touched prereqs, `--temp-stdin=` for piped stdin preservation across re-exec
- **Every built-in function**: `$(file)`, `$(let)`, `$(intcmp)`, `$(eval)`, `$(call)`, `$(foreach)`, `$(shell)`, `$(if)`, `$(and)`, `$(or)`, `$(error)`, `$(warning)`, `$(info)`, `$(value)`, `$(flavor)`, `$(abspath)`, `$(realpath)`, `$(wildcard)`, the `$(origin)` / `$(subst)` / `$(patsubst)` / `$(filter)` family, string-manipulation helpers, `$(guile)` stub (returns empty — no guile runtime)
- **Written in safe Rust** — 6 `unsafe` blocks total, all inside the POSIX signal handler where `async-signal-safe` requires raw libc
- **Single static binary** — no runtime dependencies beyond libc

## What's new in 1.1.0

1.1.0 closes the last known GNU-make-parity gaps and takes the upstream test suite from 7 / 8 on the one known-failing subtest (features/temp_stdin, baked into the old `MAX_ALLOWED_FAILS=1` gate) to **1369 / 1369**. Four fixes landed vs 1.0.14:

- **Accept TAB after directive keywords** (`include` / `-include` / `sinclude`). The old parser only accepted a space, so `include<TAB>path.mk` — a BSD-make-convention line that GNU make parses fine — errored with "missing separator". dhcpcd's `src/Makefile:12` is literally `include<TAB><TAB>${TOP}/iconfig.mk`, which is what first surfaced the bug.
- **Split `-c` option clusters into separate argv tokens**. POSIX.1-2017 defines `sh -c` as an option that takes `command_string` as its operand, so clustering `-c` with `-e` (the default `.SHELLFLAGS` under `.POSIX`) is strictly ambiguous. bash / dash / mksh are permissive; toybox sh rejects `-ec` with "Unknown option 'ec'". jmake now emits `-e -c` whenever `-c` would otherwise be clustered — equivalent everywhere, unblocks toybox.
- **Expand `.SHELLFLAGS` and `SHELL` before tokenizing**. Raw `.value.clone()` skipped Make variable expansion, so `$$` escapes survived into the child process. The upstream `targets/ONESHELL` subtest 10 (perl-as-SHELL with `my $$foo = "bar"` in .SHELLFLAGS) rejected the command with "scalar-dereference-in-my"; it now runs cleanly.
- **Narrow the bare-name exit-127 "Permission denied" path to `is_dir()`**. The previous `exists()` check mis-fired on regular files in cwd that weren't on PATH (upstream `features/errors` subtest 8 expected "Permission denied" for a directory, while `misc/general4` subtest 8 expected "No such file" for a regular file). The `is_dir()` discriminator resolves both.

The `.forgejo/workflows/build.yml` hard gate has been tightened from `MAX_ALLOWED_FAILS=1` to `MAX_ALLOWED_FAILS=0` to reflect the new baseline.

## Building

```sh
cargo build --release
```

The binary lands at `target/release/jmake`. Use it as a drop-in replacement for `make`:

```sh
ln -sf jmake /usr/local/bin/make
```

## Running the GNU Make test suite

```sh
curl -sfL https://ftp.gnu.org/gnu/make/make-4.4.1.tar.gz | tar xz
cd make-4.4.1/tests
printf '%%CONFIG_FLAGS = (AR=>"ar",CC=>"cc",CFLAGS=>"-O2",CPP=>"cc -E",CPPFLAGS=>"",LDFLAGS=>"",LIBS=>"",USE_SYSTEM_GLOB=>"no");\n1;\n' > config-flags.pm
mkdir -p work/features work/functions work/misc work/targets work/options work/variables work/vms
perl run_make_tests.pl -make /path/to/jmake
```

Expected: `1369 Tests in 129 Categories Complete ... No Failures :-)`

## License

MIT. Copyright (c) 2026 Jon-Erik G. Storm.

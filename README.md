# jmake

Clean-room drop-in replacement for [GNU Make](https://www.gnu.org/software/make/), written in Rust.

<!-- test-badge-start -->
**GNU Make 4.4.1 test suite: 1339 / 1340 (99.9%)**
<!-- test-badge-end -->

## Overview

jmake is a complete reimplementation of GNU Make built from scratch in safe Rust, without referencing the GNU Make source code (clean-room design). It passes 99%+ of the GNU Make 4.4.1 test suite and successfully builds real-world projects including Lua, zlib, pigz, nginx, hostapd, and wpa_supplicant.

## Features

- **Drop-in compatible** with GNU Make 4.4.1
- **Parallel builds** (`-j N`) with thread pool and dependency graph scheduling
- **All GNU Make features**: pattern rules, suffix rules, static pattern rules, double-colon rules, grouped targets (`&:`), order-only prerequisites, target-specific variables, second expansion (`.SECONDEXPANSION`), VPATH/vpath, `.ONESHELL`, `.POSIX`, and more
- **Built-in functions**: all standard GNU Make functions including `$(file)`, `$(let)`, `$(intcmp)`, `$(eval)`, `$(call)`, `$(foreach)`, `$(shell)`, etc.
- **Written in safe Rust** (only 6 `unsafe` blocks, all in signal handler for POSIX compliance)
- **Single static binary** with no runtime dependencies

## Building

```sh
cargo build --release
```

The binary is at `target/release/jmake`. It can be used as a drop-in replacement for `make`:

```sh
ln -sf jmake /usr/local/bin/make
```

## License

Copyright (c) 2026 Jon-Erik G. Storm. All rights reserved.

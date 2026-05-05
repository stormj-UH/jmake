# jmake

A clean-room drop-in replacement for GNU Make, written in Rust.

## Features
- Drop-in compatible with GNU Make 4.4.1 (target-specific vars, pattern rules, all built-in functions)
- 78 integration tests covering variable flavors, functions, pattern rules, conditionals, VPATH, special targets
- Single static binary (no libc dependency with musl)
- Parallel build support (-j)
- JMAKE_TEST_MODE=1 for byte-compatible output with GNU Make test suites

## Security
- Expansion depth limited to 1000 levels (prevents stack overflow from deep recursive variables)
- MAKEFLAGS -j parsing hardened against integer overflow
- Trust-boundary documentation on all shell exec, env propagation, and path-access code paths
- Signal handler verified: async-signal-safe syscalls only, no allocator calls

## Performance
- Built-in function table: zero-allocation lookup via OnceLock (built once, shared reference thereafter)
- File stat() memoization: each file stated at most once per build pass
- Hot expansion paths inlined: find_matching_close, patsubst_word, pattern_matches
- sort_unstable for $(sort) — no stability needed after dedup

## Building
```sh
cargo build --release
```

## Testing
```sh
cargo test
```

## License
MIT

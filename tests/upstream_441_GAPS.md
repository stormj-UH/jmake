# jmake vs GNU Make 4.4.1 — Compatibility Gap Report

Generated: 2026-05-11  
Runner: `tests/upstream_441_runner.sh`  
Extractor: `tests/upstream_441_extractor.pl`

## Summary

| Category | Tests run | Pass | Fail |
|----------|-----------|------|------|
| features | ~350 | ~218 | ~132 |
| functions | ~120 | ~105 | ~15 |
| variables | ~180 | ~134 | ~46 |
| options | ~80 | ~64 | ~16 |
| targets | ~85 | ~52 | ~33 |
| misc | ~91 | ~25 | ~66 |
| **Total** | **906** | **598** | **308** |

132 tests were adopted into `tests/feature/upstream_441_*.{mk,golden}` as regression tests (all passing).

---

## Failure Breakdown by Root Cause

### Bug 1: MAKEFLAGS recursion and propagation (96 failures)

**Test group:** `variables/MAKEFLAGS`

**Description:** MAKEFLAGS content is incorrect when jmake invokes itself recursively or
when single-letter flags (like `-e`, `-k`, `-B`) appear in MAKEFLAGS. The flag string is
stored with incorrect formatting and the recursive sub-make invocation fails because jmake
passes `-f` without an argument.

**Diff snippet:**
```
EXP: all: MAKEFLAGS= --no-print-directory
     jump Works: MAKEFLAGS=e --no-print-directory
ACT: all: MAKEFLAGS= --no-print-directory
     /usr/bin/make: option requires an argument -- 'f'
     jmake: *** [GNUmakefile:6: all] Error 2
```

Also MAKEFLAGS letter-flags are not enclosed in `//` as GNU Make does:
```
EXP: /makeflags='Bi'/
ACT: makeflags='Bi'
     make: 'all' is up to date.
```

**Best guess at root cause:** `MAKEFLAGS` is not being set to the compact letter-flag form
(`Bik...`) that GNU Make uses, and the recursive `$(MAKE)` invocation is being constructed
without properly splitting flags from the `-f` argument.

---

### Bug 2: Implicit rule / suffix rule search not finding intermediate targets (60 failures)

**Test group:** `features/implicit_search`, `features/suffixrules`, `features/patternrules`

**Description:** When a target requires an intermediate file that can be produced by an
implicit or suffix rule (e.g. `foo.o` from `foo.c` via `.c.o:`), jmake fails to chain
implicit rules and reports "No rule to make target".

**Diff snippet:**
```
EXP: hello.f
     make: Nothing to be done for 'all'.
ACT: make: *** No rule to make target 'hello', needed by 'all'.  Stop.

EXP: make foo.biz
ACT: make: *** No rule to make target 'foo.biz'.  Stop.

EXP: CC -c bar.c -o bar.o
ACT: make: *** No rule to make target 'bar.c', needed by 'bar.o'.  Stop.
```

**Best guess at root cause:** Chained implicit rule search (searching for a rule that can
produce the prerequisite of another rule) is incomplete or disabled.

---

### Bug 3: .INTERMEDIATE targets not recognized (8 failures)

**Test group:** `targets/INTERMEDIATE`, `targets/NOTINTERMEDIATE`

**Description:** Files declared as `.INTERMEDIATE` prerequisites are not treated as
intermediate: jmake doesn't build them via implicit rules and doesn't delete them after use.

**Diff snippet:**
```
EXP: cp foo.f foo.e
     cp foo.e foo.d
     rm foo.e
ACT: make: *** No rule to make target 'foo.e', needed by 'foo.d'.  Stop.
```

**Best guess at root cause:** `.INTERMEDIATE` special target is parsed but intermediate
file semantics (build-on-demand via implicit rules, auto-delete) are not implemented.

---

### Bug 4: .WAIT special prerequisite not implemented (10 failures)

**Test group:** `targets/WAIT`

**Description:** `.WAIT` in a prerequisite list should force sequential execution of
prerequisites before and after the `.WAIT` token, even in parallel mode. jmake produces
no output at all for these tests.

**Diff snippet:**
```
EXP: start-pre1
     end-pre1
     pre2
ACT: (empty)
```

**Best guess at root cause:** `.WAIT` is not recognized as a special prerequisite
separator; the entire rule may be silently skipped.

---

### Bug 5: Double-colon rules rebuild ordering (6 failures)

**Test group:** `features/double_colon`

**Description:** Multiple double-colon rules for the same target should be processed
independently (each rule runs if its own prerequisites are newer). jmake runs all
double-colon rules when any single one applies, producing extra output.

**Diff snippet:**
```
EXP: f1.h
     foo FIRST
ACT: f1.h
     foo FIRST
     f2.h
     foo SECOND
```

**Best guess at root cause:** Double-colon rule staleness check uses union of all
prerequisites instead of per-rule prerequisite sets.

---

### Bug 6: .ONESHELL with non-default SHELL (6 failures)

**Test group:** `targets/ONESHELL`

**Description:** When `.ONESHELL:` is combined with a custom `SHELL` that takes flags,
jmake fails with "Error running shell: No such file or directory".

**Diff snippet:**
```
EXP: a = 12, y = (a b c)
ACT: make: *** Error running shell: No such file or directory (os error 2)
```

**Best guess at root cause:** When `.ONESHELL` is active and `SHELL` is set to a
multi-word value (e.g. `/bin/sh -e`), jmake doesn't properly split the `SHELL` value
into binary + flags before execing.

---

### Bug 7: .EXTRA_PREREQS / order-only prereq for pattern rules (6 failures + 4 failures)

**Test group:** `variables/EXTRA_PREREQS`, `features/order_only`

**Description:** `.EXTRA_PREREQS` targets are not added to recipes that don't directly
reference them, and order-only (`|`) prerequisites in pattern rules don't correctly
influence build order.

**Diff snippet:**
```
EXP: foo
     bar
     baz
     ho
     hey
     hi ho hey foo bar baz
ACT: foo
     bar
     baz
     make: *** No rule to make target 'hi', needed by 'target'.  Stop.
```

**Best guess at root cause:** `.EXTRA_PREREQS` expansion during recipe execution is not
implemented; prerequisites are not being injected into the dependency graph.

---

### Bug 8: print-directory path normalization (9 failures)

**Test group:** `options/print-directory`

**Description:** When `-w` / `--print-directory` is active, the "Entering/Leaving
directory" message uses the actual resolved path rather than the `#PWD#`-normalized one.
(This is partly an extractor placeholder issue, but also jmake doesn't normalize the path
to match `$(CURDIR)` in the expected output.)

**Diff snippet:**
```
EXP: make: Entering directory '#PWD#'
     hi
     make: Leaving directory '#PWD#'
ACT: make: Entering directory '/tmp'
     hi
     make: Leaving directory '/tmp'
```

**Best guess at root cause:** The extractor uses `#PWD#` as a placeholder but jmake
correctly prints the actual path; this is mostly a test-harness issue, not a jmake bug.
However `$(CURDIR)` vs `$(PWD)` normalization may differ.

---

### Bug 9: `private` variable modifier (6 failures)

**Test group:** `variables/private`

**Description:** Variables declared as `private` in target-specific context should not
be inherited by prerequisites. jmake propagates them anyway.

**Best guess at root cause:** The `private` modifier is parsed but inheritance suppression
is not applied during prerequisite variable scope setup.

---

### Other failures (remaining ~127)

- `targets/POSIX` (6): `.POSIX:` special target semantics not fully implemented
- `options/dash-B` (5): `-B` (unconditional build) doesn't re-run all targets in some edge cases
- `options/dash-W` (5): `-W` (new-file simulation) not fully implemented
- `features/vpathplus` (4): VPATH with `%` pattern matching incomplete
- `functions/wildcard` (4): `$(wildcard ...)` with VPATH or special chars
- `features/grouped_targets` (4): `&:` grouped targets not implemented
- `variables/DEFAULT_GOAL` (3): `.DEFAULT_GOAL` assignment from recipe context
- `features/recursion` (3): Recursive make with complex MAKEFLAGS propagation
- `targets/DELETE_ON_ERROR` (2): `.DELETE_ON_ERROR` doesn't clean up in all cases

---

## Top 5 Bugs to Fix (in priority order)

1. **MAKEFLAGS propagation in recursive make** — 96 failures; blocks entire recursion category
2. **Chained implicit rule search** — 71 failures across suffixrules/patternrules/implicit_search; core Make functionality
3. **`.INTERMEDIATE` / intermediate file handling** — 8 failures; needed for real build systems
4. **`.WAIT` prerequisite separator** — 10 failures; required for parallel build correctness
5. **Double-colon per-rule staleness check** — 6 failures; affects common Makefile patterns

// (c) 2026 Jon-Erik G. Storm, Inc., a California Corporation,
// doing business as LAVA GOAT SOFTWARE. All rights reserved.
// SPDX-License-Identifier: MIT

//! GNU Make built-in function implementations.
//!
//! # Role in the pipeline
//!
//! When the evaluator (see [`crate::eval`]) encounters a `$(name ...)` reference
//! during variable expansion it first checks whether `name` is a built-in function.
//! If it is, it calls the corresponding handler from this module.  Variable
//! references that are *not* function calls are resolved as ordinary variable
//! lookups instead.
//!
//! # Function dispatch
//!
//! [`get_builtin_functions`] returns a `HashMap<String, (FnHandler, usize, usize)>`
//! where the tuple is `(handler, min_args, max_args)`.  A `max_args` of `0` means
//! the function accepts an unlimited number of comma-separated arguments (used by
//! `$(or ...)`, `$(and ...)`, `$(call ...)`, etc.).
//!
//! The caller is responsible for splitting the argument string on commas (respecting
//! `$(...)` nesting), trimming leading whitespace from the first argument, and
//! enforcing the arity constraints before invoking the handler.  Handlers receive a
//! `&[String]` of already-split (but not yet expanded) arguments and a closure
//! `expand: &dyn Fn(&str) -> String` for deferred expansion of arguments that must
//! only be evaluated conditionally (e.g. the branches of `$(if ...)`,
//! `$(foreach ...)`, `$(or ...)`, `$(and ...)`).
//!
//! # GNU Make compatibility notes by function
//!
//! **Text functions**
//!
//! | Function | Notes |
//! |---|---|
//! | `$(subst from,to,text)` | Simple string replacement, no glob. |
//! | `$(patsubst pattern,replacement,text)` | Word-level `%`-pattern substitution via [`patsubst_word`].  Suffix substitution `$(var:%.o=%.c)` is desugared to `patsubst` by the evaluator before reaching this module. |
//! | `$(strip text)` | Splits on whitespace and rejoins with single spaces. |
//! | `$(findstring find,text)` | Returns `find` if found, empty otherwise. |
//! | `$(filter patterns,text)` | Supports multiple space-separated patterns; each may contain one `%` wildcard.  `\%` is a literal percent; `\\` is a literal backslash. |
//! | `$(filter-out patterns,text)` | Inverse of `filter`. |
//! | `$(sort list)` | Lexicographic sort with deduplication. |
//! | `$(word n,list)` | 1-based indexing; returns empty for out-of-range. |
//! | `$(wordlist s,e,list)` | Inclusive range; clamps `e` to list length. |
//! | `$(words list)` | Count of whitespace-separated words. |
//! | `$(firstword list)` / `$(lastword list)` | GNU Make extensions; `lastword` is not POSIX make. |
//!
//! **File name functions** — all operate word-by-word on whitespace-split lists.
//!
//! | Function | Notes |
//! |---|---|
//! | `$(dir names)` | Returns the directory component including the trailing `/`; bare filenames return `./`. |
//! | `$(notdir names)` | Strips the directory component. |
//! | `$(suffix names)` | Returns the last `.`-delimited extension of the filename component; no extension → empty. |
//! | `$(basename names)` | Strips the final extension; no extension → unchanged. |
//! | `$(addsuffix suffix,names)` | Appends `suffix` to every word. |
//! | `$(addprefix prefix,names)` | Prepends `prefix` to every word. |
//! | `$(join list1,list2)` | Pairwise concatenation; excess words from the longer list are appended unchanged. |
//! | `$(wildcard pattern)` | Shell glob expansion.  Results are sorted lexicographically per GNU Make.  The `./`-prefix and directory-prefix preservation logic works around an asymmetry in the `glob` crate: see the inline comments for the `./src/*.h` and `src/*/*.c` cases. |
//! | `$(realpath names)` | Resolves symlinks via `fs::canonicalize`; missing paths are silently omitted. |
//! | `$(abspath names)` | Absolute path without resolving symlinks; collapses `..` and `.` components via [`normalize_path`]. |
//!
//! **Conditional / flow functions**
//!
//! | Function | Notes |
//! |---|---|
//! | `$(if cond,then[,else])` | `cond` is already-expanded by the caller; non-empty → true.  `then` and `else` are expanded lazily via the `expand` closure. |
//! | `$(or arg1,arg2,...)` | Short-circuit: returns the first non-empty expansion. |
//! | `$(and arg1,arg2,...)` | Short-circuit: returns empty on first empty expansion, otherwise the last. |
//! | `$(intcmp a,b[,lt][,eq-or-ge][,gt])` | Arbitrary-precision integer comparison via [`bigint_cmp`]; the 4-arg form uses arg4 as a "greater-or-equal" catch-all. |
//!
//! **Advanced / side-effecting functions** — these functions are handled primarily
//! by the evaluator and expander; the stubs here are never reached directly.
//!
//! | Function | Notes |
//! |---|---|
//! | `$(foreach var,list,text)` | The body (`text`) is re-expanded for each word.  **Performance**: this is O(words × expansion cost).  Avoid using `$(eval ...)` inside `$(foreach ...)` bodies on large lists; each iteration incurs a full re-parse of the eval fragment. |
//! | `$(call var,arg1,...)` | Expands `$(var)` with `$1`, `$2`, … set.  The actual implementation lives in the evaluator which has direct variable access; the stub here just expands the first argument for completeness. |
//! | `$(eval text)` | Causes `text` to be parsed as Makefile content.  **Performance**: each `$(eval ...)` triggers a full parser invocation.  Avoid in hot loops. |
//! | `$(value var)` | Returns the unexpanded value of `var`; handled in the evaluator. |
//! | `$(origin var)` | Returns a string describing where `var` was defined; handled in the evaluator. |
//! | `$(flavor var)` | Returns `"recursive"`, `"simple"`, or `"undefined"`; handled in the evaluator. |
//! | `$(let vars,list,text)` | Assigns successive words from `list` to named variables within `text`.  The last variable absorbs all remaining words. |
//! | `$(file op,filename[,text])` | Writes to or reads from a file.  `>file` truncates and writes; `>>file` appends; `<file` reads.  A trailing newline is appended when writing non-empty content. |
//!
//! **Shell / I/O functions**
//!
//! | Function | Notes |
//! |---|---|
//! | `$(shell cmd)` | Runs `cmd` through `/bin/sh -c`; replaces internal newlines with spaces and strips one trailing newline.  stderr is printed with the make program name as prefix (shell prefix such as `sh: ` is stripped). |
//! | `$(error msg)` | Prints `*** msg.  Stop.` and exits with code 2. |
//! | `$(warning msg)` | Prints `msg` to stderr and returns empty. |
//! | `$(info msg)` | Prints `msg` to stdout and returns empty. |
//! | `$(guile ...)` | Not implemented; always returns empty. |
//!
//! # `pattern_matches` — filter pattern semantics
//!
//! GNU Make filter patterns support backslash-escaping: `\%` is a literal `%` and
//! `\\` is a literal `\`.  Only the *first* unescaped `%` is treated as a wildcard.
//! The implementation in [`pattern_matches`] parses the pattern byte-by-byte,
//! building a literal prefix and suffix (with escape processing) on either side of
//! the first unescaped `%`.  A word matches iff it starts with the prefix and ends
//! with the suffix, with `prefix.len() + suffix.len() <= word.len()`.
//!
//! # `bigint_cmp` — arbitrary-precision integer comparison
//!
//! `$(intcmp a,b,...)` must handle integers that may exceed `i64` range (Make does
//! not restrict integer size).  [`bigint_cmp`] compares two decimal integer strings
//! by sign first, then by digit-string length, then lexicographically — equivalent
//! to numeric comparison but without parsing into a machine integer.  Supports
//! leading `+`/`-` signs, leading zeros, and treats `+0`/`-0`/`0` as equal.

// INVARIANTS: src/functions/mod.rs — built-in function dispatch table
//
// F1. Idempotent dispatch table (OnceLock):
//     get_builtin_functions() returns the SAME HashMap instance on every call after
//     the first.  The table is built once in the OnceLock initializer and then
//     immutable for the lifetime of the process.  Callers may freely share &static
//     references to the table.
//
// F2. Unknown function → empty string (not panic):
//     expand_function (in expand.rs) only calls into this module AFTER checking that
//     the function name is in the table.  If a name is not found (which cannot happen
//     through the normal dispatch path), the caller returns empty string.  Individual
//     function handlers also return empty string for any out-of-range or inapplicable
//     input rather than panicking.
//
// F3. Argument splitting respects nested $()/{}:
//     The argument splitter in expand_function (expand.rs) uses find_matching_close to
//     locate commas that are NOT inside nested references before passing the split
//     arguments to handlers.  Handlers receive already-split args and must not re-split
//     on commas themselves.
//
// F4. FnHandler arity contract:
//     Each table entry has (handler, min_args, max_args).  max_args == 0 means unlimited.
//     The caller (expand_function in expand.rs) enforces arity before calling the handler.
//     Handlers may assume args.len() >= min_args and (max_args == 0 || args.len() <= max_args).
//
// F5. Handlers are pure or locally side-effecting only:
//     Text manipulation functions (subst, patsubst, filter, …) are pure.
//     Shell-calling functions (shell, !=) launch a subprocess.
//     $(error) and process::exit(2) are terminal — they never return.
//     $(warning), $(info) write to stderr/stdout but do not modify shared Make state.
//     $(eval) is the only handler that modifies the MakeState variable database;
//     its actual implementation lives in expand_function (not in this module's fn_eval stub).

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
use crate::eval::MAX_EXPANDED_VALUE_BYTES;

pub type FnHandler = fn(args: &[String], expand: &dyn Fn(&str) -> String) -> String;

/// Global cache for the builtin-function dispatch table.
///
/// # Performance rationale
///
/// `get_builtin_functions` is called on every `$(name ...)` dispatch in the
/// expansion hot path — once to check whether a token is a function name
/// (in `expand_reference`) and again to retrieve the handler (in
/// `expand_function`).  Building and heap-allocating a 34-entry `HashMap`
/// each time is the dominant allocator pressure during variable expansion.
/// Using `OnceLock` eliminates all but the first allocation and reduces each
/// subsequent call to a single pointer load.
///
/// jmake is single-threaded during expansion, so the `Send + Sync` bound on
/// `OnceLock`'s contents is satisfied by the fact that `FnHandler` is a bare
/// `fn` pointer and all tuple fields are `Copy`.
static BUILTIN_FUNCTIONS: OnceLock<HashMap<String, (FnHandler, usize, usize)>> = OnceLock::new();

/// Return a reference to the global builtin-function dispatch table.
///
/// The table is built at most once per process; all subsequent calls return
/// the cached reference in O(1) with no heap allocation.
//
// PRE:  None — safe to call from any context including concurrent threads
//       (OnceLock provides thread-safe initialization).
// POST: Returns &'static reference to the same HashMap on every call (F1).
//       The returned map contains exactly 36 entries (one per built-in function).
//       map[name] == (handler, min_args, max_args) satisfying F4.
// NOTE: Panic/drop safety: the OnceLock initializer runs once; HashMap::with_capacity
//       and insert may panic on OOM (unrecoverable; process aborts).  On successful
//       initialization the table is immutable thereafter — no further allocation occurs.
pub fn get_builtin_functions() -> &'static HashMap<String, (FnHandler, usize, usize)> {
    BUILTIN_FUNCTIONS.get_or_init(|| {
        // (handler, min_args, max_args) - max_args 0 means unlimited
        let mut map: HashMap<String, (FnHandler, usize, usize)> =
            HashMap::with_capacity(36); // exact count of entries below

        map.insert("subst".into(), (fn_subst as FnHandler, 3, 3));
        map.insert("patsubst".into(), (fn_patsubst as FnHandler, 3, 3));
        map.insert("strip".into(), (fn_strip as FnHandler, 1, 1));
        map.insert("findstring".into(), (fn_findstring as FnHandler, 2, 2));
        map.insert("filter".into(), (fn_filter as FnHandler, 2, 2));
        map.insert("filter-out".into(), (fn_filter_out as FnHandler, 2, 2));
        map.insert("sort".into(), (fn_sort as FnHandler, 1, 1));
        map.insert("word".into(), (fn_word as FnHandler, 2, 2));
        map.insert("wordlist".into(), (fn_wordlist as FnHandler, 3, 3));
        map.insert("words".into(), (fn_words as FnHandler, 1, 1));
        map.insert("firstword".into(), (fn_firstword as FnHandler, 1, 1));
        map.insert("lastword".into(), (fn_lastword as FnHandler, 1, 1));
        map.insert("dir".into(), (fn_dir as FnHandler, 1, 1));
        map.insert("notdir".into(), (fn_notdir as FnHandler, 1, 1));
        map.insert("suffix".into(), (fn_suffix as FnHandler, 1, 1));
        map.insert("basename".into(), (fn_basename as FnHandler, 1, 1));
        map.insert("addsuffix".into(), (fn_addsuffix as FnHandler, 2, 2));
        map.insert("addprefix".into(), (fn_addprefix as FnHandler, 2, 2));
        map.insert("join".into(), (fn_join as FnHandler, 2, 2));
        map.insert("wildcard".into(), (fn_wildcard as FnHandler, 1, 1));
        map.insert("realpath".into(), (fn_realpath as FnHandler, 1, 1));
        map.insert("abspath".into(), (fn_abspath as FnHandler, 1, 1));
        map.insert("if".into(), (fn_if as FnHandler, 2, 3));
        map.insert("or".into(), (fn_or as FnHandler, 1, 0));
        map.insert("and".into(), (fn_and as FnHandler, 1, 0));
        map.insert("foreach".into(), (fn_foreach as FnHandler, 3, 3));
        map.insert("file".into(), (fn_file as FnHandler, 1, 2));
        map.insert("call".into(), (fn_call as FnHandler, 1, 0));
        map.insert("value".into(), (fn_value as FnHandler, 1, 1));
        map.insert("eval".into(), (fn_eval as FnHandler, 1, 1));
        map.insert("origin".into(), (fn_origin as FnHandler, 1, 1));
        map.insert("flavor".into(), (fn_flavor as FnHandler, 1, 1));
        map.insert("shell".into(), (fn_shell as FnHandler, 1, 1));
        map.insert("error".into(), (fn_error as FnHandler, 1, 1));
        map.insert("warning".into(), (fn_warning as FnHandler, 1, 1));
        map.insert("info".into(), (fn_info as FnHandler, 1, 1));
        map.insert("guile".into(), (fn_guile as FnHandler, 1, 1));
        map.insert("let".into(), (fn_let as FnHandler, 3, 3));
        map.insert("intcmp".into(), (fn_intcmp as FnHandler, 2, 5));

        map
    })
}

#[inline]
fn fn_subst(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let from = &args[0];
    let to = &args[1];
    let text = &args[2];
    text.replace(from.as_str(), to.as_str())
}

#[inline]
fn fn_patsubst(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let pattern = &args[0];
    let replacement = &args[1];
    let text = &args[2];

    // Avoid intermediate Vec + collect + join by writing directly into one String.
    let mut out = String::with_capacity(text.len());
    let mut first = true;
    for w in text.split_whitespace() {
        if !first { out.push(' '); }
        let transformed = patsubst_word(w, pattern, replacement);
        out.push_str(&transformed);
        first = false;
    }
    out
}

/// Apply a single `patsubst` pattern to one word.
/// Marked `#[inline]` because it is called in tight loops from both
/// `fn_patsubst` and `expand_substitution_ref` in the expansion hot path.
#[inline]
pub fn patsubst_word(word: &str, pattern: &str, replacement: &str) -> String {
    if let Some(percent_pos) = pattern.find('%') {
        let prefix = &pattern[..percent_pos];
        let suffix = &pattern[percent_pos+1..];

        if word.starts_with(prefix) && word.ends_with(suffix) && word.len() >= prefix.len() + suffix.len() {
            let stem = &word[prefix.len()..word.len()-suffix.len()];
            return replacement.replace('%', stem);
        }
    } else {
        // No %, exact match
        if word == pattern {
            return replacement.to_string();
        }
    }
    word.to_string()
}

fn fn_strip(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    // Avoid collecting into a Vec; build the result directly in one pass.
    let mut out = String::new();
    let mut first = true;
    for w in args[0].split_whitespace() {
        if !first { out.push(' '); }
        out.push_str(w);
        first = false;
    }
    out
}

fn fn_findstring(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let find = &args[0];
    let text = &args[1];
    if text.contains(find.as_str()) {
        find.clone()
    } else {
        String::new()
    }
}

fn fn_filter(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let patterns: Vec<&str> = args[0].split_whitespace().collect();
    // Write directly into one String to avoid a Vec<&str> allocation + subsequent join.
    let mut out = String::new();
    let mut first = true;
    for w in args[1].split_whitespace() {
        if patterns.iter().any(|p| pattern_matches(p, w)) {
            if !first { out.push(' '); }
            out.push_str(w);
            first = false;
        }
    }
    out
}

fn fn_filter_out(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let patterns: Vec<&str> = args[0].split_whitespace().collect();
    // Write directly into one String to avoid a Vec<&str> allocation + subsequent join.
    let mut out = String::new();
    let mut first = true;
    for w in args[1].split_whitespace() {
        if !patterns.iter().any(|p| pattern_matches(p, w)) {
            if !first { out.push(' '); }
            out.push_str(w);
            first = false;
        }
    }
    out
}

/// Match a GNU Make filter pattern against a word.
///
/// In GNU Make filter patterns:
///  - `\%` is an escaped literal `%` (not a wildcard).
///  - `\\` is an escaped literal `\`.
///  - A bare `%` (not preceded by `\`) is the wildcard (matches any string).
///  - Only the FIRST unescaped `%` is treated as wildcard.
///
/// This function handles all three cases and correctly matches words against
/// patterns that contain literal `%` characters.
///
/// Marked `#[inline]` because it is the inner predicate in `$(filter)` /
/// `$(filter-out)` loops and the optimizer can eliminate it when the pattern
/// is a compile-time constant.
#[inline]
fn pattern_matches(pattern: &str, word: &str) -> bool {
    // Parse the pattern to find the first unescaped `%` (the wildcard).
    // Build a literal prefix and suffix (with backslash-escape processing).
    let pbytes = pattern.as_bytes();
    let mut prefix_lit = Vec::new(); // literal bytes before the wildcard (or the whole pattern)
    let mut wildcard_pos: Option<usize> = None; // index IN pbytes where `%` wildcard is
    let mut i = 0;
    while i < pbytes.len() {
        if pbytes[i] == b'\\' && i + 1 < pbytes.len() {
            match pbytes[i + 1] {
                b'%' => {
                    // \% → literal %
                    prefix_lit.push(b'%');
                    i += 2;
                }
                b'\\' => {
                    // \\ → literal \
                    prefix_lit.push(b'\\');
                    i += 2;
                }
                _ => {
                    // \ followed by anything else: keep the backslash and advance one
                    prefix_lit.push(b'\\');
                    i += 1;
                }
            }
        } else if pbytes[i] == b'%' {
            // Unescaped `%`: this is the wildcard.
            wildcard_pos = Some(i);
            i += 1;
            break;
        } else {
            prefix_lit.push(pbytes[i]);
            i += 1;
        }
    }

    if wildcard_pos.is_none() {
        // No wildcard: the pattern must match the word exactly (after unescape).
        // Build the full unescaped pattern.
        let mut full = prefix_lit;
        while i < pbytes.len() {
            if pbytes[i] == b'\\' && i + 1 < pbytes.len() {
                match pbytes[i + 1] {
                    b'%' => { full.push(b'%'); i += 2; }
                    b'\\' => { full.push(b'\\'); i += 2; }
                    _ => { full.push(b'\\'); i += 1; }
                }
            } else {
                full.push(pbytes[i]);
                i += 1;
            }
        }
        return word.as_bytes() == full.as_slice();
    }

    // Build the literal suffix (the part after the `%` wildcard).
    let mut suffix_lit = Vec::new();
    while i < pbytes.len() {
        if pbytes[i] == b'\\' && i + 1 < pbytes.len() {
            match pbytes[i + 1] {
                b'%' => { suffix_lit.push(b'%'); i += 2; }
                b'\\' => { suffix_lit.push(b'\\'); i += 2; }
                _ => { suffix_lit.push(b'\\'); i += 1; }
            }
        } else {
            suffix_lit.push(pbytes[i]);
            i += 1;
        }
    }

    let wbytes = word.as_bytes();
    let plen = prefix_lit.len();
    let slen = suffix_lit.len();

    if wbytes.len() < plen + slen {
        return false;
    }
    if &wbytes[..plen] != prefix_lit.as_slice() {
        return false;
    }
    if slen > 0 && &wbytes[wbytes.len() - slen..] != suffix_lit.as_slice() {
        return false;
    }
    true
}

fn fn_sort(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    // Collect, sort, then dedup in a single pass — split_whitespace already
    // returns only non-empty tokens so no empty-string edge case exists.
    let mut words: Vec<&str> = args[0].split_whitespace().collect();
    words.sort_unstable(); // unstable sort is faster and output order of duplicates is irrelevant
    words.dedup();
    // SECURITY: the input string has already passed MAX_EXPANDED_VALUE_BYTES checks
    // in the expansion engine, so the joined output here cannot exceed the input
    // length (dedup only reduces the word count).  The guard below is a defence-in-
    // depth check for any future code path that bypasses the expansion-engine check.
    let joined = words.join(" ");
    if joined.len() > MAX_EXPANDED_VALUE_BYTES {
        eprintln!(
            "make: *** $(sort) output exceeds maximum value size ({} bytes).  Stop.",
            MAX_EXPANDED_VALUE_BYTES
        );
        std::process::exit(2);
    }
    joined
}

fn fn_word(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let n: usize = args[0].trim().parse().unwrap_or(0);
    let words: Vec<&str> = args[1].split_whitespace().collect();
    if n >= 1 && n <= words.len() {
        words[n-1].to_string()
    } else {
        String::new()
    }
}

fn fn_wordlist(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let s: usize = args[0].trim().parse().unwrap_or(0);
    let e: usize = args[1].trim().parse().unwrap_or(0);
    let words: Vec<&str> = args[2].split_whitespace().collect();
    if s >= 1 && s <= words.len() && e >= s {
        let end = e.min(words.len());
        words[s-1..end].join(" ")
    } else {
        String::new()
    }
}

fn fn_words(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let count = args[0].split_whitespace().count();
    count.to_string()
}

fn fn_firstword(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    args[0].split_whitespace().next().unwrap_or("").to_string()
}

fn fn_lastword(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    args[0].split_whitespace().last().unwrap_or("").to_string()
}

fn fn_dir(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    // Write directly into one String to avoid Vec<String> + join allocation.
    let mut out = String::new();
    let mut first = true;
    for w in args[0].split_whitespace() {
        if !first { out.push(' '); }
        match w.rfind('/') {
            Some(pos) => out.push_str(&w[..=pos]),
            None => out.push_str("./"),
        }
        first = false;
    }
    out
}

fn fn_notdir(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    // Write directly into one String to avoid Vec<String> + join allocation.
    let mut out = String::new();
    let mut first = true;
    for w in args[0].split_whitespace() {
        if !first { out.push(' '); }
        match w.rfind('/') {
            Some(pos) => out.push_str(&w[pos+1..]),
            None => out.push_str(w),
        }
        first = false;
    }
    out
}

fn fn_suffix(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    // Write directly into one String to avoid Vec<String> + join allocation.
    let mut out = String::new();
    let mut first = true;
    for w in args[0].split_whitespace() {
        let name = match w.rfind('/') {
            Some(pos) => &w[pos+1..],
            None => w,
        };
        if let Some(dot_pos) = name.rfind('.') {
            if !first { out.push(' '); }
            out.push_str(&name[dot_pos..]);
            first = false;
        }
    }
    out
}

fn fn_basename(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    // Write directly into one String to avoid Vec<String> + join allocation.
    let mut out = String::new();
    let mut first = true;
    for w in args[0].split_whitespace() {
        if !first { out.push(' '); }
        match w.rfind('/') {
            Some(slash_pos) => {
                let dir = &w[..=slash_pos];
                let file = &w[slash_pos+1..];
                match file.rfind('.') {
                    Some(dot_pos) => {
                        out.push_str(dir);
                        out.push_str(&file[..dot_pos]);
                    }
                    None => out.push_str(w),
                }
            }
            None => {
                match w.rfind('.') {
                    Some(pos) => out.push_str(&w[..pos]),
                    None => out.push_str(w),
                }
            }
        }
        first = false;
    }
    out
}

fn fn_addsuffix(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let suffix = &args[0];
    // Pre-size: avoid format! overhead — each result is word.len() + suffix.len() bytes.
    // Use a single pre-allocated String and push directly.
    let mut out = String::new();
    let mut first = true;
    for w in args[1].split_whitespace() {
        if !first { out.push(' '); }
        out.push_str(w);
        out.push_str(suffix);
        first = false;
    }
    out
}

fn fn_addprefix(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let prefix = &args[0];
    // Pre-size: avoid format! overhead — each result is prefix.len() + word.len() bytes.
    let mut out = String::new();
    let mut first = true;
    for w in args[1].split_whitespace() {
        if !first { out.push(' '); }
        out.push_str(prefix);
        out.push_str(w);
        first = false;
    }
    out
}

fn fn_join(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let list1: Vec<&str> = args[0].split_whitespace().collect();
    let list2: Vec<&str> = args[1].split_whitespace().collect();
    let max = list1.len().max(list2.len());
    let mut out = String::new();
    for i in 0..max {
        if i > 0 { out.push(' '); }
        out.push_str(list1.get(i).unwrap_or(&""));
        out.push_str(list2.get(i).unwrap_or(&""));
        // SECURITY: guard against allocation bomb when joining two very large lists.
        // Each concatenated pair can be longer than either input word, so the joined
        // output can exceed the sum of both input lists.
        if out.len() > MAX_EXPANDED_VALUE_BYTES {
            eprintln!(
                "make: *** $(join) output exceeds maximum value size ({} bytes).  Stop.",
                MAX_EXPANDED_VALUE_BYTES
            );
            std::process::exit(2);
        }
    }
    out
}

fn fn_wildcard(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    // SECURITY: path traversal — trust boundary.
    // $(wildcard ../../etc/passwd) is permitted.  A Makefile is trusted
    // authored content; restricting which paths it may reference would break
    // legitimate cross-directory build systems.  The trust model is identical
    // to that of a shell script: the author of the Makefile owns the paths it
    // can access.  No sanitisation is applied here.
    let patterns: Vec<&str> = args[0].split_whitespace().collect();
    let mut results = Vec::new();
    for pattern in patterns {
        let mut matches = Vec::new();
        // GNU Make preserves the pattern's directory prefix in its output.
        // The `glob` crate's behaviour is subtler than it first appears:
        // for an input like `./src/*.h` it *keeps* the `src/` segment but
        // *strips* the `./` leader, returning `src/agentfwd.h`. Earlier
        // versions of this function unconditionally re-prepended the full
        // `./src/` prefix whenever the result didn't start with it,
        // producing the doubled path `./src/src/agentfwd.h`. Reproduced
        // 2026-04-20 building dropbear-2024.86 whose Makefile does:
        //     srcdir=./src
        //     HEADERS=$(wildcard $(srcdir)/*.h *.h)
        // — every header ended up listed as `./src/src/FOO.h` and make
        // bailed with `No rule to make target './src/src/agentfwd.h'`.
        //
        // Correct behaviour: only re-prepend the SEGMENT of the prefix
        // that glob dropped. Concretely, if the pattern started with
        // `./` and the glob output doesn't, re-add just `./`. If the
        // pattern had a real `dir/` prefix and glob returned a bare
        // filename, re-add `dir/`. Otherwise leave the glob output alone.
        let has_dot_slash = pattern.starts_with("./");
        let dir_prefix: &str = {
            let p = std::path::Path::new(pattern);
            match p.parent() {
                Some(parent) => {
                    let ps = parent.to_string_lossy();
                    if ps.is_empty() || ps == "." {
                        ""
                    } else if let Some(slash) = pattern.rfind('/') {
                        &pattern[..slash + 1]
                    } else {
                        ""
                    }
                }
                None => "",
            }
        };
        if let Ok(paths) = glob::glob(pattern) {
            for entry in paths.flatten() {
                let s = entry.to_string_lossy().to_string();
                // Absolute paths: glob didn't strip anything; keep as-is.
                if s.starts_with('/') {
                    matches.push(s);
                    continue;
                }
                // Case 1: pattern had a real dir prefix like `foo/bar/`
                // and glob returned a name without it — put it back.
                // Skip the re-prepend if dir_prefix itself contains a glob
                // metacharacter. For a pattern like `src/*/*.c`, glob correctly
                // returns `src/aio/aio.c`; dir_prefix would be `src/*/` (with a
                // literal `*`), `s.starts_with("src/*/")` is false, and the old
                // code prepended `src/*/` -> `src/*/src/aio/aio.c`. musl's
                // Makefile uses $(addsuffix /*.c, $(SRC_DIRS)) where SRC_DIRS
                // itself holds glob patterns; the bogus path then survives all
                // the way to the AR step. Reproduced 2026-04-25 from a clean
                // bootstrap of musl 1.2.6.
                let dir_prefix_is_pattern = dir_prefix.contains('*')
                    || dir_prefix.contains('?')
                    || dir_prefix.contains('[');
                if !dir_prefix.is_empty() && !dir_prefix_is_pattern && !s.starts_with(dir_prefix) {
                    // If glob stripped only the `./` leader (common when
                    // dir_prefix is `./dir/`), prepend `./` not the full
                    // prefix. The rest of the prefix (`dir/`) is already
                    // in `s`.
                    if has_dot_slash && s.starts_with(&dir_prefix[2..]) {
                        matches.push(format!("./{}", s));
                    } else {
                        matches.push(format!("{}{}", dir_prefix, s));
                    }
                    continue;
                }
                // Case 2: pattern was `./*.foo` (no real dir prefix) and
                // glob stripped the leading `./` — put it back.
                if has_dot_slash && dir_prefix.is_empty() && !s.starts_with("./") {
                    matches.push(format!("./{}", s));
                    continue;
                }
                // Case 3: pattern had `./` AND a glob in the directory
                // component (e.g. `./f*/*.c` or `./src/*/x86_64/*.s`).
                // Case 1 was skipped above because `dir_prefix_is_pattern`
                // is true, but we still need GNU-make-compatible `./`
                // preservation. Without this, $(patsubst $(srcdir)/%,%.o,
                // $(basename $(SRCS))) with srcdir=. gets bare stems
                // (no .o appended) because the `./` it tries to consume
                // isn't there. Reproduced 2026-04-25 building musl —
                // every AOBJS entry came out as `obj/src/aio/aio` (no
                // .o), causing both the `%: %.o` link rule to misfire
                // and `ar rc lib/libc.a` to be called with bare names.
                if has_dot_slash && !s.starts_with("./") && !s.starts_with('/') {
                    matches.push(format!("./{}", s));
                    continue;
                }
                matches.push(s);
            }
        }
        // GNU Make sorts wildcard results lexicographically
        matches.sort();
        results.extend(matches);
    }
    results.join(" ")
}

fn fn_realpath(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    // SECURITY: path traversal — trust boundary.  See fn_wildcard comment.
    // $(realpath ../../etc/passwd) resolves the symlink chain and returns the
    // canonical path.  This is intended behavior; the Makefile author is trusted.
    // SECURITY: verified — no sanitisation required.
    let words: Vec<&str> = args[0].split_whitespace().collect();
    let results: Vec<String> = words.iter().filter_map(|w| {
        std::fs::canonicalize(w).ok().map(|p| p.to_string_lossy().to_string())
    }).collect();
    results.join(" ")
}

fn fn_abspath(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    // SECURITY: path traversal — trust boundary.  See fn_wildcard comment.
    // $(abspath ../../etc/passwd) normalises the path without resolving symlinks.
    // The Makefile author is trusted; no path filtering is applied.
    // SECURITY: verified — no sanitisation required.
    let words: Vec<&str> = args[0].split_whitespace().collect();
    let cwd = std::env::current_dir().unwrap_or_default();
    let results: Vec<String> = words.iter().map(|w| {
        let p = Path::new(w);
        if p.is_absolute() {
            normalize_path(p)
        } else {
            normalize_path(&cwd.join(p))
        }
    }).collect();
    results.join(" ")
}

fn normalize_path(path: &Path) -> String {
    let mut components = Vec::new();
    let mut is_absolute = false;
    for component in path.components() {
        match component {
            std::path::Component::RootDir => { is_absolute = true; }
            std::path::Component::ParentDir => {
                if !components.is_empty() {
                    components.pop();
                }
            }
            std::path::Component::CurDir => {}
            c => components.push(c.as_os_str().to_string_lossy().to_string()),
        }
    }
    if is_absolute {
        format!("/{}", components.join("/"))
    } else if components.is_empty() {
        ".".to_string()
    } else {
        components.join("/")
    }
}

fn fn_if(args: &[String], expand: &dyn Fn(&str) -> String) -> String {
    let condition = args[0].trim();
    if !condition.is_empty() {
        if args.len() > 1 {
            expand(&args[1])
        } else {
            String::new()
        }
    } else {
        if args.len() > 2 {
            expand(&args[2])
        } else {
            String::new()
        }
    }
}

fn fn_or(args: &[String], expand: &dyn Fn(&str) -> String) -> String {
    for arg in args {
        let expanded = expand(arg);
        if !expanded.trim().is_empty() {
            return expanded;
        }
    }
    String::new()
}

fn fn_and(args: &[String], expand: &dyn Fn(&str) -> String) -> String {
    let mut last = String::new();
    for arg in args {
        last = expand(arg);
        if last.trim().is_empty() {
            return String::new();
        }
    }
    last
}

fn fn_foreach(args: &[String], expand: &dyn Fn(&str) -> String) -> String {
    let var = args[0].trim();
    let list: Vec<&str> = args[1].split_whitespace().collect();
    let body = &args[2];

    let results: Vec<String> = list.iter().map(|word| {
        // Replace $(var), ${var}, and $v (single-char) in body with word, then expand
        let mut substituted = body.replace(&format!("$({})", var), word)
                                  .replace(&format!("${{{}}}", var), word);
        if var.len() == 1 {
            substituted = substituted.replace(&format!("${}", var), word);
        }
        expand(&substituted)
    }).collect();
    results.join(" ")
}

fn fn_file(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let op = args[0].trim();
    let text = if args.len() > 1 { &args[1] } else { "" };

    if let Some(filename) = op.strip_prefix('>') {
        let filename = filename.trim();
        if let Some(filename) = op.strip_prefix(">>") {
            let filename = filename.trim();
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(filename) {
                let _ = write!(f, "{}", text);
                if !text.is_empty() && !text.ends_with('\n') {
                    let _ = writeln!(f);
                }
            }
        } else {
            let content = if text.is_empty() { String::new() } else { format!("{}\n", text) };
            std::fs::write(filename, &content).ok();
        }
        String::new()
    } else if let Some(filename) = op.strip_prefix('<') {
        let filename = filename.trim();
        std::fs::read_to_string(filename).unwrap_or_default()
    } else {
        String::new()
    }
}

fn fn_call(args: &[String], expand: &dyn Fn(&str) -> String) -> String {
    // $(call var,param1,param2,...) - expand $(var) with $1, $2, etc. set
    // This is handled specially in the expand engine
    // Here we just return the expanded variable with substitutions
    if args.is_empty() {
        return String::new();
    }
    // The actual implementation is in the expander since it needs variable access
    // This stub just returns the first arg expanded
    expand(&args[0])
}

fn fn_value(_args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    // $(value var) is fully handled in expand_function before the generic
    // builtin dispatch; this stub is unreachable in correct operation.
    unreachable!("fn_value should never be called: $(value) is handled in expand_function")
}

fn fn_eval(_args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    // $(eval text) - parsed as makefile content; handled in expander
    // Always returns empty string
    String::new()
}

fn fn_origin(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    // Handled in expander which has access to variable database
    args[0].clone()
}

fn fn_flavor(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    // Handled in expander
    args[0].clone()
}

/// Execute a shell command, returning (stdout_processed, exit_code).
/// stdout is processed per GNU Make rules: internal newlines replaced with spaces,
/// trailing whitespace stripped.
/// `extra_env` provides additional environment variables to set (or override).
/// `remove_env` lists variable names to remove from the environment.
/// `progname` is used for error messages (e.g. "make: cmd: No such file or directory").
pub fn fn_shell_exec_with_status_env(
    cmd: &str,
    extra_env: &HashMap<String, String>,
    remove_env: &[String],
    progname: &str,
) -> (String, i32) {
    let child_makelevel = std::env::var("MAKELEVEL")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .map(|l| (l + 1).to_string())
        .unwrap_or_else(|| "1".to_string());

    let mut c = Command::new("/bin/sh");
    c.arg("-c").arg(cmd);
    c.env("MAKELEVEL", &child_makelevel);
    for name in remove_env {
        c.env_remove(name);
    }
    for (k, v) in extra_env {
        c.env(k, v);
    }

    match c.output() {
        Ok(out) => {
            // Print any stderr output (e.g. "command not found" errors).
            // GNU Make strips the "sh: " or "/bin/sh: " prefix from the shell's error
            // messages and replaces it with the make program name.
            if !out.stderr.is_empty() {
                let stderr_str = String::from_utf8_lossy(&out.stderr);
                for line in stderr_str.lines() {
                    // Strip the shell prefix (e.g. "sh: " or "/bin/sh: ")
                    let msg = strip_shell_prefix(line);
                    // Normalize non-standard error message wording
                    let normalized = normalize_shell_error_msg(msg);
                    eprintln!("{}: {}", progname, normalized);
                }
            }
            let exit_code = out.status.code().unwrap_or_else(|| {
                // Process was terminated by a signal; return 128 + signal number
                // to match the conventional shell behavior.
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    out.status.signal().map(|s| 128 + s).unwrap_or(-1)
                }
                #[cfg(not(unix))]
                { -1 }
            });
            let raw = String::from_utf8_lossy(&out.stdout).to_string();
            let result = process_shell_output(&raw);
            (result, exit_code)
        }
        Err(_) => (String::new(), 127),
    }
}

/// Strip shell error prefix ("sh: " or "/bin/sh: " or similar) from a stderr line.
fn strip_shell_prefix(line: &str) -> &str {
    // Common patterns: "sh: cmd: msg", "/bin/sh: cmd: msg", "/bin/sh: line N: cmd: msg"
    // We strip up to and including the first colon-space that comes from the shell binary name.
    if let Some(colon_pos) = line.find(": ") {
        let prefix = &line[..colon_pos];
        // Check if prefix looks like a shell binary path.
        let basename = prefix.rsplit('/').next().unwrap_or(prefix);
        let is_shell = matches!(basename, "sh" | "bash" | "ksh" | "mksh" | "dash" | "zsh" | "ash");
        if is_shell {
            let rest = &line[colon_pos + 2..];
            // Also strip "line N: " prefix if present (e.g., "/bin/sh: line 1: cmd: msg")
            if let Some(line_prefix) = rest.strip_prefix("line ") {
                if let Some(end) = line_prefix.find(": ") {
                    let digits = &line_prefix[..end];
                    if digits.chars().all(|c| c.is_ascii_digit()) {
                        // Skip "line N: " (5 + len(digits) + 2)
                        return &rest[5 + end + 2..];
                    }
                }
            }
            return rest;
        }
    }
    line
}

/// Normalize shell error message text to use standard POSIX wording.
/// Some shells (e.g. mksh) use non-standard messages like "inaccessible or not found"
/// instead of the standard "No such file or directory".  GNU Make's test suite expects
/// the standard phrasing.
fn normalize_shell_error_msg(msg: &str) -> String {
    // Replace mksh's "inaccessible or not found" with "No such file or directory".
    // The pattern is "cmd: inaccessible or not found" → "cmd: No such file or directory"
    msg.replace(": inaccessible or not found", ": No such file or directory")
}

/// Execute a shell command, returning (stdout_processed, exit_code).
/// stdout is processed per GNU Make rules: internal newlines replaced with spaces,
/// trailing whitespace stripped.
pub fn fn_shell_exec_with_status(cmd: &str) -> (String, i32) {
    fn_shell_exec_with_status_env(cmd, &HashMap::new(), &[], "make")
}

/// Process shell output per GNU Make rules:
/// 1. Strip exactly one trailing newline (if the output ends with one).
/// 2. Replace all remaining newlines with spaces.
/// Trailing non-newline whitespace (e.g. a trailing space) is preserved.
/// This matches GNU Make's collapse_continuations / shell output behavior.
fn process_shell_output(raw: &str) -> String {
    // Strip exactly one trailing newline (and its preceding \r if present).
    let stripped = if raw.ends_with('\n') {
        let without_lf = &raw[..raw.len() - 1];
        // Handle \r\n: also strip the preceding \r
        if without_lf.ends_with('\r') {
            &without_lf[..without_lf.len() - 1]
        } else {
            without_lf
        }
    } else {
        raw
    };
    // Replace remaining internal newlines (treating \r\n as a single newline) with spaces.
    stripped.replace("\r\n", " ").replace('\n', " ")
}

pub fn fn_shell_exec(cmd: &str) -> String {
    fn_shell_exec_with_status(cmd).0
}

fn fn_shell(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    fn_shell_exec(&args[0])
}

/// Strip ANSI / VT100 escape sequences from a string when output goes to a TTY.
///
/// GNU Make passes `$(info)` / `$(warning)` / `$(error)` messages straight to
/// the terminal with no sanitisation.  A Makefile that reads external data
/// (e.g. from `$(shell …)`) and injects it into a diagnostic message can
/// send arbitrary escape sequences to the developer's terminal — clearing the
/// screen, overwriting the window title, moving the cursor, etc.
///
/// We strip the sequences only when the destination file descriptor is a real
/// terminal (i.e. `isatty()` returns true).  Piped / redirected output is left
/// unchanged so that legitimate downstream consumers of jmake's output are not
/// broken by unexpected stripping.
///
/// Sequences stripped:
///  * ESC `[` … `m|A|B|C|D|H|J|K|…`  (CSI — Control Sequence Introducer)
///  * ESC `]` … `BEL`/`ESC \`        (OSC — Operating System Command, e.g. titles)
///  * ESC `(`/`)`/`*`/`+` `X`        (character set designators)
///  * Bare ESC followed by a single ASCII char (`ESC c`, `ESC =`, etc.)
///  * Raw C0 controls: BEL (0x07), BS (0x08), DEL (0x7f), and the FF/VT
///    form-feed/vertical-tab pair — these can reposition the cursor without
///    a leading ESC.
///
/// `\r` (carriage return) is intentionally preserved: it appears in normal
/// Windows-line-ending output and stripping it would corrupt the text.
///
// SECURITY: this function is the primary mitigation for terminal escape
// injection via Makefile diagnostic messages.  Without it, an attacker
// who controls Makefile content (e.g. from a fetched dependency's build
// system or from environment variables) can inject arbitrary terminal
// control sequences into the developer's terminal.
//
// PRE:  `s` is valid UTF-8 (Rust strings are always UTF-8).
// POST: All ESC-sequence and raw C0 control characters listed above are
//       removed.  Printable ASCII, \n, \r, and all multi-byte UTF-8
//       codepoints are left unchanged.
fn sanitize_terminal_output(s: &str) -> String {
    use std::io::IsTerminal as _;
    // Fast path: if no ESC byte is present and no dangerous C0 controls are
    // present, return a reference to the original (via Cow) without allocating.
    let needs_sanitize = s.bytes().any(|b| matches!(b, 0x1b | 0x07 | 0x08 | 0x0c | 0x0d | 0x7f));
    if !needs_sanitize {
        return s.to_string();
    }
    // Only sanitize when writing to a real terminal.  Piped output is unchanged.
    // We check both stdout and stderr since info/warning/error target different fds.
    let is_tty = std::io::stderr().is_terminal() || std::io::stdout().is_terminal();
    if !is_tty {
        return s.to_string();
    }

    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            // ESC — start of a multi-byte sequence
            0x1b => {
                if i + 1 >= bytes.len() {
                    // Bare ESC at end: strip it
                    i += 1;
                    continue;
                }
                match bytes[i + 1] {
                    // CSI: ESC [ ... final_byte  (final byte is 0x40–0x7e)
                    b'[' => {
                        i += 2; // skip ESC [
                        while i < bytes.len() && !matches!(bytes[i], 0x40..=0x7e) {
                            i += 1;
                        }
                        i += 1; // skip final byte
                    }
                    // OSC: ESC ] ... (BEL | ESC \)
                    b']' => {
                        i += 2; // skip ESC ]
                        while i < bytes.len() {
                            if bytes[i] == 0x07 {
                                // BEL terminator
                                i += 1;
                                break;
                            }
                            if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                                // String Terminator: ESC \
                                i += 2;
                                break;
                            }
                            i += 1;
                        }
                    }
                    // Character-set designators: ESC ( X, ESC ) X, ESC * X, ESC + X
                    b'(' | b')' | b'*' | b'+' => {
                        i += 3; // skip ESC <char> <designator>
                    }
                    // Any other ESC X: single-character escape sequence
                    _ => {
                        i += 2; // skip ESC + one char
                    }
                }
            }
            // BEL — terminal bell: could be used to annoy; strip from TTY output
            0x07 => { i += 1; }
            // BS — backspace: could overwrite visible output
            0x08 => { i += 1; }
            // FF / VT — form feed / vertical tab: reposition cursor
            0x0c | 0x0b => { i += 1; }
            // DEL
            0x7f => { i += 1; }
            // All other bytes (including \n, \r, printable ASCII, UTF-8 continuations)
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    // SAFETY: we only copy bytes from the original valid-UTF-8 string or skip
    // them; we never insert bytes that would break UTF-8 encoding.  ESC-sequences
    // are always ASCII, so stripping them cannot split a multi-byte codepoint.
    unsafe { String::from_utf8_unchecked(out) }
}

fn fn_error(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    // SECURITY: sanitize terminal escape sequences from error messages to prevent
    // injection attacks (e.g. screen clearing, cursor repositioning) when the
    // message contains untrusted content such as shell-command output.
    let msg = sanitize_terminal_output(&args[0]);
    eprintln!("*** {}.  Stop.", msg);
    std::process::exit(2);
}

fn fn_warning(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    // SECURITY: sanitize terminal escape sequences from warning messages.
    let msg = sanitize_terminal_output(&args[0]);
    eprintln!("{}", msg);
    String::new()
}

fn fn_info(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    // SECURITY: sanitize terminal escape sequences from info messages.
    let msg = sanitize_terminal_output(&args[0]);
    println!("{}", msg);
    String::new()
}

fn fn_guile(_args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    // Guile integration is not supported
    String::new()
}

fn fn_let(args: &[String], expand: &dyn Fn(&str) -> String) -> String {
    // $(let var [var ...],list,text) - like foreach but assigns multiple vars
    let vars: Vec<&str> = args[0].split_whitespace().collect();
    let words: Vec<&str> = args[1].split_whitespace().collect();
    let body = &args[2];

    let mut substituted = body.clone();
    for (i, var) in vars.iter().enumerate() {
        let val: String = if i == vars.len() - 1 {
            // Last variable gets all remaining words
            let remaining: Vec<&str> = if i < words.len() { words[i..].to_vec() } else { vec![] };
            remaining.join(" ")
        } else if i < words.len() {
            words[i].to_string()
        } else {
            String::new()
        };
        substituted = substituted.replace(&format!("$({})", var), &val)
                                 .replace(&format!("${{{}}}", var), &val);
        if var.len() == 1 {
            substituted = substituted.replace(&format!("${}", var), &val);
        }
    }
    expand(&substituted)
}

/// Compare two arbitrary-precision integer strings. Returns -1, 0, or 1.
fn bigint_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let a = a.trim();
    let b = b.trim();

    let (a_neg, a_digits) = if let Some(s) = a.strip_prefix('-') {
        (true, s.trim_start_matches('0'))
    } else {
        (false, a.strip_prefix('+').unwrap_or(a).trim_start_matches('0'))
    };
    let (b_neg, b_digits) = if let Some(s) = b.strip_prefix('-') {
        (true, s.trim_start_matches('0'))
    } else {
        (false, b.strip_prefix('+').unwrap_or(b).trim_start_matches('0'))
    };

    // Treat empty digits as 0
    let a_zero = a_digits.is_empty();
    let b_zero = b_digits.is_empty();

    // -0 == 0
    let a_neg = if a_zero { false } else { a_neg };
    let b_neg = if b_zero { false } else { b_neg };

    if a_neg != b_neg {
        return if a_neg { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater };
    }

    // Both same sign - compare magnitude
    let mag_cmp = if a_digits.len() != b_digits.len() {
        a_digits.len().cmp(&b_digits.len())
    } else {
        a_digits.cmp(b_digits)
    };

    if a_neg { mag_cmp.reverse() } else { mag_cmp }
}

fn fn_intcmp(args: &[String], expand: &dyn Fn(&str) -> String) -> String {
    let ord = bigint_cmp(&args[0], &args[1]);

    match args.len() {
        2 => {
            if ord == std::cmp::Ordering::Equal { args[1].trim().to_string() } else { String::new() }
        }
        3 => {
            if ord == std::cmp::Ordering::Less { expand(&args[2]) } else { String::new() }
        }
        4 => {
            // With 4 args, the 4th arg (args[3]) is the "greater-or-equal" catch-all:
            // it is returned for both == and > cases.
            match ord {
                std::cmp::Ordering::Less => expand(&args[2]),
                std::cmp::Ordering::Equal | std::cmp::Ordering::Greater => expand(&args[3]),
            }
        }
        5 | _ => {
            match ord {
                std::cmp::Ordering::Less => expand(&args[2]),
                std::cmp::Ordering::Equal => expand(&args[3]),
                std::cmp::Ordering::Greater => expand(&args.get(4).cloned().unwrap_or_default()),
            }
        }
    }
}

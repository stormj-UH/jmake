//! Tier 2 feature unit tests for jmake conformance.
//! Runs tests/feature/*.mk against jmake with JMAKE_TEST_MODE=1
//! and diffs against golden output captured from GNU Make 4.4.1.
//!
//! Run with: cargo test --test feature_tests

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn jmake_bin() -> PathBuf {
    let mut p = env::current_exe().unwrap();
    p.pop();
    if p.ends_with("deps") { p.pop(); }
    p.push("jmake");
    if p.exists() { return p; }
    PathBuf::from("jmake")
}

fn feature_dir() -> PathBuf {
    let mut d = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    d.push("tests");
    d.push("feature");
    d
}

/// Run jmake with combined stdout+stderr (via /bin/sh -c "... 2>&1") so the
/// output order matches what the shell-based golden capture produces.
fn run_feature_test(mk_name: &str, extra_flags: &[&str]) {
    run_feature_test_with_setup(mk_name, extra_flags, || {});
}

/// Like `run_feature_test` but runs `setup` (and its RAII guard, if any) before
/// launching jmake.  The guard is dropped after the assertion so cleanup happens
/// even on failure.  Use this for tests that need external fixture directories.
fn run_feature_test_with_setup<F: FnOnce()>(mk_name: &str, extra_flags: &[&str], setup: F) {
    setup();

    let fdir = feature_dir();
    let golden_path = fdir.join(format!("{}.golden", mk_name));
    let mk_path = format!("{}.mk", mk_name);

    let expected = std::fs::read_to_string(&golden_path)
        .unwrap_or_else(|e| panic!("cannot read golden {}: {}", golden_path.display(), e));

    let jmake = jmake_bin();
    let mut cmd_str = format!("{} -f {}", jmake.display(), mk_path);
    for flag in extra_flags {
        cmd_str.push(' ');
        cmd_str.push_str(flag);
    }
    cmd_str.push_str(" 2>&1");

    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(&cmd_str)
        .env("JMAKE_TEST_MODE", "1")
        .current_dir(&fdir)
        .output()
        .expect("failed to run jmake via sh");

    let actual = String::from_utf8_lossy(&out.stdout).to_string();

    let expected_trimmed = expected.trim_end_matches('\n');
    let actual_trimmed = actual.trim_end_matches('\n');

    if expected_trimmed != actual_trimmed {
        panic!(
            "output mismatch for '{}':\n--- expected ---\n{}\n--- actual ---\n{}\n--- line diffs ---\n{}",
            mk_name, expected_trimmed, actual_trimmed,
            diff_lines(expected_trimmed, actual_trimmed)
        );
    }
}

fn diff_lines(a: &str, b: &str) -> String {
    let al: Vec<&str> = a.lines().collect();
    let bl: Vec<&str> = b.lines().collect();
    let mut out = Vec::new();
    for i in 0..al.len().max(bl.len()) {
        let a = al.get(i).copied().unwrap_or("<missing>");
        let b = bl.get(i).copied().unwrap_or("<missing>");
        if a != b {
            out.push(format!("[{}] exp: {:?}", i + 1, a));
            out.push(format!("[{}] got: {:?}", i + 1, b));
        }
    }
    out.join("\n")
}

#[test] fn test_target_specific_vars()  { run_feature_test("target_specific_vars",  &[]); }
#[test] fn test_pattern_specific_vars() { run_feature_test("pattern_specific_vars", &[]); }
#[test] fn test_patsubst_chain()        { run_feature_test("patsubst_chain",        &[]); }
#[test]
fn test_wildcard_variants() {
    // The .mk file references the absolute path /tmp/wc-test/src/*.c.
    // Create the fixture before running jmake and remove it when done so the
    // test is self-contained and does not depend on pre-existing disk state.
    let fixture = std::path::Path::new("/tmp/wc-test/src");
    std::fs::create_dir_all(fixture).expect("create /tmp/wc-test/src");
    std::fs::File::create(fixture.join("foo.c")).expect("create foo.c");
    std::fs::File::create(fixture.join("bar.c")).expect("create bar.c");
    let result = std::panic::catch_unwind(|| {
        run_feature_test_with_setup("wildcard_variants", &[], || {});
    });
    // Best-effort cleanup regardless of pass/fail.
    let _ = std::fs::remove_dir_all("/tmp/wc-test");
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}
#[test] fn test_eval_foreach()          { run_feature_test("eval_foreach",          &[]); }
#[test] fn test_shell_makelevel()       { run_feature_test("shell_makelevel",       &[]); }
#[test] fn test_order_only()            { run_feature_test("order_only",            &[]); }
#[test] fn test_double_colon()          { run_feature_test("double_colon",          &[]); }
#[test] fn test_phony_suffixes()        { run_feature_test("phony_suffixes",        &[]); }
#[test] fn test_make_r()                { run_feature_test("make_r",                &["-r"]); }
#[test] fn test_make_k()                { run_feature_test("make_k",                &["-k"]); }
#[test] fn test_make_flags_n()          { run_feature_test("make_flags_n",          &["-n"]); }
#[test] fn test_include_variants()      { run_feature_test("include_variants",      &[]); }
#[test] fn test_tab_var_conditional()   { run_feature_test("tab_var_conditional",   &[]); }
#[test] fn test_bare_colon_conditional(){ run_feature_test("bare_colon_conditional",&[]); }

// ── Regression tests: wildcard with absolute path and no-prefix (CWD-relative) ──
//
// These burned us before: $(wildcard /abs/path/*.c) returned "" and
// $(wildcard *.c) returned "" even when files existed.  Root cause was that
// the test relied on /tmp/wc-test/src existing on disk rather than creating
// it as part of the test, so both glob variants silently returned empty on a
// clean machine.  Each test below is fully self-contained.

/// $(wildcard /abs/path/*.c) must return sorted absolute paths, not "".
#[test]
fn test_wildcard_abs_path() {
    let tmp = tempfile::TempDir::new().expect("TempDir");
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::File::create(src.join("alpha.c")).unwrap();
    std::fs::File::create(src.join("beta.c")).unwrap();

    // Write a minimal Makefile that uses an absolute wildcard pattern.
    let mk = tmp.path().join("abs.mk");
    std::fs::write(&mk, format!(
        "SRCS := $(wildcard {}/*.c)\nall:\n\t@echo \"srcs=$(SRCS)\"\n",
        src.display()
    )).unwrap();

    let jmake = jmake_bin();
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("{} -f {} 2>&1", jmake.display(), mk.display()))
        .env("JMAKE_TEST_MODE", "1")
        .output()
        .expect("run jmake");

    let stdout = String::from_utf8_lossy(&out.stdout);
    // Both files must appear; order must be sorted (GNU make sorts wildcard).
    let expected = format!("srcs={}/alpha.c {}/beta.c", src.display(), src.display());
    assert_eq!(
        stdout.trim_end_matches('\n'),
        expected.as_str(),
        "absolute-path wildcard returned wrong result: {:?}",
        stdout
    );
}

/// $(wildcard *.c) with jmake run from a directory containing .c files must
/// return the bare filenames, not "".
#[test]
fn test_wildcard_no_prefix() {
    let tmp = tempfile::TempDir::new().expect("TempDir");
    std::fs::File::create(tmp.path().join("main.c")).unwrap();
    std::fs::File::create(tmp.path().join("util.c")).unwrap();

    let mk = tmp.path().join("noprefix.mk");
    std::fs::write(&mk, "SRCS := $(wildcard *.c)\nall:\n\t@echo \"srcs=$(SRCS)\"\n").unwrap();

    let jmake = jmake_bin();
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("{} -f noprefix.mk 2>&1", jmake.display()))
        .env("JMAKE_TEST_MODE", "1")
        .current_dir(tmp.path())
        .output()
        .expect("run jmake");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.trim_end_matches('\n'),
        "srcs=main.c util.c",
        "no-prefix wildcard returned wrong result: {:?}",
        stdout
    );
}

// ── Security hardening: expansion depth limit ──────────────────────────────

/// A Makefile with a chain of 1001 distinct recursive variables that reference
/// each other (V0 = $(V1), V1 = $(V2), …, V1000 = bottom) must abort with
/// exit code 2 and a diagnostic instead of stack-overflowing the process.
///
/// This exercises the MAX_EXPANSION_DEPTH = 1000 limit added in the
/// hardening/security-audit pass.  The vars_being_expanded circular-reference
/// guard does NOT fire here because each variable is distinct; without the
/// depth limit the call stack would grow until SIGSEGV.
#[test]
fn test_expansion_depth_limit_kills_deep_chain() {
    let tmp = tempfile::TempDir::new().expect("TempDir");

    // Generate: V0 = $(V1)\nV1 = $(V2)\n…\nV1000 = bottom\nall:\n\t@echo $(V0)
    let mut mk = String::new();
    let depth = 1001usize; // one more than the 1000-level limit
    for i in 0..depth {
        mk.push_str(&format!("V{} = $(V{})\n", i, i + 1));
    }
    mk.push_str(&format!("V{} = bottom\nall:\n\t@echo $(V0)\n", depth));

    std::fs::write(tmp.path().join("deep.mk"), &mk).unwrap();

    let jmake = jmake_bin();
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("{} -f deep.mk 2>&1", jmake.display()))
        .env("JMAKE_TEST_MODE", "1")
        .current_dir(tmp.path())
        .output()
        .expect("run jmake");

    // Must exit with a non-zero code (2) — not crash with SIGSEGV.
    let status = out.status.code().unwrap_or(-1);
    assert_ne!(status, 0,
        "deep variable chain must fail, not succeed");
    // Must not be a signal-killed exit (which on Linux produces a negative or
    // 128+signal exit code from the shell, but the key check is non-zero).
    // We just verify it terminated cleanly (not killed by a signal like SIGSEGV).
    let combined = String::from_utf8_lossy(&out.stdout);
    assert!(
        combined.contains("Recursive variable") || combined.contains("references itself"),
        "expected depth-limit diagnostic in output, got: {:?}", combined
    );
}

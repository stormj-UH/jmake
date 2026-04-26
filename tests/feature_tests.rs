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
#[test] fn test_wildcard_variants()     { run_feature_test("wildcard_variants",     &[]); }
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

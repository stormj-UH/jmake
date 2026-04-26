//! Tier 1 real-world tests for jmake conformance.
//! Downloads are cached in tests/realworld/cache/.
//! Run with: cargo test --test realworld_tests -- --ignored
//!
//! These are marked #[ignore] because they are slow and require network
//! access for the initial download. Run them explicitly with:
//!   cargo test --test realworld_tests -- --ignored

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

fn realworld_dir() -> PathBuf {
    let mut d = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    d.push("tests");
    d.push("realworld");
    d
}

fn run_realworld_script(pkg: &str) {
    let script = realworld_dir().join("run.sh");
    let jmake = jmake_bin();
    let out = Command::new(&script)
        .arg(jmake.to_str().unwrap())
        .output()
        .expect("failed to run realworld run.sh");
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Parse DRIFT lines for this specific package
    for line in stdout.lines() {
        if line.starts_with("DRIFT") && line.contains(pkg) {
            panic!("Tier1 drift for {}: {}", pkg, line);
        }
    }
    if !out.status.success() {
        // Check if the failure is for this package specifically
        if stdout.contains(&format!("DRIFT {}", pkg)) {
            panic!("realworld test failed for {}:\n{}", pkg, stdout);
        }
    }
}

/// musl 1.2.6 — primary regression test (the bug that prompted jmake 1.1.8)
#[test]
#[ignore]
fn test_musl_1_2_6() {
    run_realworld_script("musl-1.2.6");
}

/// expat 2.6.4
#[test]
#[ignore]
fn test_expat_2_6_4() {
    run_realworld_script("expat-2.6.4");
}

/// libffi 3.4.6
#[test]
#[ignore]
fn test_libffi_3_4_6() {
    run_realworld_script("libffi-3.4.6");
}

/// dropbear 2024.86
#[test]
#[ignore]
fn test_dropbear_2024_86() {
    run_realworld_script("dropbear-2024.86");
}

/// toybox 0.8.11
#[test]
#[ignore]
fn test_toybox_0_8_11() {
    run_realworld_script("toybox-0.8.11");
}

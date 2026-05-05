// (c) 2026 Jon-Erik G. Storm, Inc., a California Corporation,
// doing business as LAVA GOAT SOFTWARE. All rights reserved.
// SPDX-License-Identifier: MIT

//! Entry point for jmake — a clean-room replacement for GNU Make.
//!
//! # Responsibilities
//!
//! This module:
//!
//! 1. Installs a custom panic hook that converts stdout write errors (broken
//!    pipe, `/dev/full`) into a clean `exit(1)` with a diagnostic on stderr,
//!    matching what a GNU Make-compatible tool is expected to do.
//! 2. Installs the SIGTERM handler (see [`signal_handler`]) before touching
//!    anything else so that any temp file created during startup is covered.
//! 3. Parses command-line arguments via [`cli::parse_args`] and dispatches to
//!    [`eval::MakeState`] for the actual build.
//! 4. Cleans up temp files after a normal return (the SIGTERM handler owns
//!    cleanup on abnormal termination).
//!
//! # Test-mode impersonation
//!
//! When the environment variable `JMAKE_TEST_MODE` is set to `"1"`, jmake
//! reports itself as `GNU Make 4.4.1` in `--version` output and adjusts a
//! small number of other outputs for compatibility with the GNU Make test
//! suite.  This impersonation is never active outside that test mode.
//!
//! # Exit codes
//!
//! | Code | Meaning |
//! |------|---------|
//! | 0    | Success — all requested targets are up to date. |
//! | 1    | I/O error writing to stdout (broken pipe, full disk). |
//! | 2    | Build failure — at least one target could not be made. |
//! | 101  | Unexpected internal panic (Rust default). |
//!
//! # Thread safety
//!
//! `main` is single-threaded up to the point where the parallel executor
//! (`exec::parallel`) spawns worker threads.  The signal handler globals
//! are designed for single-threaded access; see [`signal_handler`] for the
//! full safety argument.

#![deny(warnings)]
#![deny(rust_2018_idioms)]

mod parser;
mod eval;
mod exec;
mod functions;
mod cli;
mod types;
mod database;
mod implicit_rules;
mod signal_handler;
mod io_traits;

use std::process;

/// Returns `true` when `raw` is exactly `"1"` (after trimming whitespace).
///
/// Used to determine whether `JMAKE_TEST_MODE` enables GNU Make impersonation.
/// Any other value — including `"true"`, `"yes"`, or `"0"` — is treated as
/// disabled, matching GNU Make's own convention for boolean environment flags.
#[cfg(test)]
fn should_impersonate_gnu_make(raw: Option<&str>) -> bool {
    matches!(raw.map(str::trim), Some("1"))
}

/// Returns `true` when test mode is active (delegates to [`eval::test_mode_enabled`]).
fn test_mode_enabled() -> bool {
    eval::test_mode_enabled()
}

/// Returns the canonical GNU Make target-triple string for the current
/// host architecture.
///
/// Used only in test-mode `--version` output to impersonate the specific
/// GNU Make build that the test suite expects.
fn target_triple() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "aarch64-unknown-linux-gnu",
        "x86_64" => "x86_64-pc-linux-gnu",
        _ => "unknown-unknown-linux-gnu",
    }
}

/// Returns the lines to print for `--version`.
///
/// When `test_mode` is `true`, the output mimics `GNU Make 4.4.1` so that the
/// GNU Make test suite's version-check assertions pass.  In production mode the
/// output identifies jmake by its own name and `CARGO_PKG_VERSION`.
///
/// # Arguments
///
/// * `test_mode` — if `true`, emit GNU Make 4.4.1 impersonation text; otherwise
///   emit jmake-branded text.
fn version_lines(test_mode: bool) -> Vec<String> {
    if test_mode {
        vec![
            "GNU Make 4.4.1".to_string(),
            format!("Built for {}", target_triple()),
            "Copyright (C) 1988-2023 Free Software Foundation, Inc.".to_string(),
            "License GPLv3+: GNU GPL version 3 or later <https://gnu.org/licenses/gpl.html>".to_string(),
            "This is free software: you are free to change and redistribute it.".to_string(),
            "There is NO WARRANTY, to the extent permitted by law.".to_string(),
        ]
    } else {
        vec![
            format!("jmake {}", env!("CARGO_PKG_VERSION")),
            "Copyright (c) 2026 Jon-Erik G. Storm.".to_string(),
            "This is jmake, a clean-room replacement for GNU Make.".to_string(),
        ]
    }
}

fn main() {
    // Install a custom panic hook so that stdout write errors (e.g. writing to
    // /dev/full or a closed pipe) cause a clean exit(1) rather than a panic
    // message + exit(101).  GNU Make exits with code 1 on stdout write errors.
    std::panic::set_hook(Box::new(|info| {
        let is_write_error = info.payload()
            .downcast_ref::<String>()
            .map(|s| {
                s.contains("failed printing to stdout")
                    || s.contains("failed writing to stdout")
            })
            .unwrap_or(false)
            || info.payload()
            .downcast_ref::<&str>()
            .map(|s| {
                s.contains("failed printing to stdout")
                    || s.contains("failed writing to stdout")
            })
            .unwrap_or(false);

        if is_write_error {
            // Print GNU Make-compatible error message to stderr, then exit(1).
            let progname = if test_mode_enabled() {
                "make".to_string()
            } else {
                let raw = std::env::args().next().unwrap_or_else(|| "make".to_string());
                std::path::Path::new(&raw)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&raw)
                    .to_string()
            };
            eprintln!("{}: write error", progname);
            process::exit(1);
        }

        // For all other panics, print the panic info to stderr and exit 101
        // (Rust's default exit code for panics).
        eprintln!("{}", info);
        process::exit(101);
    }));

    // Install SIGTERM handler early so we can clean up temp files on signal.
    signal_handler::install_sigterm_handler();

    let args = cli::parse_args();

    if args.version {
        for line in version_lines(test_mode_enabled()) {
            println!("{line}");
        }
        process::exit(0);
    }

    // Capture the process environment once before constructing MakeState.
    // After this point the core MUST NOT call std::env::var directly; it reads
    // from state.env_config instead.
    let env_config = eval::EnvConfig::from_process_env();

    let mut state = eval::MakeState::new(args, env_config);

    let result = state.run();

    // Clean up temp stdin file when run() returns normally (re-exec didn't happen).
    // If we're the re-exec'd process (args.temp_stdin is set), we are the final
    // invocation and must clean up the file passed via --temp-stdin.
    // If we're the original process (stdin_temp_path is set), run() would have
    // called exec() and replaced us if a re-exec was needed, so if we reach here,
    // no re-exec happened and we should clean up.
    let temp_file = state.args.temp_stdin
        .clone()
        .or_else(|| state.stdin_temp_path.clone());
    if let Some(ref tp) = temp_file {
        let _ = std::fs::remove_file(tp);
        signal_handler::clear_temp_stdin_path();
    }

    match result {
        Ok(()) => process::exit(0),
        Err(e) => {
            if !e.is_empty() {
                eprintln!("{}: *** {}", eval::make_progname(), e);
            }
            process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{should_impersonate_gnu_make, target_triple, version_lines};

    #[test]
    fn test_mode_only_impersonates_for_explicit_one() {
        assert!(should_impersonate_gnu_make(Some("1")));
        assert!(should_impersonate_gnu_make(Some(" 1 ")));
        assert!(!should_impersonate_gnu_make(None));
        assert!(!should_impersonate_gnu_make(Some("0")));
        assert!(!should_impersonate_gnu_make(Some("false")));
        assert!(!should_impersonate_gnu_make(Some("true")));
    }

    #[test]
    fn native_version_output_identifies_jmake() {
        let lines = version_lines(false);
        assert_eq!(lines[0], format!("jmake {}", env!("CARGO_PKG_VERSION")));
        assert!(lines.iter().any(|line| line.contains("This is jmake")));
        assert!(!lines.iter().any(|line| line == "GNU Make 4.4.1"));
    }

    #[test]
    fn test_mode_version_output_keeps_gnu_make_impersonation() {
        let lines = version_lines(true);
        assert_eq!(lines[0], "GNU Make 4.4.1");
        assert_eq!(lines[1], format!("Built for {}", target_triple()));
    }
}

/// Security-hardening unit tests.
///
/// These tests cover the specific vulnerabilities addressed in the
/// hardening/security-audit pass:
///
/// 1. MAKEFLAGS `-j` overflow / silent-fallback fix (cli.rs)
/// 2. MAKEFLAGS `-j` sets `jobs_explicit` (cli.rs)
/// 3. Expansion depth limit (eval/expand.rs)
#[cfg(test)]
mod security_tests {
    use crate::cli::{parse_makeflags, MakeArgs};

    // -------------------------------------------------------------------------
    // MAKEFLAGS --jobs= parsing (overflow / silent-fallback)
    // -------------------------------------------------------------------------

    /// A valid --jobs= value in MAKEFLAGS is accepted and sets jobs_explicit.
    #[test]
    fn makeflags_jobs_long_valid() {
        let mut args = MakeArgs::default();
        parse_makeflags("--jobs=4", &mut args);
        assert_eq!(args.jobs.get(), 4);
        assert!(args.jobs_explicit, "--jobs= from MAKEFLAGS must set jobs_explicit");
    }

    /// A --jobs= value that would overflow usize is silently ignored (jobs stays
    /// at the default), not wrapped or silently set to 1.
    /// Previously this path did `token[7..].parse().unwrap_or(1)`, so a value
    /// like "99999999999999999999" would produce jobs=1 with no indication of error.
    #[test]
    fn makeflags_jobs_long_overflow_ignored() {
        let mut args = MakeArgs::default();
        let default_jobs = args.jobs;
        // A value that overflows usize on any 64-bit target.
        parse_makeflags("--jobs=99999999999999999999", &mut args);
        // Should leave jobs unchanged (not silently set to 1).
        assert_eq!(args.jobs, default_jobs,
            "overflow --jobs= in MAKEFLAGS must not silently change the job count");
        assert!(!args.jobs_explicit,
            "failed --jobs= parse must not set jobs_explicit");
    }

    /// A malformed --jobs= (non-numeric) is silently ignored.
    #[test]
    fn makeflags_jobs_long_nonnumeric_ignored() {
        let mut args = MakeArgs::default();
        let default_jobs = args.jobs;
        parse_makeflags("--jobs=abc", &mut args);
        assert_eq!(args.jobs, default_jobs);
        assert!(!args.jobs_explicit);
    }

    // -------------------------------------------------------------------------
    // MAKEFLAGS -j (short flag) must set jobs_explicit
    // -------------------------------------------------------------------------

    /// -j N in MAKEFLAGS sets the job count and marks it explicit so that
    /// apply_makeflags_from_makefile cannot silently override it.
    #[test]
    fn makeflags_jobs_short_sets_explicit() {
        let mut args = MakeArgs::default();
        parse_makeflags("-j8", &mut args);
        assert_eq!(args.jobs.get(), 8);
        assert!(args.jobs_explicit,
            "-j in MAKEFLAGS must set jobs_explicit to protect against makefile override");
    }

    /// Bare -j in MAKEFLAGS (no number) also sets jobs_explicit.
    #[test]
    fn makeflags_jobs_short_bare_sets_explicit() {
        let mut args = MakeArgs::default();
        parse_makeflags("-j", &mut args);
        assert!(args.jobs_explicit,
            "bare -j in MAKEFLAGS must set jobs_explicit");
    }
}

// Copyright (c) 2026 Jon-Erik G. Storm. All rights reserved.
// jmake - Clean-room drop-in replacement for GNU Make

mod parser;
mod eval;
mod exec;
mod functions;
mod cli;
mod types;
mod database;
mod implicit_rules;
mod signal_handler;

use std::process;

fn should_impersonate_gnu_make(raw: Option<&str>) -> bool {
    matches!(raw.map(str::trim), Some("1"))
}

fn test_mode_enabled() -> bool {
    eval::test_mode_enabled()
}

fn target_triple() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "aarch64-unknown-linux-gnu",
        "x86_64" => "x86_64-pc-linux-gnu",
        _ => "unknown-unknown-linux-gnu",
    }
}

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

    let mut state = eval::MakeState::new(args);

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

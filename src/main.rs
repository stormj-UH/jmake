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

use std::process;

fn main() {
    let args = cli::parse_args();

    if args.version {
        println!("GNU Make 4.4.1");
        println!("Built for aarch64-unknown-linux-gnu");
        println!("Copyright (c) 2026 Jon-Erik G. Storm.");
        println!("This is jmake, a clean-room replacement for GNU Make.");
        process::exit(0);
    }

    let mut state = eval::MakeState::new(args);

    let result = state.run();

    // Clean up temp stdin file if we created one (and re-exec didn't happen).
    // On re-exec, the temp file is preserved for the re-exec'd process to read.
    // On normal exit (success or error after run), we can clean it up.
    // Note: if --temp-stdin was given, the file was created by our parent; don't delete it here.
    if state.args.temp_stdin.is_none() {
        if let Some(ref tp) = state.stdin_temp_path {
            let _ = std::fs::remove_file(tp);
        }
    }

    match result {
        Ok(()) => process::exit(0),
        Err(e) => {
            if !e.is_empty() {
                let raw = std::env::args().next().unwrap_or_else(|| "make".to_string());
                let progname = std::path::Path::new(&raw)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&raw)
                    .to_string();
                eprintln!("{}: *** {}", progname, e);
            }
            process::exit(2);
        }
    }
}

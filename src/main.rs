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

    match state.run() {
        Ok(()) => process::exit(0),
        Err(e) => {
            let progname = std::env::args().next().unwrap_or_else(|| "make".to_string());
            eprintln!("{}: *** {}", progname, e);
            process::exit(2);
        }
    }
}

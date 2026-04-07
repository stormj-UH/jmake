// Copyright (c) 2026 Jon-Erik G. Storm. All rights reserved.

use std::env;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct MakeArgs {
    pub makefiles: Vec<PathBuf>,
    pub targets: Vec<String>,
    pub variables: Vec<(String, String)>,
    pub jobs: usize,
    pub keep_going: bool,
    pub silent: bool,
    pub ignore_errors: bool,
    pub dry_run: bool,
    pub touch: bool,
    pub question: bool,
    pub print_directory: bool,
    pub no_print_directory: bool,
    pub environment_overrides: bool,
    pub no_builtin_rules: bool,
    pub no_builtin_variables: bool,
    pub print_data_base: bool,
    pub debug: Vec<String>,
    pub debug_short: bool, // true if -d was given (short flag), shows as 'd' in MAKEFLAGS
    pub directory: Option<PathBuf>,
    pub include_dirs: Vec<PathBuf>,
    pub old_file: Vec<String>,
    pub new_file: Vec<String>,
    pub warn_undefined_variables: bool,
    pub always_make: bool,
    pub version: bool,
    pub just_print: bool,
    pub trace: bool,
    pub check_symlink_times: bool,
    pub load_average: Option<f64>,
    pub max_load: Option<f64>,
    pub output_sync: Option<String>,
    pub eval_strings: Vec<String>,
    pub what_if: Vec<String>,
    pub no_silent: bool,  // explicitly set with --no-silent (for MAKEFLAGS output)
    pub clear_include_dirs: bool,  // -I- was passed (clear default include dirs)
    /// Index in `variables` where command-line variables start.
    /// Variables before this index came from the MAKEFLAGS environment variable.
    /// Variables at or after this index came from the command line.
    pub cmdline_vars_start: usize,

    // Toggle-flag explicit-set tracking.
    // True if the flag was explicitly set from any external source (env MAKEFLAGS or
    // command line).  Used by apply_makeflags_from_makefile to protect env/cmdline
    // settings from being overridden by a makefile MAKEFLAGS assignment.
    pub print_directory_explicit: bool,
    pub no_print_directory_explicit: bool,
    pub silent_explicit: bool,
    pub no_silent_explicit: bool,
    pub keep_going_explicit: bool,
    pub no_keep_going_explicit: bool,
    /// Path to temp file holding stdin content (from --temp-stdin=PATH on re-exec).
    /// When set, read this file instead of stdin for -f- makefiles.
    pub temp_stdin: Option<PathBuf>,
    /// True if GNUMAKEFLAGS was set in the environment (even if empty).
    /// When true, export GNUMAKEFLAGS= (empty) to recipe environments.
    pub gnumakeflags_was_set: bool,
    /// Shuffle mode for prerequisite ordering.
    /// None = no shuffling, Some(0) = random, Some(n) = seeded, Some(u64::MAX) = reverse
    pub shuffle: Option<ShuffleMode>,
}

/// Shuffle mode for --shuffle option
#[derive(Debug, Clone, PartialEq)]
pub enum ShuffleMode {
    /// Random order (no seed)
    Random,
    /// Seeded random order (--shuffle=N)
    Seeded(u64),
    /// Reverse order (--shuffle=reverse)
    Reverse,
    /// Normal order (--shuffle=none or --shuffle=identity)
    Identity,
}

impl Default for MakeArgs {
    fn default() -> Self {
        MakeArgs {
            makefiles: Vec::new(),
            targets: Vec::new(),
            variables: Vec::new(),
            jobs: 1,
            keep_going: false,
            silent: false,
            ignore_errors: false,
            dry_run: false,
            touch: false,
            question: false,
            print_directory: false,
            no_print_directory: false,
            environment_overrides: false,
            no_builtin_rules: false,
            no_builtin_variables: false,
            print_data_base: false,
            debug: Vec::new(),
            debug_short: false,
            directory: None,
            include_dirs: Vec::new(),
            old_file: Vec::new(),
            new_file: Vec::new(),
            warn_undefined_variables: false,
            always_make: false,
            version: false,
            just_print: false,
            trace: false,
            check_symlink_times: false,
            load_average: None,
            max_load: None,
            output_sync: None,
            eval_strings: Vec::new(),
            what_if: Vec::new(),
            no_silent: false,
            clear_include_dirs: false,
            cmdline_vars_start: 0,
            print_directory_explicit: false,
            no_print_directory_explicit: false,
            silent_explicit: false,
            no_silent_explicit: false,
            keep_going_explicit: false,
            no_keep_going_explicit: false,
            temp_stdin: None,
            gnumakeflags_was_set: false,
            shuffle: None,
        }
    }
}

fn require_arg(args: &[String], i: usize, opt: &str) -> String {
    if i < args.len() {
        args[i].clone()
    } else {
        let progname = args.get(0).map(|s| s.as_str()).unwrap_or("make");
        eprintln!("{}: option requires an argument -- '{}'", progname, opt);
        std::process::exit(2);
    }
}

pub fn parse_args() -> MakeArgs {
    let args: Vec<String> = env::args().collect();
    let mut result = MakeArgs::default();
    let mut i = 1;

    // Check GNUMAKEFLAGS environment variable (processed before MAKEFLAGS).
    // Flags from GNUMAKEFLAGS are merged into the effective flags but GNUMAKEFLAGS
    // itself is cleared in the environment so recursive makes don't see duplicates.
    if let Ok(gnumakeflags) = env::var("GNUMAKEFLAGS") {
        result.gnumakeflags_was_set = true;
        if !gnumakeflags.trim().is_empty() {
            parse_makeflags(&gnumakeflags, &mut result);
        }
        // Clear GNUMAKEFLAGS so it doesn't affect recursive makes (they get it
        // via MAKEFLAGS which already merged the flags).
        env::set_var("GNUMAKEFLAGS", "");
    }
    // Check MAKEFLAGS environment variable
    if let Ok(makeflags) = env::var("MAKEFLAGS") {
        parse_makeflags(&makeflags, &mut result);
    }
    // Record where command-line variables start (after env MAKEFLAGS vars).
    result.cmdline_vars_start = result.variables.len();

    while i < args.len() {
        let arg = &args[i];

        if arg == "--" {
            // Everything after -- is targets
            i += 1;
            while i < args.len() {
                result.targets.push(args[i].clone());
                i += 1;
            }
            break;
        }

        if arg.starts_with("--") {
            match arg.as_str() {
                "--version" => result.version = true,
                "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                "--always-make" => result.always_make = true,
                "--directory" => {
                    i += 1;
                    if i < args.len() {
                        result.directory = Some(PathBuf::from(&args[i]));
                    }
                }
                "--dry-run" | "--just-print" | "--recon" => {
                    result.dry_run = true;
                    result.just_print = true;
                }
                "--environment-overrides" => result.environment_overrides = true,
                "--file" | "--makefile" => {
                    i += 1;
                    let val = require_arg(&args, i, if arg == "--file" { "file" } else { "makefile" });
                    result.makefiles.push(PathBuf::from(val));
                }
                "--ignore-errors" => result.ignore_errors = true,
                "--include-dir" => {
                    i += 1;
                    if i < args.len() {
                        result.include_dirs.push(PathBuf::from(&args[i]));
                    }
                }
                "--jobs" => {
                    i += 1;
                    if i < args.len() {
                        result.jobs = args[i].parse().unwrap_or(1);
                    }
                }
                "--keep-going" => { result.keep_going = true; result.keep_going_explicit = true; result.no_keep_going_explicit = false; }
                "--no-builtin-rules" => result.no_builtin_rules = true,
                "--no-builtin-variables" => result.no_builtin_variables = true,
                "--no-print-directory" => { result.no_print_directory = true; result.print_directory = false; result.no_print_directory_explicit = true; result.print_directory_explicit = false; }
                "--print-directory" => { result.print_directory = true; result.no_print_directory = false; result.print_directory_explicit = true; result.no_print_directory_explicit = false; }
                "--print-data-base" => result.print_data_base = true,
                "--question" => result.question = true,
                "--silent" | "--quiet" => { result.silent = true; result.no_silent = false; result.silent_explicit = true; result.no_silent_explicit = false; }
                "--no-silent" => { result.silent = false; result.no_silent = true; result.no_silent_explicit = true; result.silent_explicit = false; }
                "--touch" => result.touch = true,
                "--trace" => result.trace = true,
                "--warn-undefined-variables" => result.warn_undefined_variables = true,
                "--check-symlink-times" => result.check_symlink_times = true,
                "--output-sync" => {
                    i += 1;
                    if i < args.len() {
                        result.output_sync = Some(args[i].clone());
                    }
                }
                "--eval" => {
                    i += 1;
                    if i < args.len() {
                        result.eval_strings.push(args[i].clone());
                    }
                }
                "--old-file" | "--assume-old" => {
                    i += 1;
                    if i < args.len() {
                        result.old_file.push(args[i].clone());
                    }
                }
                "--new-file" | "--assume-new" | "--what-if" => {
                    i += 1;
                    if i < args.len() {
                        result.new_file.push(args[i].clone());
                        result.what_if.push(args[i].clone());
                    }
                }
                s if s.starts_with("--debug") => {
                    if let Some(eq) = s.find('=') {
                        result.debug.push(s[eq+1..].to_string());
                    } else {
                        result.debug.push("b".to_string());
                    }
                }
                s if s.starts_with("--jobs=") => {
                    let val = &s[7..];
                    match val.parse::<usize>() {
                        Ok(n) => { result.jobs = n; }
                        Err(_) => {
                            let progname = args.get(0).map(|s| s.as_str()).unwrap_or("make");
                            eprintln!("{}: invalid option -- '--jobs={}'", progname, val);
                            std::process::exit(2);
                        }
                    }
                }
                s if s.starts_with("--directory=") => {
                    result.directory = Some(PathBuf::from(&s[12..]));
                }
                s if s.starts_with("--file=") || s.starts_with("--makefile=") => {
                    let val = if s.starts_with("--file=") { &s[7..] } else { &s[11..] };
                    result.makefiles.push(PathBuf::from(val));
                }
                s if s.starts_with("--include-dir=") => {
                    result.include_dirs.push(PathBuf::from(&s[14..]));
                }
                s if s.starts_with("--output-sync=") => {
                    result.output_sync = Some(s[14..].to_string());
                }
                s if s.starts_with("--eval=") => {
                    result.eval_strings.push(s[7..].to_string());
                }
                s if s.starts_with("--old-file=") || s.starts_with("--assume-old=") => {
                    let val = if s.starts_with("--old-file=") { &s[11..] } else { &s[13..] };
                    result.old_file.push(val.to_string());
                }
                s if s.starts_with("--new-file=") || s.starts_with("--assume-new=") || s.starts_with("--what-if=") => {
                    let eq = s.find('=').unwrap();
                    let val = &s[eq+1..];
                    result.new_file.push(val.to_string());
                    result.what_if.push(val.to_string());
                }
                s if s.starts_with("--load-average") => {
                    if let Some(eq) = s.find('=') {
                        result.load_average = s[eq+1..].parse().ok();
                    }
                }
                s if s.starts_with("--temp-stdin=") => {
                    result.temp_stdin = Some(PathBuf::from(&s[13..]));
                }
                // --shuffle[=VALUE]
                s if s.starts_with("--shuffle") => {
                    let val = if s == "--shuffle" {
                        // Next arg or default to random
                        // Check if next arg could be the value (but --shuffle can also be standalone)
                        "".to_string()
                    } else if let Some(rest) = s.strip_prefix("--shuffle=") {
                        rest.to_string()
                    } else {
                        "".to_string()
                    };
                    result.shuffle = Some(match val.as_str() {
                        "" => ShuffleMode::Random,
                        "reverse" => ShuffleMode::Reverse,
                        "none" | "identity" => ShuffleMode::Identity,
                        s => {
                            if let Ok(seed) = s.parse::<u64>() {
                                ShuffleMode::Seeded(seed)
                            } else {
                                ShuffleMode::Random
                            }
                        }
                    });
                }
                s if s == "--no-keep-going" || s == "--sync-output" || s.starts_with("--output-sync") => {
                    // Accepted but not implemented
                }
                _ => {
                    let raw = env::args().next().unwrap_or_else(|| "make".to_string());
                    let progname = std::path::Path::new(&raw)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&raw)
                        .to_string();
                    eprintln!("{}: invalid option -- '{}'", progname, arg);
                    eprintln!("Usage: {} [options] [target] ...", progname);
                    eprintln!("This program built for aarch64-unknown-linux-gnu");
                    std::process::exit(2);
                }
            }
        } else if arg.starts_with('-') && arg.len() > 1 {
            // Short options - can be combined like -ksn
            let chars: Vec<char> = arg[1..].chars().collect();
            let mut j = 0;
            while j < chars.len() {
                match chars[j] {
                    'B' => result.always_make = true,
                    'C' => {
                        // -C dir
                        let rest: String = chars[j+1..].iter().collect();
                        if !rest.is_empty() {
                            result.directory = Some(PathBuf::from(rest));
                        } else {
                            i += 1;
                            if i < args.len() {
                                result.directory = Some(PathBuf::from(&args[i]));
                            }
                        }
                        j = chars.len(); // consumed rest
                        continue;
                    }
                    'd' => {
                        result.debug_short = true;
                        result.debug.push("b".to_string());
                    }
                    'E' => {
                        let rest: String = chars[j+1..].iter().collect();
                        if !rest.is_empty() {
                            result.eval_strings.push(rest);
                        } else {
                            i += 1;
                            let val = require_arg(&args, i, "E");
                            result.eval_strings.push(val);
                        }
                        j = chars.len();
                        continue;
                    }
                    'e' => result.environment_overrides = true,
                    'f' => {
                        let rest: String = chars[j+1..].iter().collect();
                        if !rest.is_empty() {
                            result.makefiles.push(PathBuf::from(rest));
                        } else {
                            i += 1;
                            let val = require_arg(&args, i, "f");
                            result.makefiles.push(PathBuf::from(val));
                        }
                        j = chars.len();
                        continue;
                    }
                    'h' => {
                        print_help();
                        std::process::exit(0);
                    }
                    'i' => result.ignore_errors = true,
                    'I' => {
                        let rest: String = chars[j+1..].iter().collect();
                        if !rest.is_empty() {
                            if rest == "-" {
                                // -I- resets the include search path.  Keep "-" as a
                                // sentinel in include_dirs so build_makeflags can emit
                                // it and .INCLUDE_DIRS computation can detect the reset.
                                result.clear_include_dirs = true;
                                result.include_dirs.push(PathBuf::from("-"));
                            } else {
                                result.include_dirs.push(PathBuf::from(rest));
                            }
                        } else {
                            i += 1;
                            if i < args.len() {
                                if args[i] == "-" {
                                    result.clear_include_dirs = true;
                                    result.include_dirs.push(PathBuf::from("-"));
                                } else {
                                    result.include_dirs.push(PathBuf::from(&args[i]));
                                }
                            }
                        }
                        j = chars.len();
                        continue;
                    }
                    'j' => {
                        let rest: String = chars[j+1..].iter().collect();
                        if !rest.is_empty() {
                            // -j<value>: if value is not a valid number, it's an error.
                            match rest.parse::<usize>() {
                                Ok(n) => { result.jobs = n; }
                                Err(_) => {
                                    let progname = args.get(0).map(|s| s.as_str()).unwrap_or("make");
                                    eprintln!("{}: invalid option -- 'j{}'", progname, rest);
                                    std::process::exit(2);
                                }
                            }
                            j = chars.len();
                            continue;
                        } else {
                            // -j <value>: if next arg is not a number, don't consume it
                            // (it will be treated as a target).
                            if i + 1 < args.len() {
                                if let Ok(n) = args[i + 1].parse::<usize>() {
                                    i += 1;
                                    result.jobs = n;
                                }
                                // else: leave i unchanged; next arg becomes a target
                            }
                        }
                    }
                    'k' => { result.keep_going = true; result.keep_going_explicit = true; result.no_keep_going_explicit = false; }
                    'L' => {
                        // -L = --check-symlink-times (no argument)
                        result.check_symlink_times = true;
                    }
                    'l' => {
                        let rest: String = chars[j+1..].iter().collect();
                        if !rest.is_empty() {
                            result.load_average = rest.parse().ok();
                            j = chars.len();
                            continue;
                        } else {
                            i += 1;
                            if i < args.len() {
                                result.load_average = args[i].parse().ok();
                            }
                        }
                    }
                    'n' => {
                        result.dry_run = true;
                        result.just_print = true;
                    }
                    'O' => {
                        let rest: String = chars[j+1..].iter().collect();
                        if !rest.is_empty() {
                            result.output_sync = Some(rest);
                        } else {
                            i += 1;
                            if i < args.len() {
                                result.output_sync = Some(args[i].clone());
                            }
                        }
                        j = chars.len();
                        continue;
                    }
                    'o' => {
                        let rest: String = chars[j+1..].iter().collect();
                        if !rest.is_empty() {
                            result.old_file.push(rest);
                        } else {
                            i += 1;
                            if i < args.len() {
                                result.old_file.push(args[i].clone());
                            }
                        }
                        j = chars.len();
                        continue;
                    }
                    'p' => result.print_data_base = true,
                    'q' => result.question = true,
                    'r' => result.no_builtin_rules = true,
                    'R' => result.no_builtin_variables = true,
                    's' => { result.silent = true; result.no_silent = false; result.silent_explicit = true; result.no_silent_explicit = false; }
                    'S' => { result.keep_going = false; result.no_keep_going_explicit = true; result.keep_going_explicit = false; } // --no-keep-going
                    't' => result.touch = true,
                    'v' => result.version = true,
                    'w' => { result.print_directory = true; result.no_print_directory = false; result.print_directory_explicit = true; result.no_print_directory_explicit = false; }
                    'W' => {
                        let rest: String = chars[j+1..].iter().collect();
                        if !rest.is_empty() {
                            result.new_file.push(rest.clone());
                            result.what_if.push(rest);
                        } else {
                            i += 1;
                            if i < args.len() {
                                result.new_file.push(args[i].clone());
                                result.what_if.push(args[i].clone());
                            }
                        }
                        j = chars.len();
                        continue;
                    }
                    _ => {}
                }
                j += 1;
            }
        } else if let Some(eq_pos) = arg.find('=') {
            // VAR=VALUE on command line
            let name = arg[..eq_pos].to_string();
            let value = arg[eq_pos+1..].to_string();
            result.variables.push((name, value));
        } else {
            // Target
            result.targets.push(arg.clone());
        }

        i += 1;
    }

    result
}

pub fn parse_makeflags(flags: &str, result: &mut MakeArgs) {
    // MAKEFLAGS format:
    //   - May start with single-letter flags bundled (no leading '-'), e.g. "erR"
    //   - Followed by long options: "--trace --no-print-directory"
    //   - Options with args like "-Idir", "-l2.5", "-Onone", "--debug=b"
    //   - Variable assignments after "--": "-- FOO=bar"
    //
    // We tokenize and parse similarly to command-line args, but the first token
    // (if it doesn't start with '-') is treated as bundled single-char flags.

    let trimmed = flags.trim();
    if trimmed.is_empty() {
        return;
    }

    // Split into tokens by whitespace but respect that "-I" args may be attached
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    let mut i = 0;
    let mut past_dashdash = false;
    let mut first_token = true;

    while i < tokens.len() {
        let token = tokens[i];

        if past_dashdash && !token.starts_with('-') {
            // Variable assignment: NAME=value or NAME:=value etc.
            // Add to variables so they're included in MAKEFLAGS output
            // and applied as command-line-level overrides in sub-makes.
            if let Some(eq_pos) = token.find('=') {
                let name = token[..eq_pos].to_string();
                let value = token[eq_pos+1..].to_string();
                result.variables.push((name, value));
            }
            i += 1;
            continue;
        }

        if token == "--" {
            past_dashdash = true;
            i += 1;
            continue;
        }

        if token.starts_with("--") {
            first_token = false;
            match token {
                "--always-make" => result.always_make = true,
                "--environment-overrides" => result.environment_overrides = true,
                "--ignore-errors" => result.ignore_errors = true,
                "--keep-going" => { result.keep_going = true; result.keep_going_explicit = true; result.no_keep_going_explicit = false; }
                "--no-keep-going" | "--stop" => { result.keep_going = false; result.no_keep_going_explicit = true; result.keep_going_explicit = false; }
                "--dry-run" | "--just-print" | "--recon" => {
                    result.dry_run = true;
                    result.just_print = true;
                }
                "--no-builtin-rules" => result.no_builtin_rules = true,
                "--no-builtin-variables" => result.no_builtin_variables = true,
                "--no-print-directory" => {
                    result.no_print_directory = true;
                    result.print_directory = false;
                    result.no_print_directory_explicit = true;
                    result.print_directory_explicit = false;
                }
                "--print-directory" => {
                    result.print_directory = true;
                    result.no_print_directory = false;
                    result.print_directory_explicit = true;
                    result.no_print_directory_explicit = false;
                }
                "--silent" | "--quiet" => { result.silent = true; result.no_silent = false; result.silent_explicit = true; result.no_silent_explicit = false; }
                "--no-silent" => { result.silent = false; result.no_silent = true; result.no_silent_explicit = true; result.silent_explicit = false; }
                "--touch" => result.touch = true,
                "--trace" => result.trace = true,
                "--warn-undefined-variables" => result.warn_undefined_variables = true,
                "--check-symlink-times" => result.check_symlink_times = true,
                _ if token.starts_with("--debug=") => {
                    result.debug.push(token[8..].to_string());
                }
                _ if token.starts_with("--jobs=") => {
                    result.jobs = token[7..].parse().unwrap_or(1);
                }
                _ if token.starts_with("--output-sync=") => {
                    result.output_sync = Some(token[14..].to_string());
                }
                _ if token.starts_with("--include-dir=") => {
                    result.include_dirs.push(std::path::PathBuf::from(&token[14..]));
                }
                _ if token.starts_with("--load-average=") => {
                    result.load_average = token[15..].parse().ok();
                }
                _ => {}
            }
            i += 1;
            continue;
        }

        if token.starts_with('-') {
            first_token = false;
            // Short options with '-' prefix (like -Idir, -l2.5, -Onone)
            let rest = &token[1..];
            if rest.is_empty() {
                i += 1;
                continue;
            }
            let chars: Vec<char> = rest.chars().collect();
            let mut j = 0;
            while j < chars.len() {
                match chars[j] {
                    'B' => result.always_make = true,
                    'e' => result.environment_overrides = true,
                    'i' => result.ignore_errors = true,
                    'I' => {
                        let arg: String = chars[j+1..].iter().collect();
                        if !arg.is_empty() {
                            result.include_dirs.push(std::path::PathBuf::from(arg));
                        } else if i + 1 < tokens.len() {
                            i += 1;
                            result.include_dirs.push(std::path::PathBuf::from(tokens[i]));
                        }
                        j = chars.len();
                        continue;
                    }
                    'k' => { result.keep_going = true; result.keep_going_explicit = true; result.no_keep_going_explicit = false; }
                    'L' => {
                        // -L = --check-symlink-times (no argument)
                        result.check_symlink_times = true;
                    }
                    'l' => {
                        let arg: String = chars[j+1..].iter().collect();
                        if !arg.is_empty() {
                            result.load_average = arg.parse().ok();
                        } else if i + 1 < tokens.len() {
                            i += 1;
                            result.load_average = tokens[i].parse().ok();
                        }
                        j = chars.len();
                        continue;
                    }
                    'n' => {
                        result.dry_run = true;
                        result.just_print = true;
                    }
                    'O' => {
                        let arg: String = chars[j+1..].iter().collect();
                        if !arg.is_empty() {
                            result.output_sync = Some(arg);
                        } else if i + 1 < tokens.len() {
                            i += 1;
                            result.output_sync = Some(tokens[i].to_string());
                        }
                        j = chars.len();
                        continue;
                    }
                    'q' => result.question = true,
                    'r' => result.no_builtin_rules = true,
                    'R' => result.no_builtin_variables = true,
                    's' => { result.silent = true; result.no_silent = false; result.silent_explicit = true; result.no_silent_explicit = false; }
                    'S' => { result.keep_going = false; result.no_keep_going_explicit = true; result.keep_going_explicit = false; }
                    't' => result.touch = true,
                    'w' => {
                        result.print_directory = true;
                        result.no_print_directory = false;
                        result.print_directory_explicit = true;
                        result.no_print_directory_explicit = false;
                    }
                    'd' => {
                        result.debug_short = true;
                        result.debug.push("b".to_string());
                    }
                    _ => {}
                }
                j += 1;
            }
            i += 1;
            continue;
        }

        // Token without '-' prefix: either bundled single-char flags (e.g. "erR", "B")
        // OR a bare variable assignment (e.g. "hello=world" from MAKEFLAGS env).
        // Check if it contains '=' to distinguish variable from flags.
        // NOTE: GNU Make allows multiple bundled-flag tokens, e.g. "i B" means flags i AND B.
        first_token = false;
        if token.contains('=') {
            // This is a bare variable assignment (e.g. "hello=world" from MAKEFLAGS env).
            // Add to variables so it's included in MAKEFLAGS output.
            if let Some(eq_pos) = token.find('=') {
                let name = token[..eq_pos].to_string();
                let value = token[eq_pos+1..].to_string();
                result.variables.push((name, value));
            }
        } else {
            // Bundled single-char flags
            for ch in token.chars() {
                match ch {
                    'B' => result.always_make = true,
                    'e' => result.environment_overrides = true,
                    'i' => result.ignore_errors = true,
                    'k' => { result.keep_going = true; result.keep_going_explicit = true; result.no_keep_going_explicit = false; }
                    'n' => {
                        result.dry_run = true;
                        result.just_print = true;
                    }
                    'q' => result.question = true,
                    'r' => result.no_builtin_rules = true,
                    'R' => result.no_builtin_variables = true,
                    's' => { result.silent = true; result.no_silent = false; result.silent_explicit = true; result.no_silent_explicit = false; }
                    'S' => { result.keep_going = false; result.no_keep_going_explicit = true; result.keep_going_explicit = false; }
                    't' => result.touch = true,
                    'w' => {
                        result.print_directory = true;
                        result.no_print_directory = false;
                        result.print_directory_explicit = true;
                        result.no_print_directory_explicit = false;
                    }
                    'L' => result.check_symlink_times = true,
                    'd' => {
                        result.debug_short = true;
                        result.debug.push("b".to_string());
                    }
                    _ => {}
                }
            }
        }

        i += 1;
    }
}

fn print_help() {
    println!("Usage: jmake [options] [target] ...");
    println!("Options:");
    println!("  -b, -m                      Ignored for compatibility.");
    println!("  -B, --always-make           Unconditionally make all targets.");
    println!("  -C DIRECTORY, --directory=DIRECTORY");
    println!("                              Change to DIRECTORY before doing anything.");
    println!("  -d                          Print lots of debugging information.");
    println!("  --debug[=FLAGS]             Print various types of debugging information.");
    println!("  -e, --environment-overrides");
    println!("                              Environment variables override makefiles.");
    println!("  -f FILE, --file=FILE, --makefile=FILE");
    println!("                              Read FILE as a makefile.");
    println!("  -h, --help                  Print this message and exit.");
    println!("  -i, --ignore-errors         Ignore errors from recipes.");
    println!("  -I DIRECTORY, --include-dir=DIRECTORY");
    println!("                              Search DIRECTORY for included makefiles.");
    println!("  -j [N], --jobs[=N]          Allow N jobs at once; infinite jobs with no arg.");
    println!("  -k, --keep-going            Keep going when some targets can't be made.");
    println!("  -l [N], --load-average[=N]  Don't start multiple jobs unless load is below N.");
    println!("  -L, --check-symlink-times   Use the latest mtime between symlinks and target.");
    println!("  -n, --just-print, --dry-run, --recon");
    println!("                              Don't actually run any recipe; just print them.");
    println!("  -o FILE, --old-file=FILE, --assume-old=FILE");
    println!("                              Consider FILE to be very old and don't remake it.");
    println!("  -O[TYPE], --output-sync[=TYPE]");
    println!("                              Synchronize output of parallel jobs by TYPE.");
    println!("  -p, --print-data-base       Print make's internal database.");
    println!("  -q, --question              Run no recipe; exit status says if up to date.");
    println!("  -r, --no-builtin-rules      Disable the built-in implicit rules.");
    println!("  -R, --no-builtin-variables  Disable the built-in variable settings.");
    println!("  -s, --silent, --quiet       Don't echo recipes.");
    println!("  -S, --no-keep-going, --stop");
    println!("                              Turns off -k.");
    println!("  -t, --touch                 Touch targets instead of remaking them.");
    println!("  -v, --version               Print the version number of make and exit.");
    println!("  -w, --print-directory       Print the current directory.");
    println!("  -W FILE, --what-if=FILE, --new-file=FILE, --assume-new=FILE");
    println!("                              Consider FILE to be infinitely new.");
    println!("  --warn-undefined-variables  Warn when an undefined variable is referenced.");
    println!();
    println!("This program built for aarch64-unknown-linux-gnu");
    println!("Report bugs to <bug-make@gnu.org>");
}

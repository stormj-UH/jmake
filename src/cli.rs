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

    // Check MAKEFLAGS environment variable
    if let Ok(makeflags) = env::var("MAKEFLAGS") {
        parse_makeflags(&makeflags, &mut result);
    }

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
                "--keep-going" => result.keep_going = true,
                "--no-builtin-rules" => result.no_builtin_rules = true,
                "--no-builtin-variables" => result.no_builtin_variables = true,
                "--no-print-directory" => result.no_print_directory = true,
                "--print-directory" => result.print_directory = true,
                "--print-data-base" => result.print_data_base = true,
                "--question" => result.question = true,
                "--silent" | "--quiet" => result.silent = true,
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
                    result.jobs = val.parse().unwrap_or(1);
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
                _ => {
                    eprintln!("jmake: Unknown option '{}'", arg);
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
                            result.include_dirs.push(PathBuf::from(rest));
                        } else {
                            i += 1;
                            if i < args.len() {
                                result.include_dirs.push(PathBuf::from(&args[i]));
                            }
                        }
                        j = chars.len();
                        continue;
                    }
                    'j' => {
                        let rest: String = chars[j+1..].iter().collect();
                        if !rest.is_empty() {
                            result.jobs = rest.parse().unwrap_or(1);
                            j = chars.len();
                            continue;
                        } else {
                            i += 1;
                            if i < args.len() {
                                result.jobs = args[i].parse().unwrap_or(1);
                            }
                        }
                    }
                    'k' => result.keep_going = true,
                    'l' | 'L' => {
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
                    's' => result.silent = true,
                    'S' => result.keep_going = false, // --no-keep-going
                    't' => result.touch = true,
                    'v' => result.version = true,
                    'w' => result.print_directory = true,
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

        if past_dashdash {
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
                "--keep-going" => result.keep_going = true,
                "--no-keep-going" | "--stop" => result.keep_going = false,
                "--dry-run" | "--just-print" | "--recon" => {
                    result.dry_run = true;
                    result.just_print = true;
                }
                "--no-builtin-rules" => result.no_builtin_rules = true,
                "--no-builtin-variables" => result.no_builtin_variables = true,
                "--no-print-directory" => {
                    result.no_print_directory = true;
                    result.print_directory = false;
                }
                "--print-directory" => {
                    result.print_directory = true;
                    result.no_print_directory = false;
                }
                "--silent" | "--quiet" => result.silent = true,
                "--no-silent" => result.silent = false,
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
                    'k' => result.keep_going = true,
                    'l' | 'L' => {
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
                    's' => result.silent = true,
                    'S' => result.keep_going = false,
                    't' => result.touch = true,
                    'w' => {
                        result.print_directory = true;
                        result.no_print_directory = false;
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

        // First token without '-' prefix: either bundled single-char flags (e.g. "erR")
        // OR a bare variable assignment (e.g. "hello=world" from MAKEFLAGS env).
        // Check if it contains '=' to distinguish variable from flags.
        if first_token {
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
                        'k' => result.keep_going = true,
                        'n' => {
                            result.dry_run = true;
                            result.just_print = true;
                        }
                        'q' => result.question = true,
                        'r' => result.no_builtin_rules = true,
                        'R' => result.no_builtin_variables = true,
                        's' => result.silent = true,
                        'S' => result.keep_going = false,
                        't' => result.touch = true,
                        'w' => {
                            result.print_directory = true;
                            result.no_print_directory = false;
                        }
                        'd' => {
                            result.debug_short = true;
                            result.debug.push("b".to_string());
                        }
                        _ => {}
                    }
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
}

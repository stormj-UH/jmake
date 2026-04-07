// Copyright (c) 2026 Jon-Erik G. Storm. All rights reserved.
// Evaluation engine - variable expansion, rule processing, main state machine

mod expand;

pub use expand::*;

use crate::cli::MakeArgs;
use crate::database::MakeDatabase;
use crate::exec;
use crate::functions;
use crate::implicit_rules;
use crate::parser::{self, Parser};
use crate::types::*;

use indexmap::IndexMap;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};

/// A pending include that couldn't be resolved during initial makefile reading.
#[derive(Clone)]
pub struct PendingInclude {
    pub file: String,
    pub parent: String,
    pub lineno: usize,
    pub ignore_missing: bool,
}

/// Information about a rule found for building an include file.
struct IncludeRuleInfo {
    recipe: Vec<(usize, String)>,
    source_file: String,
    recipe_lineno: usize,
    prerequisites: Vec<String>,
    /// True if this rule should be skipped for include-rebuild purposes
    /// (e.g. double-colon with no prerequisites).
    skippable: bool,
    /// For pattern rules with multiple target patterns: the sibling target names
    /// (i.e., the other targets that would be built by the same recipe invocation).
    /// When this recipe runs for one target, all siblings are also considered attempted.
    sibling_targets: Vec<String>,
}

pub struct MakeState {
    pub args: MakeArgs,
    /// Snapshot of args as parsed from the command line (before any makefile modifications).
    /// Used by apply_makeflags_from_makefile to preserve cmdline flags even when the
    /// makefile assigns directly to MAKEFLAGS (e.g. `MAKEFLAGS = B`).
    pub cmdline_args: MakeArgs,
    pub db: MakeDatabase,
    pub shell: String,
    pub makefile_list: Vec<PathBuf>,
    pub include_dirs: Vec<PathBuf>,
    pub eval_pending: RefCell<Vec<String>>, // pending $(eval ...) strings
    /// Current file and line for error/warning/info context (updated during parsing)
    pub current_file: RefCell<String>,
    pub current_line: RefCell<usize>,
    /// Exit status of the last $(shell ...) call, used to set .SHELLSTATUS
    pub last_shell_status: RefCell<i32>,
    /// Set to true while performing second expansion of prerequisites.
    /// When eval() is called in this context, it must not create new rules
    /// (GNU Make error: "prerequisites cannot be defined in recipes").
    pub in_second_expansion: RefCell<bool>,
    /// Pending includes that couldn't be found during initial read
    pub pending_includes: Vec<PendingInclude>,
    /// Set of include file names for which a rebuild recipe has already been
    /// attempted (ran or was considered ran via grouped pattern rules).
    /// Used to avoid running the same recipe twice for grouped pattern rules.
    pub include_recipe_ran: HashSet<String>,
    /// True once "Entering directory" has been printed, to avoid printing it twice.
    pub entering_directory_printed: bool,
}

/// Return the current working directory preferring the logical path from PWD env var.
/// On macOS, getcwd() returns the canonical path (e.g. /private/tmp) but PWD holds
/// the logical path (e.g. /tmp). GNU Make uses the logical path for display.
pub fn logical_cwd() -> std::path::PathBuf {
    if let Ok(pwd) = env::var("PWD") {
        let pwd_path = std::path::PathBuf::from(&pwd);
        // Validate that PWD actually points to the same directory as getcwd()
        // (it could be stale if someone cd'd without setting PWD)
        if let Ok(canonical_pwd) = pwd_path.canonicalize() {
            if let Ok(actual) = env::current_dir() {
                if let Ok(canonical_actual) = actual.canonicalize() {
                    if canonical_pwd == canonical_actual {
                        return pwd_path;
                    }
                }
            }
        }
    }
    env::current_dir().unwrap_or_default()
}

/// Build the progname string with optional MAKELEVEL suffix (e.g. "jmake[1]").
/// The level is read from the MAKELEVEL environment variable (set by parent make).
pub fn make_progname() -> String {
    let base = env::args().next().unwrap_or_else(|| "make".to_string());
    match env::var("MAKELEVEL").ok().and_then(|v| v.parse::<u32>().ok()) {
        Some(level) if level > 0 => format!("{}[{}]", base, level),
        _ => base,
    }
}

/// Determine whether "entering/leaving directory" messages should be printed.
fn should_print_directory(args: &crate::cli::MakeArgs) -> bool {
    if args.no_print_directory {
        return false;
    }
    if args.print_directory {
        return true;
    }
    // Recursive makes (MAKELEVEL > 0) automatically print directory messages
    matches!(env::var("MAKELEVEL").ok().and_then(|v| v.parse::<u32>().ok()), Some(level) if level > 0)
}

impl MakeState {
    pub fn new(args: MakeArgs) -> Self {
        let cmdline_args = args.clone();
        let mut state = MakeState {
            args,
            cmdline_args,
            db: MakeDatabase::new(),
            shell: "/bin/sh".to_string(),
            makefile_list: Vec::new(),
            include_dirs: Vec::new(),
            eval_pending: RefCell::new(Vec::new()),
            current_file: RefCell::new(String::new()),
            current_line: RefCell::new(0),
            last_shell_status: RefCell::new(0),
            in_second_expansion: RefCell::new(false),
            pending_includes: Vec::new(),
            include_recipe_ran: HashSet::new(),
            entering_directory_printed: false,
        };

        // Change directory if requested
        let progname = make_progname();
        if let Some(ref dir) = state.args.directory {
            if let Err(e) = env::set_current_dir(dir) {
                eprintln!("{}: Entering directory '{}'", progname, dir.display());
                eprintln!("{}: *** {}: {}.  Stop.", progname, dir.display(), e);
                std::process::exit(2);
            }
            // Update PWD to the new directory using the logical path.
            // We compute a new logical path by joining the old logical cwd with
            // the -C argument and normalizing, so that symlinks in the parent path
            // are preserved (matching shell `cd` behavior).
            let new_logical = {
                let old_pwd = env::var("PWD").map(std::path::PathBuf::from)
                    .unwrap_or_else(|_| env::current_dir().unwrap_or_default());
                let joined = if std::path::Path::new(dir).is_absolute() {
                    std::path::PathBuf::from(dir)
                } else {
                    old_pwd.join(dir)
                };
                // Normalize (remove .. and . components) without resolving symlinks
                let mut normalized = std::path::PathBuf::new();
                for comp in joined.components() {
                    match comp {
                        std::path::Component::ParentDir => { normalized.pop(); }
                        std::path::Component::CurDir => {}
                        _ => normalized.push(comp),
                    }
                }
                normalized
            };
            env::set_var("PWD", &new_logical);
            if should_print_directory(&state.args) {
                let cwd = logical_cwd();
                eprintln!("{}: Entering directory '{}'", progname, cwd.display());
                state.entering_directory_printed = true;
            }
        }

        // Set up include directories
        state.include_dirs = state.args.include_dirs.clone();

        state
    }

    pub fn run(&mut self) -> Result<(), String> {
        self.init_variables();

        if !self.args.no_builtin_rules && !self.args.no_builtin_variables {
            implicit_rules::register_default_variables(&mut self.db);
        }
        if !self.args.no_builtin_rules {
            implicit_rules::register_implicit_rules(&mut self.db);
        }

        // Print entering-directory if needed (for -w without -C)
        // Must be BEFORE read_makefiles so $(info ...) at parse time appears after the header.
        let progname = make_progname();
        let print_dir = should_print_directory(&self.args);
        if print_dir && !self.entering_directory_printed {
            // Already printed at startup for -C; print here for -w without -C
            let cwd = logical_cwd();
            eprintln!("{}: Entering directory '{}'", progname, cwd.display());
            self.entering_directory_printed = true;
        }

        // Process any --eval (-E) strings BEFORE reading makefiles.
        // This matches GNU Make behavior where -E content is prepended to the makefile.
        // Doing this first ensures rules from -E are available when processing MAKEFILES
        // env var includes and the default makefile search.
        for eval_str in self.args.eval_strings.clone() {
            self.eval_string(&eval_str)?;
        }

        // Read makefiles (MAKEFILES env var + default/specified makefiles)
        self.read_makefiles()?;

        // Process pending $(eval) calls
        loop {
            let pending: Vec<String> = std::mem::take(&mut *self.eval_pending.borrow_mut());
            if pending.is_empty() {
                break;
            }
            for s in pending {
                self.eval_string(&s)?;
            }
        }

        // Resolve pending includes (rebuild include files if rules exist)
        self.resolve_pending_includes()?;

        // Set SHELL
        if let Some(var) = self.db.variables.get("SHELL") {
            self.shell = var.value.clone();
        }
        // .SHELLFLAGS
        let shell_flags = self.db.variables.get(".SHELLFLAGS")
            .map(|v| v.value.clone())
            .unwrap_or_else(|| "-c".to_string());

        // Determine targets
        let targets = if self.args.targets.is_empty() {
            match &self.db.default_target {
                Some(t) => vec![t.clone()],
                None => {
                    return Err("No targets.  Stop.".to_string());
                }
            }
        } else {
            self.args.targets.clone()
        };

        // GNU Make: -R (--no-builtin-variables) implies -r (--no-builtin-rules).
        // Apply this implication after all makefiles are read (so it doesn't affect
        // parse-time $(info $(MAKEFLAGS)) calls) but before building targets (so it
        // does affect recipe-time $(info $(MAKEFLAGS))).
        if self.args.no_builtin_variables && !self.args.no_builtin_rules {
            self.args.no_builtin_rules = true;
            // Rebuild MAKEFLAGS so the recipe-level $(info $(MAKEFLAGS)) includes 'r'.
            let new_makeflags = self.build_makeflags();
            if let Some(var) = self.db.variables.get_mut("MAKEFLAGS") {
                var.value = new_makeflags.clone();
            }
            env::set_var("MAKEFLAGS", &new_makeflags);
        }

        // Print database and exit if -p
        if self.args.print_data_base {
            self.print_database();
            if self.args.question || self.args.targets.is_empty() {
                return Ok(());
            }
        }

        // --debug=b (basic debug) output: announce that makefiles have been read
        // and we are about to update/build targets.  GNU Make prints this just before
        // it attempts to re-read any out-of-date makefiles.  We print it here so that
        // the string "Updating makefiles" appears in the output whenever --debug=b is
        // active (from any source: cmdline, env, or makefile assignment).
        let debug_basic = self.args.debug_short
            || self.args.debug.iter().any(|d| d == "b" || d == "basic" || d == "a" || d == "all");
        if debug_basic {
            println!("Updating makefiles....");
        }

        // Build targets
        let mut executor = exec::Executor::new(
            &self.db,
            self,
            self.args.jobs,
            self.args.keep_going,
            self.args.dry_run,
            self.args.touch,
            self.args.question,
            self.args.silent,
            self.args.ignore_errors,
            &self.shell,
            &shell_flags,
            self.args.always_make,
            self.args.trace,
            progname.clone(),
            self.args.what_if.clone(),
        );

        let result = executor.build_targets(&targets);

        // Print leaving-directory if needed
        let print_dir = should_print_directory(&self.args);
        if print_dir {
            let cwd = logical_cwd();
            eprintln!("{}: Leaving directory '{}'", progname, cwd.display());
        }

        result
    }

    fn init_variables(&mut self) {
        let cwd = logical_cwd();

        // Set up built-in variables
        self.db.variables.insert("MAKE_VERSION".into(),
            Variable::new("4.4.1".into(), VarFlavor::Simple, VarOrigin::Default));
        self.db.variables.insert("MAKE".into(),
            Variable::new(env::args().next().unwrap_or_else(|| "make".into()), VarFlavor::Recursive, VarOrigin::Default));
        self.db.variables.insert("MAKE_COMMAND".into(),
            Variable::new(env::args().next().unwrap_or_else(|| "make".into()), VarFlavor::Simple, VarOrigin::Default));
        self.db.variables.insert("CURDIR".into(),
            Variable::new(cwd.to_string_lossy().to_string(), VarFlavor::Simple, VarOrigin::Default));
        self.db.variables.insert(".FEATURES".into(),
            Variable::new("target-specific order-only second-expansion else-if shortest-stem undefine oneshell check-symlink".into(),
            VarFlavor::Simple, VarOrigin::Default));
        // .INCLUDE_DIRS: the effective include path.
        // When -I- is given, the default dirs are excluded (only explicit -Idir after -I- count).
        // When -Idir is given without -I-, dirs are added to the defaults.
        {
            let has_reset = self.args.include_dirs.iter().any(|d| d.to_string_lossy() == "-");
            let explicit_dirs: Vec<String> = if has_reset {
                // Only include dirs that appear AFTER the last -I-
                let mut after_reset = Vec::new();
                let mut found_reset = false;
                for d in self.args.include_dirs.iter().rev() {
                    if d.to_string_lossy() == "-" {
                        found_reset = true;
                        break;
                    }
                    after_reset.push(d.to_string_lossy().to_string());
                }
                after_reset.into_iter().rev().collect()
            } else {
                // No -I-, include all explicit dirs plus system defaults
                let mut dirs: Vec<String> = self.args.include_dirs.iter()
                    .map(|d| d.to_string_lossy().to_string())
                    .collect();
                // Append system defaults
                for sys in &["/usr/local/include", "/usr/include"] {
                    if !dirs.iter().any(|d| d == sys) {
                        dirs.push(sys.to_string());
                    }
                }
                dirs
            };
            self.db.variables.insert(".INCLUDE_DIRS".into(),
                Variable::new(explicit_dirs.join(" "), VarFlavor::Simple, VarOrigin::Default));
        }
        self.db.variables.insert("SHELL".into(),
            Variable::new("/bin/sh".into(), VarFlavor::Simple, VarOrigin::Default));
        self.db.variables.insert(".SHELLFLAGS".into(),
            Variable::new("-c".into(), VarFlavor::Simple, VarOrigin::Default));
        {
            let mf = self.build_makeflags();
            // Also update the process environment so $(shell echo "$MAKEFLAGS") reflects
            // the canonical merged value from the start.
            env::set_var("MAKEFLAGS", &mf);
            self.db.variables.insert("MAKEFLAGS".into(),
                Variable::new(mf, VarFlavor::Recursive, VarOrigin::Default));
        }
        // MAKEOVERRIDES: command-line variable assignments portion of MAKEFLAGS.
        {
            let ov: Vec<String> = self.args.variables.iter()
                .map(|(n, v)| format!("{}={}", n, v))
                .collect();
            self.db.variables.insert("MAKEOVERRIDES".into(),
                Variable::new(ov.join(" "), VarFlavor::Recursive, VarOrigin::Default));
        }
        // MAKECMDGOALS: the list of targets specified on the command line.
        // Set before reading makefiles so it's available during makefile processing.
        let cmdgoals = self.args.targets.join(" ");
        self.db.variables.insert("MAKECMDGOALS".into(),
            Variable::new(cmdgoals, VarFlavor::Simple, VarOrigin::Default));
        // MAKELEVEL: 0 for top-level, incremented for recursive makes.
        // Read from the environment (set by the parent make), defaulting to "0".
        let makelevel_str = env::var("MAKELEVEL").unwrap_or_else(|_| "0".to_string());
        self.db.variables.insert("MAKELEVEL".into(),
            Variable::new(makelevel_str, VarFlavor::Simple, VarOrigin::Default));
        self.db.variables.insert(".DEFAULT_GOAL".into(),
            Variable::new(String::new(), VarFlavor::Recursive, VarOrigin::Default));
        self.db.variables.insert(".RECIPEPREFIX".into(),
            Variable::new(String::new(), VarFlavor::Simple, VarOrigin::Default));
        self.db.variables.insert("SUFFIXES".into(),
            Variable::new(self.db.suffixes.join(" "), VarFlavor::Simple, VarOrigin::Default));

        // Import environment variables and record which names came from the environment.
        // Variables that originally came from the environment are always exported to
        // child processes (with their current Make value, which may be overridden by
        // the Makefile), unless explicitly unexported.
        for (key, value) in env::vars() {
            self.db.env_var_names.insert(key.clone());
            self.db.variables.entry(key.clone()).or_insert_with(|| {
                Variable::new(value.clone(), VarFlavor::Recursive, VarOrigin::Environment)
            });
        }

        // Command-line variables override everything; GNU Make stores them as recursive
        for (name, value) in &self.args.variables {
            self.db.variables.insert(name.clone(),
                Variable::new(value.clone(), VarFlavor::Recursive, VarOrigin::CommandLine));
        }
    }

    fn build_makeflags(&self) -> String {
        // GNU Make MAKEFLAGS format:
        //   Single-letter flags are bundled without a leading '-': e.g. "ks" for -k -s
        //   Long options follow (space-separated) with '-' prefix: "--trace --no-print-directory"
        //   If there are only long options (no single-letter flags), the value starts with a space
        //   Options with arguments (-Idir, -l2.5, -Onone, --debug=b) follow the long options
        //   Command-line variable assignments are appended after "-- ": "ks -- FOO=bar"
        self.build_makeflags_from_args(&self.args.variables)
    }

    pub fn build_makeflags_from_args(&self, variables: &[(String, String)]) -> String {
        let mut single_flags = String::new();
        if self.args.always_make { single_flags.push('B'); }
        if self.args.debug_short { single_flags.push('d'); }
        if self.args.environment_overrides { single_flags.push('e'); }
        if self.args.ignore_errors { single_flags.push('i'); }
        if self.args.keep_going {
            single_flags.push('k');
        } else if self.args.no_keep_going_explicit {
            // -S was explicitly set; output 'S' to indicate keep-going was disabled.
            single_flags.push('S');
        }
        if self.args.dry_run { single_flags.push('n'); }
        if self.args.question { single_flags.push('q'); }
        if self.args.no_builtin_rules { single_flags.push('r'); }
        if self.args.no_builtin_variables { single_flags.push('R'); }
        if self.args.silent { single_flags.push('s'); }
        if self.args.touch { single_flags.push('t'); }
        if self.args.print_directory { single_flags.push('w'); }
        if self.args.check_symlink_times { single_flags.push('L'); }

        // Build long_parts: options-with-args first, then no-arg long options.
        // GNU Make orders them: -I, -l, -O, --debug=, then --trace, --no-print-directory, etc.
        let mut long_parts: Vec<String> = Vec::new();

        // Options with arguments (come first in long section)
        for dir in &self.args.include_dirs {
            long_parts.push(format!("-I{}", dir.display()));
        }
        if let Some(ref load) = self.args.load_average {
            long_parts.push(format!("-l{}", load));
        }
        if let Some(ref sync) = self.args.output_sync {
            long_parts.push(format!("-O{}", sync));
        }
        // Long --debug=FLAGS (only for explicit --debug=..., not for short -d)
        if !self.args.debug_short {
            for dbg in &self.args.debug {
                long_parts.push(format!("--debug={}", dbg));
            }
        }

        // No-arg long options (come after options-with-args)
        if self.args.trace { long_parts.push("--trace".to_string()); }
        if self.args.no_print_directory { long_parts.push("--no-print-directory".to_string()); }
        if self.args.no_silent { long_parts.push("--no-silent".to_string()); }
        if self.args.warn_undefined_variables { long_parts.push("--warn-undefined-variables".to_string()); }

        // Build the main flags portion
        let has_single = !single_flags.is_empty();
        let has_long = !long_parts.is_empty();
        let has_vars = !variables.is_empty();

        let mut result = String::new();

        if has_single {
            result.push_str(&single_flags);
            if has_long {
                result.push(' ');
                result.push_str(&long_parts.join(" "));
            }
        } else if has_long {
            // Leading space when there are only long options
            result.push(' ');
            result.push_str(&long_parts.join(" "));
        }

        // Append command-line variable assignments after "-- " separator.
        // GNU Make outputs variables with command-line vars in REVERSE order of how they
        // appeared on the command line (last-specified comes first), followed by env
        // MAKEFLAGS vars in their original order.
        // args.variables layout: [env_makeflags_vars..., cmdline_vars...]
        // where cmdline_vars_start marks the boundary.
        if has_vars {
            let cmdline_start = self.args.cmdline_vars_start;
            let env_vars = &variables[..cmdline_start.min(variables.len())];
            let cmdline_vars = &variables[cmdline_start.min(variables.len())..];

            let mut seen_names: HashSet<String> = HashSet::new();
            let mut var_parts: Vec<String> = Vec::new();

            // Cmdline vars in reverse order (last-specified on CLI comes first).
            for (name, value) in cmdline_vars.iter().rev() {
                let key = name.trim_end_matches(|c: char| c == ':' || c == '?' || c == '+');
                if seen_names.insert(key.to_string()) {
                    var_parts.push(format!("{}={}", name, value));
                }
            }
            // Env MAKEFLAGS vars in original order, skipping duplicates of cmdline vars.
            for (name, value) in env_vars.iter() {
                let key = name.trim_end_matches(|c: char| c == ':' || c == '?' || c == '+');
                if seen_names.insert(key.to_string()) {
                    var_parts.push(format!("{}={}", name, value));
                }
            }

            if !var_parts.is_empty() {
                result.push_str(" -- ");
                result.push_str(&var_parts.join(" "));
            }
        }

        result
    }

    fn read_makefiles(&mut self) -> Result<(), String> {
        // Process MAKEFILES environment variable: space-separated list of
        // makefiles to include before the regular makefiles (like -include).
        if let Ok(makefiles_env) = env::var("MAKEFILES") {
            for file in makefiles_env.split_whitespace() {
                // Each entry is treated as an optional include (ignore if missing,
                // but still try to build if there is a rule).
                let file_path = self.find_include_file(file);
                match file_path {
                    Some(p) => {
                        let _ = self.read_makefile(&p);
                    }
                    None => {
                        // Missing MAKEFILES entries are silently deferred (like -include)
                        self.pending_includes.push(PendingInclude {
                            file: file.to_string(),
                            parent: String::new(),
                            lineno: 0,
                            ignore_missing: true,
                        });
                    }
                }
            }
        }

        let makefiles = if self.args.makefiles.is_empty() {
            // Default makefile search order
            let candidates = vec!["GNUmakefile", "makefile", "Makefile"];
            let mut found = Vec::new();
            for name in candidates {
                if Path::new(name).exists() {
                    found.push(PathBuf::from(name));
                    break;
                }
            }
            if found.is_empty() {
                // If there are -E eval strings, we can proceed without a makefile file.
                // The eval strings themselves form the "makefile content".
                if !self.args.eval_strings.is_empty() {
                    return Ok(());
                }
                return Err("No targets.  Stop.".to_string());
            }
            found
        } else {
            self.args.makefiles.clone()
        };

        // Check for stdin specified more than once
        let stdin_count = makefiles.iter().filter(|p| {
            let s = p.to_string_lossy();
            s == "-" || s == "/dev/stdin"
        }).count();
        if stdin_count > 1 {
            return Err("Makefile from standard input specified twice.  Stop.".to_string());
        }

        for mf in &makefiles {
            self.read_makefile(mf)?;
        }

        // Set MAKEFILE_LIST
        let mf_list: Vec<String> = self.makefile_list.iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        self.db.variables.insert("MAKEFILE_LIST".into(),
            Variable::new(mf_list.join(" "), VarFlavor::Simple, VarOrigin::File));

        Ok(())
    }

    pub fn read_makefile(&mut self, path: &Path) -> Result<(), String> {
        let path_str = path.to_string_lossy();
        let is_stdin = path_str == "-" || path_str == "/dev/stdin";

        let mut parser = Parser::new(if is_stdin {
            PathBuf::from("-")
        } else {
            path.to_path_buf()
        });

        if is_stdin {
            // Read from stdin
            use std::io::Read;
            let mut content = String::new();
            if let Err(e) = std::io::stdin().read_to_string(&mut content) {
                return Err(format!("{}: {}", path_str, e));
            }
            parser.load_string(&content);
        } else if let Err(e) = parser.load_file() {
            // Format: "filename: No such file or directory" (no "os error N")
            let msg = e.to_string();
            let clean_msg = if let Some(paren) = msg.find(" (os error ") {
                msg[..paren].to_string()
            } else {
                msg
            };
            return Err(format!("{}: {}", path.display(), clean_msg));
        }

        self.makefile_list.push(if is_stdin {
            PathBuf::from("-")
        } else {
            path.to_path_buf()
        });
        self.process_parsed_lines(&mut parser)
    }

    pub fn eval_string(&mut self, content: &str) -> Result<(), String> {
        let mut parser = Parser::new(PathBuf::from("<eval>"));
        parser.load_string(content);
        self.process_parsed_lines(&mut parser)
    }

    fn process_parsed_lines(&mut self, parser: &mut Parser) -> Result<(), String> {
        let mut current_rule: Option<Rule> = None;
        // When a static pattern rule expands to multiple targets, all targets
        // share the same recipe.  `current_rule` holds the last expanded rule
        // (which collects recipe lines); these siblings wait for the recipe to
        // be finalised before being registered.
        let mut static_rule_siblings: Vec<Rule> = Vec::new();

        while let Some((line, lineno)) = parser.next_logical_line() {
            // Update current file/line context for $(error)/$(warning)/$(info)
            *self.current_file.borrow_mut() = parser.filename.to_string_lossy().to_string();
            *self.current_line.borrow_mut() = lineno;
            // Handle define/endef blocks
            if parser.in_define {
                // GNU Make supports nested define/endef blocks.
                // endef is recognized after trimming whitespace (" endef" IS a valid endef).
                // But "endef$(VAR)" is NOT recognized ($ immediately follows "endef").
                // Nested 'define ...' inside a body increments depth; matching endef decrements.
                // Only when depth == 0 does the outer define actually end.

                let trimmed_line = line.trim();
                // Check if this is a nested define start
                let eff_no_comment = parser::strip_comment(trimmed_line);
                let eff_trim = eff_no_comment.trim();
                let is_nested_define = eff_trim.starts_with("define ") || eff_trim.starts_with("define\t")
                    || eff_trim == "define"
                    || eff_trim.starts_with("override define ")
                    || eff_trim.starts_with("export define ");
                if is_nested_define {
                    parser.define_depth += 1;
                    parser.define_lines.push(line);
                    continue;
                }

                // Check for endef after whitespace trimming
                let no_comment = parser::strip_comment(trimmed_line);
                let no_comment_trimmed = no_comment.trim();
                let is_endef = trimmed_line.starts_with("endef") && {
                    let after = &trimmed_line["endef".len()..];
                    after.is_empty()
                        || after.starts_with(' ')
                        || after.starts_with('\t')
                        || after.starts_with('#')
                };
                if is_endef {
                    if parser.define_depth > 0 {
                        // This endef closes a nested define inside the body - accumulate it
                        parser.define_depth -= 1;
                        parser.define_lines.push(line);
                        continue;
                    }
                    // Warn about extraneous text after endef (non-comment text)
                    if no_comment_trimmed != "endef" && !no_comment_trimmed.is_empty() {
                        let fname = parser.filename.to_string_lossy();
                        eprintln!("{}:{}: extraneous text after 'endef' directive", fname, lineno);
                    }
                    parser.in_define = false;
                    let raw_value = parser.define_lines.join("\n");
                    // For simple (:= / ::=) assignments, expand value immediately.
                    // For shell (!=), run the shell command and store result.
                    // For recursive/append/conditional, store value verbatim.
                    let value = match parser.define_flavor {
                        VarFlavor::Simple => self.expand(&raw_value),
                        VarFlavor::Shell => {
                            let expanded_cmd = self.expand(&raw_value);
                            let (result, status) = functions::fn_shell_exec_with_status(&expanded_cmd);
                            *self.last_shell_status.borrow_mut() = status;
                            result
                        }
                        _ => raw_value.clone(),
                    };
                    let var = Variable::new(
                        value.clone(),
                        parser.define_flavor.clone(),
                        VarOrigin::File,
                    );
                    if parser.define_override {
                        // override define always wins, even over command-line variables
                        let mut v = var;
                        v.origin = VarOrigin::Override;
                        self.db.variables.insert(parser.define_name.clone(), v);
                    } else {
                        let name = parser.define_name.clone();
                        // Without override, a command-line variable cannot be replaced
                        let is_cmdline = self.db.variables.get(&name)
                            .map_or(false, |v| v.origin == VarOrigin::CommandLine);
                        if !is_cmdline {
                            match parser.define_flavor {
                                VarFlavor::Append => {
                                    if let Some(existing) = self.db.variables.get_mut(&name) {
                                        existing.value.push('\n');
                                        existing.value.push_str(&raw_value);
                                    } else {
                                        self.db.variables.insert(name, var);
                                    }
                                }
                                VarFlavor::Conditional => {
                                    self.db.variables.entry(name).or_insert(var);
                                }
                                _ => {
                                    self.db.variables.insert(name, var);
                                }
                            }
                        }
                    }
                    parser.define_lines.clear();
                    continue;
                }
                parser.define_lines.push(line);
                continue;
            }

            // Expand variables in line (except recipe lines).
            // For rule lines that contain an inline recipe after `;`, only expand
            // the rule header (targets/prerequisites) portion; the recipe part must
            // be left unexpanded so that variable references in it are evaluated at
            // execution time (just like tab-prefixed recipe lines).
            //
            // For variable assignment lines, recursive (=) and append (+=) variables
            // must NOT have their value expanded at parse time -- the unexpanded text
            // is stored as the variable value and expanded lazily when referenced.
            // Only simple (:= / ::=) assignments expand the value immediately.
            // Determine if this line is a recipe line for a custom .RECIPEPREFIX.
            // Tab-prefixed lines and custom-prefix lines must NOT be pre-expanded
            // (they are handed verbatim to parse_line and then to the executor).
            let custom_recipe_prefix: Option<char> = {
                let pfx = self.db.variables.get(".RECIPEPREFIX")
                    .and_then(|v| v.value.chars().next());
                if pfx == Some('\t') { None } else { pfx }
            };
            let is_custom_recipe_line = custom_recipe_prefix
                .map(|c| line.starts_with(c))
                .unwrap_or(false);

            let expanded = if line.starts_with('\t') || is_custom_recipe_line {
                line.clone()
            } else if let Some(semi_pos) = parser::find_semicolon(&line) {
                // Check whether the part before `;` contains a `:` (rule colon).
                // If it does, this is an inline recipe: expand only the header.
                let pre_semi = &line[..semi_pos];
                if pre_semi.contains(':') {
                    let expanded_header = self.expand(pre_semi);
                    format!("{};{}", expanded_header, &line[semi_pos + 1..])
                } else {
                    self.expand(&line)
                }
            } else {
                // Check if this is a variable assignment by parsing the raw line.
                // If so, handle value expansion based on flavor.
                let trimmed = line.trim();

                // Special case: `define` directives (and `override define`, `export define`).
                // These look like "define VAR_NAME [OP]" where VAR_NAME may contain variable
                // references.  We must expand VAR_NAME at this point (first expansion), but NOT
                // treat the line as a regular variable assignment.  `try_parse_variable_assignment`
                // would misparse e.g. `define $(NAME) =` as `variable("define $(NAME)") = ""`.
                //
                // Identifying a define directive here: the line (after stripping optional
                // `override`/`export` prefixes) starts with the word `define` followed by a
                // space/tab OR is exactly `define`, AND what follows after `define` is NOT a
                // bare assignment operator (which would make it a regular variable named "define").
                if let Some(define_expanded) = try_expand_define_name(&trimmed, self) {
                    define_expanded
                } else if let Some(raw_parsed) = parser::try_parse_variable_assignment(trimmed) {
                    if let ParsedLine::VariableAssignment { name: raw_name, value: raw_value, flavor: raw_flavor, is_override: raw_is_override, is_export: raw_is_export, is_unexport: _, is_private: raw_is_private, target: raw_target } = raw_parsed {
                        match raw_flavor {
                            VarFlavor::Simple => {
                                // Set directly without re-parsing to preserve '#' in values
                                let expanded_name = self.expand(&raw_name);
                                let expanded_value = self.expand(&raw_value);
                                if raw_target.is_none() {
                                    self.set_variable(&expanded_name, &expanded_value, &VarFlavor::Simple, raw_is_override, raw_is_export);
                                    continue;
                                }
                                self.expand(&line)
                            }
                            _ => {
                                // Deferred expansion: expand only the LHS (name / prefixes),
                                // keep the value verbatim.
                                // Reconstruct the line with only the name expanded.
                                let expanded_name = self.expand(&raw_name);
                                let op_str = match raw_flavor {
                                    VarFlavor::Append => " += ",
                                    VarFlavor::Conditional => " ?= ",
                                    VarFlavor::Shell => " != ",
                                    _ => " = ",
                                };
                                // Build modifier prefix for the variable name
                                let mut var_prefix = String::new();
                                if raw_is_override { var_prefix.push_str("override "); }
                                if raw_is_export { var_prefix.push_str("export "); }
                                if raw_is_private { var_prefix.push_str("private "); }
                                if let Some(tgt) = raw_target {
                                    // Target-specific variable: "target: [modifiers] name op value"
                                    let expanded_target = self.expand(&tgt);
                                    format!("{}: {}{}{}{}", expanded_target, var_prefix, expanded_name, op_str, raw_value)
                                } else {
                                    // Preserve outer override/export/private prefixes from the original
                                    let outer_prefix = extract_var_prefixes(trimmed);
                                    format!("{}{}{}{}", outer_prefix, expanded_name, op_str, raw_value)
                                }
                            }
                        }
                    } else {
                        self.expand(&line)
                    }
                } else {
                    self.expand(&line)
                }
            };

            let parsed = parser.parse_line(&expanded, self);

            // Handle conditionals
            match &parsed {
                ParsedLine::Conditional(kind) => {
                    let active = self.evaluate_condition(kind);
                    parser.conditional_stack.push(parser::ConditionalState {
                        active,
                        seen_true: active,
                        in_else: false,
                    });
                    continue;
                }
                ParsedLine::Else(maybe_cond) => {
                    if let Some(state) = parser.conditional_stack.last_mut() {
                        if state.seen_true {
                            state.active = false;
                        } else if let Some(kind) = maybe_cond {
                            let active = self.evaluate_condition(kind);
                            state.active = active;
                            if active { state.seen_true = true; }
                        } else {
                            state.active = true;
                            state.seen_true = true;
                        }
                        state.in_else = true;
                    }
                    continue;
                }
                ParsedLine::Endif => {
                    parser.conditional_stack.pop();
                    continue;
                }
                _ => {}
            }

            // Skip if conditionally inactive
            if !parser.is_conditionally_active() {
                continue;
            }

            match parsed {
                ParsedLine::Rule(mut rule) => {
                    // Register the previous rule FIRST.
                    //
                    // This must happen before the second-expansion check below, because the
                    // previous rule may be ".SECONDEXPANSION:" whose registration activates
                    // self.db.second_expansion.  Without this ordering, the rule immediately
                    // after ".SECONDEXPANSION:" would be processed in non-SE mode.
                    if let Some(prev) = current_rule.take() {
                        for sib in &mut static_rule_siblings {
                            sib.recipe = prev.recipe.clone();
                        }
                        for sib in static_rule_siblings.drain(..) {
                            self.register_rule(sib);
                        }
                        self.register_rule(prev);
                    }
                    static_rule_siblings.clear();

                    if self.db.second_expansion {
                        // Second expansion is active.
                        //
                        // The raw prereq text (stored by the parser after the line-level
                        // first expansion $$ → $) is in second_expansion_prereqs.  It is
                        // re-expanded at build time with automatic vars ($@, $<, $^, etc.).
                        //
                        // For rules WHERE second_expansion_prereqs is Some (raw text has
                        // '$'): actual build prereqs come entirely from second expansion.
                        // We CLEAR rule.prerequisites so incorrectly-split tokens like
                        // "$@.1" or "$(addsuffix" don't appear as build targets.
                        //
                        // For rules WITHOUT second_expansion_prereqs (plain prereqs like
                        // "bar baz"): rule.prerequisites are kept as normal build prereqs
                        // and also used for computing $<, $^ auto vars when expanding any
                        // subsequent SE rules for the same target.
                        rule.second_expansion_prereqs = rule.second_expansion_prereqs.take();
                        rule.second_expansion_order_only = rule.second_expansion_order_only.take();

                        if rule.second_expansion_prereqs.is_some() {
                            // Clear incorrectly-split first-pass tokens.
                            rule.prerequisites.clear();
                        }
                        if rule.second_expansion_order_only.is_some() {
                            rule.order_only_prerequisites.clear();
                        }
                    } else {
                        // Normal (no second expansion): prerequisites were already
                        // expanded by the line-level expansion before the parser ran.
                        // $(VAR) in source is already replaced with its value.  Do NOT
                        // call self.expand() again: that would incorrectly re-expand
                        // tokens like "$(PRE)" that came from "$$(...)" in the source
                        // (where "$$" is supposed to be a literal "$", not a deferred ref).
                        //
                        // rule.prerequisites already contains correctly-split final tokens.
                        rule.second_expansion_prereqs = None;
                        rule.second_expansion_order_only = None;
                    }

                    // Stamp the source file, lineno, and fix up inline recipe line numbers
                    rule.source_file = parser.filename.to_string_lossy().to_string();
                    rule.lineno = lineno;
                    for entry in rule.recipe.iter_mut() {
                        if entry.0 == 0 {
                            entry.0 = lineno;
                        }
                    }

                    parser.in_recipe = true;
                    current_rule = Some(rule);
                }
                ParsedLine::StaticPatternExpansion(mut expanded_rules) => {
                    // Flush the previous current_rule (and any siblings from a
                    // prior static pattern rule in this same block).
                    if let Some(prev) = current_rule.take() {
                        // Copy the recipe accumulated so far to any siblings.
                        for sib in &mut static_rule_siblings {
                            sib.recipe = prev.recipe.clone();
                        }
                        for sib in static_rule_siblings.drain(..) {
                            self.register_rule(sib);
                        }
                        self.register_rule(prev);
                    }

                    if expanded_rules.is_empty() {
                        static_rule_siblings.clear();
                        parser.in_recipe = false;
                        continue;
                    }

                    // Prepare each expanded rule: handle SE if active, stamp source
                    // file and line number.
                    let source_file = parser.filename.to_string_lossy().to_string();
                    let se_active = self.db.second_expansion;
                    let mut prepared: Vec<Rule> = expanded_rules
                        .into_iter()
                        .map(|mut rule| {
                            if se_active {
                                // In SE mode the raw prereq text is in
                                // second_expansion_prereqs.  For rules that have SE
                                // references ($@ etc.) we clear the incorrectly-split
                                // first-pass tokens; plain prereqs are left intact.
                                if rule.second_expansion_prereqs.is_some() {
                                    rule.prerequisites.clear();
                                }
                                if rule.second_expansion_order_only.is_some() {
                                    rule.order_only_prerequisites.clear();
                                }
                            } else {
                                // Not in SE mode: clear SE fields (they're not needed).
                                rule.second_expansion_prereqs = None;
                                rule.second_expansion_order_only = None;
                            }
                            rule.source_file = source_file.clone();
                            for entry in rule.recipe.iter_mut() {
                                if entry.0 == 0 {
                                    entry.0 = lineno;
                                }
                            }
                            rule
                        })
                        .collect();

                    // The last rule is `current_rule` (it collects following
                    // recipe lines).  The others are siblings: they will get a
                    // copy of the recipe when `current_rule` is finalised.
                    let last_rule = prepared.pop().unwrap();
                    static_rule_siblings = prepared;
                    parser.in_recipe = true;
                    current_rule = Some(last_rule);
                }
                ParsedLine::Recipe(recipe) => {
                    if let Some(ref mut rule) = current_rule {
                        rule.recipe.push((lineno, recipe.clone()));
                    }
                    // Sibling rules from a static pattern expansion share the
                    // same recipe as current_rule.
                    for sib in &mut static_rule_siblings {
                        sib.recipe.push((lineno, recipe.clone()));
                    }
                }
                ParsedLine::VariableAssignment { name, value, flavor, is_override, is_export, is_unexport, is_private, target } => {
                    if let Some(prev) = current_rule.take() {
                        for sib in &mut static_rule_siblings {
                            sib.recipe = prev.recipe.clone();
                        }
                        for sib in static_rule_siblings.drain(..) {
                            self.register_rule(sib);
                        }
                        self.register_rule(prev);
                        parser.in_recipe = false;
                    }
                    static_rule_siblings.clear();

                    if let Some(target_name) = target {
                        // Target-specific or pattern-specific variable
                        let var_origin = if is_override { VarOrigin::Override } else { VarOrigin::File };
                        let targets: Vec<String> = parser::split_words(&target_name);
                        for t in targets {
                            if t.contains('%') {
                                // Pattern-specific variable: stored separately for lookup at build time
                                let mut var = Variable::new(value.clone(), flavor.clone(), var_origin.clone());
                                var.is_private = is_private;
                                if is_export {
                                    var.export = Some(true);
                                } else if is_unexport {
                                    var.export = Some(false);
                                }
                                self.db.pattern_specific_vars.push(PatternSpecificVar {
                                    pattern: t,
                                    var_name: name.clone(),
                                    var,
                                    is_override,
                                });
                            } else {
                                // Target-specific variable: stored in the rule as a Vec
                                // to support multiple += entries for the same variable.
                                let mut var = Variable::new(value.clone(), flavor.clone(), var_origin.clone());
                                var.is_private = is_private;
                                if is_export {
                                    var.export = Some(true);
                                } else if is_unexport {
                                    var.export = Some(false);
                                }
                                let rules = self.db.rules.entry(t.clone()).or_insert_with(Vec::new);
                                // Add to all rules for this target
                                for r in rules.iter_mut() {
                                    r.target_specific_vars.push((name.clone(), var.clone()));
                                }
                                // If no rules yet, create a placeholder
                                if rules.is_empty() {
                                    let mut r = Rule::new();
                                    r.targets = vec![t];
                                    r.target_specific_vars.push((name.clone(), var.clone()));
                                    rules.push(r);
                                }
                            }
                        }
                    } else {
                        self.set_variable(&name, &value, &flavor, is_override, is_export);
                        // Handle unexport on global variable assignments.
                        // `unexport FOO=bar` sets FOO=bar but marks it as NOT exported.
                        if is_unexport {
                            if let Some(var) = self.db.variables.get_mut(&name) {
                                var.export = Some(false);
                            }
                        }
                        // Handle private on global variable assignments.
                        // `private FOO = bar` sets FOO but marks it as private (not inherited
                        // by prerequisites; in the global context this means targets should
                        // not see it from the global scope—treated as an implicit unexport).
                        if is_private {
                            if let Some(var) = self.db.variables.get_mut(&name) {
                                var.is_private = true;
                            }
                        }
                    }
                }
                ParsedLine::Include { paths, ignore_missing } => {
                    if let Some(prev) = current_rule.take() {
                        for sib in &mut static_rule_siblings {
                            sib.recipe = prev.recipe.clone();
                        }
                        for sib in static_rule_siblings.drain(..) {
                            self.register_rule(sib);
                        }
                        self.register_rule(prev);
                        parser.in_recipe = false;
                    }
                    static_rule_siblings.clear();

                    for path_pattern in &paths {
                        let expanded = self.expand(path_pattern);
                        let files: Vec<String> = parser::split_words(&expanded);
                        for file in files {
                            // Mark included files as explicitly mentioned so they are
                            // not treated as intermediate targets (sv63484).
                            self.db.explicitly_mentioned.insert(file.clone());
                            let file_path = self.find_include_file(&file);
                            match file_path {
                                Some(p) => {
                                    if let Err(_e) = self.read_makefile(&p) {
                                        // File exists but failed to parse (e.g. unreadable).
                                        // Defer to pending includes to decide what to do.
                                        self.pending_includes.push(PendingInclude {
                                            file: file.clone(),
                                            parent: parser.filename.to_string_lossy().to_string(),
                                            lineno,
                                            ignore_missing,
                                        });
                                    }
                                }
                                None => {
                                    // File not found: defer to pending includes.
                                    // Don't print a warning yet - the file may be buildable.
                                    self.pending_includes.push(PendingInclude {
                                        file: file.clone(),
                                        parent: parser.filename.to_string_lossy().to_string(),
                                        lineno,
                                        ignore_missing,
                                    });
                                }
                            }
                        }
                    }
                }
                ParsedLine::VpathDirective { pattern, directories } => {
                    if let Some(prev) = current_rule.take() {
                        for sib in &mut static_rule_siblings {
                            sib.recipe = prev.recipe.clone();
                        }
                        for sib in static_rule_siblings.drain(..) {
                            self.register_rule(sib);
                        }
                        self.register_rule(prev);
                        parser.in_recipe = false;
                    }
                    static_rule_siblings.clear();

                    match pattern {
                        Some(pat) => {
                            if directories.is_empty() {
                                // Clear vpath for this pattern
                                self.db.vpath.retain(|(p, _)| p != &pat);
                            } else {
                                let dirs: Vec<PathBuf> = directories.iter().map(PathBuf::from).collect();
                                self.db.vpath.push((pat, dirs));
                            }
                        }
                        None => {
                            // Clear all vpath
                            self.db.vpath.clear();
                            self.db.vpath_general.clear();
                        }
                    }
                }
                ParsedLine::ExportDirective { names, export } => {
                    if names.is_empty() {
                        self.db.export_all = true;
                    } else {
                        for name in &names {
                            if let Some(var) = self.db.variables.get_mut(name) {
                                var.export = Some(export);
                            } else {
                                let mut var = Variable::new(String::new(), VarFlavor::Recursive, VarOrigin::File);
                                var.export = Some(export);
                                self.db.variables.insert(name.clone(), var);
                            }
                        }
                    }
                }
                ParsedLine::UnExport { names } => {
                    if names.is_empty() {
                        // unexport all
                        for (_, var) in self.db.variables.iter_mut() {
                            var.export = Some(false);
                        }
                    } else {
                        for name in &names {
                            if let Some(var) = self.db.variables.get_mut(name) {
                                var.export = Some(false);
                            }
                        }
                    }
                }
                ParsedLine::Undefine { name, is_override } => {
                    // Remove the variable from the database entirely.
                    // Without override, a command-line variable cannot be undefined.
                    let is_cmdline = self.db.variables.get(&name)
                        .map_or(false, |v| v.origin == VarOrigin::CommandLine);
                    if is_override || !is_cmdline {
                        self.db.variables.shift_remove(&name);
                    }
                }
                ParsedLine::Define { name, flavor, is_override, is_export, has_extraneous } => {
                    // Warn about extraneous text after define directive
                    if has_extraneous {
                        let fname = parser.filename.to_string_lossy();
                        eprintln!("{}:{}: extraneous text after 'define' directive", fname, lineno);
                    }
                    // Expand the variable name (it may contain variable references like $(NAME))
                    let expanded_name = self.expand(&name);
                    let expanded_name = expanded_name.trim().to_string();
                    // Empty variable name is a fatal error
                    if expanded_name.is_empty() {
                        let fname = parser.filename.to_string_lossy();
                        eprintln!("{}:{}: *** empty variable name.  Stop.", fname, lineno);
                        return Err(String::new());
                    }
                    parser.in_define = true;
                    parser.define_name = expanded_name;
                    parser.define_flavor = flavor;
                    parser.define_override = is_override;
                    parser.define_export = is_export;
                    parser.define_lineno = lineno;
                    parser.define_depth = 0;
                    parser.define_lines.clear();
                }
                ParsedLine::Empty | ParsedLine::Comment => {
                    // Empty lines and comments do NOT end the recipe context.
                    // GNU Make allows blank lines between recipe lines; only a
                    // non-empty, non-recipe line (rule, assignment, directive)
                    // should clear in_recipe / current_rule.
                }
                ParsedLine::MissingSeparator(hint) => {
                    let fname = parser.filename.to_string_lossy();
                    if hint.is_empty() {
                        eprintln!("{}:{}: *** missing separator.  Stop.", fname, lineno);
                    } else {
                        eprintln!("{}:{}: *** missing separator ({}).  Stop.", fname, lineno, hint);
                    }
                    std::process::exit(2);
                }
                _ => {}
            }
        }

        // Check for unterminated define block (missing endef)
        if parser.in_define {
            let fname = parser.filename.to_string_lossy();
            eprintln!("{}:{}: *** missing 'endef', unterminated 'define'.  Stop.", fname, parser.define_lineno);
            return Err(String::new());
        }

        // Register the last rule (and any static-pattern siblings).
        if let Some(prev) = current_rule.take() {
            for sib in &mut static_rule_siblings {
                sib.recipe = prev.recipe.clone();
            }
            for sib in static_rule_siblings.drain(..) {
                self.register_rule(sib);
            }
            self.register_rule(prev);
        }

        Ok(())
    }

    fn register_rule(&mut self, rule: Rule) {
        // Handle special targets
        for target in &rule.targets {
            if let Some(special) = SpecialTarget::from_str(target) {
                let prereqs: HashSet<String> = rule.prerequisites.iter().cloned().collect();

                match special {
                    SpecialTarget::Phony | SpecialTarget::Precious |
                    SpecialTarget::Intermediate | SpecialTarget::Secondary |
                    SpecialTarget::Silent | SpecialTarget::Ignore |
                    SpecialTarget::NotIntermediate => {
                        let set = self.db.special_targets.entry(special.clone()).or_insert_with(HashSet::new);
                        set.extend(prereqs);
                    }
                    SpecialTarget::Wait => {
                        // .WAIT is a synchronization marker in dependency lists.
                        // It has no prerequisites and no special database state.
                        // Warn if .WAIT has prerequisites or a recipe.
                        if !rule.prerequisites.is_empty() {
                            eprintln!("{}:{}: .WAIT should not have prerequisites",
                                rule.source_file, rule.lineno);
                        }
                        if !rule.recipe.is_empty() {
                            let recipe_lineno = rule.recipe.first().map(|(l, _)| *l).unwrap_or(rule.lineno);
                            eprintln!("{}:{}: .WAIT should not have commands",
                                rule.source_file, recipe_lineno);
                        }
                    }
                    SpecialTarget::ExportAllVariables => {
                        // .EXPORT_ALL_VARIABLES: causes all variables to be exported
                        let set = self.db.special_targets.entry(special.clone()).or_insert_with(HashSet::new);
                        set.extend(prereqs);
                        self.db.export_all = true;
                    }
                    SpecialTarget::Suffixes => {
                        if rule.prerequisites.is_empty() {
                            // .SUFFIXES: with no prereqs clears the suffix list AND
                            // removes all built-in (default) implicit rules.
                            self.db.suffixes.clear();
                            // Remove built-in pattern rules (the first
                            // builtin_pattern_rules_count entries).
                            let n = self.db.builtin_pattern_rules_count;
                            if n > 0 && self.db.pattern_rules.len() >= n {
                                self.db.pattern_rules.drain(..n);
                                self.db.builtin_pattern_rules_count = 0;
                            }
                            // Also clear any suffix rules.
                            self.db.suffix_rules.clear();
                        } else {
                            self.db.suffixes.extend(rule.prerequisites.clone());
                        }
                    }
                    SpecialTarget::Default => {
                        self.db.default_rule = Some(rule.clone());
                    }
                    SpecialTarget::SecondExpansion => {
                        self.db.second_expansion = true;
                    }
                    SpecialTarget::OneSHell => {
                        self.db.one_shell = true;
                    }
                    SpecialTarget::Posix => {
                        self.db.posix_mode = true;
                    }
                    SpecialTarget::NotParallel => {
                        self.db.not_parallel = true;
                    }
                    _ => {}
                }
                continue;
            }

            // Set default target
            if self.db.default_target.is_none() && !target.starts_with('.') && !target.contains('%') {
                self.db.default_target = Some(target.clone());
            }
        }

        // Register pattern rules
        if rule.is_pattern {
            // For pattern rules, track literal (non-%) prerequisites as explicitly mentioned.
            // E.g., in `%.tsk: %.z test.z`, `test.z` is explicitly mentioned.
            for prereq in &rule.prerequisites {
                if !prereq.contains('%') {
                    self.db.explicitly_mentioned.insert(prereq.clone());
                }
            }
            for prereq in &rule.order_only_prerequisites {
                if !prereq.contains('%') {
                    self.db.explicitly_mentioned.insert(prereq.clone());
                }
            }
            self.db.pattern_rules.push(rule.clone());
            return;
        }

        // Register suffix rules
        if rule.targets.len() == 1 {
            let target = &rule.targets[0];
            if is_suffix_rule(target, &self.db.suffixes) {
                // Suffix rules with prerequisites are handled specially (SV 40657):
                // In POSIX mode: treat as a normal explicit rule only (no pattern rule).
                // In non-POSIX mode: emit a warning and still create a pattern rule,
                //   but the pattern rule ignores the prerequisites.
                let has_prereqs = !rule.prerequisites.is_empty();
                if has_prereqs && !self.db.posix_mode {
                    // Emit warning: ignoring prerequisites on suffix rule definition
                    eprintln!("{}:{}: warning: ignoring prerequisites on suffix rule definition",
                        rule.source_file, rule.lineno);
                }
                self.db.suffix_rules.push(rule.clone());
                // Create a pattern rule only if: no prerequisites, OR non-POSIX mode.
                if !has_prereqs || !self.db.posix_mode {
                    // Also register as pattern rule, using the actual current suffix list.
                    // The pattern rule has no prerequisites (they're ignored per SV 40657).
                    let suffixes_clone = self.db.suffixes.clone();
                    if let Some(pattern_rule) = suffix_to_pattern_rule(target, &rule, &suffixes_clone) {
                        // suffix_to_pattern_rule already sets the correct pattern
                        // prerequisites (e.g. %.baz for .baz.biz target).
                        // The explicit prerequisites of the suffix rule (e.g. foo.bar)
                        // are intentionally NOT propagated to the pattern rule.
                        self.db.pattern_rules.push(pattern_rule);
                    }
                }
            }
        }

        // Mark all explicit-rule prerequisites as explicitly mentioned.
        // This prevents targets that appear as prereqs from being considered intermediate.
        for prereq in &rule.prerequisites {
            self.db.explicitly_mentioned.insert(prereq.clone());
            self.db.explicit_dep_names.insert(prereq.clone());
        }
        for prereq in &rule.order_only_prerequisites {
            self.db.explicitly_mentioned.insert(prereq.clone());
            self.db.explicit_dep_names.insert(prereq.clone());
        }

        // Register explicit rules
        for target in &rule.targets.clone() {
            if SpecialTarget::from_str(target).is_some() {
                continue;
            }
            // For grouped target rules, set grouped_siblings = all targets except this one.
            // The parser stored ALL targets in grouped_siblings when is_grouped was set.
            let mut rule_for_target = rule.clone();
            if !rule.grouped_siblings.is_empty() {
                rule_for_target.grouped_siblings = rule.grouped_siblings.iter()
                    .filter(|t| *t != target)
                    .cloned()
                    .collect();
            }
            let rule = rule_for_target;
            let rules = self.db.rules.entry(target.clone()).or_insert_with(Vec::new);
            if rule.is_double_colon {
                rules.push(rule.clone());
            } else {
                // Single colon - merge prerequisites, replace recipe if new one given
                if let Some(existing) = rules.first_mut() {
                    // GNU Make promotes a prereq from order-only to normal when the same
                    // target lists it as a normal prereq in any rule.  Handle both directions:
                    //  (a) new rule has normal prereqs that were order-only in existing rule
                    //  (b) new rule has order-only prereqs that were already normal in existing rule
                    let new_normal: std::collections::HashSet<&String> =
                        rule.prerequisites.iter().collect();
                    // (a) Promote: remove from existing order-only if also a new normal prereq.
                    existing.order_only_prerequisites.retain(|p| !new_normal.contains(p));

                    let existing_normal: std::collections::HashSet<&String> =
                        existing.prerequisites.iter().collect();
                    // (b) Filter: don't add as order-only if already a normal prereq.
                    let filtered_order_only: Vec<String> = rule.order_only_prerequisites.iter()
                        .filter(|p| !existing_normal.contains(p))
                        .cloned()
                        .collect();

                    // When a new rule has a recipe, its prerequisites are "primary"
                    // and placed BEFORE the existing accumulated prerequisites.
                    // GNU Make uses the recipe rule to determine the primary ordering.
                    let new_has_recipe = !rule.recipe.is_empty();
                    if new_has_recipe && !rule.prerequisites.is_empty() {
                        // Prepend new rule's prereqs, then append existing ones (avoiding dups)
                        let mut new_prereqs = rule.prerequisites.clone();
                        for p in &existing.prerequisites {
                            if !new_prereqs.contains(p) {
                                new_prereqs.push(p.clone());
                            }
                        }
                        existing.prerequisites = new_prereqs;
                    } else {
                        existing.prerequisites.extend(rule.prerequisites.clone());
                    }
                    existing.order_only_prerequisites.extend(filtered_order_only);
                    // Merge second-expansion raw prerequisite text.
                    // When new rule has recipe, its SE text comes first.
                    if let Some(ref new_text) = rule.second_expansion_prereqs {
                        match existing.second_expansion_prereqs {
                            Some(ref mut existing_text) => {
                                if new_has_recipe {
                                    let old = existing_text.clone();
                                    *existing_text = if old.is_empty() {
                                        new_text.clone()
                                    } else {
                                        format!("{} {}", new_text, old)
                                    };
                                } else {
                                    if !existing_text.is_empty() {
                                        existing_text.push(' ');
                                    }
                                    existing_text.push_str(new_text);
                                }
                            }
                            None => {
                                existing.second_expansion_prereqs = Some(new_text.clone());
                            }
                        }
                    }
                    if let Some(ref new_text) = rule.second_expansion_order_only {
                        match existing.second_expansion_order_only {
                            Some(ref mut existing_text) => {
                                if !existing_text.is_empty() {
                                    existing_text.push(' ');
                                }
                                existing_text.push_str(new_text);
                            }
                            None => {
                                existing.second_expansion_order_only = Some(new_text.clone());
                            }
                        }
                    }
                    if new_has_recipe {
                        if !existing.recipe.is_empty() {
                            eprintln!("make: warning: overriding recipe for target '{}'", target);
                        }
                        existing.recipe = rule.recipe.clone();
                    }
                    // Preserve static_stem: if the new rule has a static stem (from a
                    // static pattern rule) and the existing rule doesn't, copy it.
                    if existing.static_stem.is_empty() && !rule.static_stem.is_empty() {
                        existing.static_stem = rule.static_stem.clone();
                    }
                    // Merge target-specific vars (append to list for multiple += support)
                    for (k, v) in &rule.target_specific_vars {
                        existing.target_specific_vars.push((k.clone(), v.clone()));
                    }
                } else {
                    rules.push(rule.clone());
                }
            }
        }
    }

    fn set_variable(&mut self, name: &str, value: &str, flavor: &VarFlavor, is_override: bool, is_export: bool) {
        let origin = if is_override { VarOrigin::Override } else { VarOrigin::File };

        // A non-override assignment cannot change a variable that was set via
        // the command line (CommandLine origin) OR via an `override` directive
        // (Override origin).  This mirrors GNU Make behaviour where `override`
        // protects a variable from subsequent non-override file assignments as
        // well as from command-line assignments.
        let env_overrides = self.args.environment_overrides;
        let is_protected = move |orig: &VarOrigin| {
            matches!(orig, VarOrigin::CommandLine | VarOrigin::Override)
                || (env_overrides && matches!(orig, VarOrigin::Environment))
        };

        // MAKEFLAGS is special: with -e, the makefile cannot change it.
        // This is because -e means the parent make's environment (which set MAKEFLAGS)
        // takes precedence over makefile assignments.
        let makeflags_protected = name == "MAKEFLAGS"
            && !is_override
            && self.args.environment_overrides;

        match flavor {
            VarFlavor::Append => {
                if let Some(existing) = self.db.variables.get_mut(name) {
                    if !is_override && is_protected(&existing.origin) {
                        return; // Protected variable: non-override append blocked
                    }
                    if makeflags_protected {
                        return; // -e protects MAKEFLAGS from makefile changes
                    }
                    // With -e (environment overrides), makefile cannot change environment-origin vars
                    if !is_override && self.args.environment_overrides
                        && existing.origin == VarOrigin::Environment {
                        return;
                    }
                    if existing.value.is_empty() {
                        existing.value = value.to_string();
                    } else {
                        existing.value.push(' ');
                        existing.value.push_str(value);
                    }
                    if is_override {
                        existing.origin = VarOrigin::Override;
                    }
                } else {
                    self.db.variables.insert(name.to_string(),
                        Variable::new(value.to_string(), VarFlavor::Recursive, origin));
                }
            }
            VarFlavor::Conditional => {
                // ?= only sets if not already defined
                self.db.variables.entry(name.to_string()).or_insert_with(|| {
                    Variable::new(value.to_string(), VarFlavor::Recursive, origin)
                });
            }
            VarFlavor::Shell => {
                // != executes value as shell command; expand Make variable references
                // first (like := expansion) then pass the result to the shell.
                let expanded_cmd = self.expand(value);
                let (result, status) = functions::fn_shell_exec_with_status(&expanded_cmd);
                *self.last_shell_status.borrow_mut() = status;
                let existing = self.db.variables.get(name);
                if !is_override {
                    if let Some(existing) = existing {
                        if is_protected(&existing.origin) {
                            return;
                        }
                    }
                }
                // GNU Make stores != results as recursive variables so that
                // any $(VAR) references in the shell output are re-expanded
                // when the variable is later used.
                self.db.variables.insert(name.to_string(),
                    Variable::new(result, VarFlavor::Recursive, origin));
            }
            _ => {
                let existing = self.db.variables.get(name);
                if !is_override {
                    if let Some(existing) = existing {
                        if is_protected(&existing.origin) {
                            return;
                        }
                    }
                }
                if makeflags_protected {
                    return; // -e protects MAKEFLAGS from makefile changes
                }
                self.db.variables.insert(name.to_string(),
                    Variable::new(value.to_string(), flavor.clone(), origin));
            }
        }

        // Special handling for MAKEFLAGS: when it's modified from a makefile,
        // parse the new value and apply any new flags to the runtime state.
        if name == "MAKEFLAGS" && !is_override && !makeflags_protected {
            self.apply_makeflags_from_makefile();
        }

        // MAKEOVERRIDES: when set (especially to empty), it controls the variable
        // assignments portion of MAKEFLAGS. Setting MAKEOVERRIDES= clears vars from MAKEFLAGS.
        if name == "MAKEOVERRIDES" {
            // Rebuild MAKEFLAGS with the new MAKEOVERRIDES value as the variable list
            let overrides_val = value.to_string();
            // Parse the MAKEOVERRIDES value into variable assignments
            let mut new_vars: Vec<(String, String)> = Vec::new();
            if !overrides_val.is_empty() {
                for part in overrides_val.split_whitespace() {
                    if let Some(eq) = part.find('=') {
                        new_vars.push((part[..eq].to_string(), part[eq+1..].to_string()));
                    }
                }
            }
            self.args.variables = new_vars;
            let new_mf = self.build_makeflags_from_args(&self.args.variables.clone());
            if let Some(var) = self.db.variables.get_mut("MAKEFLAGS") {
                var.value = new_mf.clone();
            }
            env::set_var("MAKEFLAGS", &new_mf);
        }

        if is_export {
            if let Some(var) = self.db.variables.get_mut(name) {
                var.export = Some(true);
            }
        }
    }

    /// Re-parse the current MAKEFLAGS value and apply any new settings to args/state.
    /// Called when MAKEFLAGS is modified from within a makefile.
    /// Rebuilds MAKEFLAGS in the canonical sorted format.
    ///
    /// GNU Make preserves command-line flags even when a makefile does a direct
    /// assignment like `MAKEFLAGS = B`.  We implement this by starting from a
    /// fresh clone of the original cmdline args (which already carry the cmdline
    /// flags) and then layering the MAKEFLAGS variable value on top of that.
    fn apply_makeflags_from_makefile(&mut self) {
        let makeflags = match self.db.variables.get("MAKEFLAGS") {
            Some(v) => v.value.clone(),
            None => return,
        };

        // Save current print_directory state to detect transition.
        let was_printing_dir = should_print_directory(&self.args);

        // Start from the original command-line args as a baseline.
        // This preserves cmdline flags (e.g. -i from `-i` on cmdline) even when
        // the makefile does a direct assignment like `MAKEFLAGS = B`.
        self.args = self.cmdline_args.clone();

        // Layer the MAKEFLAGS variable value on top of the cmdline baseline.
        // parse_makeflags ORs flags into the args (boolean flags get set, lists get appended).
        crate::cli::parse_makeflags(&makeflags, &mut self.args);

        // Restore toggle-pair flags that were explicitly set by env or command line.
        // Priority: cmdline/env > makefile.  If the original cmdline_args had an explicit
        // setting for a toggle pair, the makefile MAKEFLAGS value must not override it.
        let ca = &self.cmdline_args;
        if ca.print_directory_explicit || ca.no_print_directory_explicit {
            self.args.print_directory = ca.print_directory;
            self.args.no_print_directory = ca.no_print_directory;
            self.args.print_directory_explicit = ca.print_directory_explicit;
            self.args.no_print_directory_explicit = ca.no_print_directory_explicit;
        }
        if ca.silent_explicit || ca.no_silent_explicit {
            self.args.silent = ca.silent;
            self.args.no_silent = ca.no_silent;
            self.args.silent_explicit = ca.silent_explicit;
            self.args.no_silent_explicit = ca.no_silent_explicit;
        }
        if ca.keep_going_explicit || ca.no_keep_going_explicit {
            self.args.keep_going = ca.keep_going;
            self.args.keep_going_explicit = ca.keep_going_explicit;
            self.args.no_keep_going_explicit = ca.no_keep_going_explicit;
        }

        // If print_directory was just enabled by the makefile (transition false→true),
        // print the "Entering directory" message now so it appears before subsequent
        // $(info) or recipe output in the makefile.
        let now_printing_dir = should_print_directory(&self.args);
        if now_printing_dir && !was_printing_dir && !self.entering_directory_printed {
            let progname = make_progname();
            let cwd = logical_cwd();
            eprintln!("{}: Entering directory '{}'", progname, cwd.display());
            self.entering_directory_printed = true;
        }

        // Deduplicate include_dirs (cmdline dirs might now appear twice if also in MAKEFLAGS).
        let mut seen_dirs: std::collections::HashSet<String> = std::collections::HashSet::new();
        let deduped_dirs: Vec<std::path::PathBuf> = self.args.include_dirs.iter()
            .filter(|d| seen_dirs.insert(d.to_string_lossy().to_string()))
            .cloned()
            .collect();
        self.args.include_dirs = deduped_dirs;
        self.include_dirs = self.args.include_dirs.clone();

        // Deduplicate debug flags (same issue: cmdline --debug=b + MAKEFLAGS --debug=b → duplicates).
        let mut seen_debug: std::collections::HashSet<String> = std::collections::HashSet::new();
        self.args.debug.retain(|d| seen_debug.insert(d.clone()));

        // Deduplicate variables: keep LAST occurrence of each variable name, preserving order.
        // This means cmdline vars (which come early in the list from cmdline_args baseline)
        // may be replaced by later MAKEFLAGS-parsed entries, but their POSITION is kept.
        // Actually: we want the output order to match what build_makeflags_from_args expects
        // (which uses iter().rev() to put cmdline first). So we keep the list in its natural
        // order: env vars first (from cmdline_args baseline), cmdline-specified vars next.
        // For dedup: keep first occurrence only.
        let mut seen_var_names: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut deduped_vars: Vec<(String, String)> = Vec::new();
        for (name, value) in self.args.variables.iter() {
            let key = name.trim_end_matches(|c: char| c == ':' || c == '?' || c == '+');
            if seen_var_names.insert(key.to_string()) {
                deduped_vars.push((name.clone(), value.clone()));
            }
        }
        self.args.variables = deduped_vars;

        // Rebuild MAKEFLAGS from the updated args in canonical format.
        // This ensures single-char flags are sorted and properly bundled.
        let new_makeflags = self.build_makeflags_from_args(&self.args.variables.clone());
        if let Some(var) = self.db.variables.get_mut("MAKEFLAGS") {
            var.value = new_makeflags.clone();
        }
        // Update the process environment so $(shell echo "$MAKEFLAGS") returns the current value.
        env::set_var("MAKEFLAGS", &new_makeflags);

        // Update .INCLUDE_DIRS to reflect current include_dirs
        // GNU Make's .INCLUDE_DIRS shows the effective include path (excluding special "-" entry)
        let include_dirs_str: String = self.args.include_dirs.iter()
            .filter(|d| d.to_string_lossy() != "-")
            .map(|d| d.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        self.db.variables.insert(".INCLUDE_DIRS".into(),
            Variable::new(include_dirs_str, VarFlavor::Simple, VarOrigin::Default));

        // If -r (no_builtin_rules) was activated via MAKEFLAGS inside the makefile,
        // remove the built-in pattern rules now (they were loaded at startup).
        if self.args.no_builtin_rules {
            let n = self.db.builtin_pattern_rules_count;
            if n > 0 && self.db.pattern_rules.len() >= n {
                self.db.pattern_rules.drain(..n);
                self.db.builtin_pattern_rules_count = 0;
            }
        }
    }

    pub fn evaluate_condition(&self, kind: &ConditionalKind) -> bool {
        match kind {
            ConditionalKind::Ifdef(var) => {
                let name = self.expand(var);
                self.db.variables.get(&name).map_or(false, |v| !v.value.is_empty())
            }
            ConditionalKind::Ifndef(var) => {
                let name = self.expand(var);
                !self.db.variables.get(&name).map_or(false, |v| !v.value.is_empty())
            }
            ConditionalKind::Ifeq(a, b) => {
                let ea = self.expand(a);
                let eb = self.expand(b);
                ea == eb
            }
            ConditionalKind::Ifneq(a, b) => {
                let ea = self.expand(a);
                let eb = self.expand(b);
                ea != eb
            }
        }
    }

    fn resolve_pending_includes(&mut self) -> Result<(), String> {
        if self.pending_includes.is_empty() {
            return Ok(());
        }

        let shell = self.db.variables.get("SHELL")
            .map(|v| v.value.clone())
            .unwrap_or_else(|| "/bin/sh".to_string());
        let shell_flags = self.db.variables.get(".SHELLFLAGS")
            .map(|v| v.value.clone())
            .unwrap_or_else(|| "-c".to_string());
        let silent = self.args.silent;

        // Process all pending includes in order.
        // If building any include creates more pending includes, loop again.
        let mut rounds = 0;
        loop {
            rounds += 1;
            if rounds > 20 { break; }

            let pending = std::mem::take(&mut self.pending_includes);
            if pending.is_empty() { break; }

            let mut any_rebuilt = false;

            for pi in pending {
                let file_path = Path::new(&pi.file);

                // If the file now exists (was built in a previous round), read it
                if file_path.exists() {
                    if let Err(e) = self.read_makefile(file_path) {
                        if !pi.ignore_missing {
                            return Err(format!("{}:{}: {}", pi.parent, pi.lineno, e));
                        }
                    }
                    any_rebuilt = true;
                    continue;
                }

                // Determine if there's a buildable rule for this file.
                // GNU Make rules for include rebuilding:
                //   - Double-colon rules with no prerequisites are NOT used to rebuild includes
                //   - Phony targets are NOT used to rebuild includes
                //   - Pattern rules and explicit rules with prerequisites or recipes can be used

                let is_phony = self.db.special_targets
                    .get(&SpecialTarget::Phony)
                    .map_or(false, |set| set.contains(&pi.file));

                if is_phony {
                    // Phony targets are not rebuilt as include files
                    if !pi.ignore_missing {
                        if !pi.parent.is_empty() {
                            eprintln!("{}:{}: {}: No such file or directory",
                                pi.parent, pi.lineno, pi.file);
                        }
                        return Err(String::new());
                    }
                    continue;
                }

                // Check for buildable rule
                let rule_info = self.find_include_rule(&pi.file);

                match rule_info {
                    None => {
                        // No rule at all - file can't be built
                        if !pi.ignore_missing {
                            // Print "No such file" warning before the fatal error
                            if !pi.parent.is_empty() {
                                eprintln!("{}:{}: {}: No such file or directory",
                                    pi.parent, pi.lineno, pi.file);
                            }
                            return Err(format!("No rule to make target '{}'.  Stop.", pi.file));
                        }
                        // Optional include with no rule: silently skip
                    }
                    Some(IncludeRuleInfo { skippable: true, .. }) => {
                        // Double-colon with no prerequisites or phony: not used for include rebuilding
                        if !pi.ignore_missing {
                            if !pi.parent.is_empty() {
                                eprintln!("{}:{}: {}: No such file or directory",
                                    pi.parent, pi.lineno, pi.file);
                            }
                            return Err(String::new());
                        }
                        // Optional include: silently skip
                    }
                    Some(IncludeRuleInfo { recipe, source_file, prerequisites, sibling_targets, .. }) => {
                        // Check if this file's recipe was already run by a sibling grouped
                        // pattern target (e.g. `%_a.mk %_b.mk:` ran for inc_a.mk already).
                        if self.include_recipe_ran.contains(&pi.file) {
                            // Recipe already ran; treat as "file not created" path.
                            if !pi.ignore_missing {
                                if !pi.parent.is_empty() {
                                    eprintln!("{}:{}: Failed to remake makefile '{}'.",
                                        pi.parent, pi.lineno, pi.file);
                                }
                                return Err(String::new());
                            }
                            // Optional include: silently skip
                        } else {
                        // First, check/build prerequisites
                        let mut visited = HashSet::new();
                        visited.insert(pi.file.clone());
                        let prereq_result = self.build_include_prerequisites(
                            &prerequisites,
                            &pi.file,
                            &shell,
                            &shell_flags,
                            silent,
                            pi.ignore_missing,
                            &mut visited,
                        );

                        match prereq_result {
                            Err(prereq_err) => {
                                // Prerequisite couldn't be built
                                if !pi.ignore_missing {
                                    // Print "No such file" warning for the include file
                                    if !pi.parent.is_empty() {
                                        eprintln!("{}:{}: {}: No such file or directory",
                                            pi.parent, pi.lineno, pi.file);
                                    }
                                    return Err(prereq_err);
                                }
                                // Optional include: silently ignore prerequisite failure
                            }
                            Ok(()) => {
                                if recipe.is_empty() {
                                    // Has prerequisites but no recipe: just check if file exists
                                    if !file_path.exists() && !pi.ignore_missing {
                                        if !pi.parent.is_empty() {
                                            eprintln!("{}:{}: {}: No such file or directory",
                                                pi.parent, pi.lineno, pi.file);
                                        }
                                        return Err(format!("No rule to make target '{}'.  Stop.", pi.file));
                                    }
                                } else {
                                    // Mark this target and all siblings as having had their
                                    // recipe run, so that sibling targets don't re-run it.
                                    self.include_recipe_ran.insert(pi.file.clone());
                                    for sib in &sibling_targets {
                                        self.include_recipe_ran.insert(sib.clone());
                                    }
                                    // Run the recipe to build the file
                                    let built = self.run_include_recipe(
                                        &pi.file,
                                        &recipe,
                                        &shell,
                                        &shell_flags,
                                        silent,
                                        &source_file,
                                    );
                                    match built {
                                        Ok(()) => {
                                            if file_path.exists() {
                                                if let Err(e) = self.read_makefile(file_path) {
                                                    if !pi.ignore_missing {
                                                        return Err(format!("{}:{}: {}", pi.parent, pi.lineno, e));
                                                    }
                                                }
                                                any_rebuilt = true;
                                            } else {
                                                // Recipe ran successfully but file not created
                                                if !pi.ignore_missing && !pi.parent.is_empty() {
                                                    eprintln!("{}:{}: Failed to remake makefile '{}'.",
                                                        pi.parent, pi.lineno, pi.file);
                                                }
                                                if !pi.ignore_missing {
                                                    return Err(String::new()); // fatal but no additional message
                                                }
                                                // Optional include: silently skip if file not created
                                            }
                                        }
                                        Err(recipe_err) => {
                                            // Recipe failed (non-zero exit)
                                            if !pi.ignore_missing {
                                                // Print "No such file" warning
                                                if !pi.parent.is_empty() {
                                                    eprintln!("{}:{}: {}: No such file or directory",
                                                        pi.parent, pi.lineno, pi.file);
                                                }
                                                // recipe_err already has "[source:lineno: target] Error N" format
                                                return Err(recipe_err);
                                            }
                                            // Optional include with failed recipe: silently skip
                                        }
                                    }
                                }
                            }
                        }
                        } // end else (not in include_recipe_ran)
                    }
                }
            }

            if !any_rebuilt {
                break;
            }
        }

        Ok(())
    }

    /// Build prerequisites needed before building an include file.
    /// Returns Ok(()) if all prerequisites exist or were successfully built.
    /// Only follows explicit (non-pattern) rules to avoid infinite recursion
    /// through implicit rule chains.
    /// `ignore_missing`: if true, suppress error printing (for optional include chains).
    fn build_include_prerequisites(
        &self,
        prerequisites: &[String],
        include_target: &str,
        shell: &str,
        shell_flags: &str,
        silent: bool,
        ignore_missing: bool,
        visited: &mut HashSet<String>,
    ) -> Result<(), String> {
        for prereq in prerequisites {
            let prereq_path = Path::new(prereq);
            if prereq_path.exists() {
                continue;
            }
            if visited.contains(prereq) {
                // Cycle detected: treat as can't build
                return Err(format!("No rule to make target '{}', needed by '{}'.  Stop.",
                    prereq, include_target));
            }
            visited.insert(prereq.clone());

            // Only look at explicit rules (not pattern rules) to avoid infinite
            // recursion through implicit rule chains like %: %.o: %.c etc.
            let explicit_rule = self.db.rules.get(prereq).and_then(|rules| {
                rules.iter().find(|r| {
                    // Skip double-colon with no prerequisites
                    !(r.is_double_colon && r.prerequisites.is_empty())
                    // Has something to do (recipe or dependencies)
                    && (!r.recipe.is_empty() || !r.prerequisites.is_empty())
                })
            }).cloned();

            match explicit_rule {
                None => {
                    // No explicit rule and file doesn't exist
                    return Err(format!("No rule to make target '{}', needed by '{}'.  Stop.",
                        prereq, include_target));
                }
                Some(rule) => {
                    // Build this prereq's prerequisites first
                    if !rule.prerequisites.is_empty() {
                        self.build_include_prerequisites(
                            &rule.prerequisites.clone(),
                            prereq,
                            shell,
                            shell_flags,
                            silent,
                            ignore_missing,
                            visited,
                        )?;
                    }
                    // Run the recipe
                    if !rule.recipe.is_empty() {
                        let src = rule.source_file.clone();
                        self.run_include_recipe(
                            prereq, &rule.recipe.clone(), shell, shell_flags, silent,
                            &src,
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Information about a rule found for building an include file.
    fn find_include_rule(&self, target: &str) -> Option<IncludeRuleInfo> {
        // Check explicit rules first
        if let Some(rules) = self.db.rules.get(target) {
            for rule in rules {
                // Double-colon rules with no prerequisites are never used for include rebuilding
                if rule.is_double_colon && rule.prerequisites.is_empty() {
                    return Some(IncludeRuleInfo {
                        recipe: Vec::new(),
                        source_file: rule.source_file.clone(),
                        recipe_lineno: 0,
                        prerequisites: Vec::new(),
                        skippable: true,
                        sibling_targets: Vec::new(),
                    });
                }
                // A rule that has a recipe or has prerequisites counts
                if !rule.recipe.is_empty() || !rule.prerequisites.is_empty() {
                    let src = rule.source_file.clone();
                    let ln = rule.recipe.first().map(|(l, _)| *l).unwrap_or(0);
                    return Some(IncludeRuleInfo {
                        recipe: rule.recipe.clone(),
                        source_file: src,
                        recipe_lineno: ln,
                        prerequisites: rule.prerequisites.clone(),
                        skippable: false,
                        sibling_targets: Vec::new(),
                    });
                }
            }
        }
        // Check pattern rules - but skip built-in (implicit) rules which have empty source_file.
        // GNU Make only uses makefile-defined rules to rebuild include files, not the default
        // implicit rules (which would cause chains like %: %.o → inc2.o etc.).
        for rule in &self.db.pattern_rules {
            // Skip built-in rules (they have empty source_file)
            if rule.source_file.is_empty() {
                continue;
            }
            for pat in &rule.targets {
                if let Some(stem) = parser::match_pattern(target, pat) {
                    if !rule.recipe.is_empty() {
                        let recipe: Vec<(usize, String)> = rule.recipe.iter()
                            .map(|(ln, cmd)| (*ln, cmd.clone()))
                            .collect();
                        let src = rule.source_file.clone();
                        let ln = recipe.first().map(|(l, _)| *l).unwrap_or(0);
                        // Expand prerequisites for the stem
                        let prereqs: Vec<String> = rule.prerequisites.iter()
                            .map(|p| p.replace('%', &stem))
                            .collect();
                        // Compute sibling targets: other patterns in the same rule
                        // that also match via the same stem (for grouped pattern rules).
                        let siblings: Vec<String> = rule.targets.iter()
                            .filter(|p2| p2.as_str() != pat)
                            .map(|p2| p2.replace('%', &stem))
                            .collect();
                        return Some(IncludeRuleInfo {
                            recipe,
                            source_file: src,
                            recipe_lineno: ln,
                            prerequisites: prereqs,
                            skippable: false,
                            sibling_targets: siblings,
                        });
                    }
                }
            }
        }
        None
    }

    /// Run the recipe commands for building an include file.
    /// `source_file`: the makefile that defined the recipe (for error messages).
    /// Returns Err with formatted "[source_file:lineno: target] Error N" string on failure.
    /// Callers are responsible for printing warnings and propagating the error.
    fn run_include_recipe(
        &self, target: &str, recipe: &[(usize, String)],
        shell: &str, shell_flags: &str, silent: bool,
        source_file: &str,
    ) -> Result<(), String> {
        // Set up automatic variables for recipe expansion ($@ = target)
        let mut auto_vars = HashMap::new();
        auto_vars.insert("@".to_string(), target.to_string());

        for (lineno, cmd_template) in recipe {
            let mut cmd = cmd_template.clone();
            let mut cmd_silent = false;
            let mut ignore_error = false;
            loop {
                match cmd.chars().next().unwrap_or(' ') {
                    '@' => { cmd_silent = true; cmd = cmd[1..].to_string(); }
                    '-' => { ignore_error = true; cmd = cmd[1..].to_string(); }
                    '+' => { cmd = cmd[1..].to_string(); }
                    _ => break,
                }
            }
            let expanded_cmd = self.expand_with_auto_vars(&cmd, &auto_vars);
            if !silent && !cmd_silent {
                println!("{}", expanded_cmd);
            }
            let status = std::process::Command::new(shell)
                .arg(shell_flags)
                .arg(&expanded_cmd)
                .status();
            match status {
                Ok(s) if s.success() => {}
                Ok(s) => {
                    let code = s.code().unwrap_or(1);
                    // Error format: [source_file:lineno: target] Error N
                    // Caller is responsible for printing the "*** [...]" error message.
                    if !ignore_error {
                        return Err(format!("[{}:{}: {}] Error {}", source_file, lineno, target, code));
                    }
                }
                Err(e) => {
                    if !ignore_error {
                        return Err(format!("shell error: {}", e));
                    }
                }
            }
        }
        Ok(())
    }

    fn find_include_file(&self, file: &str) -> Option<PathBuf> {
        let path = Path::new(file);
        if path.exists() {
            return Some(path.to_path_buf());
        }

        // Search include directories
        for dir in &self.include_dirs {
            let candidate = dir.join(file);
            if candidate.exists() {
                return Some(candidate);
            }
        }

        // Search default include dirs
        let default_dirs = vec!["/usr/include", "/usr/local/include"];
        for dir in default_dirs {
            let candidate = Path::new(dir).join(file);
            if candidate.exists() {
                return Some(candidate);
            }
        }

        None
    }

    fn print_database(&self) {
        println!("# GNU Make 4.4.1 compatible database");
        println!("# jmake clean-room implementation");
        println!();

        // Variables
        println!("# Variables");
        for (name, var) in &self.db.variables {
            let flavor = match var.flavor {
                VarFlavor::Recursive => "recursive",
                VarFlavor::Simple => "simple",
                _ => "recursive",
            };
            let origin = match var.origin {
                VarOrigin::Default => "default",
                VarOrigin::Environment => "environment",
                VarOrigin::File => "file",
                VarOrigin::CommandLine => "command line",
                VarOrigin::Override => "override",
                VarOrigin::Automatic => "automatic",
            };
            println!("# {} ({}, {})", name, flavor, origin);
            println!("{} = {}", name, var.value);
        }

        println!();
        println!("# Rules");
        for (target, rules) in &self.db.rules {
            for rule in rules {
                let prereqs = rule.prerequisites.join(" ");
                let sep = if rule.is_double_colon { "::" } else { ":" };
                println!("{}{} {}", target, sep, prereqs);
                for (_lineno, line) in &rule.recipe {
                    println!("\t{}", line);
                }
            }
        }

        println!();
        println!("# Pattern Rules");
        for rule in &self.db.pattern_rules {
            let targets = rule.targets.join(" ");
            let prereqs = rule.prerequisites.join(" ");
            println!("{}: {}", targets, prereqs);
            for (_lineno, line) in &rule.recipe {
                println!("\t{}", line);
            }
        }
    }
}

fn is_suffix_rule(target: &str, suffixes: &[String]) -> bool {
    if !target.starts_with('.') {
        return false;
    }

    // Single suffix rule: .c -> builds from .c
    if suffixes.contains(&target.to_string()) {
        return true;
    }

    // Double suffix rule: .c.o -> builds .o from .c
    for s1 in suffixes {
        if target.starts_with(s1.as_str()) {
            let rest = &target[s1.len()..];
            if suffixes.contains(&rest.to_string()) {
                return true;
            }
        }
    }

    false
}

fn suffix_to_pattern_rule(target: &str, rule: &Rule, suffixes: &[String]) -> Option<Rule> {
    // Convert double-suffix rule .s1.s2 to %.s2: %.s1
    // Convert single-suffix rule .s1    to %: %.s1
    // Uses the actual current suffix list.
    for s1 in suffixes {
        if target.starts_with(s1.as_str()) {
            let s2 = &target[s1.len()..];
            if s2.is_empty() {
                // Single-suffix rule: .s1: → %: %.s1
                let mut pattern_rule = rule.clone();
                pattern_rule.targets = vec!["%".to_string()];
                pattern_rule.prerequisites = vec![format!("%{}", s1)];
                pattern_rule.is_pattern = true;
                return Some(pattern_rule);
            } else if suffixes.iter().any(|s| s.as_str() == s2) {
                // Double-suffix rule: .s1.s2 → %.s2: %.s1
                let mut pattern_rule = rule.clone();
                pattern_rule.targets = vec![format!("%{}", s2)];
                pattern_rule.prerequisites = vec![format!("%{}", s1)];
                pattern_rule.is_pattern = true;
                return Some(pattern_rule);
            }
        }
    }

    None
}

/// Try to detect and expand the variable name in a `define` directive line.
///
/// Returns `Some(expanded_line)` if the line is a `define` directive (possibly
/// prefixed with `override` and/or `export`), where the variable name (the token
/// immediately after the `define` keyword) has been expanded.
///
/// Returns `None` if the line is not a define directive (e.g. it's a regular
/// assignment like `define = value`, or starts with some other keyword).
///
/// This is called before `try_parse_variable_assignment` so that lines like
/// `define $(NAME) =` are handled as define directives (with $(NAME) expanded)
/// rather than misinterpreted as assignments with name `define $(NAME)`.
fn try_expand_define_name(trimmed: &str, state: &MakeState) -> Option<String> {
    // Strip optional `override`/`export` prefixes
    let mut prefix = String::new();
    let mut work = trimmed;
    loop {
        if work.starts_with("override ") || work.starts_with("override\t") {
            let n = "override".len();
            prefix.push_str("override ");
            work = work[n..].trim_start();
        } else if work.starts_with("export ") || work.starts_with("export\t") {
            let n = "export".len();
            prefix.push_str("export ");
            work = work[n..].trim_start();
        } else {
            break;
        }
    }

    // Must start with exactly "define" (followed by space/tab) or be exactly "define"
    let rest_after_define = if work.starts_with("define ") || work.starts_with("define\t") {
        work["define".len()..].trim_start()
    } else if work == "define" {
        ""
    } else {
        return None;
    };

    // Assignment operators that make this NOT a define directive but a regular assignment
    // (e.g. `define = value` defines a variable named "define").
    let assignment_ops = [":::=", "::=", "!=", "?=", "+=", ":=", "="];
    let starts_with_op = assignment_ops.iter().any(|op| {
        rest_after_define.starts_with(op) && (
            rest_after_define.len() == op.len()
            || rest_after_define[op.len()..].starts_with(' ')
            || rest_after_define[op.len()..].starts_with('\t')
        )
    });
    // `define : recipe` → rule with target named "define", not a define directive
    let starts_with_rule_colon = rest_after_define.starts_with(':')
        && !rest_after_define.starts_with(":=")
        && !rest_after_define.starts_with("::=")
        && !rest_after_define.starts_with(":::=");

    if starts_with_op || starts_with_rule_colon {
        return None;
    }

    // This is a define directive.  Expand ONLY the variable name token.
    // rest_after_define is `VAR_NAME [OP [EXTRANEOUS]]`.
    // We must expand variable references in VAR_NAME but keep OP and anything
    // after it verbatim (so that `has_extraneous` detection still works in
    // parse_define_start).
    //
    // Find the variable name: it is everything up to the first whitespace or
    // the first occurrence of an assignment operator that is immediately adjacent
    // to the name (e.g. `NAME=` or `NAME:=`).
    let var_name_end = {
        // Scan for the end of the name token: stop at unparenthesized whitespace
        // or when an assignment op starts at depth 0.
        // Track parenthesis depth to correctly handle names like `$(subst e,e,$(NAME))`.
        let ops = [":::=", "::=", "!=", "?=", "+=", ":=", "="];
        let bytes = rest_after_define.as_bytes();
        let mut end = 0;
        let mut paren_depth: i32 = 0;
        while end < bytes.len() {
            // Track $( and ${ as opening delimiters
            if bytes[end] == b'$' && end + 1 < bytes.len()
                && (bytes[end+1] == b'(' || bytes[end+1] == b'{')
            {
                paren_depth += 1;
                end += 2;
                continue;
            }
            match bytes[end] {
                b'(' | b'{' if paren_depth > 0 => { paren_depth += 1; }
                b')' | b'}' if paren_depth > 0 => { paren_depth -= 1; }
                _ => {}
            }
            if paren_depth == 0 {
                if bytes[end].is_ascii_whitespace() {
                    break;
                }
                // Check if any op starts here (name adjacent to op, e.g. `NAME=`)
                let suffix = &rest_after_define[end..];
                if ops.iter().any(|op| suffix.starts_with(op)) {
                    break;
                }
            }
            end += 1;
        }
        end
    };
    let var_name_raw = &rest_after_define[..var_name_end];
    let after_name = &rest_after_define[var_name_end..]; // " = VALUE" or "= VALUE" etc.

    let expanded_name = state.expand(var_name_raw);

    // Reconstruct: "define EXPANDED_NAME<rest>" (plus prefix if any)
    Some(format!("{}define {}{}", prefix, expanded_name, after_name))
}

/// Extract the `override`/`export`/`private` prefix(es) from a raw variable
/// assignment line so they can be re-prepended after expanding the name.
/// Returns the prefix string including any trailing spaces (e.g. "override ").
fn extract_var_prefixes(line: &str) -> String {
    let mut result = String::new();
    let mut work = line;
    loop {
        if work.starts_with("override ") {
            result.push_str("override ");
            work = work["override ".len()..].trim_start();
        } else if work.starts_with("export ") {
            result.push_str("export ");
            work = work["export ".len()..].trim_start();
        } else if work.starts_with("private ") {
            result.push_str("private ");
            work = work["private ".len()..].trim_start();
        } else {
            break;
        }
    }
    result
}

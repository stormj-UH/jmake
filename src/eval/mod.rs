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

// Thread-local re-entry guard for shell_exec_with_env.
// Prevents infinite recursion when exported variables contain $(shell ...).
thread_local! {
    static IN_SHELL_EXEC_WITH_ENV: RefCell<bool> = RefCell::new(false);
}

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
    /// True if the rule "imagines" the target was updated (sv 61226):
    /// a rule with no recipe AND no prerequisites. In this case the target
    /// is treated as if it was rebuilt but the file is never actually created,
    /// so no re-exec is triggered and no error is reported.
    imagined: bool,
    /// For pattern rules with multiple target patterns: the sibling target names
    /// (i.e., the other targets that would be built by the same recipe invocation).
    /// When this recipe runs for one target, all siblings are also considered attempted.
    sibling_targets: Vec<String>,
    /// The stem matched when this came from a pattern rule (empty for explicit rules).
    stem: String,
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
    /// Exit status of the last $(shell ...) call, used to set .SHELLSTATUS.
    /// None means no $(shell ...) has been called yet (so .SHELLSTATUS is empty).
    pub last_shell_status: RefCell<Option<i32>>,
    /// Set to true while performing second expansion of prerequisites.
    /// When eval() is called in this context, it must not create new rules
    /// (GNU Make error: "prerequisites cannot be defined in recipes").
    pub in_second_expansion: RefCell<bool>,
    /// Set to true while executing a recipe (expanding recipe lines for a shell).
    /// When eval() is called in this context and the content defines new
    /// prerequisites, it must produce the "prerequisites cannot be defined in
    /// recipes" error (GNU Make Savannah bug #12124).
    pub in_recipe_execution: RefCell<bool>,
    /// Pending includes that couldn't be found during initial read
    pub pending_includes: Vec<PendingInclude>,
    /// Current include depth (for detecting infinite recursion)
    pub include_depth: usize,
    /// Set of include file names for which a rebuild recipe has already been
    /// attempted (ran or was considered ran via grouped pattern rules).
    /// Used to avoid running the same recipe twice for grouped pattern rules.
    pub include_recipe_ran: HashSet<String>,
    /// True once "Entering directory" has been printed, to avoid printing it twice.
    pub entering_directory_printed: bool,
    /// Set to true when MAKEOVERRIDES= (empty) has been assigned from a makefile.
    /// Causes the `-- ` separator to be shown immediately (with empty vars), but
    /// MAKEFLAGS is rebuilt cleanly (without `-- `) before executing recipes.
    pub makeoverrides_cleared: bool,
    /// Path to temp file created for stdin (-f-) content.
    /// Used to pass --temp-stdin=PATH when re-exec'ing, and deleted on exit.
    /// None if -f- was not used or if re-exec read from --temp-stdin= (already have path in args).
    pub stdin_temp_path: Option<PathBuf>,
    /// Set of file names whose include-phase recipe ran but produced no file (sv 61226 "imagined").
    /// During the main build phase, these files are treated as if they were already attempted
    /// so we don't re-run their recipes and avoid duplicate output.
    pub include_imagined: HashSet<String>,
    /// Stack of call parameter contexts for $(call).
    /// Each entry is a HashMap mapping "0", "1", "2", etc. to their values.
    /// Pushed when entering a $(call) and popped when leaving.
    /// When variable `1`, `2`, etc. is looked up and not found in db.variables,
    /// the top of this stack is checked. This allows $(eval) inside $(call) to
    /// correctly expand `$1` to the call argument.
    pub call_context_stack: RefCell<Vec<HashMap<String, String>>>,
    /// Stack of (file, line) pairs saved before each recursive-variable context override
    /// in expand_var_value.  The $(error) and $(warning) functions use the outermost
    /// (deepest stack) entry so that "Error found!" in a lazy var reports the recipe
    /// line where the variable was referenced, not the variable's definition line.
    pub expansion_caller_stack: RefCell<Vec<(String, usize)>>,
    /// Set of variable names currently being expanded (used to detect/prevent infinite
    /// recursion when a recursive variable references itself, e.g. via $(eval ...)).
    /// When expansion of a recursive variable is requested and the variable is already
    /// in this set, the expansion returns empty string instead of recursing infinitely.
    pub vars_being_expanded: RefCell<std::collections::HashSet<String>>,
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
    let raw = env::args().next().unwrap_or_else(|| "make".to_string());
    // Use basename only (GNU Make behavior: program name is always the filename, not full path)
    let base = std::path::Path::new(&raw)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&raw)
        .to_string();
    match env::var("MAKELEVEL").ok().and_then(|v| v.parse::<u32>().ok()) {
        Some(level) if level > 0 => format!("{}[{}]", base, level),
        _ => base,
    }
}

/// Execute pre-expanded recipe lines for an include file.
/// `expanded`: (lineno, expanded_cmd, silent, ignore_error) tuples from expand_include_recipe_lines.
/// Does NOT require &MakeState — safe to run in a thread.
pub fn execute_include_recipe_expanded(
    expanded: &[(usize, String, bool, bool)],
    target: &str,
    shell: &str,
    shell_flags: &str,
    silent: bool,
    source_file: &str,
) -> Result<(), String> {
    use std::os::unix::process::ExitStatusExt;
    let progname = make_progname();

    for (lineno, expanded_cmd, cmd_silent, ignore_error) in expanded {
        if !silent && !cmd_silent && !expanded_cmd.is_empty() {
            println!("{}", expanded_cmd);
        }

        if expanded_cmd.is_empty() {
            continue;
        }

        let term_msg = format!(
            "{}: *** [{}:{}: {}] Terminated\n",
            progname, source_file, lineno, target
        );
        crate::signal_handler::set_term_message(&term_msg);

        let status = std::process::Command::new(shell)
            .arg(shell_flags)
            .arg(expanded_cmd)
            .status();

        crate::signal_handler::clear_term_message();

        match status {
            Ok(s) if s.success() => {}
            Ok(s) => {
                if let Some(sig) = s.signal() {
                    if !ignore_error {
                        let sig_name = match sig {
                            libc::SIGTERM => "Terminated",
                            libc::SIGINT => "Interrupt",
                            libc::SIGHUP => "Hangup",
                            libc::SIGKILL => "Killed",
                            _ => "Signal",
                        };
                        return Err(format!("[{}:{}: {}] {}", source_file, lineno, target, sig_name));
                    }
                } else {
                    let code = s.code().unwrap_or(1);
                    if !ignore_error {
                        return Err(format!("[{}:{}: {}] Error {}", source_file, lineno, target, code));
                    }
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
            last_shell_status: RefCell::new(None),
            in_second_expansion: RefCell::new(false),
            in_recipe_execution: RefCell::new(false),
            pending_includes: Vec::new(),
            include_depth: 0,
            include_recipe_ran: HashSet::new(),
            entering_directory_printed: false,
            makeoverrides_cleared: false,
            stdin_temp_path: None,
            include_imagined: HashSet::new(),
            call_context_stack: RefCell::new(Vec::new()),
            expansion_caller_stack: RefCell::new(Vec::new()),
            vars_being_expanded: RefCell::new(std::collections::HashSet::new()),
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
            // Check if we already printed "Entering directory" in a parent re-exec invocation.
            let entering_already_printed = env::var("JMAKE_ENTERING_PRINTED").as_deref() == Ok("1");
            if should_print_directory(&state.args) && !entering_already_printed {
                let cwd = logical_cwd();
                println!("{}: Entering directory '{}'", progname, cwd.display());
                state.entering_directory_printed = true;
            } else if entering_already_printed {
                state.entering_directory_printed = true;
            }
        }

        // Set up include directories
        state.include_dirs = state.args.include_dirs.clone();

        state
    }

    pub fn run(&mut self) -> Result<(), String> {
        self.init_variables();

        // Print version banner when any debug output is requested (-d or --debug).
        // GNU Make always prints its version string at the start of debug output.
        let debug_active = self.args.debug_short || !self.args.debug.is_empty();
        if debug_active {
            println!("GNU Make 4.4.1");
        }

        if !self.args.no_builtin_rules && !self.args.no_builtin_variables {
            implicit_rules::register_default_variables(&mut self.db);
        }
        if !self.args.no_builtin_rules {
            implicit_rules::register_implicit_rules(&mut self.db);
        }

        // Print entering-directory if needed (for -w without -C)
        // Must be BEFORE read_makefiles so $(info ...) at parse time appears after the header.
        // When re-exec'ing after a makefile rebuild, JMAKE_ENTERING_PRINTED=1 suppresses the
        // duplicate "Entering directory" message (the outer invocation already printed it).
        let progname = make_progname();
        let print_dir = should_print_directory(&self.args);
        let entering_already_printed = env::var("JMAKE_ENTERING_PRINTED").as_deref() == Ok("1");
        if print_dir && !self.entering_directory_printed && !entering_already_printed {
            // Already printed at startup for -C; print here for -w without -C
            let cwd = logical_cwd();
            println!("{}: Entering directory '{}'", progname, cwd.display());
            self.entering_directory_printed = true;
        } else if entering_already_printed {
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

        // Try to update makefiles (rebuild included/main makefiles if out of date).
        // If any makefile is rebuilt, this re-execs the process (never returns).
        self.try_update_makefiles()?;

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
        if self.args.no_builtin_variables {
            self.args.no_builtin_rules = true;
            // Rebuild MAKEFLAGS so the recipe-level $(info $(MAKEFLAGS)) includes 'r'.
            let new_makeflags = self.build_makeflags();
            if let Some(var) = self.db.variables.get_mut("MAKEFLAGS") {
                var.value = new_makeflags.clone();
            }
            env::set_var("MAKEFLAGS", &new_makeflags);
        }

        // MAKEOVERRIDES= was set to empty during makefile parsing, which caused MAKEFLAGS
        // to be set to "rR -- " (with trailing -- separator). Before running recipes,
        // rebuild MAKEFLAGS in canonical form (without the trailing -- since there are no vars).
        if self.makeoverrides_cleared {
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
            self.args.load_average,
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
            self.args.shuffle.clone(),
        );

        let result = executor.build_targets(&targets);

        // Print leaving-directory if needed
        let print_dir = should_print_directory(&self.args);
        if print_dir {
            let cwd = logical_cwd();
            println!("{}: Leaving directory '{}'", progname, cwd.display());
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
        // Build $(-*-eval-flags-*-) from --eval strings (used in MAKEFLAGS expansion).
        // Each eval string is quoted: $ → $$, spaces/backslashes → \<char>.
        // GNU Make stores them as "--eval=quoted_string [--eval=quoted_string ...]".
        {
            fn quote_for_env(s: &str) -> String {
                let mut out = String::with_capacity(s.len() * 2);
                for ch in s.chars() {
                    if ch == '$' {
                        out.push('$');
                    } else if ch == ' ' || ch == '\t' || ch == '\\' {
                        out.push('\\');
                    }
                    out.push(ch);
                }
                out
            }
            let eval_flags: String = self.args.eval_strings.iter()
                .map(|s| format!("--eval={}", quote_for_env(s)))
                .collect::<Vec<_>>()
                .join(" ");
            self.db.variables.insert("-*-eval-flags-*-".into(),
                Variable::new(eval_flags, VarFlavor::Simple, VarOrigin::Default));
        }
        {
            let mf = self.build_makeflags();
            // Also update the process environment so $(shell echo "$MAKEFLAGS") reflects
            // the canonical merged value from the start.
            env::set_var("MAKEFLAGS", &mf);
            self.db.variables.insert("MAKEFLAGS".into(),
                Variable::new(mf, VarFlavor::Recursive, VarOrigin::Default));
        }
        // MAKEOVERRIDES: command-line variable assignments portion of MAKEFLAGS.
        // Only include variables that came from the actual command line (not from env MAKEFLAGS).
        {
            let cmdline_start = self.args.cmdline_vars_start;
            let ov: Vec<String> = self.args.variables[cmdline_start..]
                .iter()
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
        // MAKE_RESTARTS: number of times make has re-exec'd itself to reread makefiles.
        // Empty on the first run; set to "1", "2", etc. by the re-exec mechanism.
        // Read from the MAKE_RESTARTS environment variable (set by the reinvoke logic).
        let make_restarts = env::var("MAKE_RESTARTS").unwrap_or_default();
        self.db.variables.insert("MAKE_RESTARTS".into(),
            Variable::new(make_restarts, VarFlavor::Simple, VarOrigin::Default));
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

        // Command-line variables override everything; GNU Make stores them as recursive.
        // Handle operator suffixes: FOO+=val (append), FOO?=val (conditional), FOO:=val (simple).
        for (name, value) in &self.args.variables {
            if name.ends_with('+') {
                // Append operator: strip '+', append value to existing
                let real_name = name.trim_end_matches('+').to_string();
                let existing = self.db.variables.get(&real_name)
                    .map(|v| v.value.clone())
                    .unwrap_or_default();
                let new_val = if existing.is_empty() {
                    value.clone()
                } else {
                    format!("{} {}", existing, value)
                };
                self.db.variables.insert(real_name,
                    Variable::new(new_val, VarFlavor::Recursive, VarOrigin::CommandLine));
            } else if name.ends_with('?') {
                // Conditional: only set if not already defined
                let real_name = name.trim_end_matches('?').to_string();
                self.db.variables.entry(real_name).or_insert_with(||
                    Variable::new(value.clone(), VarFlavor::Recursive, VarOrigin::CommandLine));
            } else if name.ends_with(':') {
                // Simple assignment
                let real_name = name.trim_end_matches(':').to_string();
                self.db.variables.insert(real_name,
                    Variable::new(value.clone(), VarFlavor::Simple, VarOrigin::CommandLine));
            } else {
                self.db.variables.insert(name.clone(),
                    Variable::new(value.clone(), VarFlavor::Recursive, VarOrigin::CommandLine));
            }
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

    /// Execute a shell command with the makefile's exported variable environment.
    /// Returns (stdout_processed, exit_status).
    /// This ensures $(shell ...) sees the correct values of exported make variables,
    /// overriding environment variables that have been redefined in the makefile.
    pub fn shell_exec_with_env(&self, cmd: &str) -> (String, i32) {
        // Re-entry guard: if we are already inside shell_exec_with_env (because
        // expanding an exported variable itself triggered $(shell ...)), fall back
        // to a plain shell execution without computing extra exports. This prevents
        // infinite recursion (and a SIGSEGV from stack overflow).
        let already_in = IN_SHELL_EXEC_WITH_ENV.with(|flag| *flag.borrow());
        if already_in {
            let progname = self.progname();
            return functions::fn_shell_exec_with_status_env(cmd, &HashMap::new(), &[], &progname);
        }

        // Set re-entry guard (RAII: use a struct that resets on drop)
        struct ShellGuard;
        impl Drop for ShellGuard {
            fn drop(&mut self) {
                IN_SHELL_EXEC_WITH_ENV.with(|flag| *flag.borrow_mut() = false);
            }
        }
        IN_SHELL_EXEC_WITH_ENV.with(|flag| *flag.borrow_mut() = true);
        let _guard = ShellGuard;

        let mut extra_env: HashMap<String, String> = HashMap::new();
        let mut remove_env: Vec<String> = Vec::new();
        for (name, var) in &self.db.variables {
            if name == "MAKELEVEL" {
                continue;
            }
            let always_export = matches!(name.as_str(), "MAKEFLAGS" | "MAKE" | "MAKECMDGOALS");
            let was_from_env = self.db.env_var_names.contains(name.as_str());
            let should_export = always_export || match var.export {
                Some(true) => true,
                Some(false) => false,
                None => self.db.export_all || was_from_env,
            };
            if should_export {
                let value = self.expand(&var.value);
                extra_env.insert(name.clone(), value);
            } else {
                // Remove from env so the child doesn't see a stale env value.
                // Only do this for vars that were originally from the environment.
                if was_from_env {
                    remove_env.push(name.clone());
                }
            }
        }

        let progname = self.progname();
        functions::fn_shell_exec_with_status_env(cmd, &extra_env, &remove_env, &progname)
    }

    /// Return the program name for error messages.
    fn progname(&self) -> String {
        make_progname()
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

        // Job count: -j N (only when > 1; -j1 is the default and not passed on)
        if self.args.jobs > 1 {
            long_parts.push(format!("-j{}", self.args.jobs));
        }

        // No-arg long options (come after options-with-args)
        if self.args.trace { long_parts.push("--trace".to_string()); }
        if self.args.no_print_directory { long_parts.push("--no-print-directory".to_string()); }
        if self.args.no_silent { long_parts.push("--no-silent".to_string()); }
        if self.args.warn_undefined_variables { long_parts.push("--warn-undefined-variables".to_string()); }

        // --eval=... strings are appended via the $(-*-eval-flags-*-) reference.
        // Build that reference if we have eval strings.
        // GNU Make quotes each eval string: $ → $$, spaces/backslashes → \<char>
        if !self.args.eval_strings.is_empty() {
            long_parts.push("$(-*-eval-flags-*-)".to_string());
        }

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
        // Process MAKEFILES variable: space-separated list of makefiles to
        // include before the regular makefiles (like -include).
        // GNU Make checks the MAKEFILES Make variable (which may be set from
        // the environment OR from a command-line assignment like `make MAKEFILES=x`).
        // Prefer the Make variable (which reflects command-line overrides) over
        // the raw OS environment variable.
        let makefiles_val = self.db.variables.get("MAKEFILES")
            .map(|v| v.value.clone())
            .or_else(|| env::var("MAKEFILES").ok())
            .unwrap_or_default();
        if !makefiles_val.is_empty() {
            // GNU Make: files listed in MAKEFILES must not affect the default goal.
            // Save the default goal state and restore it after processing MAKEFILES
            // files, so that the first rule from the main makefile becomes the default.
            let saved_default_target = self.db.default_target.clone();
            let saved_default_goal_explicit = self.db.default_goal_explicit;
            for file in makefiles_val.split_whitespace() {
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
            // Restore default goal state — MAKEFILES files don't set the default goal.
            self.db.default_target = saved_default_target;
            self.db.default_goal_explicit = saved_default_goal_explicit;
            // Also reset .DEFAULT_GOAL variable to empty (since MAKEFILES files
            // may have set it to their first target).
            if let Some(var) = self.db.variables.get_mut(".DEFAULT_GOAL") {
                var.value = String::new();
            }
        }

        let makefiles = if self.args.makefiles.is_empty() {
            // Default makefile search order
            let candidates = vec!["GNUmakefile", "makefile", "Makefile"];
            let mut found = Vec::new();
            for name in &candidates {
                if Path::new(name).exists() {
                    found.push(PathBuf::from(name));
                    break;
                }
            }
            if found.is_empty() {
                // No default makefile found.
                // If there are -E eval strings, we can proceed; but we still add the
                // default makefile names as "pending includes" so they can be rebuilt
                // via rules (GNU Make behavior: tries GNUmakefile, makefile, Makefile
                // via available rules even when -E strings provide content).
                if !self.args.eval_strings.is_empty() {
                    // Add default makefile names as pending includes (ignore_missing=true
                    // so we don't error if they can't be built).
                    for name in &candidates {
                        self.pending_includes.push(PendingInclude {
                            file: name.to_string(),
                            parent: String::new(),
                            lineno: 0,
                            ignore_missing: true,
                        });
                    }
                    return Ok(());
                }
                return Err("No targets.  Stop.".to_string());
            }
            // GNU Make always tries to remake ALL three default makefile candidates
            // (GNUmakefile, makefile, Makefile), even if one was found and read.
            // Add all three as pending includes so they can be updated/created by rules.
            for name in &candidates {
                self.pending_includes.push(PendingInclude {
                    file: name.to_string(),
                    parent: String::new(),
                    lineno: 0,
                    ignore_missing: true,
                });
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
            if let Err(e) = self.read_makefile(mf) {
                let mf_str = mf.to_string_lossy();
                let is_stdin = mf_str == "-" || mf_str == "/dev/stdin";
                // For non-stdin -f files that can't be read because they don't exist,
                // GNU Make prints a warning without "***" and continues processing
                // remaining makefiles. The missing file is then treated as a rebuild
                // target; if no rule exists it will fail as "No rule to make target".
                if !is_stdin && e.contains("No such file or directory") {
                    let progname = crate::eval::make_progname();
                    eprintln!("{}: {}", progname, e);
                    // Defer: add to pending so try_update_makefiles can attempt a rebuild
                    self.pending_includes.push(PendingInclude {
                        file: mf_str.to_string(),
                        parent: String::new(),
                        lineno: 0,
                        ignore_missing: false,
                    });
                } else {
                    return Err(e);
                }
            }
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
        self.read_makefile_display(path, None)
    }

    /// Like read_makefile but uses `display_name` as the filename shown in error messages.
    /// GNU Make uses the original include argument (without the -I directory prefix) in errors.
    pub fn read_makefile_display(&mut self, path: &Path, display_name: Option<&str>) -> Result<(), String> {
        // Guard against infinite include recursion (e.g., a makefile including itself)
        self.include_depth += 1;
        if self.include_depth > 200 {
            self.include_depth -= 1;
            let progname = std::env::args().next().unwrap_or_else(|| "make".into());
            return Err(format!("{}: *** Recursive include of '{}'. Stop.",
                progname, path.display()));
        }
        let result = self.read_makefile_display_inner(path, display_name);
        self.include_depth -= 1;
        result
    }

    fn read_makefile_display_inner(&mut self, path: &Path, display_name: Option<&str>) -> Result<(), String> {
        let path_str = path.to_string_lossy();
        let is_stdin = path_str == "-" || path_str == "/dev/stdin";

        // Always use the actual path for load_file(), so the file is found correctly.
        let mut parser = Parser::new(if is_stdin {
            PathBuf::from("-")
        } else {
            path.to_path_buf()
        });

        if is_stdin {
            // Read stdin content. If --temp-stdin=PATH was given, read from that file
            // (re-exec case). Otherwise read from actual stdin, saving to a temp file
            // so that a possible re-exec can pass the content forward.
            use std::io::Read;
            let content = if let Some(ref temp_path) = self.args.temp_stdin {
                // Re-exec path: read the temp file created by our parent invocation
                std::fs::read_to_string(temp_path)
                    .map_err(|e| format!("{}: {}", temp_path.display(), e))?
            } else {
                // First-run path: read stdin and save to a temp file so re-exec can use it
                let mut raw = String::new();
                if let Err(e) = std::io::stdin().read_to_string(&mut raw) {
                    return Err(format!("{}: {}", path_str, e));
                }
                // Create a temp file in TMPDIR (or /tmp).
                // We create it even if we might not re-exec, since we won't know until later.
                let tmpdir = env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
                let temp_path = std::path::PathBuf::from(&tmpdir)
                    .join(format!("jmake-stdin-{}.mk", std::process::id()));
                if let Err(e) = std::fs::write(&temp_path, &raw) {
                    let progname = crate::eval::make_progname();
                    eprintln!("{}: cannot store makefile from stdin to a temporary file.  Stop.", progname);
                    let _ = e;
                    return Err(String::new());
                }
                // Register temp file path with signal handler so SIGTERM can clean it up.
                crate::signal_handler::set_temp_stdin_path(
                    &temp_path.to_string_lossy()
                );
                self.stdin_temp_path = Some(temp_path);
                raw
            };
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

        // Override parser.filename with display_name after loading so error messages use
        // the original include argument (without the -I directory prefix), matching GNU Make.
        if let Some(dn) = display_name {
            if !is_stdin {
                parser.filename = PathBuf::from(dn);
            }
        }

        self.makefile_list.push(if is_stdin {
            PathBuf::from("-")
        } else {
            path.to_path_buf()
        });
        // Update MAKEFILE_LIST variable immediately so $(MAKEFILE_LIST) is available
        // during parsing of this file (GNU Make updates it incrementally as each file
        // is read, not just at the end of all reading).
        {
            let mf_list: Vec<String> = self.makefile_list.iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect();
            self.db.variables.insert("MAKEFILE_LIST".into(),
                Variable::new(mf_list.join(" "), VarFlavor::Simple, VarOrigin::File));
        }
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
                    // For simple (:= / ::= / :::=) assignments, expand value immediately.
                    // For shell (!=), run the shell command and store result.
                    // For recursive/append/conditional, store value verbatim.
                    let value = match parser.define_flavor {
                        VarFlavor::Simple | VarFlavor::PosixSimple => self.expand(&raw_value),
                        VarFlavor::Shell => {
                            let expanded_cmd = self.expand(&raw_value);
                            let (result, status) = self.shell_exec_with_env(&expanded_cmd);
                            *self.last_shell_status.borrow_mut() = Some(status);
                            result
                        }
                        _ => raw_value.clone(),
                    };
                    // For PosixSimple (:::=), escape dollar signs in the expanded value
                    // and store as Recursive so subsequent += appends raw (unexpanded) text.
                    let (stored_value, stored_flavor) = if parser.define_flavor == VarFlavor::PosixSimple {
                        (value.replace('$', "$$"), VarFlavor::Recursive)
                    } else {
                        (value.clone(), parser.define_flavor.clone())
                    };
                    let var = Variable::new(
                        stored_value,
                        stored_flavor,
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
            // When inside an inactive conditional branch, only process
            // conditional directives (ifdef/ifndef/ifeq/ifneq/else/endif).
            // Skip expansion and everything else to avoid side effects like $(info).
            if !parser.is_conditionally_active() {
                let trimmed_check = line.trim();
                let is_conditional = trimmed_check.starts_with("ifdef ")
                    || trimmed_check.starts_with("ifndef ")
                    || trimmed_check.starts_with("ifeq ")
                    || trimmed_check.starts_with("ifeq(")
                    || trimmed_check.starts_with("ifneq ")
                    || trimmed_check.starts_with("ifneq(")
                    || trimmed_check.starts_with("else")
                    || trimmed_check == "endif"
                    || trimmed_check.starts_with("endif ");
                if is_conditional {
                    let parsed = parser.parse_line(&line, self);
                    match &parsed {
                        ParsedLine::Conditional(kind) => {
                            // Don't evaluate - just push inactive state
                            parser.conditional_stack.push(parser::ConditionalState {
                                active: false,
                                seen_true: false,
                                in_else: false,
                            });
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
                        }
                        ParsedLine::Endif => {
                            parser.conditional_stack.pop();
                        }
                        _ => {}
                    }
                }
                continue;
            }

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
                // Use trim_start() (not trim()) so that trailing whitespace in values
                // like `$(eval res:=word )` is preserved (GNU Make behavior).
                let trimmed = line.trim_start();

                // Comment lines (starting with #) are handled by parse_line; don't
                // run try_parse_variable_assignment on them as it may falsely detect
                // a "missing separator" (e.g. `# Test = escaping` has name "# Test").
                if trimmed.starts_with('#') {
                    trimmed.to_string()
                } else

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
                    // If the raw-line parse already detected a fatal problem (e.g. a
                    // whitespace-containing variable name like `x $X=`), emit the error
                    // immediately without expansion and without going through parse_line again.
                    if let ParsedLine::MissingSeparator(ref hint) = raw_parsed {
                        let fname = parser.filename.to_string_lossy();
                        if hint.is_empty() {
                            eprintln!("{}:{}: *** missing separator.  Stop.", fname, lineno);
                        } else {
                            eprintln!("{}:{}: *** missing separator ({}).  Stop.", fname, lineno, hint);
                        }
                        std::process::exit(2);
                    }
                    if let ParsedLine::VariableAssignment { name: raw_name, value: raw_value, flavor: raw_flavor, is_override: raw_is_override, is_export: raw_is_export, is_unexport: raw_is_unexport, is_private: raw_is_private, target: raw_target } = raw_parsed {
                        match raw_flavor {
                            VarFlavor::Simple | VarFlavor::PosixSimple => {
                                // For := / ::= (Simple) and :::= (PosixSimple), expand value
                                // immediately. Strip comments from the value first.
                                let comment_stripped = parser::strip_comment(trimmed);
                                let stripped_parsed = parser::try_parse_variable_assignment(&comment_stripped);
                                let (stripped_name, stripped_value) = if let Some(ParsedLine::VariableAssignment { name: sn, value: sv, .. }) = stripped_parsed {
                                    (sn, sv)
                                } else {
                                    (raw_name.clone(), raw_value.clone())
                                };
                                let expanded_name = self.expand(&stripped_name);
                                let expanded_value = self.expand(&stripped_value);
                                if raw_target.is_none() {
                                    // Flush any pending current_rule before applying the
                                    // variable assignment. This ensures the rule's effect on
                                    // state (e.g. .DEFAULT_GOAL tracking) happens before the
                                    // assignment modifies that state. Without this, a sequence
                                    // like `foo: ; @:` followed by `.DEFAULT_GOAL :=` would
                                    // register `foo` AFTER the reset, incorrectly making `foo`
                                    // the default again.
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
                                    self.set_variable(&expanded_name, &expanded_value, &raw_flavor, raw_is_override, raw_is_export);
                                    // Handle unexport and private flags (not passed through set_variable).
                                    if raw_is_unexport {
                                        if let Some(var) = self.db.variables.get_mut(&expanded_name) {
                                            var.export = Some(false);
                                        }
                                    }
                                    if raw_is_private {
                                        if let Some(var) = self.db.variables.get_mut(&expanded_name) {
                                            var.is_private = true;
                                        }
                                    }
                                    // Drain any eval_pending items queued during value expansion.
                                    loop {
                                        let pending: Vec<String> = std::mem::take(&mut *self.eval_pending.borrow_mut());
                                        if pending.is_empty() { break; }
                                        for s in pending { self.eval_string(&s)?; }
                                    }
                                    continue;
                                }
                                self.expand(&line)
                            }
                            _ => {
                                // Deferred expansion: expand only the LHS (name / prefixes),
                                // keep the value verbatim.
                                // Reconstruct the line with only the name expanded.
                                // EXCEPTION: for target-specific vars with variable references
                                // in the name (e.g. `four:VAR$(FOO)=ok`), do NOT expand the
                                // name here -- it must be expanded in the target's own variable
                                // context (where `FOO=x` for target `four` makes `VAR$(FOO)=VARx`).
                                let expanded_name = if raw_target.is_some() && raw_name.contains('$') {
                                    raw_name.clone()
                                } else {
                                    self.expand(&raw_name)
                                };
                                let op_str = match raw_flavor {
                                    VarFlavor::Append => " += ",
                                    VarFlavor::Conditional => " ?= ",
                                    VarFlavor::Shell => " != ",
                                    _ => " = ",
                                };
                                // Build modifier prefix for the variable name.
                                // Order matters: put `private` first so that `parse_line`
                                // does not mistake the reconstructed line for a bare `export`
                                // directive (which would call `parse_export`, dropping the
                                // `private` flag).  Reconstructed as "private [override] export ..."
                                // ensures `try_parse_variable_assignment` handles it.
                                let mut var_prefix = String::new();
                                if raw_is_private { var_prefix.push_str("private "); }
                                if raw_is_override { var_prefix.push_str("override "); }
                                if raw_is_export { var_prefix.push_str("export "); }
                                if raw_is_unexport { var_prefix.push_str("unexport "); }
                                if let Some(tgt) = raw_target {
                                    // Target-specific variable: "target: [modifiers] name op value"
                                    let expanded_target = self.expand(&tgt);
                                    format!("{}: {}{}{}{}", expanded_target, var_prefix, expanded_name, op_str, raw_value)
                                } else {
                                    // Use var_prefix (built from parsed flags) instead of re-extracting
                                    // from the original line. Re-extracting from the original can produce
                                    // double prefixes when the variable name is also a keyword (e.g.,
                                    // `export export = 456` where "export" is both a prefix and the name).
                                    format!("{}{}{}{}", var_prefix, expanded_name, op_str, raw_value)
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

            // If the raw line looked like a rule (has a rule colon) but NOT an
            // assignment (try_parse_variable_assignment returned None above), and
            // expansion introduced a bare `=` into the prereq portion (e.g.
            // `ten: one $(EQ) two` → `ten: one = two`), we must not let
            // parse_line re-detect the result as a target-specific variable.
            //
            // Strategy: if the raw line has a rule colon AND try_parse_rule
            // succeeds on the expanded form, use that result directly.  This
            // correctly handles the expansion-introduced `=` case while leaving
            // genuine target-specific variables (where the raw line already had
            // a literal `=`) to fall through the normal path.
            // IMPORTANT: Custom recipe-prefix lines must NOT be passed through the
            // rule-colon heuristic below (they look like rules because a recipe line
            // like `> @cmd '...' > $@` may contain a colon).  Detect them here and
            // go directly to parse_line so that parse_line's recipe-prefix check runs.
            let raw_has_rule_colon_no_assignment = if line.starts_with('\t') || is_custom_recipe_line {
                // Tab-prefixed and custom-prefix lines are always recipes:
                // skip the rule-colon heuristic entirely.
                false
            } else {
                let trimmed_raw = line.trim();
                // Skip the rule-colon heuristic for define directives (e.g. `define simple :=`).
                // `find_rule_colon` incorrectly returns Some for lines like `define name :=`
                // because it sees the `:` in `:=` as a rule colon.  These lines are already
                // handled by `try_expand_define_name` above and must go through `parse_line`
                // directly so they are recognised as define directive starters.
                let is_define_directive_line = {
                    let mut w = trimmed_raw;
                    // Strip optional override/export prefixes
                    loop {
                        if w.starts_with("override ") { w = w["override ".len()..].trim_start(); }
                        else if w.starts_with("export ") { w = w["export ".len()..].trim_start(); }
                        else { break; }
                    }
                    w.starts_with("define ") || w.starts_with("define\t") || w == "define"
                };
                !trimmed_raw.starts_with('#')
                    && !is_define_directive_line
                    && parser::try_parse_variable_assignment(trimmed_raw).is_none()
                    && parser::find_rule_colon_pub(trimmed_raw).is_some()
            };
            let parsed = if raw_has_rule_colon_no_assignment {
                // Try rule parse with TSV detection DISABLED.  The original line
                // had no literal `=`, so any `=` in `expanded` came from variable
                // expansion (e.g. `ten: one $(EQ) two` → `ten: one = two`).
                // We must not treat the expanded `=` as a target-specific variable.
                if let Some(rule_result) = parser::try_parse_rule_force(&expanded) {
                    rule_result
                } else {
                    parser.parse_line(&expanded, self)
                }
            } else {
                parser.parse_line(&expanded, self)
            };

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
                        // Sync parser flags that may have been set by register_rule
                        // (e.g. posix_mode is set when .POSIX: is registered).
                        parser.posix_mode = self.db.posix_mode;
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

                    // GNU Make detects .POSIX: upon definition and immediately applies it.
                    // We must set posix_mode NOW (before the next line is read) so that
                    // the parser's next_logical_line correctly handles backslash-newlines.
                    if rule.targets.iter().any(|t| t == ".POSIX") {
                        self.db.posix_mode = true;
                        parser.posix_mode = true;
                        // POSIX mode sets specific default variable values,
                        // but only if not already overridden by user/environment.
                        let posix_defaults = [("CC", "c99"), ("CFLAGS", "-O1"), ("FC", "fort77"), ("FFLAGS", "-O1"), ("ARFLAGS", "-rv"), ("SCCSGETFLAGS", "-s")];
                        // POSIX mode: shell runs with -e (exit on error)
                        // Only set if not already overridden
                        let sf_should_set = match self.db.variables.get(".SHELLFLAGS") {
                            None => true,
                            Some(v) => v.origin == VarOrigin::Default,
                        };
                        if sf_should_set {
                            self.db.variables.insert(".SHELLFLAGS".into(),
                                Variable::new("-ec".into(), VarFlavor::Simple, VarOrigin::Default));
                        }
                        for (name, val) in &posix_defaults {
                            let should_set = match self.db.variables.get(*name) {
                                None => true,
                                Some(v) => v.origin == VarOrigin::Default,
                            };
                            if should_set {
                                self.db.variables.insert(name.to_string(),
                                    Variable::new(val.to_string(), VarFlavor::Simple, VarOrigin::Default));
                            }
                        }
                    }

                    // Eagerly update .DEFAULT_GOAL when the first valid target is seen.
                    // This must happen NOW (before the next line is expanded) so that
                    // conditionals like `ifneq ($(.DEFAULT_GOAL),foo)` appearing on the
                    // very next line after `foo: ; @:` see the correct value.
                    if self.db.default_target.is_none() && !self.db.default_goal_explicit {
                        for t in &rule.targets {
                            let is_special = t.starts_with('.') && !t.contains('/');
                            if !is_special && !t.contains('%') {
                                self.db.default_target = Some(t.clone());
                                self.db.variables.insert(".DEFAULT_GOAL".into(),
                                    Variable::new(t.clone(), VarFlavor::Simple, VarOrigin::Default));
                                break;
                            }
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
                        // The path_pattern was already expanded by the pre-expansion step
                        // (the include line went through self.expand() before parse_line).
                        // We must NOT expand again — that would cause double-expansion of
                        // any `$` signs (e.g. `include foo$$bar` → `include foo$bar` after
                        // first expansion; re-expanding would turn `$b` → empty, giving `fooar`).
                        let files: Vec<String> = parser::split_words(path_pattern);
                        for file in files {
                            // Mark included files as explicitly mentioned so they are
                            // not treated as intermediate targets (sv63484).
                            self.db.explicitly_mentioned.insert(file.clone());
                            let file_path = self.find_include_file(&file);
                            match file_path {
                                Some(p) => {
                                    // Use the original `file` name as the display name for error
                                    // messages. GNU Make reports the name from the include directive
                                    // (without the -I directory prefix), not the resolved path.
                                    let display = if p.to_string_lossy() != file.as_str() {
                                        Some(file.as_str())
                                    } else {
                                        None
                                    };
                                    if let Err(_e) = self.read_makefile_display(&p, display) {
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
                            // Expand variable references in the name (e.g. `export $(FOO)`)
                            let expanded_name = self.expand(name);
                            for n in parser::split_words(&expanded_name) {
                                if let Some(var) = self.db.variables.get_mut(&n) {
                                    var.export = Some(export);
                                } else {
                                    let mut var = Variable::new(String::new(), VarFlavor::Recursive, VarOrigin::File);
                                    var.export = Some(export);
                                    self.db.variables.insert(n, var);
                                }
                            }
                        }
                    }
                }
                ParsedLine::UnExport { names } => {
                    if names.is_empty() {
                        // `unexport` with no args: set global unexport-all flag.
                        // This is the default (don't export) but explicit per-variable
                        // `export VAR` directives still take precedence.
                        // We do NOT iterate all variables here to avoid overwriting
                        // explicit per-variable export flags.
                        self.db.unexport_all = true;
                    } else {
                        for name in &names {
                            // Expand variable references in the name (e.g. `unexport $(FOO)`)
                            let expanded_name = self.expand(name);
                            for n in parser::split_words(&expanded_name) {
                                if let Some(var) = self.db.variables.get_mut(&n) {
                                    var.export = Some(false);
                                } else {
                                    // Create a placeholder to mark as unexported even if not yet defined
                                    let mut var = Variable::new(String::new(), VarFlavor::Recursive, VarOrigin::File);
                                    var.export = Some(false);
                                    self.db.variables.insert(n, var);
                                }
                            }
                        }
                    }
                }
                ParsedLine::Undefine { name, is_override } => {
                    // Remove the variable from the database entirely.
                    // Without override, a command-line variable cannot be undefined.
                    // Empty name (from `undefine $empty`) is a fatal error.
                    if name.is_empty() {
                        let fname = parser.filename.to_string_lossy();
                        eprintln!("{}:{}: *** empty variable name.  Stop.", fname, lineno);
                        std::process::exit(2);
                    }
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
                ParsedLine::InvalidConditional => {
                    let fname = parser.filename.to_string_lossy();
                    eprintln!("{}:{}: *** invalid syntax in conditional.  Stop.", fname, lineno);
                    std::process::exit(2);
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
                ParsedLine::FatalError(msg) => {
                    let fname = parser.filename.to_string_lossy();
                    eprintln!("{}:{}: *** {}", fname, lineno, msg);
                    std::process::exit(2);
                }
                _ => {}
            }

            // Keep parser.recipe_prefix in sync with .RECIPEPREFIX so that
            // next_logical_line can handle backslash-newline continuation
            // in custom-prefix recipe lines correctly.
            {
                let pfx = self.db.variables.get(".RECIPEPREFIX")
                    .and_then(|v| v.value.chars().next());
                parser.recipe_prefix = if pfx == Some('\t') { None } else { pfx };
            }

            // Process any $(eval) calls that were queued during this line's expansion.
            // GNU Make processes $(eval) immediately, so effects (like undefine) are
            // visible to subsequent lines in the same makefile.
            //
            // Before draining eval_pending, flush any current_rule that was accumulated
            // before the eval call.  This is needed when the $(eval) appears in a
            // non-recipe context (e.g., a variable assignment line or a define block).
            // Without flushing, an eval'd rule that references the same target as an
            // already-parsed rule would be registered first, reversing the order of
            // double-colon rules.
            //
            // The decision to flush is based on whether the CURRENT SOURCE LINE is a
            // recipe line (starts with TAB or the custom recipe prefix).  If the current
            // line is NOT a recipe line, $(eval) appearing in it is a top-level directive
            // and any pending current_rule must be registered before the eval'd content.
            // We cannot rely on parser.in_recipe here because that flag persists from a
            // previous rule definition even when subsequent non-recipe lines are processed
            // (blank lines and `$(eval ...)` lines that expand to empty keep in_recipe=true).
            let current_line_is_recipe = {
                let custom_pfx: Option<char> = self.db.variables.get(".RECIPEPREFIX")
                    .and_then(|v| v.value.chars().next())
                    .filter(|&c| c != '\t');
                line.starts_with('\t')
                    || custom_pfx.map(|c| line.starts_with(c)).unwrap_or(false)
            };
            if !self.eval_pending.borrow().is_empty() && !current_line_is_recipe {
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
            }
            loop {
                let pending: Vec<String> = std::mem::take(&mut *self.eval_pending.borrow_mut());
                if pending.is_empty() { break; }
                for s in pending {
                    self.eval_string(&s)?;
                }
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
        // Grouped target rules (&:) must have a recipe.
        // A rule is grouped (has multiple targets in the group) when grouped_siblings is
        // non-empty (the parser sets this only when there are 2+ targets).
        if !rule.grouped_siblings.is_empty() && rule.recipe.is_empty() && !rule.is_pattern {
            // Check that none of the grouped targets already has a recipe from a prior rule.
            // If all targets already have recipes, no error; the prior recipe covers them.
            let any_target_has_recipe = rule.targets.iter().any(|t| {
                self.db.rules.get(t).map_or(false, |rules| {
                    rules.iter().any(|r| !r.recipe.is_empty())
                })
            });
            if !any_target_has_recipe {
                eprintln!("{}:{}: *** grouped targets must provide a recipe.  Stop.",
                    rule.source_file, rule.lineno);
                std::process::exit(2);
            }
        }

        // Handle special targets
        for target in &rule.targets {
            if let Some(special) = SpecialTarget::from_str(target) {
                let prereqs: HashSet<String> = rule.prerequisites.iter().cloned().collect();

                match special {
                    SpecialTarget::Phony | SpecialTarget::Precious |
                    SpecialTarget::Silent | SpecialTarget::Ignore => {
                        let set = self.db.special_targets.entry(special.clone()).or_insert_with(HashSet::new);
                        set.extend(prereqs);
                    }
                    SpecialTarget::NotIntermediate => {
                        // Check for conflicts with .INTERMEDIATE and .SECONDARY
                        let progname = make_progname();
                        if prereqs.is_empty() {
                            // .NOTINTERMEDIATE: (all) conflicts with .SECONDARY: (all)
                            let secondary_set = self.db.special_targets.get(&SpecialTarget::Secondary);
                            if secondary_set.map_or(false, |s| s.is_empty()) {
                                eprintln!("{}: *** .NOTINTERMEDIATE and .SECONDARY are mutually exclusive.  Stop.",
                                    progname);
                                std::process::exit(2);
                            }
                        } else {
                            // Check each prereq for conflict with .INTERMEDIATE or .SECONDARY
                            for name in &prereqs {
                                if self.db.special_targets.get(&SpecialTarget::Intermediate)
                                    .map_or(false, |s| s.contains(name)) {
                                    eprintln!("{}: *** {} cannot be both .NOTINTERMEDIATE and .INTERMEDIATE.  Stop.",
                                        progname, name);
                                    std::process::exit(2);
                                }
                                if self.db.special_targets.get(&SpecialTarget::Secondary)
                                    .map_or(false, |s| s.contains(name)) {
                                    eprintln!("{}: *** {} cannot be both .NOTINTERMEDIATE and .SECONDARY.  Stop.",
                                        progname, name);
                                    std::process::exit(2);
                                }
                            }
                        }
                        let set = self.db.special_targets.entry(special.clone()).or_insert_with(HashSet::new);
                        set.extend(prereqs);
                    }
                    SpecialTarget::Intermediate => {
                        // Check for conflicts with .NOTINTERMEDIATE
                        let progname = make_progname();
                        for name in &prereqs {
                            if self.db.special_targets.get(&SpecialTarget::NotIntermediate)
                                .map_or(false, |s| s.contains(name) || s.is_empty()) {
                                eprintln!("{}: *** {} cannot be both .NOTINTERMEDIATE and .INTERMEDIATE.  Stop.",
                                    progname, name);
                                std::process::exit(2);
                            }
                        }
                        let set = self.db.special_targets.entry(special.clone()).or_insert_with(HashSet::new);
                        set.extend(prereqs);
                    }
                    SpecialTarget::Secondary => {
                        let progname = make_progname();
                        if prereqs.is_empty() {
                            // .SECONDARY: (all) conflicts with .NOTINTERMEDIATE: (all)
                            let ni_set = self.db.special_targets.get(&SpecialTarget::NotIntermediate);
                            if ni_set.map_or(false, |s| s.is_empty()) {
                                eprintln!("{}: *** .NOTINTERMEDIATE and .SECONDARY are mutually exclusive.  Stop.",
                                    progname);
                                std::process::exit(2);
                            }
                        } else {
                            // Check each prereq for conflict with .NOTINTERMEDIATE
                            for name in &prereqs {
                                if self.db.special_targets.get(&SpecialTarget::NotIntermediate)
                                    .map_or(false, |s| s.contains(name) || s.is_empty()) {
                                    eprintln!("{}: *** {} cannot be both .NOTINTERMEDIATE and .SECONDARY.  Stop.",
                                        progname, name);
                                    std::process::exit(2);
                                }
                            }
                        }
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
                        // .NOTPARALLEL with no prerequisites: global sequential mode.
                        // .NOTPARALLEL: target1 target2: only those targets run sequentially.
                        if rule.prerequisites.is_empty() {
                            self.db.not_parallel = true;
                        } else {
                            for prereq in &rule.prerequisites {
                                self.db.not_parallel_targets.insert(prereq.clone());
                            }
                        }
                    }
                    SpecialTarget::DeleteOnError => {
                        // .DELETE_ON_ERROR: causes make to delete the target file on error.
                        // Simply record that this special target was seen.
                        self.db.special_targets.entry(special.clone()).or_insert_with(HashSet::new);
                    }
                    SpecialTarget::LowResolutionTime => {
                        // Record that .LOW_RESOLUTION_TIME was seen.
                        self.db.special_targets.entry(special.clone()).or_insert_with(HashSet::new);
                    }
                    _ => {}
                }
                continue;
            }

            // Set default target.
            // Skip special targets (those starting with '.' but NOT containing '/',
            // like .PHONY, .DEFAULT, etc.).  File targets like ./foo or ../bar start
            // with '.' but contain '/' and are valid default targets.
            // Also skip pattern rules (contain '%').
            let is_special_target = target.starts_with('.') && !target.contains('/');
            if self.db.default_target.is_none() && !is_special_target && !target.contains('%')
                && !self.db.default_goal_explicit
            {
                self.db.default_target = Some(target.clone());
                // Also update .DEFAULT_GOAL variable so it's readable at parse time.
                self.db.variables.insert(".DEFAULT_GOAL".into(),
                    Variable::new(target.clone(), VarFlavor::Simple, VarOrigin::Default));
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

            // GNU Make: a user pattern rule with no recipe cancels the matching
            // built-in implicit rules for that pattern.  A builtin is cancelled
            // when it has the same target-pattern set AND the same prerequisite-pattern
            // set as the user's no-recipe rule.
            // E.g., `%.o: %.f` with no recipe removes the builtin `%.o: %.f` rule,
            // but does NOT remove `%.o: %.c` or `%.o: %.s`.
            if rule.recipe.is_empty() {
                let cancel_targets: std::collections::HashSet<String> =
                    rule.targets.iter().cloned().collect();
                let cancel_prereqs: std::collections::HashSet<String> =
                    rule.prerequisites.iter().cloned().collect();
                let mut i = 0;
                while i < self.db.builtin_pattern_rules_count
                    && i < self.db.pattern_rules.len()
                {
                    // Cancel if both target patterns AND prereq patterns match exactly.
                    let same_targets = self.db.pattern_rules[i].targets.iter()
                        .all(|t| cancel_targets.contains(t))
                        && self.db.pattern_rules[i].targets.len() == cancel_targets.len();
                    let same_prereqs = self.db.pattern_rules[i].prerequisites.iter()
                        .all(|p| cancel_prereqs.contains(p))
                        && self.db.pattern_rules[i].prerequisites.len() == cancel_prereqs.len();
                    let remove = same_targets && same_prereqs;
                    if remove {
                        self.db.pattern_rules.remove(i);
                        self.db.builtin_pattern_rules_count -= 1;
                        // don't increment i; the next element shifts into position i
                    } else {
                        i += 1;
                    }
                }
                // A cancellation-only rule (no recipe AND no prereqs, including SE prereqs) is done.
                // A rule with no recipe BUT with prereqs (including SE prereqs) is added as-is.
                if rule.prerequisites.is_empty()
                    && rule.order_only_prerequisites.is_empty()
                    && rule.second_expansion_prereqs.is_none()
                    && rule.second_expansion_order_only.is_none()
                {
                    return;
                }
            }

            // A double-colon pattern rule (%:: ...) is "terminal" in GNU Make:
            // it can be used directly (pass 1) but cannot be used for chaining
            // intermediates (pass 2). Set is_terminal accordingly.
            let mut pattern_rule = rule.clone();
            if pattern_rule.is_double_colon {
                pattern_rule.is_terminal = true;
            }
            self.db.pattern_rules.push(pattern_rule);
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

        // Register explicit rules.
        // Expand glob wildcards in target names (GNU Make expands them at parse time).
        // For example, `a.o[Nn][Ee] a.t*: ; @echo $@` is expanded to the matching files.
        let mut expanded_targets: Vec<String> = Vec::new();
        for target in &rule.targets {
            if (target.contains('*') || target.contains('?') || target.contains('['))
                && !target.contains('%')
            {
                let mut matched: Vec<String> = Vec::new();
                if let Ok(paths) = ::glob::glob(target) {
                    for entry in paths.flatten() {
                        matched.push(entry.to_string_lossy().to_string());
                    }
                }
                if matched.is_empty() {
                    expanded_targets.push(target.clone());
                } else {
                    expanded_targets.extend(matched);
                }
            } else {
                expanded_targets.push(target.clone());
            }
        }
        for target in &expanded_targets {
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
                        // Also update source_file and lineno to reflect the rule that
                        // provides the recipe (so error messages reference the correct file/line).
                        if !rule.source_file.is_empty() {
                            existing.source_file = rule.source_file.clone();
                            existing.lineno = rule.lineno;
                        }
                    }
                    // Update grouped_siblings: if the new rule brings grouped siblings
                    // (e.g. `a b&: ; recipe` following a standalone `a:`), adopt them.
                    if !rule.grouped_siblings.is_empty() {
                        existing.grouped_siblings = rule.grouped_siblings.clone();
                    }
                    // Update static_stem: when multiple static pattern rules match the
                    // same target (possibly with different pattern widths), the LAST
                    // rule's stem is used for $* — matching GNU Make behaviour where
                    // the most-recently-seen static pattern rule determines $*.
                    if !rule.static_stem.is_empty() {
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
        // Capture current source location for error reporting during lazy expansion.
        let src_file = self.current_file.borrow().clone();
        let src_line = *self.current_line.borrow();

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

        // Helper to create a new Variable with source location attached.
        let make_var = |val: String, fl: VarFlavor, orig: VarOrigin| {
            let mut v = Variable::new(val, fl, orig);
            v.source_file = src_file.clone();
            v.source_line = src_line;
            v
        };

        match flavor {
            VarFlavor::Append => {
                // Check guards and get the existing variable's flavor before taking
                // a mutable borrow (borrow checker requires this since self.expand()
                // also borrows self).
                let append_info = if let Some(existing) = self.db.variables.get(name) {
                    if !is_override && is_protected(&existing.origin) {
                        return; // Protected variable: non-override append blocked
                    }
                    if makeflags_protected {
                        return; // -e protects MAKEFLAGS from makefile changes
                    }
                    if !is_override && self.args.environment_overrides
                        && existing.origin == VarOrigin::Environment {
                        return;
                    }
                    Some((existing.flavor.clone(), existing.value.is_empty()))
                } else {
                    None
                };

                if let Some((existing_flavor, existing_is_empty)) = append_info {
                    // For simply-expanded (:= / ::= / :::=) variables, the appended value
                    // must be immediately expanded (GNU Make semantics: `foo := a; foo += $(bar)`
                    // is equivalent to `foo := a $(bar_expanded_now)`).
                    let append_val = if existing_flavor == VarFlavor::Simple {
                        self.expand(value)
                    } else {
                        value.to_string()
                    };
                    if !append_val.is_empty() {
                        let existing = self.db.variables.get_mut(name).unwrap();
                        if existing_is_empty {
                            existing.value = append_val;
                        } else {
                            existing.value.push(' ');
                            existing.value.push_str(&append_val);
                        }
                    }
                    if is_override {
                        if let Some(existing) = self.db.variables.get_mut(name) {
                            existing.origin = VarOrigin::Override;
                        }
                    }
                    if let Some(existing) = self.db.variables.get_mut(name) {
                        existing.source_file = src_file.clone();
                        existing.source_line = src_line;
                    }
                } else {
                    self.db.variables.insert(name.to_string(),
                        make_var(value.to_string(), VarFlavor::Recursive, origin));
                }
            }
            VarFlavor::Conditional => {
                // ?= only sets if not already defined
                self.db.variables.entry(name.to_string()).or_insert_with(|| {
                    make_var(value.to_string(), VarFlavor::Recursive, origin)
                });
            }
            VarFlavor::Shell => {
                // != executes value as shell command; expand Make variable references
                // first (like := expansion) then pass the result to the shell.
                let expanded_cmd = self.expand(value);
                let (result, status) = self.shell_exec_with_env(&expanded_cmd);
                *self.last_shell_status.borrow_mut() = Some(status);
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
                    make_var(result, VarFlavor::Recursive, origin));
            }
            VarFlavor::PosixSimple => {
                // PosixSimple (:::=): the value is already immediately expanded at the
                // call site (in the pre-expansion loop). We store the result with dollar
                // signs escaped ($ → $$) so that subsequent += operations can append raw
                // (unexpanded) text and the combined value can be expanded recursively at
                // use time. The flavor is stored as Recursive to enable this lazy behavior.
                let existing = self.db.variables.get(name);
                if !is_override {
                    if let Some(existing) = existing {
                        if is_protected(&existing.origin) {
                            return;
                        }
                    }
                }
                if makeflags_protected {
                    return;
                }
                // Escape dollar signs in the already-expanded value
                let escaped = value.replace('$', "$$");
                self.db.variables.insert(name.to_string(),
                    make_var(escaped, VarFlavor::Recursive, origin));
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
                    make_var(value.to_string(), flavor.clone(), origin));
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
            let mut new_mf = self.build_makeflags_from_args(&self.args.variables.clone());
            // When MAKEOVERRIDES is explicitly set to empty, MAKEFLAGS keeps "-- "
            // to indicate the variable section was present but cleared.
            if overrides_val.is_empty() && !new_mf.contains("--") {
                new_mf.push_str(" -- ");
                self.makeoverrides_cleared = true;
            } else {
                self.makeoverrides_cleared = false;
            }
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

        // Special handling for .DEFAULT_GOAL: when explicitly set in a makefile,
        // update the default_target accordingly so it is used when building.
        if name == ".DEFAULT_GOAL" {
            // Determine whether the raw (pre-expansion) value is blank.
            // For := (Simple), `value` is already expanded by the caller.
            // For = (Recursive), `value` is the raw unexpanded string.
            // A truly-empty raw value (`.DEFAULT_GOAL :=`) means "reset".
            // A non-empty raw value that expands to whitespace (test 5) means
            // "explicit empty goal" → block auto-setting from rules.
            let raw_is_empty = value.trim().is_empty();

            // Expand, strip comments, then trim to get the effective goal string.
            let raw_expanded = self.expand(value);
            let comment_stripped = parser::strip_comment(&raw_expanded);
            let expanded_goal = comment_stripped.trim().to_string();

            if raw_is_empty {
                // Literal/already-expanded empty value: true reset.
                // Next non-special rule will become the default again.
                self.db.default_target = None;
                self.db.default_goal_explicit = false;
                if let Some(var) = self.db.variables.get_mut(".DEFAULT_GOAL") {
                    var.value = String::new();
                }
            } else if expanded_goal.is_empty() {
                // Non-empty raw value that expands to whitespace-only (e.g.
                // `.DEFAULT_GOAL = $N  $N  # comment` where N is empty).
                // GNU Make treats this as explicit-empty: no default target,
                // error "No targets. Stop." at build time.
                self.db.default_target = None;
                self.db.default_goal_explicit = true;  // block auto-setting
                if let Some(var) = self.db.variables.get_mut(".DEFAULT_GOAL") {
                    var.value = String::new();
                }
            } else {
                // Check for multiple words (GNU Make fatal error).
                let words: Vec<&str> = expanded_goal.split_whitespace().collect();
                if words.len() > 1 {
                    let progname = std::env::args().next().unwrap_or_else(|| "make".to_string());
                    let progname = std::path::Path::new(&progname)
                        .file_name().and_then(|n| n.to_str()).unwrap_or("make").to_string();
                    eprintln!("{}: *** .DEFAULT_GOAL contains more than one target.  Stop.", progname);
                    std::process::exit(2);
                }
                // Set to specific goal; update default_target
                self.db.default_target = Some(expanded_goal.clone());
                self.db.default_goal_explicit = true;
                if let Some(var) = self.db.variables.get_mut(".DEFAULT_GOAL") {
                    var.value = expanded_goal;
                }
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
        // Command-line -jN takes precedence over makefile MAKEFLAGS += -jM.
        if ca.jobs_explicit {
            self.args.jobs = ca.jobs;
            self.args.jobs_explicit = ca.jobs_explicit;
        }

        // If print_directory was just enabled by the makefile (transition false→true),
        // print the "Entering directory" message now so it appears before subsequent
        // $(info) or recipe output in the makefile.
        let now_printing_dir = should_print_directory(&self.args);
        if now_printing_dir && !was_printing_dir && !self.entering_directory_printed {
            let progname = make_progname();
            let cwd = logical_cwd();
            println!("{}: Entering directory '{}'", progname, cwd.display());
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
        let include_dirs_str: String = self.args.include_dirs.iter()
            .filter(|d| d.to_string_lossy() != "-")
            .map(|d| d.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        self.db.variables.insert(".INCLUDE_DIRS".into(),
            Variable::new(include_dirs_str, VarFlavor::Simple, VarOrigin::Default));

        // GNU Make: -R implies -r. Apply AFTER MAKEFLAGS rebuild (so parse-time
        // $(info $(MAKEFLAGS)) shows just R) but before builtin removal check.
        if self.args.no_builtin_variables {
            self.args.no_builtin_rules = true;
        }

        // If -r (no_builtin_rules) was activated via MAKEFLAGS inside the makefile,
        // remove the built-in pattern rules now (they were loaded at startup).
        if self.args.no_builtin_rules {
            let n = self.db.builtin_pattern_rules_count;
            if n > 0 && self.db.pattern_rules.len() >= n {
                self.db.pattern_rules.drain(..n);
                self.db.builtin_pattern_rules_count = 0;
            }
        }

        // If -R (no_builtin_variables) was activated via MAKEFLAGS inside the makefile
        // (and wasn't already set from the command line), remove all default built-in
        // variables now (they were registered at startup by register_default_variables).
        if self.args.no_builtin_variables && !self.cmdline_args.no_builtin_variables {
            self.db.variables.retain(|_k, v| v.origin != VarOrigin::Default);
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
        // Use keep-going semantics: when one prerequisite fails, continue building
        // siblings so that parallel-like behavior is approximated (all siblings at
        // the same level run even if one fails). Return the first error at the end.
        let mut first_error: Option<String> = None;

        for prereq in prerequisites {
            let prereq_path = Path::new(prereq);

            if visited.contains(prereq) {
                // Cycle detected: skip silently
                continue;
            }

            // Check if this prereq has ANY explicit rule (even an empty one like `force:`).
            // Empty rules (no recipe, no prereqs) act as "satisfied" prerequisites.
            let has_any_rule = self.db.rules.get(prereq).map_or(false, |r| !r.is_empty());

            // Only look at explicit rules (not pattern rules) to avoid infinite
            // recursion through implicit rule chains like %: %.o: %.c etc.
            // Filter out double-colon rules with no prereqs (skippable), but keep
            // rules with a recipe or prerequisites.
            let explicit_rule = self.db.rules.get(prereq).and_then(|rules| {
                rules.iter().find(|r| {
                    !(r.is_double_colon && r.prerequisites.is_empty())
                    && (!r.recipe.is_empty() || !r.prerequisites.is_empty())
                })
            }).cloned();

            if !prereq_path.exists() {
                if has_any_rule && explicit_rule.is_none() {
                    // Has a rule but it has no recipe and no prereqs (e.g. `force:`).
                    // This is an "imagined" target: treat as satisfied, continue.
                    visited.insert(prereq.clone());
                    continue;
                }
                match explicit_rule {
                    None => {
                        if !ignore_missing {
                            // Collect error but continue to build siblings.
                            if first_error.is_none() {
                                first_error = Some(format!(
                                    "No rule to make target '{}', needed by '{}'.  Stop.",
                                    prereq, include_target));
                            }
                        }
                        continue;
                    }
                    Some(rule) => {
                        visited.insert(prereq.clone());
                        if !rule.prerequisites.is_empty() {
                            if let Err(e) = self.build_include_prerequisites(
                                &rule.prerequisites.clone(), prereq,
                                shell, shell_flags, silent, ignore_missing, visited,
                            ) {
                                if first_error.is_none() { first_error = Some(e); }
                                // Continue building siblings even on error.
                            }
                        }
                        // Skip running the recipe if it was already queued or run
                        // as a pending include (Phase A sets include_recipe_ran when
                        // it schedules the file for Phase B, before Phase B actually
                        // creates the file on disk).
                        if !rule.recipe.is_empty()
                            && !self.include_recipe_ran.contains(prereq.as_str())
                        {
                            let src = rule.source_file.clone();
                            if let Err(e) = self.run_include_recipe(
                                prereq, "", &rule.recipe.clone(), shell, shell_flags, silent, &src,
                            ) {
                                if first_error.is_none() { first_error = Some(e); }
                                // Continue building siblings.
                            }
                        }
                    }
                }
            } else if let Some(ref rule) = explicit_rule {
                // File exists: check if it's out of date (any prereq is newer).
                let prereq_mtime = prereq_path.metadata().ok().and_then(|m| m.modified().ok());
                // Recursively build this prereq's prerequisites first
                if !rule.prerequisites.is_empty() {
                    visited.insert(prereq.clone());
                    if let Err(e) = self.build_include_prerequisites(
                        &rule.prerequisites.clone(), prereq,
                        shell, shell_flags, silent, ignore_missing, visited,
                    ) {
                        if first_error.is_none() { first_error = Some(e); }
                    }
                }
                // Check if any prereq is now newer than this target
                let needs_rebuild = rule.prerequisites.iter().any(|p| {
                    std::fs::metadata(p).ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|pt| prereq_mtime.map(|tt| pt > tt))
                        .unwrap_or(false)
                });
                if needs_rebuild && !rule.recipe.is_empty() {
                    let src = rule.source_file.clone();
                    if let Err(e) = self.run_include_recipe(
                        prereq, "", &rule.recipe.clone(), shell, shell_flags, silent, &src,
                    ) {
                        if first_error.is_none() { first_error = Some(e); }
                    }
                }
            }
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
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
                        imagined: false,
                        sibling_targets: Vec::new(),
                        stem: String::new(),
                    });
                }
                // Single-colon rule with no recipe AND no prerequisites: sv 61226
                // GNU Make "imagines" the target was updated; no file is created,
                // no re-exec triggered, no error reported.
                if !rule.is_double_colon && rule.recipe.is_empty() && rule.prerequisites.is_empty() {
                    return Some(IncludeRuleInfo {
                        recipe: Vec::new(),
                        source_file: rule.source_file.clone(),
                        recipe_lineno: 0,
                        prerequisites: Vec::new(),
                        skippable: false,
                        imagined: true,
                        sibling_targets: Vec::new(),
                        stem: String::new(),
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
                        imagined: false,
                        sibling_targets: Vec::new(),
                        stem: String::new(),
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
                        // Expand prerequisites for the stem (first % per word only)
                        let prereqs: Vec<String> = rule.prerequisites.iter()
                            .map(|p| {
                                if let Some(pos) = p.find('%') {
                                    let mut s = String::with_capacity(p.len() + stem.len());
                                    s.push_str(&p[..pos]);
                                    s.push_str(&stem);
                                    s.push_str(&p[pos+1..]);
                                    s
                                } else {
                                    p.clone()
                                }
                            })
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
                            imagined: false,
                            sibling_targets: siblings,
                            stem: stem.clone(),
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
        &self, target: &str, stem: &str, recipe: &[(usize, String)],
        shell: &str, shell_flags: &str, silent: bool,
        source_file: &str,
    ) -> Result<(), String> {
        use std::os::unix::process::ExitStatusExt;

        // Set up automatic variables for recipe expansion.
        // $@ = target, $* = stem (for pattern rules)
        let mut auto_vars = HashMap::new();
        auto_vars.insert("@".to_string(), target.to_string());
        auto_vars.insert("*".to_string(), stem.to_string());

        let progname = make_progname();

        for (lineno, cmd_template) in recipe {
            // Trim leading whitespace that may remain after stripping a custom
            // RECIPEPREFIX character (e.g. `>` leaves a leading space in ` @cmd`).
            let mut cmd = cmd_template.trim_start().to_string();
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
            // Only echo the recipe if not silent and the expansion is non-empty.
            // When make functions like $(info ...) expand to empty string, don't print
            // a blank line (the function's side effects already produced output).
            if !silent && !cmd_silent && !expanded_cmd.is_empty() {
                println!("{}", expanded_cmd);
            }

            // Set up the SIGTERM message for this recipe line.
            // If jmake receives SIGTERM while waiting for the child, the signal
            // handler will print this message and clean up the temp stdin file.
            let term_msg = format!(
                "{}: *** [{}:{}: {}] Terminated\n",
                progname, source_file, lineno, target
            );
            crate::signal_handler::set_term_message(&term_msg);

            let status = std::process::Command::new(shell)
                .arg(shell_flags)
                .arg(&expanded_cmd)
                .status();

            // Clear the term message after the command completes.
            crate::signal_handler::clear_term_message();

            match status {
                Ok(s) if s.success() => {}
                Ok(s) => {
                    // Check if killed by signal
                    if let Some(sig) = s.signal() {
                        if !ignore_error {
                            // Signal names for common signals
                            let sig_name = match sig {
                                libc::SIGTERM => "Terminated",
                                libc::SIGINT => "Interrupt",
                                libc::SIGHUP => "Hangup",
                                libc::SIGKILL => "Killed",
                                _ => "Signal",
                            };
                            return Err(format!("[{}:{}: {}] {}", source_file, lineno, target, sig_name));
                        }
                    } else {
                        let code = s.code().unwrap_or(1);
                        // Error format: [source_file:lineno: target] Error N
                        // Caller is responsible for printing the "*** [...]" error message.
                        if !ignore_error {
                            return Err(format!("[{}:{}: {}] Error {}", source_file, lineno, target, code));
                        }
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

    /// Pre-expand recipe lines for an include file recipe into (lineno, expanded_cmd, silent, ignore_error).
    /// This is used to separate the variable expansion (requires &self) from the execution
    /// (can run in a thread without &self).
    fn expand_include_recipe_lines(
        &self, target: &str, stem: &str, recipe: &[(usize, String)]
    ) -> Vec<(usize, String, bool, bool)> {
        let mut auto_vars = HashMap::new();
        auto_vars.insert("@".to_string(), target.to_string());
        auto_vars.insert("*".to_string(), stem.to_string());

        recipe.iter().map(|(lineno, cmd_template)| {
            let mut cmd = cmd_template.trim_start().to_string();
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
            (*lineno, expanded_cmd, cmd_silent, ignore_error)
        }).collect()
    }

    fn find_include_file(&self, file: &str) -> Option<PathBuf> {
        let path = Path::new(file);
        if path.exists() {
            return Some(path.to_path_buf());
        }

        // Determine if -I- (clear include dirs) was used by looking for "-" sentinel
        // in the include_dirs list. When present, only dirs AFTER the last "-" are
        // searched, and the default system include dirs are suppressed.
        let has_reset = self.include_dirs.iter().any(|d| d.to_string_lossy() == "-");
        let effective_dirs: Vec<&PathBuf> = if has_reset {
            // Find the position of the LAST "-" sentinel and only use dirs after it.
            let last_reset_pos = self.include_dirs.iter().rposition(|d| d.to_string_lossy() == "-")
                .unwrap_or(0);
            self.include_dirs[last_reset_pos + 1..].iter().collect()
        } else {
            self.include_dirs.iter().collect()
        };

        // Search include directories (only effective ones after any -I- reset)
        for dir in &effective_dirs {
            let candidate = dir.join(file);
            if candidate.exists() {
                return Some(candidate);
            }
        }

        // Search default include dirs only when -I- was NOT specified
        if !has_reset {
            let default_dirs = vec!["/usr/include", "/usr/local/include"];
            for dir in default_dirs {
                let candidate = Path::new(dir).join(file);
                if candidate.exists() {
                    return Some(candidate);
                }
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

    /// Re-execute the current process from scratch after makefiles were updated.
    /// This never returns if successful (it replaces the current process via exec).
    /// If exec fails, it logs an error and returns the appropriate error string.
    /// `restart_count`: the new MAKE_RESTARTS value (1 for first restart, etc.)
    fn do_reinvoke(&self, restart_count: u32) -> String {
        use std::os::unix::process::CommandExt;

        let progname = make_progname();
        let debug_basic = self.args.debug_short
            || self.args.debug.iter().any(|d| d == "b" || d == "basic" || d == "a" || d == "all");

        // Get the current executable path.
        let exe = match env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("{}: failed to get executable path: {}", progname, e);
                return String::new();
            }
        };

        // Build args: skip argv[0] (the program name)
        let orig_args: Vec<String> = env::args().skip(1)
            .filter(|a| !a.starts_with("--temp-stdin="))  // strip any previous --temp-stdin
            .collect();

        let mut cmd = std::process::Command::new(&exe);
        cmd.args(&orig_args);

        // If -f- was given, pass the temp file path so the re-exec'd process can read stdin content
        let temp_stdin_path = self.args.temp_stdin.clone()
            .or_else(|| self.stdin_temp_path.clone());
        if let Some(ref tp) = temp_stdin_path {
            cmd.arg(format!("--temp-stdin={}", tp.display()));
        }

        // Set MAKE_RESTARTS in the environment for the re-exec'd process
        cmd.env("MAKE_RESTARTS", restart_count.to_string());

        // If "Entering directory" was already printed by this invocation, suppress it
        // in the re-exec'd process to avoid printing it twice.
        if self.entering_directory_printed {
            cmd.env("JMAKE_ENTERING_PRINTED", "1");
        } else {
            cmd.env_remove("JMAKE_ENTERING_PRINTED");
        }

        if debug_basic {
            // Build the full command string for debug output
            let mut parts = vec![exe.to_string_lossy().to_string()];
            parts.extend(orig_args.iter().cloned());
            if let Some(ref tp) = temp_stdin_path {
                parts.push(format!("--temp-stdin={}", tp.display()));
            }
            eprintln!("Re-executing: {}", parts.join(" "));
        }

        // exec() replaces the current process. It only returns on failure.
        let err = cmd.exec();
        // exec failed - format the error message without the "(os error N)" suffix
        // that Rust's io::Error adds on Linux (GNU Make doesn't include it).
        let err_str = {
            let s = err.to_string();
            // Strip trailing " (os error N)" if present
            if let Some(pos) = s.rfind(" (os error ") {
                s[..pos].to_string()
            } else {
                s
            }
        };
        eprintln!("{}: {}: {}", progname, exe.display(), err_str);
        // Try to clean up the temp stdin file on exec failure
        if let Some(ref tp) = temp_stdin_path {
            let _ = std::fs::remove_file(tp);
        }
        // Exit with 127 to indicate exec failure (same as shell convention)
        std::process::exit(127);
    }

    /// Check if any of the read makefiles (in makefile_list) are out of date
    /// and need to be rebuilt. Also handles pending includes.
    /// If any file is rebuilt, re-execs the process.
    /// Returns Ok(()) normally (re-exec never returns), or Err on fatal error.
    fn try_update_makefiles(&mut self) -> Result<(), String> {
        // Skip if there are no makefiles to check
        if self.makefile_list.is_empty() && self.pending_includes.is_empty() {
            return Ok(());
        }

        let shell = self.db.variables.get("SHELL")
            .map(|v| v.value.clone())
            .unwrap_or_else(|| "/bin/sh".to_string());
        let shell_flags = self.db.variables.get(".SHELLFLAGS")
            .map(|v| v.value.clone())
            .unwrap_or_else(|| "-c".to_string());
        let silent = self.args.silent;

        let mut any_really_rebuilt = false;

        // First, check existing makefiles that may be out of date.
        // Any makefile in makefile_list that has a rule and whose prerequisites
        // are newer than it should be rebuilt.
        // Skip stdin (-) since it can't be checked for staleness.
        // NOTE: This must run BEFORE pending_includes so that regular makefile
        // rebuilds (e.g. bye.mk) happen before we try to rebuild missing -f files.
        {
            let makefile_list_snap = self.makefile_list.clone();
            for mf_path in &makefile_list_snap {
                let mf_str = mf_path.to_string_lossy();
                if mf_str == "-" {
                    // Skip stdin makefiles
                    continue;
                }
                let mf_name = mf_str.to_string();

                // Check if this makefile has a rule to rebuild it
                let rule_info = self.find_include_rule(&mf_name);
                let rule_info = match rule_info {
                    Some(ri) if !ri.skippable && !ri.imagined => ri,
                    _ => continue, // no applicable rule
                };

                if self.include_recipe_ran.contains(&mf_name) {
                    continue;
                }

                let target_mtime = mf_path.metadata().ok().and_then(|m| m.modified().ok());

                // Build prerequisites first (using ignore_missing=true so that prereqs
                // without rules are silently skipped rather than causing errors).
                // Prereqs that DO have rules and don't yet exist will be built here.
                let prereqs = rule_info.prerequisites.clone();
                let mut visited = HashSet::new();
                visited.insert(mf_name.clone());
                let _ = self.build_include_prerequisites(
                    &prereqs, &mf_name, &shell, &shell_flags, silent, true, &mut visited,
                );

                // Now determine if the makefile needs rebuild (after prereqs were built).
                let needs_rebuild = if target_mtime.is_none() {
                    // File doesn't exist → needs rebuild
                    true
                } else {
                    let target_time = target_mtime.unwrap();
                    // A phony prerequisite always makes the target out of date.
                    // A regular prereq triggers rebuild only if it now EXISTS and is newer.
                    // A prereq that has ANY rule but doesn't exist (e.g. `force:` with no
                    // recipe and no prereqs) acts as an "always satisfied" forcing target:
                    // it was effectively "built" by its empty rule and makes the parent
                    // out of date.
                    let phony_targets = self.db.special_targets
                        .get(&SpecialTarget::Phony)
                        .cloned()
                        .unwrap_or_default();
                    let what_if = self.args.what_if.clone();
                    // When already restarted (MAKE_RESTARTS > 0), do NOT apply -W/--what-if
                    // to the include-rebuild check.  On restart 1+, what-if has already
                    // triggered the necessary rebuild (in the previous invocation); applying
                    // it again would cause infinite re-exec loops.
                    let make_restarts = env::var("MAKE_RESTARTS")
                        .ok()
                        .and_then(|v| v.parse::<u32>().ok())
                        .unwrap_or(0);
                    rule_info.prerequisites.iter().any(|prereq| {
                        if phony_targets.contains(prereq.as_str()) {
                            // Phony prerequisite → always out of date
                            return true;
                        }
                        // -W/--what-if: if the prereq is what-if, it appears infinitely new.
                        // Only apply on the first invocation; skip on restarts to prevent loops.
                        if make_restarts == 0 && what_if.iter().any(|w| w == prereq) {
                            return true;
                        }
                        // A prereq that has a rule but no file → was "built" (via empty rule
                        // or recipe that produced no file) → forces parent rebuild.
                        if !Path::new(prereq).exists()
                            && self.db.rules.get(prereq).map_or(false, |r| !r.is_empty())
                        {
                            return true;
                        }
                        std::fs::metadata(prereq).ok()
                            .and_then(|m| m.modified().ok())
                            .map_or(false, |pt| pt > target_time)
                    })
                };

                if !needs_rebuild {
                    continue;
                }

                if rule_info.recipe.is_empty() {
                    // Has prerequisites but no recipe - prereqs were built but no file update
                    // sv 61226: imagined update, no re-exec
                    continue;
                }

                // Mark as ran
                self.include_recipe_ran.insert(mf_name.clone());
                for sib in &rule_info.sibling_targets {
                    self.include_recipe_ran.insert(sib.clone());
                }

                // Run the recipe
                let built = self.run_include_recipe(
                    &mf_name, &rule_info.stem, &rule_info.recipe.clone(), &shell, &shell_flags, silent,
                    &rule_info.source_file.clone(),
                );

                match built {
                    Ok(()) => {
                        // Re-exec only if the included makefile was actually modified.
                        // Compare mtime before and after the recipe to detect changes.
                        // - File created (didn't exist before) → always re-exec.
                        // - File mtime changed (either direction, to handle future-timestamped
                        //   files overwritten with current-time content) → re-exec.
                        // - File exists but mtime unchanged → recipe ran but didn't modify
                        //   the file (e.g. `test -f $@ || echo >> $@` guard) → no re-exec.
                        //   This prevents infinite loops (sv reinvoke F=b case).
                        // - File still doesn't exist (sv 61226) → no re-exec, no error.
                        let new_mtime = mf_path.metadata().ok().and_then(|m| m.modified().ok());
                        let actually_updated = match (target_mtime, new_mtime) {
                            (_, None) => false,              // file still doesn't exist
                            (None, Some(_)) => true,         // file was created
                            (Some(old_t), Some(new_t)) => new_t != old_t, // mtime changed
                        };
                        if actually_updated {
                            any_really_rebuilt = true;
                        }
                        // If file not updated/created (sv 61226): no re-exec, no error
                    }
                    Err(recipe_err) => {
                        return Err(recipe_err);
                    }
                }
            }
        }

        // If any regular makefile was really rebuilt, re-exec from scratch now
        // (before processing pending includes, so that on re-exec we can try
        // to rebuild missing makefiles with the updated rules).
        // BUT: don't re-exec if there are pending includes with ignore_missing=false
        // that don't exist and have no rule — those would fail on re-exec too, so
        // just fail now without the extra invocation (and extra duplicate warning).
        if any_really_rebuilt {
            let has_fatal_pending = self.pending_includes.iter().any(|pi| {
                !pi.ignore_missing
                    && !Path::new(&pi.file).exists()
                    && self.find_include_rule(&pi.file).is_none()
            });
            if !has_fatal_pending {
                let restart_val = env::var("MAKE_RESTARTS")
                    .ok()
                    .and_then(|v| v.parse::<u32>().ok())
                    .unwrap_or(0);
                self.do_reinvoke(restart_val + 1);
            }
        }

        // Second, handle pending includes (files that were missing when first read).
        //
        // Three-phase approach to support parallel recipe execution:
        //   Phase A (sequential): build prerequisites, expand recipes, collect work items.
        //   Phase B (parallel):   run recipes in parallel threads when jobs > 1.
        //   Phase C (sequential): process results, read rebuilt makefiles.
        //
        // This is needed for tests like parallelism diff.3/diff.4 where two included
        // files use thelp synchronisation to verify they are built concurrently.

        // Outcome of Phase A for a single pending include.
        enum PendingOutcome {
            AlreadyExists,
            Error(String),    // empty string = silent error (just return Err(""))
            Imagined,
            SiblingAlreadyRan,
            /// This file's recipe was already queued by another work item (primary_idx).
            /// Resolve after Phase B using the primary's recipe result.
            SiblingOf(usize),
            PrereqsOnlyNoRecipe,
            RunRecipe {
                expanded: Vec<(usize, String, bool, bool)>,
                source_file: String,
            },
        }

        struct PendingWork {
            pi_file: String,
            pi_ignore_missing: bool,
            pi_parent: String,
            pi_lineno: usize,
            outcome: PendingOutcome,
            /// If true, print "No such file or directory" in Phase C before reporting error.
            deferred_no_such_file: bool,
            /// For pattern rules with multiple targets: sibling targets that should also be
            /// updated. If any sibling doesn't exist after the recipe, emit a peer warning.
            also_make_siblings: Vec<String>,
            /// Source file and line for the rule (for the peer warning message).
            rule_source_file: String,
            rule_lineno: usize,
            /// File mtime before recipe ran (for detecting actual changes).
            pre_mtime: Option<std::time::SystemTime>,
        }

        let pending = std::mem::take(&mut self.pending_includes);
        let parallel_jobs = self.args.jobs;

        // ── Phase A ─────────────────────────────────────────────────────────────
        let mut work_items: Vec<PendingWork> = Vec::new();
        // Map from filename to work_item index, for SiblingOf resolution.
        let mut file_to_work_idx: HashMap<String, usize> = HashMap::new();

        for pi in pending {
            let file_path_buf = std::path::PathBuf::from(&pi.file);
            let file_path = file_path_buf.as_path();

            if file_path.exists() {
                let idx = work_items.len();
                file_to_work_idx.entry(pi.file.clone()).or_insert(idx);
                work_items.push(PendingWork {
                    pi_file: pi.file,
                    pi_ignore_missing: pi.ignore_missing,
                    pi_parent: pi.parent,
                    pi_lineno: pi.lineno,
                    outcome: PendingOutcome::AlreadyExists,
                    deferred_no_such_file: false,
                    also_make_siblings: Vec::new(),
                    rule_source_file: String::new(),
                    rule_lineno: 0, pre_mtime: None,
                });
                continue;
            }

            let is_phony = self.db.special_targets
                .get(&SpecialTarget::Phony)
                .map_or(false, |set| set.contains(&pi.file));
            if is_phony {
                // Phony targets can't be used as include files.
                // Defer the "No such file or directory" message to Phase C so it
                // appears after any recipe output from other includes.
                let idx = work_items.len();
                file_to_work_idx.entry(pi.file.clone()).or_insert(idx);
                work_items.push(PendingWork {
                    pi_file: pi.file,
                    pi_ignore_missing: pi.ignore_missing,
                    pi_parent: pi.parent,
                    pi_lineno: pi.lineno,
                    outcome: if pi.ignore_missing {
                        PendingOutcome::SiblingAlreadyRan
                    } else {
                        PendingOutcome::Error(String::new())
                    },
                    deferred_no_such_file: !pi.ignore_missing,
                    also_make_siblings: Vec::new(),
                    rule_source_file: String::new(),
                    rule_lineno: 0, pre_mtime: None,
                });
                continue;
            }

            let rule_info = self.find_include_rule(&pi.file);
            // Capture sibling info for the peer-target warning (emitted in Phase C).
            let mut also_make_siblings_for_work: Vec<String> = Vec::new();
            let mut rule_source_file_for_work = String::new();
            let mut rule_lineno_for_work: usize = 0;
            let (outcome, deferred_no_such_file) = match rule_info {
                None => {
                    // No rule to rebuild this include.
                    // For -include (ignore_missing): silently skip; do NOT add to
                    // include_imagined, so the main build phase still checks for rules.
                    if pi.ignore_missing {
                        (PendingOutcome::SiblingAlreadyRan, false)
                    } else {
                        // Required include with no rule: defer the "No such file or
                        // directory" message to Phase C so ordering is correct.
                        (PendingOutcome::Error(
                            format!("No rule to make target '{}'.  Stop.", pi.file)
                        ), !pi.parent.is_empty())
                    }
                }
                Some(IncludeRuleInfo { skippable: true, .. }) => {
                    // Double-colon rule with no prereqs: skippable.
                    if pi.ignore_missing {
                        (PendingOutcome::SiblingAlreadyRan, false)
                    } else {
                        (PendingOutcome::Error(String::new()), !pi.parent.is_empty())
                    }
                }
                Some(IncludeRuleInfo { imagined: true, .. }) => {
                    (PendingOutcome::Imagined, false)
                }
                Some(IncludeRuleInfo { recipe, source_file, recipe_lineno, prerequisites, sibling_targets, stem, .. }) => {
                    // Capture sibling info for peer-target warning.
                    also_make_siblings_for_work = sibling_targets.clone();
                    rule_source_file_for_work = source_file.clone();
                    rule_lineno_for_work = recipe_lineno;
                    if self.include_recipe_ran.contains(&pi.file) {
                        // This file's recipe was already queued by a previous work item
                        // (either the same file included twice, or a sibling pattern rule).
                        // Defer to Phase C: resolve using the primary work item's recipe result.
                        let primary_idx = file_to_work_idx.get(&pi.file).copied();
                        let outcome = match primary_idx {
                            Some(idx) if matches!(work_items[idx].outcome, PendingOutcome::RunRecipe { .. }) => {
                                PendingOutcome::SiblingOf(idx)
                            }
                            _ => {
                                // Primary already ran (e.g. from a prior include-phase pass)
                                // or no RunRecipe tracked: treat as silent already-ran.
                                PendingOutcome::SiblingAlreadyRan
                            }
                        };
                        (outcome, false)
                    } else {
                        // Build prerequisites (sequential — requires &mut self)
                        let mut visited = HashSet::new();
                        visited.insert(pi.file.clone());
                        let prereq_result = self.build_include_prerequisites(
                            &prerequisites, &pi.file, &shell, &shell_flags, silent,
                            pi.ignore_missing, &mut visited,
                        );
                        match prereq_result {
                            Err(prereq_err) => {
                                if pi.ignore_missing {
                                    (PendingOutcome::Imagined, false)
                                } else {
                                    (PendingOutcome::Error(prereq_err), !pi.parent.is_empty())
                                }
                            }
                            Ok(()) => {
                                if recipe.is_empty() {
                                    (PendingOutcome::PrereqsOnlyNoRecipe, false)
                                } else {
                                    // Mark this file and its siblings so subsequent
                                    // includes of the same file can use SiblingOf.
                                    self.include_recipe_ran.insert(pi.file.clone());
                                    for sib in &sibling_targets {
                                        self.include_recipe_ran.insert(sib.clone());
                                    }
                                    let expanded = self.expand_include_recipe_lines(
                                        &pi.file, &stem, &recipe
                                    );
                                    // Record this as the primary work item for siblings.
                                    let idx = work_items.len();
                                    // Register siblings → this work item index.
                                    for sib in &sibling_targets {
                                        file_to_work_idx.entry(sib.clone()).or_insert(idx);
                                    }
                                    file_to_work_idx.entry(pi.file.clone()).or_insert(idx);
                                    (PendingOutcome::RunRecipe {
                                        expanded,
                                        source_file: source_file.clone(),
                                    }, false)
                                }
                            }
                        }
                    }
                }
            };

            let idx = work_items.len();
            file_to_work_idx.entry(pi.file.clone()).or_insert(idx);
            work_items.push(PendingWork {
                pi_file: pi.file,
                pi_ignore_missing: pi.ignore_missing,
                pi_parent: pi.parent,
                pi_lineno: pi.lineno,
                outcome,
                deferred_no_such_file,
                also_make_siblings: also_make_siblings_for_work,
                rule_source_file: rule_source_file_for_work,
                rule_lineno: rule_lineno_for_work,
                pre_mtime: file_path_buf.metadata().ok().and_then(|m| m.modified().ok()),
            });
        }

        // ── Phase B ─────────────────────────────────────────────────────────────
        // Run recipes. When jobs > 1 and multiple items need recipes, run them in
        // parallel threads; otherwise run sequentially.

        let mut recipe_results: Vec<Option<Result<(), String>>> =
            vec![None; work_items.len()];

        let recipe_indices: Vec<usize> = work_items.iter().enumerate()
            .filter_map(|(i, w)| {
                if matches!(w.outcome, PendingOutcome::RunRecipe { .. }) {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();

        if parallel_jobs > 1 && recipe_indices.len() > 1 {
            // Parallel execution via scoped threads (no lifetime issues)
            struct ThreadWork {
                idx: usize,
                expanded: Vec<(usize, String, bool, bool)>,
                target: String,
                source_file: String,
            }
            let thread_work: Vec<ThreadWork> = recipe_indices.iter().map(|&i| {
                if let PendingOutcome::RunRecipe { ref expanded, ref source_file } = work_items[i].outcome {
                    ThreadWork {
                        idx: i,
                        expanded: expanded.clone(),
                        target: work_items[i].pi_file.clone(),
                        source_file: source_file.clone(),
                    }
                } else {
                    unreachable!()
                }
            }).collect();

            let results: Vec<(usize, Result<(), String>)> =
                std::thread::scope(|s| {
                    let handles: Vec<_> = thread_work.into_iter().map(|tw| {
                        let sh = shell.as_str();
                        let shf = shell_flags.as_str();
                        s.spawn(move || {
                            let r = execute_include_recipe_expanded(
                                &tw.expanded, &tw.target, sh, shf, silent, &tw.source_file,
                            );
                            (tw.idx, r)
                        })
                    }).collect();
                    handles.into_iter().map(|h| h.join().unwrap()).collect()
                });

            for (idx, result) in results {
                recipe_results[idx] = Some(result);
            }
        } else {
            // Sequential execution
            for &i in &recipe_indices {
                if let PendingOutcome::RunRecipe { ref expanded, ref source_file } = work_items[i].outcome {
                    let target = work_items[i].pi_file.clone();
                    let r = execute_include_recipe_expanded(
                        expanded, &target, &shell, &shell_flags, silent, source_file,
                    );
                    recipe_results[i] = Some(r);
                }
            }
        }

        // ── Phase C ─────────────────────────────────────────────────────────────
        // Process all outcomes sequentially.
        // Note: recipe_results is indexed by work_item index; only RunRecipe items have results.
        // We need shared access to recipe_results for SiblingOf lookups, so collect into a Vec
        // first, then process.
        let work_items_vec: Vec<PendingWork> = work_items;

        // Helper: resolve the recipe result for a given work item index (for SiblingOf).
        // We peek at recipe_results without consuming.
        for i in 0..work_items_vec.len() {
            let file_path = std::path::PathBuf::from(&work_items_vec[i].pi_file);
            let pi_file = work_items_vec[i].pi_file.clone();
            let pi_ignore_missing = work_items_vec[i].pi_ignore_missing;
            let pi_parent = work_items_vec[i].pi_parent.clone();
            let pi_lineno = work_items_vec[i].pi_lineno;
            let deferred_no_such_file = work_items_vec[i].deferred_no_such_file;
            let also_make_siblings = work_items_vec[i].also_make_siblings.clone();
            let rule_source_file = work_items_vec[i].rule_source_file.clone();
            let rule_lineno = work_items_vec[i].rule_lineno;

            match &work_items_vec[i].outcome {
                PendingOutcome::AlreadyExists => {
                    // The file already existed before any recipe ran — it was NOT rebuilt.
                    // Re-read it if it wasn't already in makefile_list (e.g., an `include`
                    // of a new file that happens to exist). But do NOT set any_really_rebuilt:
                    // a file that existed before is not a "rebuild", so no re-exec is needed.
                    // This is critical for the three default makefile candidates (GNUmakefile,
                    // makefile, Makefile) when only one was selected: the others also exist but
                    // were not selected, and reading them + triggering re-exec would create
                    // an infinite restart loop.
                    let already_read = self.makefile_list.iter().any(|p| p == &file_path);
                    if !already_read {
                        if let Err(e) = self.read_makefile(file_path.as_path()) {
                            if !pi_ignore_missing {
                                return Err(format!("{}:{}: {}", pi_parent, pi_lineno, e));
                            }
                        }
                        // NOTE: Do NOT set any_really_rebuilt here. AlreadyExists means
                        // the file was not rebuilt by a recipe — no re-exec is warranted.
                    }
                }
                PendingOutcome::Error(msg) => {
                    if !pi_ignore_missing {
                        // Print deferred "No such file or directory" now (after recipes ran)
                        if deferred_no_such_file && !pi_parent.is_empty() {
                            eprintln!("{}:{}: {}: No such file or directory",
                                pi_parent, pi_lineno, pi_file);
                        }
                        if self.args.keep_going {
                            // In -k mode: print error without ".  Stop.", add "Failed to
                            // remake", then continue (but still return error at the end).
                            let progname = make_progname();
                            let clean = if msg.ends_with("  Stop.") {
                                &msg[..msg.len() - "  Stop.".len()]
                            } else {
                                msg.as_str()
                            };
                            if !clean.is_empty() {
                                eprintln!("{}: *** {}", progname, clean);
                            }
                            if !pi_parent.is_empty() {
                                eprintln!("{}:{}: Failed to remake makefile '{}'.",
                                    pi_parent, pi_lineno, pi_file);
                            }
                            return Err(String::new());
                        }
                        return Err(msg.clone());
                    }
                }
                PendingOutcome::Imagined => {
                    self.include_imagined.insert(pi_file.clone());
                }
                PendingOutcome::SiblingAlreadyRan => {
                    // Silently skipped (no rule, ignore_missing, or already processed).
                }
                PendingOutcome::SiblingOf(primary_idx) => {
                    // This file is a sibling of another work item's recipe.
                    // Check the primary's recipe result.
                    let primary_idx = *primary_idx;
                    let recipe_result = recipe_results[primary_idx].as_ref().map(|r| r.is_ok());
                    match recipe_result {
                        Some(true) => {
                            // Primary recipe succeeded.
                            if file_path.exists() {
                                // Do NOT read the rebuilt file now: re-exec will handle it.
                                any_really_rebuilt = true;
                            } else {
                                // Recipe ran but file not created → imagined
                                self.include_imagined.insert(pi_file.clone());
                            }
                        }
                        Some(false) | None => {
                            // Primary recipe failed (or no result available).
                            if !pi_ignore_missing {
                                if !pi_parent.is_empty() {
                                    eprintln!("{}:{}: Failed to remake makefile '{}'.",
                                        pi_parent, pi_lineno, pi_file);
                                }
                                return Err(String::new());
                            }
                            // For ignore_missing siblings: silently continue
                        }
                    }
                }
                PendingOutcome::PrereqsOnlyNoRecipe => {
                    if !file_path.exists() {
                        self.include_imagined.insert(pi_file.clone());
                    }
                }
                PendingOutcome::RunRecipe { .. } => {
                    // Use as_ref() (not take()) so SiblingOf items processed later
                    // can still inspect recipe_results[i].
                    let result = recipe_results[i].as_ref().unwrap();
                    match result {
                        Ok(()) => {
                            if file_path.exists() {
                                // Check if the file was actually modified by the recipe.
                                // If mtime is unchanged, the recipe ran but didn't modify
                                // the file (e.g., `@echo force $@`). No re-exec needed.
                                let post_mtime = file_path.metadata().ok().and_then(|m| m.modified().ok());
                                let pre = work_items_vec[i].pre_mtime;
                                let actually_changed = match (pre, post_mtime) {
                                    (Some(old), Some(new)) => new != old,
                                    (None, Some(_)) => true,  // file created
                                    _ => false,
                                };
                                if !actually_changed {
                                    // Recipe ran but file unchanged — don't re-exec.
                                    // Still read it if not already read.
                                    let already_read = self.makefile_list.iter().any(|p| p == &file_path);
                                    if !already_read {
                                        let _ = self.read_makefile(file_path.as_path());
                                    }
                                } else {
                                    any_really_rebuilt = true;
                                }
                                // Emit peer-target warning if siblings weren't created.
                                // This mirrors the warning in build_with_pattern_rule for
                                // the include-rebuild path (e.g. `include gta` with `%a %b: ...`).
                                if !also_make_siblings.is_empty() {
                                    let loc = if !rule_source_file.is_empty() && rule_lineno > 0 {
                                        format!("{}:{}: ", rule_source_file, rule_lineno)
                                    } else {
                                        String::new()
                                    };
                                    for sib in &also_make_siblings {
                                        if !std::path::Path::new(sib).exists() {
                                            eprintln!("{}warning: pattern recipe did not update peer target '{}'.", loc, sib);
                                        }
                                    }
                                }
                            } else {
                                // sv 61226: recipe ran but file not created → imagined
                                self.include_imagined.insert(pi_file.clone());
                            }
                        }
                        Err(recipe_err) => {
                            if !pi_ignore_missing {
                                if !pi_parent.is_empty() {
                                    eprintln!("{}:{}: {}: No such file or directory",
                                        pi_parent, pi_lineno, pi_file);
                                }
                                // With -k (keep-going): encode "Failed to remake" as a
                                // suffix line in the error string. GNU Make prints this
                                // message after the "*** Error N" line. Since main.rs
                                // prints "jmake: *** {err}" after run() returns, we embed
                                // the "Failed to remake" in the error string so it follows
                                // the "*** Error N" line in the output.
                                // Without -k: just return the error, no "Failed to remake".
                                let recipe_err_clean = if self.args.keep_going {
                                    // In -k mode: print error directly, add "Failed to remake"
                                    let e = if recipe_err.ends_with("  Stop.") {
                                        recipe_err[..recipe_err.len() - "  Stop.".len()].to_string()
                                    } else {
                                        recipe_err.to_string()
                                    };
                                    let progname = make_progname();
                                    if !e.is_empty() {
                                        eprintln!("{}: *** {}", progname, e);
                                    }
                                    if !pi_parent.is_empty() {
                                        eprintln!("{}:{}: Failed to remake makefile '{}'.",
                                            pi_parent, pi_lineno, pi_file);
                                    }
                                    return Err(String::new()); // Empty = already printed
                                } else {
                                    recipe_err.clone()
                                };
                                return Err(recipe_err_clean);
                            }
                        }
                    }
                }
            }
        }

        // If any pending include was really rebuilt, re-exec from scratch
        if any_really_rebuilt {
            let restart_val = env::var("MAKE_RESTARTS")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(0);
            self.do_reinvoke(restart_val + 1);
        }

        Ok(())
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

    // If the original variable name token was non-empty (e.g. `$(VAR)`) but expanded to
    // empty, reconstruct as `"define "` (without the operator) so that parse_line will
    // correctly recognize this as a define directive with an empty name.  If we kept the
    // operator (e.g. returning `"define  ="`) parse_line would trim the rest and see `"="`
    // directly after `"define"`, mistakenly treating it as a variable assignment to a
    // variable named `"define"`.  By omitting the operator, parse_define_start is called
    // and the empty-name check in the eval handler fires with the correct error message.
    if !var_name_raw.is_empty() && expanded_name.is_empty() {
        return Some(format!("{}define ", prefix));
    }

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

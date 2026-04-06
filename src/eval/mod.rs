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
}

pub struct MakeState {
    pub args: MakeArgs,
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
    /// Pending includes that couldn't be found during initial read
    pub pending_includes: Vec<PendingInclude>,
}

impl MakeState {
    pub fn new(args: MakeArgs) -> Self {
        let mut state = MakeState {
            args,
            db: MakeDatabase::new(),
            shell: "/bin/sh".to_string(),
            makefile_list: Vec::new(),
            include_dirs: Vec::new(),
            eval_pending: RefCell::new(Vec::new()),
            current_file: RefCell::new(String::new()),
            current_line: RefCell::new(0),
            last_shell_status: RefCell::new(0),
            pending_includes: Vec::new(),
        };

        // Change directory if requested
        let progname = env::args().next().unwrap_or_else(|| "make".to_string());
        if let Some(ref dir) = state.args.directory {
            if let Err(e) = env::set_current_dir(dir) {
                eprintln!("{}: Entering directory '{}'", progname, dir.display());
                eprintln!("{}: *** {}: {}.  Stop.", progname, dir.display(), e);
                std::process::exit(2);
            }
            if state.args.print_directory || !state.args.no_print_directory {
                let cwd = env::current_dir().unwrap_or_default();
                eprintln!("{}: Entering directory '{}'", progname, cwd.display());
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

        // Read makefiles
        self.read_makefiles()?;

        // Process any --eval strings
        for eval_str in self.args.eval_strings.clone() {
            self.eval_string(&eval_str)?;
        }

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

        // Print database and exit if -p
        if self.args.print_data_base {
            self.print_database();
            if self.args.question || self.args.targets.is_empty() {
                return Ok(());
            }
        }

        // Build targets
        let progname = env::args().next().unwrap_or_else(|| "make".to_string());
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
            progname,
        );

        let result = executor.build_targets(&targets);

        // Print directory on exit if needed
        if let Some(ref _dir) = self.args.directory {
            if self.args.print_directory || !self.args.no_print_directory {
                let cwd = env::current_dir().unwrap_or_default();
                let progname = env::args().next().unwrap_or_else(|| "make".to_string());
                eprintln!("{}: Leaving directory '{}'", progname, cwd.display());
            }
        }

        result
    }

    fn init_variables(&mut self) {
        let cwd = env::current_dir().unwrap_or_default();

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
        self.db.variables.insert(".INCLUDE_DIRS".into(),
            Variable::new("/usr/include /usr/local/include".into(), VarFlavor::Simple, VarOrigin::Default));
        self.db.variables.insert("SHELL".into(),
            Variable::new("/bin/sh".into(), VarFlavor::Simple, VarOrigin::Default));
        self.db.variables.insert(".SHELLFLAGS".into(),
            Variable::new("-c".into(), VarFlavor::Simple, VarOrigin::Default));
        self.db.variables.insert("MAKEFLAGS".into(),
            Variable::new(self.build_makeflags(), VarFlavor::Recursive, VarOrigin::Default));
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

        // Command-line variables override everything
        for (name, value) in &self.args.variables {
            self.db.variables.insert(name.clone(),
                Variable::new(value.clone(), VarFlavor::Simple, VarOrigin::CommandLine));
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
        if self.args.keep_going { single_flags.push('k'); }
        if self.args.dry_run { single_flags.push('n'); }
        if self.args.question { single_flags.push('q'); }
        if self.args.no_builtin_rules { single_flags.push('r'); }
        if self.args.no_builtin_variables { single_flags.push('R'); }
        if self.args.silent { single_flags.push('s'); }
        if self.args.touch { single_flags.push('t'); }
        if self.args.print_directory { single_flags.push('w'); }
        if self.args.check_symlink_times { single_flags.push('L'); }

        // Long options (no single-char equivalent)
        let mut long_parts: Vec<String> = Vec::new();
        if self.args.trace { long_parts.push("--trace".to_string()); }
        if self.args.no_print_directory { long_parts.push("--no-print-directory".to_string()); }
        if self.args.warn_undefined_variables { long_parts.push("--warn-undefined-variables".to_string()); }

        // Options with arguments
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
        // GNU Make outputs variables in reverse-insertion order (last cmdline var first),
        // with deduplication keeping the LAST occurrence (cmdline beats env since cmdline
        // is added after env MAKEFLAGS is parsed).
        if has_vars {
            // Deduplicate: iterate in reverse, keep first-seen (= last-specified)
            let mut seen_names: HashSet<String> = HashSet::new();
            let mut var_parts: Vec<String> = Vec::new();
            for (name, value) in variables.iter().rev() {
                // Strip trailing ':' (and '?' '+') from name for dedup key
                // (handles 'hello:' from 'hello:=world')
                let key = name.trim_end_matches(|c| c == ':' || c == '?' || c == '+');
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
                if line.trim() == "endef" {
                    parser.in_define = false;
                    let value = parser.define_lines.join("\n");
                    let var = Variable::new(
                        value,
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
                                        existing.value.push_str(&parser.define_lines.join("\n"));
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
            let expanded = if line.starts_with('\t') {
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
                if let Some(raw_parsed) = parser::try_parse_variable_assignment(trimmed) {
                    if let ParsedLine::VariableAssignment { name: raw_name, value: raw_value, flavor: raw_flavor, .. } = raw_parsed {
                        match raw_flavor {
                            VarFlavor::Simple => {
                                // Immediate expansion: expand the whole line
                                self.expand(&line)
                            }
                            _ => {
                                // Deferred expansion: expand only the LHS (name / prefixes),
                                // keep the value verbatim.
                                // Reconstruct the line with only the name expanded.
                                let expanded_name = self.expand(&raw_name);
                                // Re-build a minimal version that the parser can handle.
                                // We preserve the original prefixes (override, export, private)
                                // from the trimmed line by replacing the raw name with the
                                // expanded name.
                                let op_str = match raw_flavor {
                                    VarFlavor::Append => " += ",
                                    VarFlavor::Conditional => " ?= ",
                                    VarFlavor::Shell => " != ",
                                    _ => " = ",
                                };
                                // Preserve override/export/private prefixes from the original
                                let prefix = extract_var_prefixes(trimmed);
                                format!("{}{}{}{}", prefix, expanded_name, op_str, raw_value)
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

                    // Stamp the source file and fix up inline recipe line numbers
                    rule.source_file = parser.filename.to_string_lossy().to_string();
                    for entry in rule.recipe.iter_mut() {
                        if entry.0 == 0 {
                            entry.0 = lineno;
                        }
                    }

                    // Register the previous rule (and any static-pattern siblings).
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

                    // Prepare each expanded rule: expand variable references in
                    // the already-substituted prerequisites, stamp source file
                    // and line number.
                    let source_file = parser.filename.to_string_lossy().to_string();
                    let mut prepared: Vec<Rule> = expanded_rules
                        .into_iter()
                        .map(|mut rule| {
                            rule.prerequisites = rule.prerequisites.iter()
                                .flat_map(|p| {
                                    let e = self.expand(p);
                                    parser::split_words(&e)
                                })
                                .collect();
                            rule.order_only_prerequisites = rule.order_only_prerequisites.iter()
                                .flat_map(|p| {
                                    let e = self.expand(p);
                                    parser::split_words(&e)
                                })
                                .collect();
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
                ParsedLine::VariableAssignment { name, value, flavor, is_override, is_export, is_private, target } => {
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
                                let var = Variable::new(value.clone(), flavor.clone(), var_origin.clone());
                                self.db.pattern_specific_vars.push(PatternSpecificVar {
                                    pattern: t,
                                    var_name: name.clone(),
                                    var,
                                    is_override,
                                });
                            } else {
                                // Target-specific variable: stored in the rule
                                let var = Variable::new(value.clone(), flavor.clone(), var_origin.clone());
                                let rules = self.db.rules.entry(t.clone()).or_insert_with(Vec::new);
                                // Add to all rules for this target
                                for r in rules.iter_mut() {
                                    r.target_specific_vars.insert(name.clone(), var.clone());
                                }
                                // If no rules yet, create a placeholder
                                if rules.is_empty() {
                                    let mut r = Rule::new();
                                    r.targets = vec![t];
                                    r.target_specific_vars.insert(name.clone(), var.clone());
                                    rules.push(r);
                                }
                            }
                        }
                    } else {
                        self.set_variable(&name, &value, &flavor, is_override, is_export);
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
                            let file_path = self.find_include_file(&file);
                            match file_path {
                                Some(p) => {
                                    if let Err(e) = self.read_makefile(&p) {
                                        if !ignore_missing {
                                            // Required include failed to parse - warn and defer
                                            eprintln!("{}:{}: {}", parser.filename.display(), lineno, e);
                                        }
                                        self.pending_includes.push(PendingInclude {
                                            file: file.clone(),
                                            parent: parser.filename.to_string_lossy().to_string(),
                                            lineno,
                                            ignore_missing,
                                        });
                                    }
                                }
                                None => {
                                    // File not found
                                    if !ignore_missing {
                                        // Required include: print warning (no *** prefix)
                                        eprintln!("{}:{}: {}: No such file or directory",
                                            parser.filename.display(), lineno, file);
                                    }
                                    // Always add to pending (both required and optional may be buildable)
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
                ParsedLine::Define { name, flavor, is_override, is_export } => {
                    parser.in_define = true;
                    parser.define_name = name;
                    parser.define_flavor = flavor;
                    parser.define_override = is_override;
                    parser.define_export = is_export;
                    parser.define_lines.clear();
                }
                ParsedLine::Empty | ParsedLine::Comment => {
                    // Empty lines and comments do NOT end the recipe context.
                    // GNU Make allows blank lines between recipe lines; only a
                    // non-empty, non-recipe line (rule, assignment, directive)
                    // should clear in_recipe / current_rule.
                }
                _ => {}
            }
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
                    SpecialTarget::Silent | SpecialTarget::Ignore => {
                        let set = self.db.special_targets.entry(special.clone()).or_insert_with(HashSet::new);
                        set.extend(prereqs);
                    }
                    SpecialTarget::ExportAllVariables => {
                        // .EXPORT_ALL_VARIABLES: causes all variables to be exported
                        let set = self.db.special_targets.entry(special.clone()).or_insert_with(HashSet::new);
                        set.extend(prereqs);
                        self.db.export_all = true;
                    }
                    SpecialTarget::Suffixes => {
                        if rule.prerequisites.is_empty() {
                            self.db.suffixes.clear();
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
            self.db.pattern_rules.push(rule.clone());
            return;
        }

        // Register suffix rules
        if rule.targets.len() == 1 {
            let target = &rule.targets[0];
            if is_suffix_rule(target, &self.db.suffixes) {
                self.db.suffix_rules.push(rule.clone());
                // Also register as pattern rule
                if let Some(pattern_rule) = suffix_to_pattern_rule(target, &rule) {
                    self.db.pattern_rules.push(pattern_rule);
                }
            }
        }

        // Register explicit rules
        for target in &rule.targets {
            if SpecialTarget::from_str(target).is_some() {
                continue;
            }
            let rules = self.db.rules.entry(target.clone()).or_insert_with(Vec::new);
            if rule.is_double_colon {
                rules.push(rule.clone());
            } else {
                // Single colon - merge prerequisites, replace recipe if new one given
                if let Some(existing) = rules.first_mut() {
                    existing.prerequisites.extend(rule.prerequisites.clone());
                    existing.order_only_prerequisites.extend(rule.order_only_prerequisites.clone());
                    // Merge second-expansion raw prerequisite text.
                    if let Some(ref new_text) = rule.second_expansion_prereqs {
                        match existing.second_expansion_prereqs {
                            Some(ref mut existing_text) => {
                                if !existing_text.is_empty() {
                                    existing_text.push(' ');
                                }
                                existing_text.push_str(new_text);
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
                    if !rule.recipe.is_empty() {
                        if !existing.recipe.is_empty() {
                            eprintln!("make: warning: overriding recipe for target '{}'", target);
                        }
                        existing.recipe = rule.recipe.clone();
                    }
                    // Merge target-specific vars
                    for (k, v) in &rule.target_specific_vars {
                        existing.target_specific_vars.insert(k.clone(), v.clone());
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
        let is_protected = |orig: &VarOrigin| {
            matches!(orig, VarOrigin::CommandLine | VarOrigin::Override)
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
                // != executes value as shell command; also sets .SHELLSTATUS
                let (result, status) = functions::fn_shell_exec_with_status(value);
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
        // This allows things like `MAKEFLAGS += -Ibar` to actually add to include_dirs.
        // (makeflags_protected covers the -e case where MAKEFLAGS is already protected above)
        if name == "MAKEFLAGS" && !is_override && !makeflags_protected {
            self.apply_makeflags_from_makefile();
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
    fn apply_makeflags_from_makefile(&mut self) {
        let makeflags = match self.db.variables.get("MAKEFLAGS") {
            Some(v) => v.value.clone(),
            None => return,
        };

        // Save existing include_dirs to detect new additions
        let old_include_dirs = self.args.include_dirs.clone();

        // Parse the full current MAKEFLAGS value, merging into a copy of self.args
        // to apply any new flags that were added.
        crate::cli::parse_makeflags(&makeflags, &mut self.args);

        // Apply any newly added include dirs to self.include_dirs as well
        for dir in &self.args.include_dirs.clone() {
            if !old_include_dirs.contains(dir) {
                self.include_dirs.push(dir.clone());
            }
        }

        // Rebuild MAKEFLAGS from the updated args in canonical format.
        // This ensures single-char flags are sorted and properly bundled.
        let new_makeflags = self.build_makeflags_from_args(&self.args.variables.clone());
        if let Some(var) = self.db.variables.get_mut("MAKEFLAGS") {
            var.value = new_makeflags;
        }

        // Update .INCLUDE_DIRS to reflect current include_dirs
        // GNU Make's .INCLUDE_DIRS shows the effective include path (excluding special "-" entry)
        let include_dirs_str: String = self.args.include_dirs.iter()
            .filter(|d| d.to_string_lossy() != "-")
            .map(|d| d.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        self.db.variables.insert(".INCLUDE_DIRS".into(),
            Variable::new(include_dirs_str, VarFlavor::Simple, VarOrigin::Default));
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
                        return Err(format!("No rule to make target '{}'.  Stop.", pi.file));
                    }
                    continue;
                }

                // Check for buildable rule
                let rule_info = self.find_include_rule(&pi.file);

                match rule_info {
                    None => {
                        // No rule at all
                        if !pi.ignore_missing {
                            return Err(format!("No rule to make target '{}'.  Stop.", pi.file));
                        }
                        // Optional include with no rule: silently skip
                    }
                    Some(IncludeRuleInfo { skippable: true, .. }) => {
                        // Double-colon with no prerequisites or other skippable rule:
                        // treat as if there's no rule for include-rebuild purposes
                        if !pi.ignore_missing {
                            if !pi.parent.is_empty() {
                                eprintln!("{}:{}: {}: No such file or directory",
                                    pi.parent, pi.lineno, pi.file);
                            }
                            return Err(format!("No rule to make target '{}'.  Stop.", pi.file));
                        }
                        // Optional include: silently skip
                    }
                    Some(IncludeRuleInfo { recipe, source_file, recipe_lineno, prerequisites, skippable: false }) => {
                        // First, check/build prerequisites
                        let prereq_result = self.build_include_prerequisites(
                            &prerequisites,
                            &shell,
                            &shell_flags,
                            silent,
                        );

                        match prereq_result {
                            Err(prereq_err) => {
                                // Prerequisite couldn't be built
                                if !pi.ignore_missing {
                                    if !pi.parent.is_empty() {
                                        eprintln!("{}:{}: {}: No such file or directory",
                                            pi.parent, pi.lineno, pi.file);
                                    }
                                    return Err(prereq_err);
                                }
                                // Optional include: silently skip
                            }
                            Ok(()) => {
                                if recipe.is_empty() {
                                    // Has prerequisites but no recipe: just check if file exists
                                    if !file_path.exists() && !pi.ignore_missing {
                                        return Err(format!("No rule to make target '{}'.  Stop.", pi.file));
                                    }
                                } else {
                                    // Run the recipe to build the file
                                    let built = self.run_include_recipe(
                                        &pi.file,
                                        &recipe,
                                        &shell,
                                        &shell_flags,
                                        silent,
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
                                                // Recipe ran but file not created
                                                if !pi.parent.is_empty() {
                                                    eprintln!("{}:{}: Failed to remake makefile '{}'.",
                                                        pi.parent, pi.lineno, pi.file);
                                                }
                                                if !pi.ignore_missing {
                                                    return Err(format!("[{}:{}] Error 1",
                                                        source_file, recipe_lineno));
                                                }
                                            }
                                        }
                                        Err(recipe_err) => {
                                            // Recipe failed
                                            if !pi.ignore_missing {
                                                if !pi.parent.is_empty() {
                                                    eprintln!("{}:{}: {}: No such file or directory",
                                                        pi.parent, pi.lineno, pi.file);
                                                }
                                                return Err(format!("[{}:{}] {}", source_file, recipe_lineno, recipe_err));
                                            }
                                            // Optional include with failed recipe: silently skip
                                        }
                                    }
                                }
                            }
                        }
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
    fn build_include_prerequisites(
        &self,
        prerequisites: &[String],
        shell: &str,
        shell_flags: &str,
        silent: bool,
    ) -> Result<(), String> {
        for prereq in prerequisites {
            let prereq_path = Path::new(prereq);
            if prereq_path.exists() {
                continue;
            }
            // Check if prereq has a rule with a recipe
            let rule_info = self.find_include_rule(prereq);
            match rule_info {
                None => {
                    return Err(format!("No rule to make target '{}', needed by '{}'.  Stop.",
                        prereq, prereq));
                }
                Some(info) if info.skippable => {
                    return Err(format!("No rule to make target '{}', needed by '{}'.  Stop.",
                        prereq, prereq));
                }
                Some(info) => {
                    // Build prerequisites of this prereq first
                    self.build_include_prerequisites(&info.prerequisites, shell, shell_flags, silent)?;
                    // Run the recipe
                    if !info.recipe.is_empty() {
                        self.run_include_recipe(prereq, &info.recipe, shell, shell_flags, silent)?;
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
                    });
                }
            }
        }
        // Check pattern rules
        for rule in &self.db.pattern_rules {
            for pat in &rule.targets {
                if let Some(stem) = parser::match_pattern(target, pat) {
                    if !rule.recipe.is_empty() {
                        let recipe: Vec<(usize, String)> = rule.recipe.iter()
                            .map(|(ln, cmd)| {
                                // Substitute automatic variables in recipe at this point
                                let cmd_expanded = cmd
                                    .replace("$*", &stem)
                                    .replace("$@", target);
                                (*ln, cmd_expanded)
                            })
                            .collect();
                        let src = rule.source_file.clone();
                        let ln = recipe.first().map(|(l, _)| *l).unwrap_or(0);
                        // Expand prerequisites for the stem
                        let prereqs: Vec<String> = rule.prerequisites.iter()
                            .map(|p| p.replace('%', &stem))
                            .collect();
                        return Some(IncludeRuleInfo {
                            recipe,
                            source_file: src,
                            recipe_lineno: ln,
                            prerequisites: prereqs,
                            skippable: false,
                        });
                    }
                }
            }
        }
        None
    }

    fn run_include_recipe(
        &self, target: &str, recipe: &[(usize, String)],
        shell: &str, shell_flags: &str, silent: bool,
    ) -> Result<(), String> {
        let progname = env::args().next().unwrap_or_else(|| "make".to_string());
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
            let expanded_cmd = self.expand(&cmd);
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
                    eprintln!("{}: *** [{}:{}] Error {}", progname, target, lineno, code);
                    if !ignore_error {
                        return Err(format!("[{}:{}] Error {}", target, lineno, code));
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

fn suffix_to_pattern_rule(target: &str, rule: &Rule) -> Option<Rule> {
    // Convert .c.o to %.o: %.c
    let suffixes_db = vec![
        ".out", ".a", ".ln", ".o", ".c", ".cc", ".C", ".cpp",
        ".p", ".f", ".F", ".m", ".r", ".y", ".l", ".s", ".S",
        ".h", ".sh",
    ];

    for s1 in &suffixes_db {
        if target.starts_with(s1) {
            let s2 = &target[s1.len()..];
            if !s2.is_empty() && suffixes_db.contains(&s2) {
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

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
                    return Err("No targets specified and no makefile found".to_string());
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
            Variable::new("target-specific order-only second-expansion else-if shortest-stem undefine oneshell archives jobserver output-sync check-symlink".into(),
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

        // Import environment variables
        for (key, value) in env::vars() {
            let origin = if self.args.environment_overrides {
                VarOrigin::Environment
            } else {
                VarOrigin::Environment
            };
            self.db.variables.entry(key.clone()).or_insert_with(|| {
                Variable::new(value.clone(), VarFlavor::Recursive, origin)
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
        //   Long options and flags with arguments follow (space-separated): "ks --jobserver-auth=..."
        //   Command-line variable assignments are appended at the end
        let mut single_flags = String::new();
        if self.args.always_make { single_flags.push('B'); }
        if self.args.environment_overrides { single_flags.push('e'); }
        if self.args.ignore_errors { single_flags.push('i'); }
        if self.args.keep_going { single_flags.push('k'); }
        if self.args.dry_run { single_flags.push('n'); }
        if self.args.no_builtin_rules { single_flags.push('r'); }
        if self.args.no_builtin_variables { single_flags.push('R'); }
        if self.args.silent { single_flags.push('s'); }
        if self.args.touch { single_flags.push('t'); }
        if self.args.print_directory { single_flags.push('w'); }

        let mut parts: Vec<String> = Vec::new();
        if !single_flags.is_empty() {
            parts.push(single_flags);
        }

        // Append command-line variable assignments
        for (name, value) in &self.args.variables {
            parts.push(format!("{}={}", name, value));
        }

        parts.join(" ")
    }

    fn read_makefiles(&mut self) -> Result<(), String> {
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
                return Err("No targets.  Stop.".to_string());
            }
            found
        } else {
            self.args.makefiles.clone()
        };

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
        let mut parser = Parser::new(path.to_path_buf());
        if let Err(e) = parser.load_file() {
            return Err(format!("{}: {}", path.display(), e));
        }

        self.makefile_list.push(path.to_path_buf());
        self.process_parsed_lines(&mut parser)
    }

    pub fn eval_string(&mut self, content: &str) -> Result<(), String> {
        let mut parser = Parser::new(PathBuf::from("<eval>"));
        parser.load_string(content);
        self.process_parsed_lines(&mut parser)
    }

    fn process_parsed_lines(&mut self, parser: &mut Parser) -> Result<(), String> {
        let mut current_rule: Option<Rule> = None;

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
                self.expand(&line)
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
                    // Expand prerequisites
                    rule.prerequisites = rule.prerequisites.iter()
                        .flat_map(|p| {
                            let expanded = self.expand(p);
                            parser::split_words(&expanded)
                        })
                        .collect();
                    rule.order_only_prerequisites = rule.order_only_prerequisites.iter()
                        .flat_map(|p| {
                            let expanded = self.expand(p);
                            parser::split_words(&expanded)
                        })
                        .collect();

                    // Stamp the source file and fix up inline recipe line numbers
                    rule.source_file = parser.filename.to_string_lossy().to_string();
                    for entry in rule.recipe.iter_mut() {
                        if entry.0 == 0 {
                            entry.0 = lineno;
                        }
                    }

                    // Store current rule and register it
                    if let Some(prev) = current_rule.take() {
                        self.register_rule(prev);
                    }
                    parser.in_recipe = true;
                    current_rule = Some(rule);
                }
                ParsedLine::Recipe(recipe) => {
                    if let Some(ref mut rule) = current_rule {
                        rule.recipe.push((lineno, recipe));
                    }
                }
                ParsedLine::VariableAssignment { name, value, flavor, is_override, is_export, is_private, target } => {
                    if let Some(ref mut rule) = current_rule {
                        self.register_rule(rule.clone());
                        current_rule = None;
                        parser.in_recipe = false;
                    }

                    if let Some(target_name) = target {
                        // Target-specific variable
                        let targets: Vec<String> = parser::split_words(&target_name);
                        for t in targets {
                            let rules = self.db.rules.entry(t.clone()).or_insert_with(Vec::new);
                            let var = Variable::new(value.clone(), flavor.clone(), VarOrigin::File);
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
                    } else {
                        self.set_variable(&name, &value, &flavor, is_override, is_export);
                    }
                }
                ParsedLine::Include { paths, ignore_missing } => {
                    if let Some(ref mut rule) = current_rule {
                        self.register_rule(rule.clone());
                        current_rule = None;
                        parser.in_recipe = false;
                    }

                    for path_pattern in &paths {
                        let expanded = self.expand(path_pattern);
                        let files: Vec<String> = parser::split_words(&expanded);
                        for file in files {
                            let file_path = self.find_include_file(&file);
                            match file_path {
                                Some(p) => {
                                    if let Err(e) = self.read_makefile(&p) {
                                        if !ignore_missing {
                                            return Err(format!("{}:{}: {}", parser.filename.display(), lineno, e));
                                        }
                                    }
                                }
                                None => {
                                    if !ignore_missing {
                                        return Err(format!("{}:{}: {}: No such file or directory", parser.filename.display(), lineno, file));
                                    }
                                }
                            }
                        }
                    }
                }
                ParsedLine::VpathDirective { pattern, directories } => {
                    if let Some(ref mut rule) = current_rule {
                        self.register_rule(rule.clone());
                        current_rule = None;
                        parser.in_recipe = false;
                    }

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

        // Register the last rule
        if let Some(rule) = current_rule {
            self.register_rule(rule);
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
                    SpecialTarget::Silent | SpecialTarget::ExportAllVariables |
                    SpecialTarget::Ignore => {
                        let set = self.db.special_targets.entry(special.clone()).or_insert_with(HashSet::new);
                        set.extend(prereqs);
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
                    SpecialTarget::ExportAllVariables => {
                        self.db.export_all = true;
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

        match flavor {
            VarFlavor::Append => {
                if let Some(existing) = self.db.variables.get_mut(name) {
                    if !is_override && existing.origin == VarOrigin::CommandLine {
                        return; // Can't override command-line vars without override
                    }
                    if existing.value.is_empty() {
                        existing.value = value.to_string();
                    } else {
                        existing.value.push(' ');
                        existing.value.push_str(value);
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
                        if existing.origin == VarOrigin::CommandLine {
                            return;
                        }
                    }
                }
                self.db.variables.insert(name.to_string(),
                    Variable::new(result, VarFlavor::Simple, origin));
            }
            _ => {
                let existing = self.db.variables.get(name);
                if !is_override {
                    if let Some(existing) = existing {
                        if existing.origin == VarOrigin::CommandLine {
                            return;
                        }
                    }
                }
                self.db.variables.insert(name.to_string(),
                    Variable::new(value.to_string(), flavor.clone(), origin));
            }
        }

        if is_export {
            if let Some(var) = self.db.variables.get_mut(name) {
                var.export = Some(true);
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

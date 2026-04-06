// Copyright (c) 2026 Jon-Erik G. Storm. All rights reserved.
// Recipe execution engine - dependency resolution and recipe running

use crate::database::MakeDatabase;
use crate::eval::MakeState;
use crate::functions;
use crate::types::*;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::SystemTime;

pub struct Executor<'a> {
    db: &'a MakeDatabase,
    state: &'a MakeState,
    jobs: usize,
    keep_going: bool,
    dry_run: bool,
    touch: bool,
    question: bool,
    silent: bool,
    ignore_errors: bool,
    shell: &'a str,
    shell_flags: &'a str,
    always_make: bool,
    trace: bool,
    built: HashMap<String, bool>, // target -> was rebuilt
    building: HashSet<String>,    // cycle detection
    question_out_of_date: bool,
    errors: Vec<String>,
}

impl<'a> Executor<'a> {
    pub fn new(
        db: &'a MakeDatabase,
        state: &'a MakeState,
        jobs: usize,
        keep_going: bool,
        dry_run: bool,
        touch: bool,
        question: bool,
        silent: bool,
        ignore_errors: bool,
        shell: &'a str,
        shell_flags: &'a str,
        always_make: bool,
        trace: bool,
    ) -> Self {
        Executor {
            db,
            state,
            jobs,
            keep_going,
            dry_run,
            touch,
            question,
            silent,
            ignore_errors,
            shell,
            shell_flags,
            always_make,
            trace,
            built: HashMap::new(),
            building: HashSet::new(),
            question_out_of_date: false,
            errors: Vec::new(),
        }
    }

    pub fn build_targets(&mut self, targets: &[String]) -> Result<(), String> {
        for target in targets {
            match self.build_target(target) {
                Ok(_) => {}
                Err(e) => {
                    if self.keep_going {
                        self.errors.push(e);
                    } else {
                        return Err(e);
                    }
                }
            }
        }

        if !self.errors.is_empty() {
            return Err(format!("Target(s) not remade because of errors:\n{}",
                self.errors.join("\n")));
        }

        if self.question && self.question_out_of_date {
            std::process::exit(1);
        }

        Ok(())
    }

    fn build_target(&mut self, target: &str) -> Result<bool, String> {
        // Already built?
        if let Some(&rebuilt) = self.built.get(target) {
            return Ok(rebuilt);
        }

        // Cycle detection
        if self.building.contains(target) {
            eprintln!("jmake: Circular {} <- {} dependency dropped.", target, target);
            return Ok(false);
        }
        self.building.insert(target.to_string());

        let result = self.build_target_inner(target);

        self.building.remove(target);

        match &result {
            Ok(rebuilt) => {
                self.built.insert(target.to_string(), *rebuilt);
            }
            Err(_) => {}
        }

        result
    }

    fn build_target_inner(&mut self, target: &str) -> Result<bool, String> {
        // Find rule for this target
        let rules = self.db.rules.get(target).cloned();
        let is_phony = self.db.is_phony(target);

        // If we have explicit rules
        if let Some(rules) = &rules {
            if !rules.is_empty() {
                return self.build_with_rules(target, rules, is_phony);
            }
        }

        // Try pattern rules
        if let Some((pattern_rule, stem)) = self.find_pattern_rule(target) {
            return self.build_with_pattern_rule(target, &pattern_rule, &stem, is_phony);
        }

        // Check if file exists (no rule needed)
        if Path::new(target).exists() {
            return Ok(false);
        }

        // Try VPATH
        if let Some(found) = self.find_in_vpath(target) {
            return Ok(false);
        }

        // Try .DEFAULT rule
        if let Some(ref default_rule) = self.db.default_rule {
            if !default_rule.recipe.is_empty() {
                let mut auto_vars = HashMap::new();
                auto_vars.insert("@".to_string(), target.to_string());
                auto_vars.insert("<".to_string(), String::new());
                auto_vars.insert("^".to_string(), String::new());
                auto_vars.insert("+".to_string(), String::new());
                auto_vars.insert("?".to_string(), String::new());
                auto_vars.insert("*".to_string(), String::new());
                return self.execute_recipe(target, &default_rule.recipe, &auto_vars, false);
            }
        }

        Err(format!("No rule to make target '{}'.  Stop.", target))
    }

    fn build_with_rules(&mut self, target: &str, rules: &[Rule], is_phony: bool) -> Result<bool, String> {
        let mut all_prereqs = Vec::new();
        let mut all_order_only = Vec::new();
        let mut recipe = Vec::new();
        let mut target_vars: HashMap<String, String> = HashMap::new();

        for rule in rules {
            all_prereqs.extend(rule.prerequisites.clone());
            all_order_only.extend(rule.order_only_prerequisites.clone());
            if !rule.recipe.is_empty() {
                recipe = rule.recipe.clone();
            }
            for (k, v) in &rule.target_specific_vars {
                target_vars.insert(k.clone(), v.value.clone());
            }
        }

        // Build prerequisites
        let mut any_prereq_rebuilt = false;
        let mut prereq_errors = Vec::new();

        for prereq in &all_prereqs {
            let expanded = self.state.expand(prereq);
            for p in expanded.split_whitespace() {
                match self.build_target(p) {
                    Ok(rebuilt) => {
                        if rebuilt { any_prereq_rebuilt = true; }
                    }
                    Err(e) => {
                        if self.keep_going {
                            prereq_errors.push(e);
                        } else {
                            return Err(format!("No rule to make target '{}', needed by '{}'.  Stop.", p, target));
                        }
                    }
                }
            }
        }

        // Build order-only prerequisites
        for prereq in &all_order_only {
            let expanded = self.state.expand(prereq);
            for p in expanded.split_whitespace() {
                let _ = self.build_target(p); // Errors in order-only are less critical
            }
        }

        if !prereq_errors.is_empty() {
            return Err(prereq_errors.join("\n"));
        }

        // Determine if we need to rebuild
        let needs_rebuild = if self.always_make || is_phony {
            true
        } else if recipe.is_empty() {
            false
        } else {
            self.needs_rebuild(target, &all_prereqs, any_prereq_rebuilt)
        };

        if !needs_rebuild {
            return Ok(false);
        }

        if self.question {
            self.question_out_of_date = true;
            return Ok(true);
        }

        if recipe.is_empty() {
            return Ok(any_prereq_rebuilt);
        }

        // Set up automatic variables
        let oo_refs: Vec<&str> = all_order_only.iter().map(|s| s.as_str()).collect();
        let auto_vars = self.make_auto_vars(target, &all_prereqs, &oo_refs, "");

        self.execute_recipe(target, &recipe, &auto_vars, is_phony)
    }

    fn build_with_pattern_rule(&mut self, target: &str, rule: &Rule, stem: &str, is_phony: bool) -> Result<bool, String> {
        // Expand pattern prerequisites using the stem
        let prereqs: Vec<String> = rule.prerequisites.iter()
            .map(|p| p.replace('%', stem))
            .collect();

        // Build prerequisites
        let mut any_rebuilt = false;
        for prereq in &prereqs {
            match self.build_target(prereq) {
                Ok(rebuilt) => {
                    if rebuilt { any_rebuilt = true; }
                }
                Err(e) => {
                    // If prerequisite can't be built, this pattern rule doesn't apply
                    // Try next pattern rule
                    return Err(format!("No rule to make target '{}', needed by '{}'.  Stop.", prereq, target));
                }
            }
        }

        let needs_rebuild = if self.always_make || is_phony {
            true
        } else {
            self.needs_rebuild(target, &prereqs, any_rebuilt)
        };

        if !needs_rebuild {
            return Ok(false);
        }

        if self.question {
            self.question_out_of_date = true;
            return Ok(true);
        }

        let auto_vars = self.make_auto_vars(target, &prereqs, &[], stem);

        self.execute_recipe(target, &rule.recipe, &auto_vars, is_phony)
    }

    fn find_pattern_rule(&self, target: &str) -> Option<(Rule, String)> {
        for rule in &self.db.pattern_rules {
            if rule.recipe.is_empty() && !rule.is_terminal {
                continue; // Skip pattern rules with no recipe (canceling rules)
            }
            for pattern_target in &rule.targets {
                if let Some(stem) = match_pattern(pattern_target, target) {
                    // Check if prerequisites can be satisfied
                    let prereqs_ok = rule.prerequisites.iter().all(|p| {
                        let resolved = p.replace('%', &stem);
                        Path::new(&resolved).exists()
                            || self.db.rules.contains_key(&resolved)
                            || self.find_pattern_rule_exists(&resolved)
                            || self.find_in_vpath(&resolved).is_some()
                    });
                    if prereqs_ok {
                        return Some((rule.clone(), stem));
                    }
                }
            }
        }
        None
    }

    fn find_pattern_rule_exists(&self, target: &str) -> bool {
        for rule in &self.db.pattern_rules {
            if rule.recipe.is_empty() { continue; }
            for pattern_target in &rule.targets {
                if let Some(stem) = match_pattern(pattern_target, target) {
                    let prereqs_ok = rule.prerequisites.iter().all(|p| {
                        let resolved = p.replace('%', &stem);
                        Path::new(&resolved).exists()
                    });
                    if prereqs_ok {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn find_in_vpath(&self, target: &str) -> Option<String> {
        // Search VPATH and vpath for the target
        for (pattern, dirs) in &self.db.vpath {
            if vpath_pattern_matches(pattern, target) {
                for dir in dirs {
                    let candidate = dir.join(target);
                    if candidate.exists() {
                        return Some(candidate.to_string_lossy().to_string());
                    }
                }
            }
        }
        for dir in &self.db.vpath_general {
            let candidate = dir.join(target);
            if candidate.exists() {
                return Some(candidate.to_string_lossy().to_string());
            }
        }

        // Also check VPATH variable
        if let Some(var) = self.db.variables.get("VPATH") {
            for dir in var.value.split(':') {
                let dir = dir.trim();
                if !dir.is_empty() {
                    let candidate = Path::new(dir).join(target);
                    if candidate.exists() {
                        return Some(candidate.to_string_lossy().to_string());
                    }
                }
            }
        }

        None
    }

    fn needs_rebuild(&self, target: &str, prereqs: &[String], any_prereq_rebuilt: bool) -> bool {
        if any_prereq_rebuilt {
            return true;
        }

        let target_time = match get_mtime(target) {
            Some(t) => t,
            None => return true, // Target doesn't exist
        };

        for prereq in prereqs {
            let prereq_time = match get_mtime(prereq) {
                Some(t) => t,
                None => {
                    // Check VPATH
                    if let Some(found) = self.find_in_vpath(prereq) {
                        match get_mtime(&found) {
                            Some(t) => t,
                            None => continue,
                        }
                    } else if self.db.is_phony(prereq) {
                        return true; // Phony prereqs always trigger rebuild
                    } else {
                        continue;
                    }
                }
            };

            if prereq_time > target_time {
                return true;
            }
        }

        false
    }

    fn make_auto_vars(&self, target: &str, prereqs: &[String], order_only: &[&str], stem: &str) -> HashMap<String, String> {
        let mut vars = HashMap::new();

        // $@ - target
        vars.insert("@".to_string(), target.to_string());

        // $< - first prerequisite
        let first_prereq = prereqs.first().cloned().unwrap_or_default();
        // Check VPATH for first prereq
        let first_prereq_resolved = if Path::new(&first_prereq).exists() {
            first_prereq.clone()
        } else if let Some(found) = self.find_in_vpath(&first_prereq) {
            found
        } else {
            first_prereq.clone()
        };
        vars.insert("<".to_string(), first_prereq_resolved);

        // $^ - all prerequisites (no duplicates)
        let mut seen = HashSet::new();
        let unique_prereqs: Vec<String> = prereqs.iter()
            .filter(|p| seen.insert(p.to_string()))
            .map(|p| {
                if Path::new(p).exists() {
                    p.clone()
                } else if let Some(found) = self.find_in_vpath(p) {
                    found
                } else {
                    p.clone()
                }
            })
            .collect();
        vars.insert("^".to_string(), unique_prereqs.join(" "));

        // $+ - all prerequisites (with duplicates)
        let all_prereqs: Vec<String> = prereqs.iter()
            .map(|p| {
                if Path::new(p).exists() {
                    p.clone()
                } else if let Some(found) = self.find_in_vpath(p) {
                    found
                } else {
                    p.clone()
                }
            })
            .collect();
        vars.insert("+".to_string(), all_prereqs.join(" "));

        // $? - prerequisites newer than target
        let target_time = get_mtime(target);
        let newer: Vec<String> = prereqs.iter()
            .filter(|p| {
                if let Some(tt) = target_time {
                    get_mtime(p).map_or(true, |pt| pt > tt)
                } else {
                    true
                }
            })
            .cloned()
            .collect();
        vars.insert("?".to_string(), newer.join(" "));

        // $* - stem (for pattern rules)
        vars.insert("*".to_string(), stem.to_string());

        // $(@D) $(@F) etc - directory and file parts
        vars.insert("@D".to_string(), dir_of(target));
        vars.insert("@F".to_string(), file_of(target));
        vars.insert("<D".to_string(), dir_of(&first_prereq));
        vars.insert("<F".to_string(), file_of(&first_prereq));
        vars.insert("*D".to_string(), dir_of(stem));
        vars.insert("*F".to_string(), file_of(stem));

        vars
    }

    fn execute_recipe(&mut self, target: &str, recipe: &[String], auto_vars: &HashMap<String, String>, _is_phony: bool) -> Result<bool, String> {
        if self.touch {
            // Just touch the target
            if !self.silent {
                println!("touch {}", target);
            }
            if !self.dry_run {
                touch_file(target);
            }
            return Ok(true);
        }

        let one_shell = self.db.one_shell;
        let is_silent_target = self.db.is_silent_target(target);

        if one_shell {
            // Execute all recipe lines as one shell script
            let mut script = String::new();
            for line in recipe {
                let expanded = self.state.expand_with_auto_vars(line, auto_vars);
                script.push_str(&expanded);
                script.push('\n');
            }

            if !self.silent && !is_silent_target {
                // Print the script (or first line)
                for line in recipe {
                    let expanded = self.state.expand_with_auto_vars(line, auto_vars);
                    let (display, _, _) = parse_recipe_prefix(&expanded);
                    if !display.is_empty() {
                        println!("{}", display);
                    }
                }
            }

            if !self.dry_run {
                let status = Command::new(self.shell)
                    .arg(self.shell_flags)
                    .arg(&script)
                    .env("MAKELEVEL", self.get_makelevel())
                    .status();

                match status {
                    Ok(s) if !s.success() => {
                        let code = s.code().unwrap_or(1);
                        if !self.ignore_errors {
                            let msg = format!("[{}] Error {} (ignored)", target, code);
                            if !self.ignore_errors {
                                return Err(format!("recipe for target '{}' failed", target));
                            }
                            eprintln!("jmake: {}", msg);
                        }
                    }
                    Err(e) => {
                        return Err(format!("Error running shell: {}", e));
                    }
                    _ => {}
                }
            }

            return Ok(true);
        }

        // Execute each recipe line separately
        for line in recipe {
            let expanded = self.state.expand_with_auto_vars(line, auto_vars);
            let (display_line, silent, ignore_error) = parse_recipe_prefix(&expanded);

            let effective_silent = silent || self.silent || is_silent_target;
            let effective_ignore = ignore_error || self.ignore_errors;

            if !effective_silent && !self.dry_run {
                println!("{}", display_line);
            } else if !effective_silent {
                println!("{}", display_line);
            }

            if self.dry_run {
                // In dry-run, still execute lines starting with +
                if !expanded.trim_start().starts_with('+') {
                    continue;
                }
            }

            // Get the actual command (strip @, -, + prefixes)
            let cmd = strip_recipe_prefixes(&expanded);

            if cmd.trim().is_empty() {
                continue;
            }

            let mut child = Command::new(self.shell);
            child.arg(self.shell_flags).arg(&cmd);
            child.env("MAKELEVEL", self.get_makelevel());

            // Set up exported variables
            self.setup_exports(&mut child);

            let status = child.status();

            match status {
                Ok(s) if !s.success() => {
                    let code = s.code().unwrap_or(1);
                    if effective_ignore {
                        eprintln!("jmake: [{}] Error {} (ignored)", target, code);
                    } else {
                        eprintln!("jmake: *** [{}] Error {}", target, code);
                        if !self.db.is_precious(target) && !self.db.is_phony(target) {
                            // Delete target on error unless .PRECIOUS
                            if Path::new(target).exists() {
                                eprintln!("jmake: Deleting file '{}'", target);
                                let _ = fs::remove_file(target);
                            }
                        }
                        return Err(format!("recipe for target '{}' failed", target));
                    }
                }
                Err(e) => {
                    if effective_ignore {
                        eprintln!("jmake: [{}] Error: {} (ignored)", target, e);
                    } else {
                        return Err(format!("Error executing recipe for '{}': {}", target, e));
                    }
                }
                _ => {}
            }
        }

        Ok(true)
    }

    fn setup_exports(&self, cmd: &mut Command) {
        for (name, var) in &self.db.variables {
            let should_export = match var.export {
                Some(true) => true,
                Some(false) => false,
                None => self.db.export_all && var.origin != VarOrigin::Default,
            };
            if should_export {
                let value = self.state.expand(&var.value);
                cmd.env(name, &value);
            }
        }
    }

    fn get_makelevel(&self) -> String {
        let level: u32 = self.state.db.variables.get("MAKELEVEL")
            .and_then(|v| v.value.parse().ok())
            .unwrap_or(0);
        (level + 1).to_string()
    }
}

fn match_pattern(pattern: &str, target: &str) -> Option<String> {
    if let Some(percent_pos) = pattern.find('%') {
        let prefix = &pattern[..percent_pos];
        let suffix = &pattern[percent_pos+1..];

        if target.starts_with(prefix) && target.ends_with(suffix) && target.len() >= prefix.len() + suffix.len() {
            let stem = &target[prefix.len()..target.len()-suffix.len()];
            return Some(stem.to_string());
        }
    } else if pattern == target {
        return Some(String::new());
    }
    None
}

fn vpath_pattern_matches(pattern: &str, target: &str) -> bool {
    // VPATH patterns use % as wildcard
    if let Some(percent_pos) = pattern.find('%') {
        let prefix = &pattern[..percent_pos];
        let suffix = &pattern[percent_pos+1..];
        target.starts_with(prefix) && target.ends_with(suffix)
    } else {
        pattern == target
    }
}

fn get_mtime(path: &str) -> Option<SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

fn touch_file(path: &str) {
    if Path::new(path).exists() {
        let _ = filetime::set_file_mtime(path, filetime::FileTime::now());
    } else {
        let _ = fs::File::create(path);
    }
}

fn parse_recipe_prefix(line: &str) -> (String, bool, bool) {
    let mut silent = false;
    let mut ignore = false;
    let mut i = 0;
    let bytes = line.as_bytes();

    while i < bytes.len() {
        match bytes[i] {
            b'@' => silent = true,
            b'-' => ignore = true,
            b'+' => {} // force execution even in dry-run
            b' ' | b'\t' => {}
            _ => break,
        }
        i += 1;
    }

    let cmd = &line[i..];
    (cmd.to_string(), silent, ignore)
}

fn strip_recipe_prefixes(line: &str) -> String {
    let mut i = 0;
    let bytes = line.as_bytes();

    while i < bytes.len() {
        match bytes[i] {
            b'@' | b'-' | b'+' | b' ' | b'\t' => i += 1,
            _ => break,
        }
    }

    line[i..].to_string()
}

fn dir_of(path: &str) -> String {
    match path.rfind('/') {
        Some(pos) => path[..=pos].to_string(),
        None => "./".to_string(),
    }
}

fn file_of(path: &str) -> String {
    match path.rfind('/') {
        Some(pos) => path[pos+1..].to_string(),
        None => path.to_string(),
    }
}

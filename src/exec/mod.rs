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
    progname: String,
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
        progname: String,
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
            progname,
        }
    }

    pub fn build_targets(&mut self, targets: &[String]) -> Result<(), String> {
        for target in targets {
            match self.build_target(target) {
                Ok(rebuilt) => {
                    // Print status for top-level targets that weren't rebuilt
                    if !rebuilt && !self.silent && !self.question {
                        let has_recipe = self.target_has_recipe(target);
                        if has_recipe {
                            println!("{}: '{}' is up to date.", self.progname, target);
                        } else {
                            println!("{}: Nothing to be done for '{}'.", self.progname, target);
                        }
                    }
                }
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
            eprintln!("{}: Circular {} <- {} dependency dropped.", self.progname, target, target);
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
                return self.execute_recipe(target, &default_rule.recipe, &default_rule.source_file, &auto_vars, false);
            }
        }

        Err(format!("No rule to make target '{}'.  Stop.", target))
    }

    /// Perform second expansion of a raw prerequisite text string.
    /// Returns the list of prerequisite names after expansion and splitting.
    /// `base_auto_vars` contains the automatic variables computed from the
    /// non-SE prerequisites ($@, $<, $^, $+, $*, $|).
    fn second_expand_prereqs(
        &self,
        raw_text: &str,
        base_auto_vars: &HashMap<String, String>,
        target: &str,
    ) -> Vec<String> {
        // Expand the raw text using auto vars (second expansion).
        let expanded = self.state.expand_with_auto_vars(raw_text, base_auto_vars);
        // Split the expanded result by whitespace.
        expanded.split_whitespace()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect()
    }

    fn build_with_rules(&mut self, target: &str, rules: &[Rule], is_phony: bool) -> Result<bool, String> {
        // Double-colon rules are treated as independent rules: each is evaluated
        // and executed separately if its prerequisites are out of date.
        if rules.first().map_or(false, |r| r.is_double_colon) {
            return self.build_with_double_colon_rules(target, rules, is_phony);
        }

        // Collect prerequisites and second-expansion texts from all rules.
        //
        // rule.prerequisites: already-expanded (first-pass) prereqs.  For non-SE rules
        //   these are plain target names; for SE rules with second_expansion_prereqs=Some,
        //   this field was cleared (so it's empty) because the actual prereqs come from
        //   second expansion at build time.
        //
        // rule.second_expansion_prereqs: raw text (post-$$ → $ first expansion) for rules
        //   that have deferred ('$$'-escaped) prerequisites.  Expanded at build time with
        //   auto vars ($@, $<, $^, etc.) computed from non-SE prereqs.

        let mut all_prereqs: Vec<String> = Vec::new();  // non-SE prereqs (direct build + auto vars)
        let mut all_order_only: Vec<String> = Vec::new();
        let mut se_prereq_texts: Vec<String> = Vec::new();
        let mut se_order_only_texts: Vec<String> = Vec::new();
        let mut recipe = Vec::new();
        let mut recipe_source_file = String::new();

        for rule in rules {
            all_prereqs.extend(rule.prerequisites.clone());
            all_order_only.extend(rule.order_only_prerequisites.clone());
            if let Some(ref text) = rule.second_expansion_prereqs {
                se_prereq_texts.push(text.clone());
            }
            if let Some(ref text) = rule.second_expansion_order_only {
                se_order_only_texts.push(text.clone());
            }
            if !rule.recipe.is_empty() {
                recipe = rule.recipe.clone();
                recipe_source_file = rule.source_file.clone();
            }
        }

        // Build the auto-var set from the non-SE prereqs collected above.
        // This is used to compute $<, $^, $+ for second expansion.
        // (all_prereqs at this point contains ONLY the already-expanded non-SE prereqs.)
        let auto_var_prereqs: Vec<String> = all_prereqs.clone();
        let auto_var_order_only: Vec<String> = all_order_only.clone();

        // Perform second expansion if there are SE texts.
        let mut se_expanded_prereqs: Vec<String> = Vec::new();
        let mut se_expanded_order_only: Vec<String> = Vec::new();

        if !se_prereq_texts.is_empty() || !se_order_only_texts.is_empty() {
            let stem = rules.iter()
                .find(|r| !r.static_stem.is_empty())
                .map(|r| r.static_stem.clone())
                .unwrap_or_default();
            let oo_refs: Vec<&str> = auto_var_order_only.iter().map(|s| s.as_str()).collect();
            let base_auto_vars = self.make_auto_vars(target, &auto_var_prereqs, &oo_refs, &stem);

            for text in &se_prereq_texts {
                let expanded = self.second_expand_prereqs(text, &base_auto_vars, target);
                for p in expanded {
                    if !p.is_empty() {
                        se_expanded_prereqs.push(p);
                    }
                }
            }
            for text in &se_order_only_texts {
                let expanded = self.second_expand_prereqs(text, &base_auto_vars, target);
                for p in expanded {
                    if !p.is_empty() {
                        se_expanded_order_only.push(p);
                    }
                }
            }
        }

        // Add SE expansion results to the build prerequisite lists.
        all_prereqs.extend(se_expanded_prereqs);
        all_order_only.extend(se_expanded_order_only);

        // Build prerequisites
        let mut any_prereq_rebuilt = false;
        let mut prereq_errors = Vec::new();

        for prereq in all_prereqs.clone() {
            match self.build_target(&prereq) {
                Ok(rebuilt) => {
                    if rebuilt { any_prereq_rebuilt = true; }
                }
                Err(e) => {
                    if self.keep_going {
                        prereq_errors.push(e);
                    } else {
                        return Err(format!("No rule to make target '{}', needed by '{}'.  Stop.", prereq, target));
                    }
                }
            }
        }

        // Build order-only prerequisites
        for prereq in all_order_only.clone() {
            let _ = self.build_target(&prereq);
        }

        if !prereq_errors.is_empty() {
            return Err(prereq_errors.join("\n"));
        }

        // If there is no recipe, try to find a matching pattern rule whose recipe
        // can be used. GNU Make allows explicit rules with no recipe to supply
        // prerequisites while a pattern rule provides the recipe.
        let (recipe, recipe_source_file, pattern_stem) = if recipe.is_empty() {
            if let Some((pattern_rule, stem)) = self.find_pattern_rule(target) {
                // Add the pattern rule's prerequisites/order-only to our lists and build them.
                let pat_prereqs: Vec<String> = pattern_rule.prerequisites.iter()
                    .map(|p| p.replace('%', &stem))
                    .collect();
                let pat_order_only: Vec<String> = pattern_rule.order_only_prerequisites.iter()
                    .map(|p| p.replace('%', &stem))
                    .collect();

                for prereq in &pat_prereqs {
                    if !all_prereqs.contains(prereq) {
                        match self.build_target(prereq) {
                            Ok(rebuilt) => { if rebuilt { any_prereq_rebuilt = true; } }
                            Err(e) => {
                                if self.keep_going {
                                    // continue
                                } else {
                                    return Err(format!("No rule to make target '{}', needed by '{}'.  Stop.", prereq, target));
                                }
                            }
                        }
                        all_prereqs.push(prereq.clone());
                    }
                }
                for prereq in &pat_order_only {
                    if !all_order_only.contains(prereq) {
                        let _ = self.build_target(prereq);
                        all_order_only.push(prereq.clone());
                    }
                }

                (pattern_rule.recipe.clone(), pattern_rule.source_file.clone(), stem)
            } else {
                // No recipe and no matching pattern rule: nothing to execute.
                return Ok(any_prereq_rebuilt);
            }
        } else {
            // Use static stem from explicit rule if available.
            let stem = rules.iter()
                .find(|r| !r.static_stem.is_empty())
                .map(|r| r.static_stem.clone())
                .unwrap_or_default();
            (recipe, recipe_source_file, stem)
        };

        // Determine if we need to rebuild
        let needs_rebuild = if self.always_make || is_phony {
            true
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

        // Set up automatic variables.
        let oo_refs: Vec<&str> = all_order_only.iter().map(|s| s.as_str()).collect();
        let mut auto_vars = self.make_auto_vars(target, &all_prereqs, &oo_refs, &pattern_stem);

        // Merge target-specific and pattern-specific variables into auto_vars,
        // respecting command-line variable priority and override semantics.
        let collected_target_vars = self.collect_target_vars(target);
        self.apply_target_vars_to_auto_vars(&collected_target_vars, &mut auto_vars);

        self.execute_recipe(target, &recipe, &recipe_source_file, &auto_vars, is_phony)
    }

    fn build_with_double_colon_rules(&mut self, target: &str, rules: &[Rule], is_phony: bool) -> Result<bool, String> {
        // Each double-colon rule is an independent rule. Build its prerequisites
        // independently and run its recipe if needed.
        let mut any_rebuilt = false;

        for rule in rules {
            let rule = rule.clone();
            let prereqs = rule.prerequisites.clone();
            let order_only = rule.order_only_prerequisites.clone();

            // Build this rule's prerequisites
            let mut any_prereq_rebuilt = false;
            let mut prereq_errors = Vec::new();

            for prereq in &prereqs {
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

            for prereq in &order_only {
                let expanded = self.state.expand(prereq);
                for p in expanded.split_whitespace() {
                    let _ = self.build_target(p);
                }
            }

            if !prereq_errors.is_empty() {
                return Err(prereq_errors.join("\n"));
            }

            // A double-colon rule with no recipe but with prerequisites: used only to
            // add prerequisites; no rebuild action.
            if rule.recipe.is_empty() {
                continue;
            }

            // Determine if this rule needs to run: a double-colon rule with no
            // prerequisites always runs (like a phony target). With prerequisites,
            // it runs only if those are out of date.
            let needs_rebuild = if self.always_make || is_phony {
                true
            } else if prereqs.is_empty() {
                // No prerequisites: always run (GNU Make behaviour for :: rules)
                true
            } else {
                self.needs_rebuild(target, &prereqs, any_prereq_rebuilt)
            };

            if !needs_rebuild {
                continue;
            }

            if self.question {
                self.question_out_of_date = true;
                return Ok(true);
            }

            let oo_refs: Vec<&str> = order_only.iter().map(|s| s.as_str()).collect();
            let stem = if rule.static_stem.is_empty() { "" } else { &rule.static_stem };
            let mut auto_vars = self.make_auto_vars(target, &prereqs, &oo_refs, stem);

            // Apply target-specific and pattern-specific variables
            let collected_target_vars = self.collect_target_vars(target);
            self.apply_target_vars_to_auto_vars(&collected_target_vars, &mut auto_vars);

            match self.execute_recipe(target, &rule.recipe, &rule.source_file, &auto_vars, is_phony) {
                Ok(_) => { any_rebuilt = true; }
                Err(e) => {
                    if self.keep_going {
                        // continue to next double-colon rule
                    } else {
                        return Err(e);
                    }
                }
            }
        }

        Ok(any_rebuilt)
    }

    fn build_with_pattern_rule(&mut self, target: &str, rule: &Rule, stem: &str, is_phony: bool) -> Result<bool, String> {
        // Expand pattern prerequisites using the stem.
        // For normal (non-SE) prerequisites, substitute % with the stem.
        let mut prereqs: Vec<String> = rule.prerequisites.iter()
            .map(|p| p.replace('%', stem))
            .collect();

        // Also expand any explicit prerequisites that came from `build_target_inner`
        // combining explicit rules with this pattern rule.
        // (Already handled via all_prereqs in build_target_inner for explicit rules.)

        // Handle second-expansion prerequisites for pattern rules.
        let mut order_only: Vec<String> = rule.order_only_prerequisites.iter()
            .map(|p| p.replace('%', stem))
            .collect();

        if rule.second_expansion_prereqs.is_some() || rule.second_expansion_order_only.is_some() {
            // Build base auto vars from normal prereqs (before SE)
            let oo_refs: Vec<&str> = order_only.iter().map(|s| s.as_str()).collect();
            let base_auto_vars = self.make_auto_vars(target, &prereqs, &oo_refs, stem);

            if let Some(ref text) = rule.second_expansion_prereqs {
                // Substitute % in the raw text for the stem before expanding
                let stem_subst = text.replace('%', stem);
                let expanded_prereqs = self.second_expand_prereqs(&stem_subst, &base_auto_vars, target);
                for p in expanded_prereqs {
                    if !p.is_empty() {
                        prereqs.push(p);
                    }
                }
            }
            if let Some(ref text) = rule.second_expansion_order_only {
                let stem_subst = text.replace('%', stem);
                let expanded_oo = self.second_expand_prereqs(&stem_subst, &base_auto_vars, target);
                for p in expanded_oo {
                    if !p.is_empty() {
                        order_only.push(p);
                    }
                }
            }
        }

        // Build prerequisites
        let mut any_rebuilt = false;
        for prereq in prereqs.clone() {
            match self.build_target(&prereq) {
                Ok(rebuilt) => {
                    if rebuilt { any_rebuilt = true; }
                }
                Err(_e) => {
                    // If prerequisite can't be built, this pattern rule doesn't apply
                    // Try next pattern rule
                    return Err(format!("No rule to make target '{}', needed by '{}'.  Stop.", prereq, target));
                }
            }
        }

        for prereq in order_only.clone() {
            let _ = self.build_target(&prereq);
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

        let oo_refs: Vec<&str> = order_only.iter().map(|s| s.as_str()).collect();
        let mut auto_vars = self.make_auto_vars(target, &prereqs, &oo_refs, stem);

        // Apply target-specific and pattern-specific variables
        let collected_target_vars = self.collect_target_vars(target);
        self.apply_target_vars_to_auto_vars(&collected_target_vars, &mut auto_vars);

        self.execute_recipe(target, &rule.recipe, &rule.source_file, &auto_vars, is_phony)
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

    /// Collect all applicable target-specific and pattern-specific variables for a target.
    /// Returns a map of variable name → (value, is_override).
    /// Pattern-specific variables are matched with shortest-stem semantics.
    fn collect_target_vars(&self, target: &str) -> HashMap<String, (String, bool)> {
        let mut result: HashMap<String, (String, bool)> = HashMap::new();

        // 1. Apply pattern-specific variables.
        //    Build list of (stem_len, declaration_index, entry) for all matching patterns.
        let mut pattern_vars_with_stem: Vec<(usize, usize, &PatternSpecificVar)> = Vec::new();
        for (idx, psv) in self.db.pattern_specific_vars.iter().enumerate() {
            if let Some(stem) = match_pattern_simple(&psv.pattern, target) {
                pattern_vars_with_stem.push((stem.len(), idx, psv));
            }
        }
        // Sort: descending stem length (less-specific first), then ascending index.
        // This way shorter-stem (more-specific) patterns overwrite longer-stem ones,
        // and among same stem length, earlier declarations win (applied last).
        pattern_vars_with_stem.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));

        for (_, _, psv) in &pattern_vars_with_stem {
            let expanded = self.state.expand(&psv.var.value);
            let val = match psv.var.flavor {
                VarFlavor::Append => {
                    let base = result.get(&psv.var_name)
                        .map(|(v, _)| v.clone())
                        .or_else(|| self.db.variables.get(&psv.var_name)
                            .map(|v| self.state.expand(&v.value)))
                        .unwrap_or_default();
                    if base.is_empty() { expanded } else { format!("{} {}", base, expanded) }
                }
                VarFlavor::Conditional => {
                    if result.contains_key(&psv.var_name) { continue; }
                    expanded
                }
                _ => expanded,
            };
            result.insert(psv.var_name.clone(), (val, psv.is_override));
        }

        // 2. Apply target-specific variables (they override pattern-specific).
        if let Some(rules) = self.db.rules.get(target) {
            for rule in rules {
                for (var_name, var) in &rule.target_specific_vars {
                    let is_override = var.origin == VarOrigin::Override;
                    let expanded = self.state.expand(&var.value);
                    let val = match var.flavor {
                        VarFlavor::Append => {
                            let base = result.get(var_name)
                                .map(|(v, _)| v.clone())
                                .or_else(|| self.db.variables.get(var_name)
                                    .map(|v| self.state.expand(&v.value)))
                                .unwrap_or_default();
                            if base.is_empty() { expanded } else { format!("{} {}", base, expanded) }
                        }
                        VarFlavor::Conditional => {
                            if result.contains_key(var_name) { continue; }
                            expanded
                        }
                        _ => expanded,
                    };
                    result.insert(var_name.clone(), (val, is_override));
                }
            }
        }

        result
    }

    /// Apply collected target vars to auto_vars, respecting command-line variable priority.
    fn apply_target_vars_to_auto_vars(
        &self,
        target_vars: &HashMap<String, (String, bool)>,
        auto_vars: &mut HashMap<String, String>,
    ) {
        for (var_name, (value, is_override)) in target_vars {
            // Non-override target-specific vars don't override command-line vars
            let is_cmdline = self.db.variables.get(var_name.as_str())
                .map_or(false, |v| v.origin == VarOrigin::CommandLine);
            if is_cmdline && !is_override {
                continue;
            }
            auto_vars.insert(var_name.clone(), value.clone());
        }
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

        // $| - order-only prerequisites
        let oo_list: Vec<String> = order_only.iter().map(|s| s.to_string()).collect();
        vars.insert("|".to_string(), oo_list.join(" "));

        // $(@D) $(@F) etc - directory and file parts
        vars.insert("@D".to_string(), dir_of(target));
        vars.insert("@F".to_string(), file_of(target));
        vars.insert("<D".to_string(), dir_of(&first_prereq));
        vars.insert("<F".to_string(), file_of(&first_prereq));
        vars.insert("*D".to_string(), dir_of(stem));
        vars.insert("*F".to_string(), file_of(stem));

        vars
    }

    fn execute_recipe(&mut self, target: &str, recipe: &[(usize, String)], source_file: &str, auto_vars: &HashMap<String, String>, _is_phony: bool) -> Result<bool, String> {
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
            // Execute all recipe lines as one shell script.
            // Strip Make recipe prefix chars (@, -, +) before adding to the script.
            let mut script = String::new();
            for (_lineno, line) in recipe {
                let expanded = self.state.expand_with_auto_vars(line, auto_vars);
                let cmd_line = strip_recipe_prefixes(&expanded);
                script.push_str(&cmd_line);
                script.push('\n');
            }

            if !self.silent && !is_silent_target {
                // Print each recipe line (respecting @ prefix)
                for (_lineno, line) in recipe {
                    let expanded = self.state.expand_with_auto_vars(line, auto_vars);
                    let (display, line_silent, _, _) = parse_recipe_prefix(&expanded);
                    if !line_silent && !display.is_empty() {
                        println!("{}", display);
                    }
                }
            }

            if !self.dry_run {
                let mut child = Command::new(self.shell);
                child.arg(self.shell_flags).arg(&script);
                child.env("MAKELEVEL", self.get_makelevel());
                self.setup_exports(&mut child);
                let status = child.status();

                match status {
                    Ok(s) if !s.success() => {
                        let code = s.code().unwrap_or(1);
                        if self.ignore_errors {
                            let loc = make_location(source_file, 0);
                            eprintln!("{}: [{}{}] Error {} (ignored)", self.progname, loc, target, code);
                        } else {
                            let loc = make_location(source_file, 0);
                            eprintln!("{}: *** [{}{}] Error {}", self.progname, loc, target, code);
                            return Err(String::new());
                        }
                    }
                    Err(e) => {
                        eprintln!("{}: *** Error running shell: {}", self.progname, e);
                        return Err(String::new());
                    }
                    _ => {}
                }
            }

            return Ok(true);
        }

        // Execute each recipe line separately
        for (lineno, line) in recipe {
            let expanded = self.state.expand_with_auto_vars(line, auto_vars);
            let (display_line, line_silent, ignore_error, force) = parse_recipe_prefix(&expanded);

            let effective_silent = line_silent || self.silent || is_silent_target;
            let effective_ignore = ignore_error || self.ignore_errors;

            // Echo the command BEFORE executing it (unless silenced)
            if !effective_silent {
                println!("{}", display_line);
            }

            if self.dry_run {
                // In dry-run mode, lines are printed but not executed, EXCEPT:
                // 1. Lines with '+' prefix (force execution)
                // 2. Lines that contain $(MAKE) or ${MAKE} - recursive make invocations
                let contains_make_var = line.contains("$(MAKE)") || line.contains("${MAKE}");
                if !force && !contains_make_var {
                    continue;
                }
            }

            // Get the actual command (strip @, -, + prefixes - none of them go to the shell)
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
                    let loc = make_location(source_file, *lineno);
                    if effective_ignore {
                        eprintln!("{}: [{}{}] Error {} (ignored)", self.progname, loc, target, code);
                    } else {
                        eprintln!("{}: *** [{}{}] Error {}", self.progname, loc, target, code);
                        if !self.db.is_precious(target) && !self.db.is_phony(target) {
                            // Delete target on error unless .PRECIOUS
                            if Path::new(target).exists() {
                                eprintln!("{}: Deleting file '{}'", self.progname, target);
                                let _ = fs::remove_file(target);
                            }
                        }
                        return Err(String::new());
                    }
                }
                Err(e) => {
                    if effective_ignore {
                        let loc = make_location(source_file, *lineno);
                        eprintln!("{}: [{}{}] Error: {} (ignored)", self.progname, loc, target, e);
                    } else {
                        let loc = make_location(source_file, *lineno);
                        eprintln!("{}: *** [{}{}] Error: {}", self.progname, loc, target, e);
                        return Err(String::new());
                    }
                }
                _ => {}
            }
        }

        Ok(true)
    }

    fn target_has_recipe(&self, target: &str) -> bool {
        if let Some(rules) = self.db.rules.get(target) {
            for rule in rules {
                if !rule.recipe.is_empty() {
                    return true;
                }
            }
        }
        // Check pattern rules
        if let Some((rule, _)) = self.find_pattern_rule(target) {
            if !rule.recipe.is_empty() {
                return true;
            }
        }
        false
    }

    fn setup_exports(&self, cmd: &mut Command) {
        for (name, var) in &self.db.variables {
            // MAKELEVEL is handled separately (incremented), skip it here
            if name == "MAKELEVEL" {
                continue;
            }
            // These special make variables are always exported to sub-makes
            let always_export = matches!(name.as_str(), "MAKEFLAGS" | "MAKE" | "MAKECMDGOALS");
            // A variable originally imported from the process environment is always
            // re-exported to children (possibly with an overridden value from the
            // Makefile), unless explicitly unexported.
            let was_from_env = self.db.env_var_names.contains(name.as_str());
            let should_export = always_export || match var.export {
                Some(true) => true,
                Some(false) => false,
                None => self.db.export_all || was_from_env,
            };
            if should_export {
                let value = self.state.expand(&var.value);
                cmd.env(name, &value);
            } else {
                // Ensure the child does not see this variable from the inherited
                // environment. This covers:
                // - explicitly unexported variables (export = Some(false))
                // - file-defined variables that shadow an env var but aren't exported
                // - make-internal default variables that should not leak to children
                cmd.env_remove(name);
            }
        }
    }

    fn get_makelevel(&self) -> String {
        // The MAKELEVEL env var for sub-make processes is the current level + 1.
        // The current level is stored in the MAKELEVEL variable (0 for top-level).
        let level: u32 = self.state.db.variables.get("MAKELEVEL")
            .and_then(|v| v.value.parse().ok())
            .unwrap_or(0);
        (level + 1).to_string()
    }
}

/// Format a "file:line: " location prefix for error messages.
/// If source_file is empty or lineno is 0, returns an empty string.
fn make_location(source_file: &str, lineno: usize) -> String {
    if source_file.is_empty() {
        String::new()
    } else if lineno == 0 {
        format!("{}: ", source_file)
    } else {
        format!("{}:{}: ", source_file, lineno)
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

/// Match a pattern (containing `%`) against a target and return the stem, or None.
fn match_pattern_simple(pattern: &str, target: &str) -> Option<String> {
    match_pattern(pattern, target)
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

/// Parse the leading prefix characters of a recipe line.
///
/// Returns (display_line, silent, ignore_error, force) where:
///   display_line: the line with all prefixes (@, -, +) stripped (what gets echoed)
///   silent: true if `@` was present (suppresses echoing this line)
///   ignore_error: true if `-` was present (non-zero exit is ignored)
///   force: true if `+` was present (force execution even in dry-run)
fn parse_recipe_prefix(line: &str) -> (String, bool, bool, bool) {
    let mut silent = false;
    let mut ignore = false;
    let mut force = false;
    let bytes = line.as_bytes();
    let mut i = 0;

    // Scan prefix characters: @, -, + (and optional leading/interleaved whitespace)
    // GNU Make allows whitespace before and between recipe prefix characters.
    while i < bytes.len() {
        match bytes[i] {
            b'@' => {
                silent = true;
                i += 1;
            }
            b'-' => {
                ignore = true;
                i += 1;
            }
            b'+' => {
                force = true;
                i += 1;
            }
            b' ' | b'\t' => {
                // Allow whitespace at the start and between prefix chars,
                // but only continue scanning if we haven't yet seen any
                // non-whitespace non-prefix content (i.e., a prefix or more ws).
                i += 1;
            }
            _ => break,
        }
    }

    // display_line: everything after the prefix characters
    let display = line[i..].to_string();

    (display, silent, ignore, force)
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

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
    /// Set to true the first time any recipe is executed.
    /// Suppresses "is up to date" / "Nothing to be done" diagnostics.
    any_recipe_ran: bool,
    /// Intermediate targets that were actually built this run (candidates for deletion).
    /// Stored as Vec to maintain insertion order (for consistent rm output).
    intermediate_built: Vec<String>,
    /// Top-level targets (not subject to intermediate deletion even if .INTERMEDIATE).
    top_level_targets: HashSet<String>,
    /// Targets/files marked as "infinitely new" via -W/--what-if.
    what_if: Vec<String>,
    /// Stack of inherited target-specific variables from parent targets.
    /// When building a target's prerequisites, the parent's collected target vars
    /// are pushed here so that prereqs can inherit them.
    inherited_vars_stack: Vec<HashMap<String, (String, bool, bool)>>,
    /// Extra variables to export to child processes for the current target.
    /// Set before calling execute_recipe and cleared after.
    /// Used for target-specific and pattern-specific variables with export=Some(true).
    target_extra_exports: HashMap<String, String>,
    /// Variables to explicitly unexport for the current target (target-specific unexport).
    /// Set before calling execute_recipe and cleared after.
    target_extra_unexports: HashSet<String>,
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
        what_if: Vec<String>,
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
            any_recipe_ran: false,
            intermediate_built: Vec::new(),
            top_level_targets: HashSet::new(),
            what_if,
            inherited_vars_stack: Vec::new(),
            target_extra_exports: HashMap::new(),
            target_extra_unexports: HashSet::new(),
        }
    }

    pub fn build_targets(&mut self, targets: &[String]) -> Result<(), String> {
        // Record top-level targets so they are not deleted even if .INTERMEDIATE
        for t in targets {
            self.top_level_targets.insert(t.clone());
        }
        for target in targets {
            match self.build_target(target) {
                Ok(rebuilt) => {
                    // Print status for top-level targets that weren't rebuilt,
                    // but ONLY when no recipe ran anywhere in this make session.
                    // GNU Make suppresses these messages when any work was done
                    // (even for unrelated or order-only prerequisites).
                    if !rebuilt && !self.silent && !self.question && !self.any_recipe_ran {
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
                        // Clean up intermediate files even on error
                        self.delete_intermediate_files();
                        return Err(e);
                    }
                }
            }
        }

        // Delete intermediate files that were built during this run
        self.delete_intermediate_files();

        if !self.errors.is_empty() {
            return Err(format!("Target(s) not remade because of errors:\n{}",
                self.errors.join("\n")));
        }

        if self.question && self.question_out_of_date {
            std::process::exit(1);
        }

        Ok(())
    }

    /// Delete intermediate files that were built during this run.
    fn delete_intermediate_files(&mut self) {
        let to_delete: Vec<String> = self.intermediate_built.iter()
            .filter(|t| {
                !self.top_level_targets.contains(*t)
                    && !self.db.is_precious(t)
                    && !self.db.is_phony(t)
                    && !self.db.is_secondary(t)
                    && !self.db.is_notintermediate(t)
            })
            .cloned()
            .collect();
        // Collect files that actually exist and need to be deleted.
        let existing: Vec<String> = to_delete.iter()
            .filter(|t| Path::new(t.as_str()).exists())
            .cloned()
            .collect();
        // GNU Make prints ONE rm command with all files, in the order they were built.
        if !existing.is_empty() && !self.silent {
            println!("rm {}", existing.join(" "));
        }
        if !self.dry_run {
            for t in &existing {
                let _ = fs::remove_file(t);
            }
        }
        // Remove deleted (or non-existent) entries from intermediate_built
        for t in &to_delete {
            self.intermediate_built.retain(|x| x != t);
        }
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

        // Collect any grouped siblings for this target.
        // Grouped targets (&:) are built together: when any one is built, all
        // siblings are also built (each with their own SE expansion context).
        let grouped_siblings: Vec<String> = if let Some(ref rules) = rules {
            rules.iter()
                .flat_map(|r| r.grouped_siblings.iter().cloned())
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .filter(|s| !self.built.contains_key(s.as_str())
                         && !self.building.contains(s.as_str()))
                .collect()
        } else {
            Vec::new()
        };

        // If we have explicit rules
        if let Some(rules) = &rules {
            if !rules.is_empty() {
                // For grouped targets (&:): pass the siblings to build_with_rules so it
                // can build their prerequisites BEFORE running the recipe, and mark them
                // as built afterward. The recipe runs ONCE for the primary target only.
                let result = self.build_with_rules_grouped(target, rules, is_phony, &grouped_siblings);
                return result;
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

        // A phony target with no recipe and no file is still "successfully built" - it's
        // a no-op target. This allows .PHONY targets without recipes to be prerequisites.
        if is_phony {
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
                self.target_extra_exports = self.compute_target_exports(target);
                self.target_extra_unexports = self.compute_target_unexports(target);
                let result = self.execute_recipe(target, &default_rule.recipe, &default_rule.source_file, &auto_vars, false);
                self.target_extra_exports.clear();
                self.target_extra_unexports.clear();
                return result;
            }
        }

        Err(format!("No rule to make target '{}'.  Stop.", target))
    }

    /// Perform second expansion of a raw prerequisite text string.
    /// Returns `(normal_prereqs, order_only_prereqs)` after expansion and splitting.
    /// `|` in the expanded result separates normal prereqs from order-only prereqs.
    /// `.WAIT` markers are filtered from both lists.
    /// `base_auto_vars` contains the automatic variables computed from the
    /// non-SE prerequisites ($@, $<, $^, $+, $*, $|).
    fn second_expand_prereqs(
        &self,
        raw_text: &str,
        base_auto_vars: &HashMap<String, String>,
        target: &str,
    ) -> (Vec<String>, Vec<String>) {
        // Expand the raw text using auto vars (second expansion).
        // Set the in_second_expansion flag so that eval() in this context
        // can detect and reject attempts to create new rules.
        *self.state.in_second_expansion.borrow_mut() = true;
        let expanded = self.state.expand_with_auto_vars(raw_text, base_auto_vars);
        *self.state.in_second_expansion.borrow_mut() = false;
        // Split on whitespace, handle '|' as separator for order-only prereqs.
        let mut normal = Vec::new();
        let mut order_only = Vec::new();
        let mut is_order_only = false;
        for token in expanded.split_whitespace() {
            if token.is_empty() { continue; }
            if token == "|" { is_order_only = true; continue; }
            if token == ".WAIT" { continue; } // filter .WAIT markers
            if is_order_only {
                order_only.push(token.to_string());
            } else {
                normal.push(token.to_string());
            }
        }
        (normal, order_only)
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

        // Filter .WAIT markers early so they don't appear in auto vars ($^, $<, $+, $|).
        all_prereqs.retain(|p| p != ".WAIT");
        all_order_only.retain(|p| p != ".WAIT");

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
            let mut base_auto_vars = self.make_auto_vars(target, &auto_var_prereqs, &oo_refs, &stem);

            // Merge target-specific and pattern-specific variables into the SE
            // auto vars, so that e.g. `foo: a := bar; foo: $$a` can see `a`.
            let collected_target_vars = self.collect_target_vars(target);
            self.apply_target_vars_to_auto_vars(&collected_target_vars, &mut base_auto_vars);

            for text in &se_prereq_texts {
                let (normal, oo) = self.second_expand_prereqs(text, &base_auto_vars, target);
                se_expanded_prereqs.extend(normal);
                se_expanded_order_only.extend(oo);
            }
            for text in &se_order_only_texts {
                let (normal, oo) = self.second_expand_prereqs(text, &base_auto_vars, target);
                se_expanded_order_only.extend(normal);
                se_expanded_order_only.extend(oo);
            }
        }

        // Build prerequisites in the correct GNU Make order:
        //   1. Non-SE normal prereqs
        //   2. Non-SE order-only prereqs (built BEFORE SE-expanded prereqs)
        //   3. SE-expanded normal prereqs
        //   4. SE-expanded order-only prereqs
        //
        // This ensures non-SE OO prereqs are built before SE prereqs, matching
        // GNU Make's dependency resolution order.

        // Filter .WAIT markers from SE results
        se_expanded_prereqs.retain(|p| p != ".WAIT");
        se_expanded_order_only.retain(|p| p != ".WAIT");

        // Pre-check: if the target exists and none of the prereqs (by effective mtime, which
        // accounts for deleted intermediates) are newer than the target, skip rebuilding.
        // This handles the case where intermediate files were deleted after a previous build:
        // they should not cause an unnecessary rebuild if their sources are still old.
        // Only applicable for non-phony targets with no SE prereqs and no always-make.
        if !is_phony && !self.always_make && se_prereq_texts.is_empty() && se_order_only_texts.is_empty() {
            if let Some(target_time) = get_mtime(target).or_else(|| {
                self.find_in_vpath(target).and_then(|f| get_mtime(&f))
            }) {
                let any_prereq_newer = all_prereqs.iter().any(|p| {
                    if p == ".WAIT" { return false; }
                    if self.what_if.iter().any(|w| w == p) { return true; }
                    if self.db.is_phony(p) { return true; }
                    // Use effective_mtime to handle deleted intermediates
                    match self.effective_mtime(p, 0) {
                        Some(pt) => pt > target_time,
                        None => false, // Can't determine mtime → assume not newer
                    }
                });
                if !any_prereq_newer {
                    return Ok(false);
                }
            }
        }

        // Push this target's collected vars onto the inheritance stack.
        let my_target_vars = self.collect_target_vars(target);
        self.inherited_vars_stack.push(my_target_vars);

        let mut any_prereq_rebuilt = false;
        let mut prereq_errors = Vec::new();

        // Step 1: build non-SE normal prereqs
        for prereq in all_prereqs.clone() {
            match self.build_target(&prereq) {
                Ok(rebuilt) => {
                    if rebuilt { any_prereq_rebuilt = true; }
                }
                Err(e) => {
                    let propagated = if e.starts_with("No rule to make target '") && !e.contains(", needed by '") {
                        let base = e.trim_end_matches(".  Stop.").trim_end_matches(".");
                        format!("{}, needed by '{}'.  Stop.", base, target)
                    } else {
                        e
                    };
                    if self.keep_going {
                        prereq_errors.push(propagated);
                    } else {
                        self.inherited_vars_stack.pop();
                        return Err(propagated);
                    }
                }
            }
        }

        // Step 2: build non-SE order-only prereqs
        for prereq in all_order_only.clone() {
            let _ = self.build_target(&prereq);
        }

        // Step 3: build SE-expanded normal prereqs
        for prereq in se_expanded_prereqs.clone() {
            match self.build_target(&prereq) {
                Ok(rebuilt) => {
                    if rebuilt { any_prereq_rebuilt = true; }
                }
                Err(e) => {
                    let propagated = if e.starts_with("No rule to make target '") && !e.contains(", needed by '") {
                        let base = e.trim_end_matches(".  Stop.").trim_end_matches(".");
                        format!("{}, needed by '{}'.  Stop.", base, target)
                    } else {
                        e
                    };
                    if self.keep_going {
                        prereq_errors.push(propagated);
                    } else {
                        self.inherited_vars_stack.pop();
                        return Err(propagated);
                    }
                }
            }
        }

        // Step 4: build SE-expanded order-only prereqs
        for prereq in se_expanded_order_only.clone() {
            let _ = self.build_target(&prereq);
        }

        // Combine all prereqs for needs_rebuild and auto-var computation.
        all_prereqs.extend(se_expanded_prereqs);
        all_order_only.extend(se_expanded_order_only);
        all_prereqs.retain(|p| p != ".WAIT");
        all_order_only.retain(|p| p != ".WAIT");

        self.inherited_vars_stack.pop();

        if !prereq_errors.is_empty() {
            return Err(prereq_errors.join("\n"));
        }

        // If there is no recipe, try to find a matching pattern rule whose recipe
        // can be used. GNU Make allows explicit rules with no recipe to supply
        // prerequisites while a pattern rule provides the recipe.
        // Pass the already-accumulated explicit prereqs so they count as "ought to exist".
        let (recipe, recipe_source_file, pattern_stem) = if recipe.is_empty() {
            if let Some((pattern_rule, stem)) = self.find_pattern_rule_inner(target, &all_prereqs) {
                // Add the pattern rule's prerequisites/order-only to our lists and build them.
                let mut pat_prereqs: Vec<String> = pattern_rule.prerequisites.iter()
                    .map(|p| p.replace('%', &stem))
                    .collect();
                let mut pat_order_only: Vec<String> = pattern_rule.order_only_prerequisites.iter()
                    .map(|p| p.replace('%', &stem))
                    .collect();

                // Handle second expansion for the pattern rule.
                // Auto vars are built from the ALREADY-accumulated explicit prereqs
                // (all_prereqs at this point), giving $+ the value from the explicit
                // rule(s) - which is what GNU Make uses for $+ in SE pattern rules.
                if pattern_rule.second_expansion_prereqs.is_some() || pattern_rule.second_expansion_order_only.is_some() {
                    let oo_refs: Vec<&str> = all_order_only.iter().map(|s| s.as_str()).collect();
                    let mut pat_se_auto_vars = self.make_auto_vars(target, &all_prereqs, &oo_refs, &stem);
                    let collected_target_vars = self.collect_target_vars(target);
                    self.apply_target_vars_to_auto_vars(&collected_target_vars, &mut pat_se_auto_vars);

                    if let Some(ref text) = pattern_rule.second_expansion_prereqs {
                        let stem_subst = text.replace('%', &stem);
                        let (normal, oo) = self.second_expand_prereqs(&stem_subst, &pat_se_auto_vars, target);
                        pat_prereqs.extend(normal);
                        pat_order_only.extend(oo);
                    }
                    if let Some(ref text) = pattern_rule.second_expansion_order_only {
                        let stem_subst = text.replace('%', &stem);
                        let (normal, oo) = self.second_expand_prereqs(&stem_subst, &pat_se_auto_vars, target);
                        pat_order_only.extend(normal);
                        pat_order_only.extend(oo);
                    }
                }

                // Build each unique pattern-rule prereq once, but add ALL occurrences
                // (including duplicates) to all_prereqs so that $+ is computed correctly.
                // The pattern rule's prereqs are prepended so they come first in $^/$+.
                let mut already_built: std::collections::HashSet<String> = std::collections::HashSet::new();
                // Also collect which prereqs were already in all_prereqs before pattern rule.
                for p in &all_prereqs {
                    already_built.insert(p.clone());
                }
                // Prepend pattern rule prereqs: they come first (pattern rule is primary).
                let orig_explicit_prereqs = all_prereqs.clone();
                all_prereqs.clear();
                for prereq in &pat_prereqs {
                    if !already_built.contains(prereq) {
                        match self.build_target(prereq) {
                            Ok(rebuilt) => { if rebuilt { any_prereq_rebuilt = true; } }
                            Err(e) => {
                                let propagated = if e.starts_with("No rule to make target '") && !e.contains(", needed by '") {
                                    let base = e.trim_end_matches(".  Stop.").trim_end_matches(".");
                                    format!("{}, needed by '{}'.  Stop.", base, target)
                                } else {
                                    e
                                };
                                if self.keep_going {
                                    // continue
                                } else {
                                    return Err(propagated);
                                }
                            }
                        }
                        already_built.insert(prereq.clone());
                    }
                    all_prereqs.push(prereq.clone());
                }
                // Append original explicit rule prereqs after pattern rule prereqs.
                all_prereqs.extend(orig_explicit_prereqs);

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

        // Set up automatic variables.
        let oo_refs: Vec<&str> = all_order_only.iter().map(|s| s.as_str()).collect();
        let mut auto_vars = self.make_auto_vars(target, &all_prereqs, &oo_refs, &pattern_stem);

        // Merge target-specific and pattern-specific variables into auto_vars,
        // respecting command-line variable priority and override semantics.
        let collected_target_vars = self.collect_target_vars(target);
        self.apply_target_vars_to_auto_vars(&collected_target_vars, &mut auto_vars);

        if self.question {
            // In question mode: expand the recipe and run make-function side-effects,
            // but don't execute any shell commands. A target is considered "out of date"
            // only if real shell commands would run (not just make-functions like $(info)).
            let has_real_cmds = self.recipe_has_real_commands(&recipe, &auto_vars);
            if has_real_cmds {
                self.question_out_of_date = true;
            }
            return Ok(has_real_cmds);
        }

        self.target_extra_exports = self.compute_target_exports(target);
        self.target_extra_unexports = self.compute_target_unexports(target);
        let result = self.execute_recipe(target, &recipe, &recipe_source_file, &auto_vars, is_phony);
        self.target_extra_exports.clear();
        self.target_extra_unexports.clear();
        // Track if this is an intermediate target that was built.
        if let Ok(true) = &result {
            if self.db.is_intermediate(target) {
                if !self.intermediate_built.contains(&target.to_string()) {
                    self.intermediate_built.push(target.to_string());
                }
            }
        }
        result
    }

    /// Extract the prerequisite-building phase of `build_with_rules`, without running
    /// the recipe. Returns `(any_prereq_rebuilt, all_prereqs, all_order_only, recipe,
    /// recipe_source_file, pattern_stem)` so the caller can run the recipe itself.
    /// Used by `build_with_rules_grouped` to sequence prereq phases for grouped targets.
    fn build_with_rules_prereqs(
        &mut self,
        target: &str,
        rules: &[Rule],
        is_phony: bool,
    ) -> Result<(bool, Vec<String>, Vec<String>, Vec<(usize, String)>, String, String), String> {
        // Double-colon rules are not used for grouped targets, but handle gracefully
        // by delegating — in practice grouped targets use single-colon rules.

        let mut all_prereqs: Vec<String> = Vec::new();
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

        all_prereqs.retain(|p| p != ".WAIT");
        all_order_only.retain(|p| p != ".WAIT");

        let auto_var_prereqs: Vec<String> = all_prereqs.clone();
        let auto_var_order_only: Vec<String> = all_order_only.clone();

        let mut se_expanded_prereqs: Vec<String> = Vec::new();
        let mut se_expanded_order_only: Vec<String> = Vec::new();

        if !se_prereq_texts.is_empty() || !se_order_only_texts.is_empty() {
            let stem = rules.iter()
                .find(|r| !r.static_stem.is_empty())
                .map(|r| r.static_stem.clone())
                .unwrap_or_default();
            let oo_refs: Vec<&str> = auto_var_order_only.iter().map(|s| s.as_str()).collect();
            let mut base_auto_vars = self.make_auto_vars(target, &auto_var_prereqs, &oo_refs, &stem);
            let collected_target_vars = self.collect_target_vars(target);
            self.apply_target_vars_to_auto_vars(&collected_target_vars, &mut base_auto_vars);

            for text in &se_prereq_texts {
                let (normal, oo) = self.second_expand_prereqs(text, &base_auto_vars, target);
                se_expanded_prereqs.extend(normal);
                se_expanded_order_only.extend(oo);
            }
            for text in &se_order_only_texts {
                let (normal, oo) = self.second_expand_prereqs(text, &base_auto_vars, target);
                se_expanded_order_only.extend(normal);
                se_expanded_order_only.extend(oo);
            }
        }

        se_expanded_prereqs.retain(|p| p != ".WAIT");
        se_expanded_order_only.retain(|p| p != ".WAIT");

        let my_target_vars = self.collect_target_vars(target);
        self.inherited_vars_stack.push(my_target_vars);

        let mut any_prereq_rebuilt = false;
        let mut prereq_errors = Vec::new();

        for prereq in all_prereqs.clone() {
            match self.build_target(&prereq) {
                Ok(rebuilt) => {
                    if rebuilt { any_prereq_rebuilt = true; }
                }
                Err(e) => {
                    let propagated = if e.starts_with("No rule to make target '") && !e.contains(", needed by '") {
                        let base = e.trim_end_matches(".  Stop.").trim_end_matches(".");
                        format!("{}, needed by '{}'.  Stop.", base, target)
                    } else {
                        e
                    };
                    if self.keep_going {
                        prereq_errors.push(propagated);
                    } else {
                        self.inherited_vars_stack.pop();
                        return Err(propagated);
                    }
                }
            }
        }

        for prereq in all_order_only.clone() {
            let _ = self.build_target(&prereq);
        }

        for prereq in se_expanded_prereqs.clone() {
            match self.build_target(&prereq) {
                Ok(rebuilt) => {
                    if rebuilt { any_prereq_rebuilt = true; }
                }
                Err(e) => {
                    let propagated = if e.starts_with("No rule to make target '") && !e.contains(", needed by '") {
                        let base = e.trim_end_matches(".  Stop.").trim_end_matches(".");
                        format!("{}, needed by '{}'.  Stop.", base, target)
                    } else {
                        e
                    };
                    if self.keep_going {
                        prereq_errors.push(propagated);
                    } else {
                        self.inherited_vars_stack.pop();
                        return Err(propagated);
                    }
                }
            }
        }

        for prereq in se_expanded_order_only.clone() {
            let _ = self.build_target(&prereq);
        }

        all_prereqs.extend(se_expanded_prereqs);
        all_order_only.extend(se_expanded_order_only);
        all_prereqs.retain(|p| p != ".WAIT");
        all_order_only.retain(|p| p != ".WAIT");

        self.inherited_vars_stack.pop();

        if !prereq_errors.is_empty() {
            return Err(prereq_errors.join("\n"));
        }

        // If there is no recipe, try to find a matching pattern rule whose recipe
        // can be used (same logic as build_with_rules).
        let (recipe, recipe_source_file, pattern_stem) = if recipe.is_empty() {
            if let Some((pattern_rule, stem)) = self.find_pattern_rule_inner(target, &all_prereqs) {
                let mut pat_prereqs: Vec<String> = pattern_rule.prerequisites.iter()
                    .map(|p| p.replace('%', &stem))
                    .collect();
                let mut pat_order_only: Vec<String> = pattern_rule.order_only_prerequisites.iter()
                    .map(|p| p.replace('%', &stem))
                    .collect();

                if pattern_rule.second_expansion_prereqs.is_some() || pattern_rule.second_expansion_order_only.is_some() {
                    let oo_refs: Vec<&str> = all_order_only.iter().map(|s| s.as_str()).collect();
                    let mut pat_se_auto_vars = self.make_auto_vars(target, &all_prereqs, &oo_refs, &stem);
                    let collected_target_vars = self.collect_target_vars(target);
                    self.apply_target_vars_to_auto_vars(&collected_target_vars, &mut pat_se_auto_vars);

                    if let Some(ref text) = pattern_rule.second_expansion_prereqs {
                        let stem_subst = text.replace('%', &stem);
                        let (normal, oo) = self.second_expand_prereqs(&stem_subst, &pat_se_auto_vars, target);
                        pat_prereqs.extend(normal);
                        pat_order_only.extend(oo);
                    }
                    if let Some(ref text) = pattern_rule.second_expansion_order_only {
                        let stem_subst = text.replace('%', &stem);
                        let (normal, oo) = self.second_expand_prereqs(&stem_subst, &pat_se_auto_vars, target);
                        pat_order_only.extend(normal);
                        pat_order_only.extend(oo);
                    }
                }

                let mut already_built: std::collections::HashSet<String> = all_prereqs.iter().cloned().collect();
                let orig_explicit_prereqs = all_prereqs.clone();
                all_prereqs.clear();
                for prereq in &pat_prereqs {
                    if !already_built.contains(prereq) {
                        match self.build_target(prereq) {
                            Ok(rebuilt) => { if rebuilt { any_prereq_rebuilt = true; } }
                            Err(e) => {
                                let propagated = if e.starts_with("No rule to make target '") && !e.contains(", needed by '") {
                                    let base = e.trim_end_matches(".  Stop.").trim_end_matches(".");
                                    format!("{}, needed by '{}'.  Stop.", base, target)
                                } else { e };
                                if !self.keep_going {
                                    return Err(propagated);
                                }
                            }
                        }
                        already_built.insert(prereq.clone());
                    }
                    all_prereqs.push(prereq.clone());
                }
                all_prereqs.extend(orig_explicit_prereqs);

                for prereq in &pat_order_only {
                    if !all_order_only.contains(prereq) {
                        let _ = self.build_target(prereq);
                        all_order_only.push(prereq.clone());
                    }
                }

                (pattern_rule.recipe.clone(), pattern_rule.source_file.clone(), stem)
            } else {
                // No recipe and no matching pattern rule.
                let stem = rules.iter()
                    .find(|r| !r.static_stem.is_empty())
                    .map(|r| r.static_stem.clone())
                    .unwrap_or_default();
                (recipe, recipe_source_file, stem)
            }
        } else {
            let stem = rules.iter()
                .find(|r| !r.static_stem.is_empty())
                .map(|r| r.static_stem.clone())
                .unwrap_or_default();
            (recipe, recipe_source_file, stem)
        };

        Ok((any_prereq_rebuilt, all_prereqs, all_order_only, recipe, recipe_source_file, pattern_stem))
    }

    /// Wrapper for grouped targets (&:): builds the primary target AND its siblings.
    /// For GNU Make grouped targets:
    ///   1. Primary target's prerequisites (including SE) are built first.
    ///   2. Each sibling's prerequisites (including SE) are built next.
    ///   3. Recipe runs ONCE with primary target's auto vars.
    ///   4. All siblings are marked as built.
    fn build_with_rules_grouped(
        &mut self,
        target: &str,
        rules: &[Rule],
        is_phony: bool,
        grouped_siblings: &[String],
    ) -> Result<bool, String> {
        if grouped_siblings.is_empty() {
            return self.build_with_rules(target, rules, is_phony);
        }

        // Collect sibling rules upfront (clone before any mutable borrows)
        let sibling_rules: Vec<(String, Vec<Rule>)> = grouped_siblings.iter()
            .filter_map(|s| self.db.rules.get(s).cloned().map(|r| (s.clone(), r)))
            .collect();

        // Phase 1: build primary target's prerequisites (the SE expansion + prereq build
        // for the primary target). We get back (needs_rebuild, all_prereqs, all_order_only,
        // recipe, recipe_source, pattern_stem).
        let primary_result = self.build_with_rules_prereqs(target, rules, is_phony);
        let (any_prereq_rebuilt, all_prereqs, all_order_only, recipe, recipe_source_file, pattern_stem) = match primary_result {
            Err(e) => {
                // Mark siblings as failed
                for (sibling, _) in &sibling_rules {
                    self.built.insert(sibling.clone(), false);
                }
                return Err(e);
            }
            Ok(parts) => parts,
        };

        // Phase 2: build each sibling's prerequisites only.
        for (sibling, s_rules) in &sibling_rules {
            self.build_sibling_prereqs_only(sibling, s_rules);
        }

        // Phase 3: determine if we need to rebuild and run the recipe.
        let needs_rebuild = if self.always_make || is_phony {
            true
        } else {
            self.needs_rebuild(target, &all_prereqs, any_prereq_rebuilt)
        };

        if !needs_rebuild {
            // Mark siblings as not rebuilt.
            for (sibling, _) in &sibling_rules {
                self.built.insert(sibling.clone(), false);
            }
            return Ok(false);
        }

        let oo_refs: Vec<&str> = all_order_only.iter().map(|s| s.as_str()).collect();
        let mut auto_vars = self.make_auto_vars(target, &all_prereqs, &oo_refs, &pattern_stem);
        let collected_target_vars = self.collect_target_vars(target);
        self.apply_target_vars_to_auto_vars(&collected_target_vars, &mut auto_vars);

        if self.question {
            let has_real_cmds = self.recipe_has_real_commands(&recipe, &auto_vars);
            if has_real_cmds {
                self.question_out_of_date = true;
            }
            let rebuilt = has_real_cmds;
            for (sibling, _) in &sibling_rules {
                self.built.insert(sibling.clone(), rebuilt);
            }
            return Ok(rebuilt);
        }

        self.target_extra_exports = self.compute_target_exports(target);
        self.target_extra_unexports = self.compute_target_unexports(target);
        let result = self.execute_recipe(target, &recipe, &recipe_source_file, &auto_vars, is_phony);
        self.target_extra_exports.clear();
        self.target_extra_unexports.clear();

        let rebuilt = result.as_ref().copied().unwrap_or(false);
        for (sibling, _) in &sibling_rules {
            self.built.insert(sibling.clone(), rebuilt);
        }

        result
    }

    /// Build only the prerequisites of a grouped sibling target (no recipe execution).
    /// This is called before the primary grouped target's recipe runs.
    fn build_sibling_prereqs_only(&mut self, target: &str, rules: &[Rule]) {
        // Collect prereqs and SE texts (same logic as build_with_rules preamble)
        let mut all_prereqs: Vec<String> = Vec::new();
        let mut all_order_only: Vec<String> = Vec::new();
        let mut se_prereq_texts: Vec<String> = Vec::new();
        let mut se_order_only_texts: Vec<String> = Vec::new();

        for rule in rules {
            all_prereqs.extend(rule.prerequisites.clone());
            all_order_only.extend(rule.order_only_prerequisites.clone());
            if let Some(ref text) = rule.second_expansion_prereqs {
                se_prereq_texts.push(text.clone());
            }
            if let Some(ref text) = rule.second_expansion_order_only {
                se_order_only_texts.push(text.clone());
            }
        }

        all_prereqs.retain(|p| p != ".WAIT");
        all_order_only.retain(|p| p != ".WAIT");

        // Perform SE expansion with $@ = sibling target name
        if !se_prereq_texts.is_empty() || !se_order_only_texts.is_empty() {
            let stem = rules.iter()
                .find(|r| !r.static_stem.is_empty())
                .map(|r| r.static_stem.clone())
                .unwrap_or_default();
            let oo_refs: Vec<&str> = all_order_only.iter().map(|s| s.as_str()).collect();
            let base_auto_vars = self.make_auto_vars(target, &all_prereqs, &oo_refs, &stem);

            for text in &se_prereq_texts {
                let (normal, oo) = self.second_expand_prereqs(text, &base_auto_vars, target);
                all_prereqs.extend(normal);
                all_order_only.extend(oo);
            }
            for text in &se_order_only_texts {
                let (normal, oo) = self.second_expand_prereqs(text, &base_auto_vars, target);
                all_order_only.extend(normal);
                all_order_only.extend(oo);
            }
        }

        all_prereqs.retain(|p| p != ".WAIT");
        all_order_only.retain(|p| p != ".WAIT");

        // Build the prerequisites (ignore errors for sibling prereq building)
        for prereq in all_prereqs {
            let _ = self.build_target(&prereq);
        }
        for prereq in all_order_only {
            let _ = self.build_target(&prereq);
        }
    }

    fn build_with_double_colon_rules(&mut self, target: &str, rules: &[Rule], is_phony: bool) -> Result<bool, String> {
        // Each double-colon rule is an independent rule. Build its prerequisites
        // independently and run its recipe if needed.
        //
        // For double-colon rules with second expansion: each rule is independent,
        // so the auto vars used during SE are per-rule.  For the 1st rule, $< etc.
        // are empty (as in single-colon 1st-rule SE).  But in contrast to single-
        // colon rules, even the 2nd double-colon rule has empty $</$^/etc because
        // there is no "accumulated" context from sibling rules.
        let mut any_rebuilt = false;

        for rule in rules {
            let rule = rule.clone();
            // non-SE prerequisites are already expanded
            let mut prereqs = rule.prerequisites.clone();
            let mut order_only = rule.order_only_prerequisites.clone();

            // Handle second expansion: for double-colon rules all auto vars except
            // $@ are empty (each rule is independent).
            if rule.second_expansion_prereqs.is_some() || rule.second_expansion_order_only.is_some() {
                let stem = if rule.static_stem.is_empty() { "" } else { &rule.static_stem };
                // Build auto vars with empty prereqs (per GNU Make semantics for :: rules)
                let empty_prereqs: Vec<String> = Vec::new();
                let empty_oo: Vec<&str> = Vec::new();
                let base_auto_vars = self.make_auto_vars(target, &empty_prereqs, &empty_oo, stem);

                if let Some(ref text) = rule.second_expansion_prereqs {
                    let (normal, oo) = self.second_expand_prereqs(text, &base_auto_vars, target);
                    prereqs.extend(normal);
                    order_only.extend(oo);
                }
                if let Some(ref text) = rule.second_expansion_order_only {
                    let (normal, oo) = self.second_expand_prereqs(text, &base_auto_vars, target);
                    order_only.extend(normal);
                    order_only.extend(oo);
                }
            }

            // Build this rule's prerequisites
            // Push target vars onto stack for inheritance by prerequisites.
            let my_target_vars = self.collect_target_vars(target);
            self.inherited_vars_stack.push(my_target_vars);

            let mut any_prereq_rebuilt = false;
            let mut prereq_errors = Vec::new();

            for prereq in &prereqs {
                match self.build_target(prereq) {
                    Ok(rebuilt) => {
                        if rebuilt { any_prereq_rebuilt = true; }
                    }
                    Err(e) => {
                        let propagated = if e.starts_with("No rule to make target '") && !e.contains(", needed by '") {
                            let base = e.trim_end_matches(".  Stop.").trim_end_matches(".");
                            format!("{}, needed by '{}'.  Stop.", base, target)
                        } else {
                            e
                        };
                        if self.keep_going {
                            prereq_errors.push(propagated);
                        } else {
                            self.inherited_vars_stack.pop();
                            return Err(propagated);
                        }
                    }
                }
            }

            for prereq in &order_only {
                let _ = self.build_target(prereq);
            }

            self.inherited_vars_stack.pop();

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

            let oo_refs: Vec<&str> = order_only.iter().map(|s| s.as_str()).collect();
            let stem = if rule.static_stem.is_empty() { "" } else { &rule.static_stem };
            let mut auto_vars = self.make_auto_vars(target, &prereqs, &oo_refs, stem);

            // Apply target-specific and pattern-specific variables
            let collected_target_vars = self.collect_target_vars(target);
            self.apply_target_vars_to_auto_vars(&collected_target_vars, &mut auto_vars);

            if self.question {
                // In question mode: check if real shell commands would run.
                let has_real_cmds = self.recipe_has_real_commands(&rule.recipe, &auto_vars);
                if has_real_cmds {
                    self.question_out_of_date = true;
                    return Ok(true);
                }
                // Only make-functions would run; not "out of date" for shell work.
                continue;
            }

            self.target_extra_exports = self.compute_target_exports(target);
            self.target_extra_unexports = self.compute_target_unexports(target);
            match self.execute_recipe(target, &rule.recipe, &rule.source_file, &auto_vars, is_phony) {
                Ok(rebuilt) => { if rebuilt { any_rebuilt = true; } }
                Err(e) => {
                    if self.keep_going {
                        // continue to next double-colon rule
                    } else {
                        self.target_extra_exports.clear();
                        self.target_extra_unexports.clear();
                        return Err(e);
                    }
                }
            }
            self.target_extra_exports.clear();
            self.target_extra_unexports.clear();
        }

        Ok(any_rebuilt)
    }

    fn build_with_pattern_rule(&mut self, target: &str, rule: &Rule, stem: &str, is_phony: bool) -> Result<bool, String> {
        // Expand pattern prerequisites using the stem.
        // For normal (non-SE) prerequisites, substitute % with the stem.
        // .WAIT markers are filtered since they are ordering hints, not real targets.
        let mut prereqs: Vec<String> = rule.prerequisites.iter()
            .filter(|p| p.as_str() != ".WAIT")
            .map(|p| p.replace('%', stem))
            .collect();

        // Also expand any explicit prerequisites that came from `build_target_inner`
        // combining explicit rules with this pattern rule.
        // (Already handled via all_prereqs in build_target_inner for explicit rules.)

        // Handle second-expansion prerequisites for pattern rules.
        let mut order_only: Vec<String> = rule.order_only_prerequisites.iter()
            .filter(|p| p.as_str() != ".WAIT")
            .map(|p| p.replace('%', stem))
            .collect();

        if rule.second_expansion_prereqs.is_some() || rule.second_expansion_order_only.is_some() {
            // Build base auto vars from normal prereqs (before SE)
            let oo_refs: Vec<&str> = order_only.iter().map(|s| s.as_str()).collect();
            let base_auto_vars = self.make_auto_vars(target, &prereqs, &oo_refs, stem);

            // For pattern rule SE expansion, GNU Make does not include a file:line prefix
            // in error messages (unlike explicit rule SE).  Temporarily clear the file context.
            let saved_file = self.state.current_file.borrow().clone();
            let saved_line = *self.state.current_line.borrow();
            *self.state.current_file.borrow_mut() = String::new();
            *self.state.current_line.borrow_mut() = 0;

            if let Some(ref text) = rule.second_expansion_prereqs {
                // Substitute % in the raw text for the stem before expanding
                let stem_subst = text.replace('%', stem);
                let (normal, oo) = self.second_expand_prereqs(&stem_subst, &base_auto_vars, target);
                prereqs.extend(normal);
                order_only.extend(oo);
            }
            if let Some(ref text) = rule.second_expansion_order_only {
                let stem_subst = text.replace('%', stem);
                let (normal, oo) = self.second_expand_prereqs(&stem_subst, &base_auto_vars, target);
                order_only.extend(normal);
                order_only.extend(oo);
            }
            // Restore file context after pattern rule SE expansion.
            *self.state.current_file.borrow_mut() = saved_file;
            *self.state.current_line.borrow_mut() = saved_line;
        }

        // Build prerequisites
        // Push target vars onto stack for inheritance by prerequisites.
        let my_target_vars = self.collect_target_vars(target);
        self.inherited_vars_stack.push(my_target_vars);

        let mut any_rebuilt = false;
        for prereq in prereqs.clone() {
            match self.build_target(&prereq) {
                Ok(rebuilt) => {
                    if rebuilt { any_rebuilt = true; }
                }
                Err(e) => {
                    // Propagate "No rule to make target" errors correctly
                    let propagated = if e.starts_with("No rule to make target '") && !e.contains(", needed by '") {
                        let base = e.trim_end_matches(".  Stop.").trim_end_matches(".");
                        format!("{}, needed by '{}'.  Stop.", base, target)
                    } else {
                        e
                    };
                    self.inherited_vars_stack.pop();
                    return Err(propagated);
                }
            }
        }

        for prereq in order_only.clone() {
            let _ = self.build_target(&prereq);
        }

        self.inherited_vars_stack.pop();

        let needs_rebuild = if self.always_make || is_phony {
            true
        } else {
            self.needs_rebuild(target, &prereqs, any_rebuilt)
        };

        if !needs_rebuild {
            return Ok(false);
        }

        let oo_refs: Vec<&str> = order_only.iter().map(|s| s.as_str()).collect();
        let mut auto_vars = self.make_auto_vars(target, &prereqs, &oo_refs, stem);

        // Apply target-specific and pattern-specific variables
        let collected_target_vars = self.collect_target_vars(target);
        self.apply_target_vars_to_auto_vars(&collected_target_vars, &mut auto_vars);

        if self.question {
            // In question mode: check if real shell commands would run.
            let has_real_cmds = self.recipe_has_real_commands(&rule.recipe, &auto_vars);
            if has_real_cmds {
                self.question_out_of_date = true;
                return Ok(true);
            }
            // Only make-functions would run; not out-of-date in terms of shell work.
            if !self.any_recipe_ran { self.any_recipe_ran = true; }
            return Ok(true);
        }

        self.target_extra_exports = self.compute_target_exports(target);
        self.target_extra_unexports = self.compute_target_unexports(target);
        let result = self.execute_recipe(target, &rule.recipe, &rule.source_file, &auto_vars, is_phony);
        self.target_extra_exports.clear();
        self.target_extra_unexports.clear();
        // Track if this is an intermediate target that was built via pattern rule.
        // Targets built by implicit rules are potentially intermediate UNLESS they are:
        // 1. Top-level targets
        // 2. Explicitly mentioned in the makefile (as target or prereq of any rule)
        // 3. Marked .PRECIOUS
        // 4. Marked .NOTINTERMEDIATE
        // 5. Marked .SECONDARY
        if let Ok(true) = &result {
            if self.db.is_intermediate(target) {
                // Explicitly marked .INTERMEDIATE
                if !self.intermediate_built.contains(&target.to_string()) {
                    self.intermediate_built.push(target.to_string());
                }
            } else if !self.db.is_precious(target)
                && !self.db.is_notintermediate(target)
                && !self.db.is_secondary(target)
            {
                // Only mark as intermediate if the target is NOT explicitly mentioned
                // in the makefile and NOT a top-level target.
                let is_explicit = self.top_level_targets.contains(target)
                    || self.db.is_explicitly_mentioned(target);
                if !is_explicit {
                    if !self.intermediate_built.contains(&target.to_string()) {
                        self.intermediate_built.push(target.to_string());
                    }
                }
            }
        }
        result
    }

    fn find_pattern_rule(&self, target: &str) -> Option<(Rule, String)> {
        self.find_pattern_rule_inner(target, &[])
    }

    /// Core pattern rule search.
    /// `explicit_prereqs`: already-known explicit deps of `target` (they "ought to exist").
    fn find_pattern_rule_inner(&self, target: &str, explicit_prereqs: &[String]) -> Option<(Rule, String)> {
        // GNU Make implicit rule search (implicit.c):
        //
        // - User-defined rules (after builtin_count) have priority over built-ins.
        // - If a specific (non-%) rule matches, non-terminal match-anything rules rejected.
        // - Pass 1: prereqs immediately satisfiable (on disk, VPATH, or "ought to exist").
        //   "Ought to exist" = explicit target in db.rules OR in explicit_prereqs.
        // - Pass 2: prereqs can be built via chaining. Terminal (%::) rules skipped.
        // - Compat rule fallback: if prereq is only mentioned as someone else's dep
        //   (not as an explicit target), use the rule as last resort.

        let n_builtins = self.db.builtin_pattern_rules_count;
        let n_total = self.db.pattern_rules.len();
        let user_rules = &self.db.pattern_rules[n_builtins..n_total];
        let builtin_rules = &self.db.pattern_rules[0..n_builtins];

        // Check if a specific (non-%) rule matches this target.
        let specific_rule_matched = user_rules.iter().chain(builtin_rules.iter())
            .any(|rule| rule.targets.iter().any(|pt| pt.len() > 1 && match_pattern(pt, target).is_some()));

        // User rules first (higher priority), then built-ins.
        let all_rules: Vec<&Rule> = user_rules.iter().chain(builtin_rules.iter()).collect();

        let mut compat_rule: Option<(Rule, String)> = None;

        // Pass 1: all prereqs immediately satisfiable.
        for rule in &all_rules {
            // Skip rules with no recipe unless they have SE prereqs (which may produce
            // errors or provide prerequisites at build time) or are terminal rules.
            if rule.recipe.is_empty()
                && !rule.is_terminal
                && rule.second_expansion_prereqs.is_none()
                && rule.second_expansion_order_only.is_none()
            { continue; }
            for pattern_target in &rule.targets {
                if specific_rule_matched && pattern_target == "%" && !rule.is_terminal { continue; }
                if let Some(stem) = match_pattern(pattern_target, target) {
                    let mut found_compat = false;
                    let prereqs_ok = if rule.prerequisites.is_empty() {
                        true
                    } else {
                        let mut all_ok = true;
                        for p in &rule.prerequisites {
                            if p == ".WAIT" { continue; }
                            let resolved = p.replace('%', &stem);
                            let ok = Path::new(&resolved).exists()
                                || self.db.is_phony(&resolved)
                                || self.db.rules.contains_key(&resolved)
                                || explicit_prereqs.iter().any(|ep| ep == &resolved)
                                || self.find_in_vpath(&resolved).is_some();
                            if !ok {
                                if self.db.explicit_dep_names.contains(&resolved)
                                    && !self.db.rules.contains_key(&resolved)
                                {
                                    found_compat = true;
                                }
                                all_ok = false;
                                break;
                            }
                        }
                        all_ok
                    };
                    if prereqs_ok {
                        return Some(((**rule).clone(), stem));
                    } else if found_compat && compat_rule.is_none() {
                        compat_rule = Some(((**rule).clone(), stem.clone()));
                    }
                }
            }
        }

        // Pass 2: prereqs can be built via chaining. Terminal rules skipped.
        for rule in &all_rules {
            if rule.recipe.is_empty()
                && !rule.is_terminal
                && rule.second_expansion_prereqs.is_none()
                && rule.second_expansion_order_only.is_none()
            { continue; }
            if rule.is_terminal { continue; }
            for pattern_target in &rule.targets {
                if specific_rule_matched && pattern_target == "%" && !rule.is_terminal { continue; }
                if let Some(stem) = match_pattern(pattern_target, target) {
                    let mut found_compat = false;
                    let prereqs_ok = if rule.prerequisites.is_empty() {
                        true
                    } else {
                        let mut all_ok = true;
                        for p in &rule.prerequisites {
                            if p == ".WAIT" { continue; }
                            let resolved = p.replace('%', &stem);
                            let ok = Path::new(&resolved).exists()
                                || self.db.rules.contains_key(&resolved)
                                || self.db.is_phony(&resolved)
                                || explicit_prereqs.iter().any(|ep| ep == &resolved)
                                || self.find_pattern_rule_exists(&resolved)
                                || self.find_in_vpath(&resolved).is_some();
                            if !ok {
                                if self.db.explicit_dep_names.contains(&resolved)
                                    && !self.db.rules.contains_key(&resolved)
                                {
                                    found_compat = true;
                                }
                                all_ok = false;
                                break;
                            }
                        }
                        all_ok
                    };
                    if prereqs_ok {
                        return Some(((**rule).clone(), stem));
                    } else if found_compat && compat_rule.is_none() {
                        compat_rule = Some(((**rule).clone(), stem.clone()));
                    }
                }
            }
        }

        compat_rule
    }

    /// Check if `target` can be built by any pattern rule (recursive/intermediate context).
    /// Non-terminal match-anything rules (%) are skipped: they cannot create intermediates.
    fn find_pattern_rule_exists(&self, target: &str) -> bool {
        let n_builtins = self.db.builtin_pattern_rules_count;
        let n_total = self.db.pattern_rules.len();
        let user_rules = &self.db.pattern_rules[n_builtins..n_total];
        let builtin_rules = &self.db.pattern_rules[0..n_builtins];

        let specific_rule_matched = user_rules.iter().chain(builtin_rules.iter())
            .any(|rule| rule.targets.iter().any(|pt| pt.len() > 1 && match_pattern(pt, target).is_some()));

        for rule in user_rules.iter().chain(builtin_rules.iter()) {
            if rule.recipe.is_empty() { continue; }
            for pattern_target in &rule.targets {
                // Skip non-terminal match-anything rules in recursive context.
                if pattern_target == "%" && !rule.is_terminal { continue; }
                if specific_rule_matched && pattern_target == "%" && !rule.is_terminal { continue; }
                if let Some(stem) = match_pattern(pattern_target, target) {
                    let prereqs_ok = if rule.prerequisites.is_empty() {
                        true
                    } else {
                        rule.prerequisites.iter().all(|p| {
                            if p == ".WAIT" { return true; }
                            let resolved = p.replace('%', &stem);
                            Path::new(&resolved).exists()
                                || self.db.rules.contains_key(&resolved)
                                || self.db.is_phony(&resolved)
                                || self.find_in_vpath(&resolved).is_some()
                        })
                    };
                    if prereqs_ok { return true; }
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
    /// Returns a map of variable name → (value, is_override, is_private).
    /// Pattern-specific variables are matched with shortest-stem semantics.
    fn collect_target_vars(&self, target: &str) -> HashMap<String, (String, bool, bool)> {
        // We use a two-pass approach for Recursive variables:
        //  Pass 1: collect Simple/Conditional/Shell vars (expanded immediately) and
        //          store Recursive/Append vars' raw values.
        //  Pass 2: expand Recursive vars in the context of the already-collected vars.
        //
        // This allows `a = global: $(global) pattern: $(pattern)` to correctly
        // see `pattern` set by a pattern-specific var when `a` is expanded.

        // Internal representation: (raw_value, is_expanded, is_override, flavor)
        // is_expanded=true  → value is already final (Simple, already-expanded Append)
        // is_expanded=false → value is a raw Recursive template needing second pass
        let mut staging: Vec<(String, String, bool, bool, VarFlavor)> = Vec::new();
        // Map from var_name → index in staging (for quick lookup/update)
        let mut staging_idx: HashMap<String, usize> = HashMap::new();
        // Track which vars are private (not inherited by prerequisites).
        let mut private_flags: HashSet<String> = HashSet::new();

        // 0. Seed with inherited vars from parent target (lowest priority).
        // Target-specific variables are inherited by prerequisites unless marked private.
        // The inherited vars come from the parent target's collected vars pushed onto
        // inherited_vars_stack before building this target as a prerequisite.
        if let Some(inherited) = self.inherited_vars_stack.last() {
            for (name, (val, is_override, is_private_flag)) in inherited {
                if *is_private_flag { continue; } // private vars are not inherited
                let idx = staging.len();
                staging.push((name.clone(), val.clone(), true, *is_override, VarFlavor::Simple));
                staging_idx.insert(name.clone(), idx);
            }
        }

        // Helper: get raw value and is_expanded flag for var_name from staging.
        let get_staging_entry = |staging: &Vec<(String, String, bool, bool, VarFlavor)>,
                                  staging_idx: &HashMap<String, usize>,
                                  var_name: &str| -> Option<(String, bool)> {
            staging_idx.get(var_name).map(|&i| {
                let (_, val, is_expanded, _, _) = &staging[i];
                (val.clone(), *is_expanded)
            })
        };
        let get_global_val = |state: &MakeState, var_name: &str| -> Option<String> {
            state.db.variables.get(var_name).map(|v| {
                if v.flavor == VarFlavor::Simple { v.value.clone() } else { state.expand(&v.value) }
            })
        };
        // Helper: expand a value immediately using the state.
        let expand_imm = |state: &MakeState, val: &str| -> String {
            state.expand(val)
        };
        // Helper: get the expanded value for var_name from staging.
        let get_staging_val = |staging: &Vec<(String, String, bool, bool, VarFlavor)>,
                                staging_idx: &HashMap<String, usize>,
                                var_name: &str| -> Option<String> {
            staging_idx.get(var_name).map(|&i| {
                let (_, val, is_expanded, _, _) = &staging[i];
                if *is_expanded {
                    val.clone()
                } else {
                    // Unexpanded recursive value - expand it now
                    val.clone() // Return raw for now; expansion happens in pass 2
                }
            })
        };

        // 1. Apply pattern-specific variables.
        let mut pattern_vars_with_stem: Vec<(usize, usize, &PatternSpecificVar)> = Vec::new();
        for (idx, psv) in self.db.pattern_specific_vars.iter().enumerate() {
            if let Some(stem) = match_pattern_simple(&psv.pattern, target) {
                pattern_vars_with_stem.push((stem.len(), idx, psv));
            }
        }
        // Sort: descending stem length (less-specific first), then ascending index.
        pattern_vars_with_stem.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));

        for (_, _, psv) in &pattern_vars_with_stem {
            if psv.var.is_private {
                private_flags.insert(psv.var_name.clone());
            } else {
                private_flags.remove(&psv.var_name);
            }
            match psv.var.flavor {
                VarFlavor::Simple => {
                    // Already expanded at assignment; use as-is.
                    let val = psv.var.value.clone();
                    let idx = staging.len();
                    staging.push((psv.var_name.clone(), val, true, psv.is_override, VarFlavor::Simple));
                    staging_idx.insert(psv.var_name.clone(), idx);
                }
                VarFlavor::Append => {
                    // A non-override Append is blocked if the existing var has override flag.
                    let existing_is_override = staging_idx.get(&psv.var_name)
                        .map_or(false, |&i| staging[i].3);
                    if !psv.is_override && existing_is_override {
                        // Skip this non-override append; preserve existing override value.
                    } else {
                        let rhs = expand_imm(&self.state, &psv.var.value);
                        let base = get_staging_val(&staging, &staging_idx, &psv.var_name)
                            .or_else(|| get_global_val(&self.state, &psv.var_name))
                            .unwrap_or_default();
                        let val = if base.is_empty() { rhs } else { format!("{} {}", base, rhs) };
                        // Preserve override flag if existing entry was override.
                        let new_is_override = psv.is_override || existing_is_override;
                        let idx = staging.len();
                        staging.push((psv.var_name.clone(), val, true, new_is_override, VarFlavor::Append));
                        staging_idx.insert(psv.var_name.clone(), idx);
                    }
                }
                VarFlavor::Conditional => {
                    // ?= : only set if not already set
                    if !staging_idx.contains_key(&psv.var_name) {
                        let val = expand_imm(&self.state, &psv.var.value);
                        let idx = staging.len();
                        staging.push((psv.var_name.clone(), val, true, psv.is_override, VarFlavor::Conditional));
                        staging_idx.insert(psv.var_name.clone(), idx);
                    }
                }
                VarFlavor::Shell => {
                    // Already-run shell command value; store as-is
                    let val = psv.var.value.clone();
                    let idx = staging.len();
                    staging.push((psv.var_name.clone(), val, true, psv.is_override, VarFlavor::Shell));
                    staging_idx.insert(psv.var_name.clone(), idx);
                }
                VarFlavor::Recursive => {
                    // Store raw value for second-pass expansion
                    let raw = psv.var.value.clone();
                    let idx = staging.len();
                    staging.push((psv.var_name.clone(), raw, false, psv.is_override, VarFlavor::Recursive));
                    staging_idx.insert(psv.var_name.clone(), idx);
                }
            }
        }

        // 2. Apply target-specific variables (they override pattern-specific).
        if let Some(rules) = self.db.rules.get(target) {
            for rule in rules {
                for (raw_var_name, var) in &rule.target_specific_vars {
                    // Expand the variable name using already-collected target vars.
                    // This handles `four:VAR$(FOO)=ok` where FOO is itself a target-specific var.
                    // Include both simple (is_exp=true) and recursive (is_exp=false) staging entries,
                    // expanding recursive ones immediately so that `VAR$(FOO)` with FOO=x gives VARx.
                    let expanded_var_name: String;
                    let var_name: &str = if raw_var_name.contains('$') {
                        let ctx: HashMap<String, String> = staging.iter()
                            .map(|(n, v, is_exp, _, _)| {
                                let val = if *is_exp {
                                    v.clone()
                                } else {
                                    self.state.expand(v)
                                };
                                (n.clone(), val)
                            })
                            .collect();
                        expanded_var_name = self.state.expand_with_auto_vars(raw_var_name, &ctx);
                        &expanded_var_name
                    } else {
                        raw_var_name.as_str()
                    };
                    let is_override = var.origin == VarOrigin::Override;
                    // Track private flag for this variable.
                    if var.is_private {
                        private_flags.insert(var_name.to_string());
                    } else {
                        private_flags.remove(var_name);
                    }
                    match var.flavor {
                        VarFlavor::Simple => {
                            let val = var.value.clone();
                            if let Some(&i) = staging_idx.get(var_name) {
                                staging[i] = (var_name.to_string(), val, true, is_override, VarFlavor::Simple);
                            } else {
                                let idx = staging.len();
                                staging.push((var_name.to_string(), val, true, is_override, VarFlavor::Simple));
                                staging_idx.insert(var_name.to_string(), idx);
                            }
                        }
                        VarFlavor::Append => {
                            // A non-override Append is blocked if the existing var has override flag.
                            let existing_is_override = staging_idx.get(var_name)
                                .map_or(false, |&i| staging[i].3);
                            if !is_override && existing_is_override {
                                // Skip this non-override append.
                            } else {
                                // Determine if the base is simple (already expanded) or recursive.
                                // For recursive base: keep result recursive (don't expand rhs yet).
                                // For simple base: expand rhs immediately with staging context.
                                let base_is_expanded = staging_idx.get(var_name)
                                    .map(|&i| staging[i].2)
                                    .unwrap_or_else(|| {
                                        // Check global var flavor
                                        self.db.variables.get(var_name)
                                            .map(|v| v.flavor == VarFlavor::Simple)
                                            .unwrap_or(false)
                                    });
                                let new_is_override = is_override || existing_is_override;
                                if base_is_expanded {
                                    // Simple base: expand rhs with staging context for
                                    // correct target-specific var references.
                                    let staging_ctx: HashMap<String, String> = staging.iter()
                                        .filter(|(_, _, is_exp, _, _)| *is_exp)
                                        .map(|(n, v, _, _, _)| (n.clone(), v.clone()))
                                        .collect();
                                    let rhs = self.state.expand_with_auto_vars(&var.value, &staging_ctx);
                                    let base = get_staging_val(&staging, &staging_idx, var_name)
                                        .or_else(|| self.db.variables.get(var_name).map(|v| v.value.clone()))
                                        .unwrap_or_default();
                                    let val = if base.is_empty() { rhs } else { format!("{} {}", base, rhs) };
                                    if let Some(&i) = staging_idx.get(var_name) {
                                        staging[i] = (var_name.to_string(), val, true, new_is_override, VarFlavor::Append);
                                    } else {
                                        let idx = staging.len();
                                        staging.push((var_name.to_string(), val, true, new_is_override, VarFlavor::Append));
                                        staging_idx.insert(var_name.to_string(), idx);
                                    }
                                } else {
                                    // Recursive base: keep result recursive.
                                    // Get raw (unexpanded) base value from staging or global.
                                    let base_raw = staging_idx.get(var_name)
                                        .map(|&i| staging[i].1.clone())
                                        .or_else(|| self.db.variables.get(var_name).map(|v| v.value.clone()))
                                        .unwrap_or_default();
                                    let rhs_raw = &var.value;
                                    let val = if base_raw.is_empty() { rhs_raw.clone() } else { format!("{} {}", base_raw, rhs_raw) };
                                    if let Some(&i) = staging_idx.get(var_name) {
                                        staging[i] = (var_name.to_string(), val, false, new_is_override, VarFlavor::Recursive);
                                    } else {
                                        let idx = staging.len();
                                        staging.push((var_name.to_string(), val, false, new_is_override, VarFlavor::Recursive));
                                        staging_idx.insert(var_name.to_string(), idx);
                                    }
                                }
                            }
                        }
                        VarFlavor::Conditional => {
                            if !staging_idx.contains_key(var_name) {
                                let val = expand_imm(&self.state, &var.value);
                                let idx = staging.len();
                                staging.push((var_name.to_string(), val, true, is_override, VarFlavor::Conditional));
                                staging_idx.insert(var_name.to_string(), idx);
                            }
                        }
                        VarFlavor::Shell => {
                            let val = var.value.clone();
                            if let Some(&i) = staging_idx.get(var_name) {
                                staging[i] = (var_name.to_string(), val, true, is_override, VarFlavor::Shell);
                            } else {
                                let idx = staging.len();
                                staging.push((var_name.to_string(), val, true, is_override, VarFlavor::Shell));
                                staging_idx.insert(var_name.to_string(), idx);
                            }
                        }
                        VarFlavor::Recursive => {
                            let raw = var.value.clone();
                            if let Some(&i) = staging_idx.get(var_name) {
                                staging[i] = (var_name.to_string(), raw, false, is_override, VarFlavor::Recursive);
                            } else {
                                let idx = staging.len();
                                staging.push((var_name.to_string(), raw, false, is_override, VarFlavor::Recursive));
                                staging_idx.insert(var_name.to_string(), idx);
                            }
                        }
                    }
                }
            }
        }

        // 3. Second pass: expand Recursive vars using all already-expanded vars as context.
        //    Build an auto_vars map from the already-expanded staging entries.
        let mut expansion_context: HashMap<String, String> = HashMap::new();
        for (name, val, is_expanded, _, _) in &staging {
            if *is_expanded {
                expansion_context.insert(name.clone(), val.clone());
            }
        }
        let mut result: HashMap<String, (String, bool, bool)> = HashMap::new();
        for (name, raw, is_expanded, is_override, _flavor) in &staging {
            let val = if *is_expanded {
                raw.clone()
            } else {
                // Recursive: expand using global state + already-expanded target vars as context
                self.state.expand_with_auto_vars(raw, &expansion_context)
            };
            let is_priv = private_flags.contains(name.as_str());
            result.insert(name.clone(), (val, *is_override, is_priv));
        }

        result
    }

    /// Apply collected target vars to auto_vars, respecting command-line variable priority.
    /// Also shadows globally-private variables with empty strings so that recipe expansion
    /// does not see the global value for `private VAR = val` assignments.
    fn apply_target_vars_to_auto_vars(
        &self,
        target_vars: &HashMap<String, (String, bool, bool)>,
        auto_vars: &mut HashMap<String, String>,
    ) {
        // First, shadow any globally-private variables that aren't already set
        // by target-specific vars. This ensures that `private F = g` at global
        // scope is not visible in recipe expansion (only visible at parse time).
        for (var_name, var) in &self.db.variables {
            if var.is_private && !target_vars.contains_key(var_name.as_str()) {
                // Private global var: make it appear as empty in recipe context.
                // (Only insert if not already shadowed by auto var like @, <, etc.)
                if !auto_vars.contains_key(var_name.as_str()) {
                    auto_vars.insert(var_name.clone(), String::new());
                }
            }
        }
        for (var_name, (value, is_override, _is_private)) in target_vars {
            // Non-override target-specific vars don't override command-line vars
            let is_cmdline = self.db.variables.get(var_name.as_str())
                .map_or(false, |v| v.origin == VarOrigin::CommandLine);
            if is_cmdline && !is_override {
                continue;
            }
            auto_vars.insert(var_name.clone(), value.clone());
        }
    }

    /// Compute the "effective mtime" of a target for the purpose of checking if a parent needs rebuild.
    /// For a target that doesn't exist but is intermediate/deletable, this computes the maximum
    /// mtime of the target's sources, representing "when would this have been built".
    /// Returns None if the target doesn't exist and has no known sources.
    fn effective_mtime(&self, target: &str, depth: usize) -> Option<SystemTime> {
        // Prevent infinite recursion
        if depth > 10 { return None; }
        // If target exists, use its actual mtime
        if let Some(t) = get_mtime(target).or_else(|| self.find_in_vpath(target).and_then(|f| get_mtime(&f))) {
            return Some(t);
        }
        // Target doesn't exist. If phony, treat as always new (infinite mtime).
        if self.db.is_phony(target) {
            return Some(SystemTime::now());
        }
        // Look for a rule that would build this target and find its sources' max mtime.
        // Check explicit rules first.
        if let Some(rules) = self.db.rules.get(target) {
            let max_prereq_time = rules.iter()
                .flat_map(|r| r.prerequisites.iter())
                .filter_map(|p| self.effective_mtime(p, depth + 1))
                .max();
            return max_prereq_time;
        }
        // Check pattern rules
        if let Some((rule, stem)) = self.find_pattern_rule(target) {
            let max_prereq_time = rule.prerequisites.iter()
                .filter(|p| p.as_str() != ".WAIT")
                .map(|p| p.replace('%', &stem))
                .filter_map(|p| self.effective_mtime(&p, depth + 1))
                .max();
            return max_prereq_time;
        }
        None
    }

    fn needs_rebuild(&self, target: &str, prereqs: &[String], any_prereq_rebuilt: bool) -> bool {
        if any_prereq_rebuilt {
            return true;
        }

        // -W/--what-if: if any prereq is in the what_if list, treat it as infinitely new
        for prereq in prereqs {
            if self.what_if.iter().any(|w| w == prereq) {
                return true;
            }
        }

        let target_time = match get_mtime(target) {
            Some(t) => t,
            None => return true, // Target doesn't exist
        };

        for prereq in prereqs {
            // Skip .WAIT markers (should already be filtered, but be safe)
            if prereq == ".WAIT" { continue; }

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
                        // Secondary files that don't exist don't trigger rebuilds
                        if self.db.is_secondary(prereq) {
                            continue;
                        }
                        // For non-existent non-phony prereqs (potentially intermediate files
                        // that were deleted), compute effective mtime from their sources.
                        // If sources are all older than the target, no rebuild needed.
                        match self.effective_mtime(prereq, 0) {
                            Some(eff_t) if eff_t > target_time => return true,
                            _ => continue,
                        }
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

        // $? - prerequisites that are newer than the target.
        // Includes prereqs that:
        //   - exist on disk AND are newer than the target
        //   - target doesn't exist but prereq exists on disk
        //   - prereq doesn't exist on disk but was visited/built this make run and target doesn't exist
        let target_time = get_mtime(target);
        let newer: Vec<String> = prereqs.iter()
            .filter(|p| {
                let prereq_mtime = get_mtime(p).or_else(|| {
                    self.find_in_vpath(p).and_then(|found| get_mtime(&found))
                });
                match (target_time, prereq_mtime) {
                    (None, Some(_)) => true,       // target doesn't exist, prereq does: newer
                    (Some(tt), Some(pt)) => pt > tt, // both exist: compare times
                    (_, None) => {
                        // prereq doesn't exist as file: include if target doesn't exist AND prereq was visited
                        target_time.is_none() && self.built.contains_key(p.as_str())
                    }
                    _ => false,
                }
            })
            .cloned()
            .collect();
        vars.insert("?".to_string(), newer.join(" "));

        // $* - stem
        // For pattern rules, stem is provided. For explicit rules, compute the stem
        // by removing the longest suffix of target that appears in .SUFFIXES.
        let effective_stem = if !stem.is_empty() {
            stem.to_string()
        } else {
            // Find the longest suffix of target that is in .SUFFIXES
            let mut best_stem = String::new();
            let mut best_suffix_len = 0;
            for suffix in &self.db.suffixes {
                if target.ends_with(suffix.as_str()) && suffix.len() > best_suffix_len {
                    best_suffix_len = suffix.len();
                    best_stem = target[..target.len() - suffix.len()].to_string();
                }
            }
            best_stem
        };
        vars.insert("*".to_string(), effective_stem.clone());
        // Update $(*D) and $(*F) with effective stem
        vars.insert("*D".to_string(), dir_of(&effective_stem));
        vars.insert("*F".to_string(), file_of(&effective_stem));

        // $| - order-only prerequisites (deduplicated, preserving first occurrence order)
        let mut oo_seen = HashSet::new();
        let oo_list: Vec<String> = order_only.iter()
            .filter(|s| oo_seen.insert(s.to_string()))
            .map(|s| s.to_string())
            .collect();
        vars.insert("|".to_string(), oo_list.join(" "));

        // $(@D) $(@F) etc - directory and file parts
        vars.insert("@D".to_string(), dir_of(target));
        vars.insert("@F".to_string(), file_of(target));
        vars.insert("<D".to_string(), dir_of(&first_prereq));
        vars.insert("<F".to_string(), file_of(&first_prereq));
        // $(*D) and $(*F) are already inserted above with effective_stem

        vars
    }

    /// Check whether the recipe has any real shell commands (not just make-function side-effects).
    /// Also runs make-function side-effects (like $(info)) as a side-effect of expansion.
    /// Used in question mode to determine if the target is "out of date".
    fn recipe_has_real_commands(&mut self, recipe: &[(usize, String)], auto_vars: &HashMap<String, String>) -> bool {
        for (_lineno, line) in recipe {
            let expanded = self.state.expand_with_auto_vars(line, auto_vars);
            let cmd = strip_recipe_prefixes(&expanded);
            if !cmd.trim().is_empty() {
                return true;
            }
        }
        false
    }

    fn execute_recipe(&mut self, target: &str, recipe: &[(usize, String)], source_file: &str, auto_vars: &HashMap<String, String>, _is_phony: bool) -> Result<bool, String> {
        // With --trace, print "file:line: update target 'X' due to: reason" before executing.
        if self.trace && !recipe.is_empty() {
            let (lineno, _) = &recipe[0];
            let loc = make_location(source_file, *lineno);
            // We don't have the reason computed here; callers pass it via
            // execute_recipe_with_trace. This path is the legacy call site.
            // For backward compat, compute a basic reason: if target doesn't exist, say so.
            let reason = if !Path::new(target).exists() {
                "target does not exist".to_string()
            } else {
                "target is out of date".to_string()
            };
            eprintln!("{}: update target '{}' due to: {}", loc, target, reason);
        }
        if self.touch {
            // Just touch the target
            if !self.silent {
                println!("touch {}", target);
            }
            if !self.dry_run {
                touch_file(target);
            }
            self.any_recipe_ran = true;
            return Ok(true);
        }

        let one_shell = self.db.one_shell;
        let is_silent_target = self.db.is_silent_target(target);

        if one_shell {
            // Execute all recipe lines as one shell script.
            // In .ONESHELL mode:
            //   - ALL recipe lines are joined and passed to a single shell invocation.
            //   - Prefix chars (@, -, +) on EVERY line are stripped from the script content.
            //   - Echo behaviour is controlled by the FIRST recipe line's prefix only:
            //     if the first line starts with @, the whole recipe is silent; otherwise
            //     ALL lines are echoed (using their content stripped of prefix chars).
            //   - Inner lines' @ etc. do NOT suppress their individual echo.

            let mut script = String::new();
            let mut first_line_silent = false;
            let mut first_line_ignore = false;
            let mut is_first = true;

            for (_lineno, line) in recipe {
                let expanded = self.state.expand_with_auto_vars(line, auto_vars);
                let cmd_line = strip_recipe_prefixes(&expanded);
                if is_first {
                    let (_d, ls, li, _lf) = parse_recipe_prefix(&expanded);
                    first_line_silent = ls;
                    first_line_ignore = li;
                    is_first = false;
                }
                script.push_str(&cmd_line);
                script.push('\n');
            }

            let effective_silent = first_line_silent || self.silent || is_silent_target;
            let effective_ignore = first_line_ignore || self.ignore_errors;

            if !effective_silent {
                // Echo ALL lines (using content stripped of prefix chars)
                for (_lineno, line) in recipe {
                    let expanded = self.state.expand_with_auto_vars(line, auto_vars);
                    let display = strip_recipe_prefixes(&expanded);
                    if !display.trim().is_empty() {
                        println!("{}", display.trim_end());
                    }
                }
            }

            if script.trim().is_empty() {
                // Recipe expanded to nothing (all make-functions, no shell commands).
                // Don't count as a recipe ran; return false so "is up to date" can print.
                return Ok(false);
            }

            self.any_recipe_ran = true;
            if !self.dry_run {
                // Split shell_flags by whitespace into separate arguments.
                // .SHELLFLAGS = "-e -c" means pass -e and -c as separate args.
                let flags: Vec<&str> = self.shell_flags.split_whitespace().collect();
                let mut child = Command::new(self.shell);
                for flag in &flags {
                    child.arg(flag);
                }
                // Script is the final argument (after any flags from .SHELLFLAGS)
                child.arg(&script);
                child.env("MAKELEVEL", self.get_makelevel());
                self.setup_exports(&mut child);
                let status = child.status();

                match status {
                    Ok(s) if !s.success() => {
                        let code = s.code().unwrap_or(1);
                        if effective_ignore {
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

        // Execute each recipe line separately.
        // Track whether any actual shell commands were executed.
        let mut any_cmd_ran = false;
        for (lineno, line) in recipe {
            let expanded = self.state.expand_with_auto_vars(line, auto_vars);

            // Extract prefix flags (@, -, +) from the ORIGINAL recipe line (before expansion).
            // These flags propagate to ALL sub-lines from the expansion.
            // For example, `@$(MULTI_LINE_VAR)` silences every line in the expansion,
            // not just the first one.
            let (_outer_display, outer_silent, outer_ignore, outer_force) = parse_recipe_prefix(line);

            // A recipe line may expand to multiple sub-lines when it contains a
            // multi-line `define` variable.  Split on bare newlines (not preceded
            // by a backslash) and treat each sub-line as an independent recipe
            // command with its own prefix chars.  Backslash-newline sequences
            // (shell line continuations) must NOT be split: they are passed as a
            // single command to the shell, which handles the continuation itself.
            let sub_lines: Vec<String> = split_recipe_sub_lines(&expanded);

            'sub_line_loop: for sub_line in &sub_lines {
                let (display_line, line_silent, ignore_error, force_sub) = parse_recipe_prefix(sub_line);
                // Outer flags (from the original recipe line before expansion) propagate to
                // all sub-lines.  This handles `@$(MULTI)` where MULTI has multiple lines.
                let force = force_sub || outer_force;

                let effective_silent = line_silent || outer_silent || self.silent || is_silent_target;
                let effective_ignore = ignore_error || outer_ignore || self.ignore_errors;

                // Get the actual command (strip @, -, + prefixes - none of them go to the shell)
                let cmd = strip_recipe_prefixes(sub_line);

                if cmd.trim().is_empty() {
                    // Empty command after expansion (e.g., $(info ...) expands to nothing).
                    // Don't count this as a real shell command, and don't echo it.
                    continue 'sub_line_loop;
                }

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
                        // Count as a command that would run in normal mode
                        any_cmd_ran = true;
                        continue 'sub_line_loop;
                    }
                }

                any_cmd_ran = true;
                self.any_recipe_ran = true;

                // Use target-specific SHELL/.SHELLFLAGS if present in auto_vars,
                // otherwise fall back to the global defaults.
                let eff_shell_raw = auto_vars.get("SHELL").map(|s| s.as_str()).unwrap_or(self.shell);
                let eff_flags = if let Some(f) = auto_vars.get(".SHELLFLAGS") {
                    f.as_str()
                } else {
                    self.shell_flags
                };

                // When SHELL contains spaces (e.g. "echo hi"), GNU Make composes the full
                // invocation as a shell string and runs it through /bin/sh -c, so that
                // shell quoting in .SHELLFLAGS is properly interpreted.
                // Otherwise use direct execvp with the shell program.
                let child_status = if eff_shell_raw.contains(' ') {
                    // Compose full command string: "SHELL SHELLFLAGS cmd"
                    let composed = format!("{} {} {}", eff_shell_raw, eff_flags, cmd);
                    let mut c = Command::new("/bin/sh");
                    c.arg("-c").arg(&composed);
                    c.env("MAKELEVEL", self.get_makelevel());
                    self.setup_exports(&mut c);
                    c.status()
                } else {
                    // Direct exec: shell_prog [shell_flags] cmd
                    let flags: Vec<&str> = eff_flags.split_whitespace().collect();
                    let mut c = Command::new(eff_shell_raw);
                    for flag in &flags {
                        c.arg(flag);
                    }
                    c.arg(&cmd);
                    c.env("MAKELEVEL", self.get_makelevel());
                    self.setup_exports(&mut c);
                    c.status()
                };
                let status = child_status;

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
            } // end sub_lines loop
        }

        // Return true (rebuilt) only if actual shell commands ran.
        // If only make-functions ($(info), etc.) ran, return false so
        // "is up to date" message can be printed for the target.
        Ok(any_cmd_ran)
    }

    fn target_has_recipe(&self, target: &str) -> bool {
        // A "real" recipe has at least one non-empty line after stripping
        // whitespace. An empty inline recipe (`target: prereqs ;` with nothing
        // after the semicolon) does NOT count as having a recipe for the purposes
        // of "is up to date" vs "Nothing to be done" messages.
        let recipe_is_real = |recipe: &[(usize, String)]| -> bool {
            recipe.iter().any(|(_, line)| {
                let stripped = strip_recipe_prefixes(line);
                !stripped.trim().is_empty()
            })
        };
        if let Some(rules) = self.db.rules.get(target) {
            for rule in rules {
                if recipe_is_real(&rule.recipe) {
                    return true;
                }
            }
        }
        // Check pattern rules
        if let Some((rule, _)) = self.find_pattern_rule(target) {
            if recipe_is_real(&rule.recipe) {
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
        // Also export any target-specific or pattern-specific variables that were
        // declared with the 'export' keyword for the current target.
        for (name, value) in &self.target_extra_exports {
            cmd.env(name, value);
        }
        // Remove variables that are explicitly unexported for the current target.
        for name in &self.target_extra_unexports {
            cmd.env_remove(name);
        }
    }

    /// Compute the set of variables that should be exported to the shell for `target`
    /// due to target-specific or pattern-specific `export` declarations.
    /// Returns a map from variable name to its expanded value.
    fn compute_target_exports(&self, target: &str) -> HashMap<String, String> {
        // Use the collect_target_vars result to find the final values of all
        // applicable target-specific and pattern-specific variables.
        let target_vars = self.collect_target_vars(target);
        let mut exports: HashMap<String, String> = HashMap::new();
        // Track vars that are explicitly unexported for this target.
        let mut unexports: HashSet<String> = HashSet::new();

        // Helper: determine if the global var should be exported.
        let global_should_export = |var_name: &str| -> bool {
            if matches!(var_name, "MAKEFLAGS" | "MAKE" | "MAKECMDGOALS") {
                return true;
            }
            let was_from_env = self.db.env_var_names.contains(var_name);
            match self.db.variables.get(var_name).map(|v| v.export) {
                Some(Some(true)) => true,
                Some(Some(false)) => false,
                _ => self.db.export_all || was_from_env,
            }
        };

        // Process pattern-specific vars matching this target to find unexports.
        for psv in &self.db.pattern_specific_vars {
            if match_pattern_simple(&psv.pattern, target).is_some() {
                if psv.var.export == Some(false) {
                    unexports.insert(psv.var_name.clone());
                } else if psv.var.export == Some(true) {
                    unexports.remove(&psv.var_name);
                }
            }
        }

        // Process target-specific vars for this exact target to find unexports.
        if let Some(rules) = self.db.rules.get(target) {
            for rule in rules {
                for (var_name, var) in &rule.target_specific_vars {
                    if var.export == Some(false) {
                        unexports.insert(var_name.clone());
                    } else if var.export == Some(true) {
                        unexports.remove(var_name);
                    }
                }
            }
        }

        // For all target-specific/pattern-specific vars:
        // - If explicitly exported with 'export' keyword → add to exports
        // - If corresponding global var is exported → override global value with target-specific value
        // The final values come from collect_target_vars.
        for (var_name, (value, _is_override, _is_private)) in &target_vars {
            // Skip automatic vars and other special names.
            if var_name.len() == 1 && "@<^+?*!|%/".contains(var_name.as_str()) {
                continue;
            }
            if unexports.contains(var_name.as_str()) {
                // Explicitly unexported for this target: mark with sentinel for removal.
                // We use an empty string with a sentinel; we'll handle removal separately.
                // Actually, we need a way to signal "remove this env var". We'll use a
                // separate set (handled in setup_exports).
                continue;
            }
            // Check if the var should be exported (either explicitly or via global setting).
            // Check for explicit target-specific export.
            let target_explicitly_exported = self.db.rules.get(target)
                .map(|rules| rules.iter().any(|r| r.target_specific_vars.iter().any(|(n, v)| n == var_name && v.export == Some(true))))
                .unwrap_or(false)
                || self.db.pattern_specific_vars.iter().any(|psv| {
                    match_pattern_simple(&psv.pattern, target).is_some()
                    && &psv.var_name == var_name
                    && psv.var.export == Some(true)
                });
            if target_explicitly_exported || global_should_export(var_name) {
                exports.insert(var_name.clone(), value.clone());
            }
        }

        exports
    }

    /// Compute the set of variable names that should be explicitly unexported for this target.
    /// These override any global export settings.
    fn compute_target_unexports(&self, target: &str) -> HashSet<String> {
        let mut unexports = HashSet::new();

        // Check pattern-specific vars matching this target
        for psv in &self.db.pattern_specific_vars {
            if match_pattern_simple(&psv.pattern, target).is_some() {
                if psv.var.export == Some(false) {
                    unexports.insert(psv.var_name.clone());
                } else if psv.var.export == Some(true) {
                    unexports.remove(&psv.var_name);
                }
            }
        }

        // Check target-specific vars for this exact target
        if let Some(rules) = self.db.rules.get(target) {
            for rule in rules {
                for (var_name, var) in &rule.target_specific_vars {
                    if var.export == Some(false) {
                        unexports.insert(var_name.clone());
                    } else if var.export == Some(true) {
                        unexports.remove(var_name);
                    }
                }
            }
        }

        unexports
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

/// Split a recipe line on bare newlines (i.e., `\n` NOT preceded by `\`).
///
/// When the parser joins backslash-newline continuations in recipe lines it
/// stores them as a single string with an embedded `\\\n` (backslash followed
/// by newline).  The shell is expected to handle that continuation itself, so
/// we must NOT split there.  Plain `\n` characters (from multiline `define`
/// variables expanded into a recipe) ARE split into separate sub-commands.
fn split_recipe_sub_lines(s: &str) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    let mut current = String::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            // Check if preceded by backslash (continuation): if so, keep together.
            if current.ends_with('\\') {
                current.push('\n');
            } else {
                result.push(current.clone());
                current.clear();
            }
        } else {
            current.push(bytes[i] as char);
        }
        i += 1;
    }
    result.push(current);
    result
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

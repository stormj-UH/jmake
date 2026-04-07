// Copyright (c) 2026 Jon-Erik G. Storm. All rights reserved.
// Recipe execution engine - dependency resolution and recipe running

pub mod parallel;

use crate::cli::ShuffleMode;
use crate::database::MakeDatabase;
use crate::eval::MakeState;
use crate::functions;
use crate::types::*;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::sync::mpsc;
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
    building_stack: Vec<String>,  // ordered stack for cycle detection messages
    question_out_of_date: bool,
    errors: Vec<String>,
    progname: String,
    /// Set to true the first time any recipe is executed.
    /// Suppresses "is up to date" / "Nothing to be done" diagnostics.
    any_recipe_ran: bool,
    /// Targets that were covered as grouped siblings (not the primary target that ran the
    /// recipe). When explicitly requested, these always get "Nothing to be done" printed.
    grouped_covered: HashSet<String>,
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
    /// Shuffle mode for prerequisite ordering.
    shuffle: Option<ShuffleMode>,
    /// RNG seed for shuffle (updated after each use for seeded mode to simulate sequence).
    shuffle_seed: u64,
    /// Targets that have already failed (in keep-going mode).
    /// Avoids re-running recipes for targets that already failed in this session.
    failed_targets: HashSet<String>,
    /// When true, execute_recipe() stores a TargetPlan instead of running the recipe.
    /// Used during the graph-resolution phase of parallel builds.
    collect_plans_mode: bool,
    /// Accumulated TargetPlans during graph resolution (only Some when collect_plans_mode).
    pending_plans: Option<HashMap<String, parallel::TargetPlan>>,
    /// Resolved TargetPlans for the current parallel build (set after collect_plans()).
    /// Used by build_job_from_plan() to look up plan data.
    parallel_plans: Option<HashMap<String, parallel::TargetPlan>>,
    /// Prerequisites for the current target being resolved (set before execute_recipe).
    /// Used in collect_plans_mode to populate TargetPlan.prerequisites.
    pending_plan_prereqs: Vec<String>,
    /// Order-only prerequisites for the current target (set before execute_recipe).
    pending_plan_order_only: Vec<String>,
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
        shuffle: Option<ShuffleMode>,
    ) -> Self {
        // Initialize seed for seeded/random shuffle modes.
        let shuffle_seed = match &shuffle {
            Some(ShuffleMode::Seeded(s)) => *s,
            Some(ShuffleMode::Random) => {
                // Use a time-based seed for true randomness.
                use std::time::{SystemTime, UNIX_EPOCH};
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
                    .unwrap_or(42)
            }
            _ => 0,
        };
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
            building_stack: Vec::new(),
            question_out_of_date: false,
            errors: Vec::new(),
            progname,
            any_recipe_ran: false,
            grouped_covered: HashSet::new(),
            intermediate_built: Vec::new(),
            top_level_targets: HashSet::new(),
            what_if,
            inherited_vars_stack: Vec::new(),
            target_extra_exports: HashMap::new(),
            target_extra_unexports: HashSet::new(),
            shuffle,
            shuffle_seed,
            failed_targets: HashSet::new(),
            collect_plans_mode: false,
            pending_plans: None,
            parallel_plans: None,
            pending_plan_prereqs: Vec::new(),
            pending_plan_order_only: Vec::new(),
        }
    }

    pub fn build_targets(&mut self, targets: &[String]) -> Result<(), String> {
        // Dispatch to parallel or sequential path based on job count and .NOTPARALLEL.
        if self.jobs > 1 && !self.db.not_parallel {
            return self.build_targets_parallel(targets);
        }
        self.build_targets_sequential(targets)
    }

    /// Sequential build path (existing logic, unchanged from original build_targets).
    /// Used when jobs == 1 or .NOTPARALLEL is set.
    fn build_targets_sequential(&mut self, targets: &[String]) -> Result<(), String> {
        // Record top-level targets so they are not deleted even if .INTERMEDIATE
        for t in targets {
            self.top_level_targets.insert(t.clone());
        }
        // Apply shuffle to goal ordering (unless .NOTPARALLEL is set).
        let targets_list = if self.shuffle.is_some() && !self.db.not_parallel {
            self.shuffle_list(targets.to_vec())
        } else {
            targets.to_vec()
        };
        let targets = &targets_list;
        let mut failed_top_level_targets: Vec<String> = Vec::new();
        for target in targets {
            match self.build_target(target) {
                Ok(rebuilt) => {
                    // Print status for top-level targets that weren't rebuilt,
                    // but ONLY when no recipe ran anywhere in this make session.
                    // GNU Make suppresses these messages when any work was done
                    // (even for unrelated or order-only prerequisites).
                    // Print "Nothing to be done" / "is up to date" when not rebuilt.
                    // Normally suppressed when any recipe ran, but grouped-covered siblings
                    // always get the message (GNU Make prints it for them even after recipes ran).
                    let is_grouped_covered = self.grouped_covered.contains(target);
                    if !rebuilt && !self.silent && !self.question
                        && (!self.any_recipe_ran || is_grouped_covered)
                    {
                        // Grouped-covered siblings always get "Nothing to be done" since
                        // they were covered by the group recipe, not individually built.
                        let has_recipe = !is_grouped_covered && self.target_has_recipe(target);
                        if has_recipe {
                            println!("{}: '{}' is up to date.", self.progname, target);
                        } else {
                            println!("{}: Nothing to be done for '{}'.", self.progname, target);
                        }
                    }
                }
                Err(e) => {
                    if self.keep_going {
                        failed_top_level_targets.push(target.clone());
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
            // Print "Target 'X' not remade because of errors." without "***",
            // matching GNU Make's output format.
            let target_list: String = if failed_top_level_targets.len() == 1 {
                format!("'{}'", failed_top_level_targets[0])
            } else {
                failed_top_level_targets.iter()
                    .map(|t| format!("'{}'", t))
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            eprintln!("{}: Target {} not remade because of errors.", self.progname, target_list);
            // Return an empty error so main.rs does not print a duplicate message.
            return Err(String::new());
        }

        if self.question && self.question_out_of_date {
            std::process::exit(1);
        }

        Ok(())
    }

    /// Parallel build path: used when jobs > 1 and .NOTPARALLEL is not set.
    ///
    /// Phase 1: Sequential graph resolution on the main thread to compute TargetPlans,
    /// then parallel execution with a thread pool.
    fn build_targets_parallel(&mut self, targets: &[String]) -> Result<(), String> {
        use parallel::{ParallelScheduler, TargetState};

        // Record top-level targets for intermediate deletion.
        for t in targets {
            self.top_level_targets.insert(t.clone());
        }

        // ------------------------------------------------------------------
        // Phase 1: Graph resolution (sequential, main thread)
        //
        // Run the full build graph walk in "plan collection" mode.
        // execute_recipe() stores TargetPlans instead of spawning processes.
        // After this call, all TargetPlans are in self.parallel_plans.
        // ------------------------------------------------------------------
        let plans = self.collect_plans(targets)?;

        // DEBUG: show collected plans
        eprintln!("[DBG] collect_plans returned {} plans:", plans.len());
        for (k, v) in &plans {
            eprintln!("[DBG]   plan {:?}: needs_rebuild={}, recipe_lines={}", k, v.needs_rebuild, v.recipe.len());
        }

        // Store plans so build_job_from_plan can look them up.
        self.parallel_plans = Some(plans);

        // Snapshot execution parameters before moving anything.
        let env_ops = self.build_env_ops_for_workers();
        let shell = self.shell.to_string();
        let shell_flags = self.shell_flags.to_string();
        let makelevel = self.get_makelevel();
        let gnumakeflags_was_set = self.state.args.gnumakeflags_was_set;
        let one_shell = self.db.one_shell;
        let delete_on_error = self.db.special_targets.contains_key(&SpecialTarget::DeleteOnError);

        // ------------------------------------------------------------------
        // Phase 2: Set up worker thread pool and scheduler
        // ------------------------------------------------------------------
        let num_workers = self.jobs;
        let (job_tx, job_rx) = mpsc::channel::<parallel::Job>();
        let (result_tx, result_rx) = mpsc::channel::<parallel::JobResult>();
        let job_rx_shared = Arc::new(Mutex::new(job_rx));

        // Move plans into the scheduler (clone from parallel_plans).
        let sched_plans = self.parallel_plans.take().unwrap_or_default();
        self.parallel_plans = Some(sched_plans.clone()); // keep a copy for build_job_from_plan

        let workers = parallel::spawn_workers(num_workers, job_rx_shared, result_tx);

        let mut scheduler = ParallelScheduler::new(
            self.jobs,
            sched_plans,
            job_tx,
            result_rx,
            self.keep_going,
            self.progname.clone(),
        );

        scheduler.find_initial_ready(targets);

        // DEBUG: show initial ready queue
        eprintln!("[DBG] initial ready_queue: {:?}", scheduler.ready_queue);
        eprintln!("[DBG] initial states: {:?}", scheduler.states.keys().collect::<Vec<_>>());

        // ------------------------------------------------------------------
        // Phase 3: Scheduler main loop
        // ------------------------------------------------------------------
        loop {
            // Launch jobs up to the slot limit.
            while scheduler.should_launch() {
                let target = match scheduler.pop_ready() {
                    Some(t) => t,
                    None => break,
                };

                let job = self.build_job_from_plan(
                    &target,
                    &env_ops,
                    &shell,
                    &shell_flags,
                    &makelevel,
                    gnumakeflags_was_set,
                    one_shell,
                    delete_on_error,
                );

                match job {
                    Some(j) => {
                        eprintln!("[DBG] launching job: {:?}", target);
                        scheduler.states.insert(target.clone(), TargetState::Running);
                        scheduler.running_count += 1;
                        scheduler.send_job(j);
                    }
                    None => {
                        eprintln!("[DBG] no-recipe target: {:?}", target);
                        // No recipe or not needed — complete immediately without
                        // going through a worker thread.
                        // We mark it done and propagate to dependents directly.
                        // Do NOT call handle_completion (which does running_count -= 1).
                        scheduler.states.insert(target.clone(), TargetState::Done(false));
                        // Propagate to dependents.
                        if let Some(deps) = scheduler.dependents_of.get(&target).cloned() {
                            for dep in deps {
                                if !scheduler.states.contains_key(dep.as_str()) {
                                    if scheduler.all_prereqs_done_pub(&dep) {
                                        scheduler.states.insert(dep.clone(), TargetState::Ready);
                                        scheduler.ready_queue.push_back(dep);
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // If nothing is running and nothing is queued, we're done.
            if !scheduler.has_work() {
                break;
            }

            // Wait for a result from a worker.
            match scheduler.recv_result() {
                Some(result) => {
                    scheduler.handle_completion(result);
                }
                None => break, // all workers exited unexpectedly
            }
        }

        // Drain any in-flight jobs (handles error + remaining workers).
        scheduler.drain_running();

        // Close the job channel so worker threads can exit.
        // The scheduler holds job_tx; dropping scheduler closes it.
        // But we need to explicitly drop job_tx from the scheduler first.
        // Actually, scheduler's job_tx will be dropped when scheduler goes out of scope.
        // Force it now so workers see the closed channel:
        {
            let _ = scheduler.job_tx.clone(); // just to reference it; it's dropped with scheduler
        }

        // Wait for all worker threads to finish.
        // First we must drop the scheduler (and its job_tx) to signal workers.
        let any_recipe_ran = scheduler.any_recipe_ran;
        let intermediate_built = scheduler.intermediate_built.clone();
        let final_error = scheduler.final_error();
        let target_states = scheduler.states.clone();

        // Drop scheduler (closes job_tx, workers will exit their recv loop).
        drop(scheduler);

        for w in workers {
            let _ = w.join();
        }

        // Clean up parallel plans.
        self.parallel_plans = None;

        // ------------------------------------------------------------------
        // Phase 4: Post-build reporting
        // ------------------------------------------------------------------

        if let Some(e) = final_error {
            self.delete_intermediate_files();
            if e.is_empty() {
                return Err(String::new());
            }
            return Err(e);
        }

        // Print "is up to date" / "Nothing to be done" for top-level targets.
        for target in targets {
            if let Some(TargetState::Done(rebuilt)) = target_states.get(target.as_str()) {
                if !rebuilt && !self.silent && !self.question && !any_recipe_ran {
                    let has_recipe = self.target_has_recipe(target);
                    if has_recipe {
                        println!("{}: '{}' is up to date.", self.progname, target);
                    } else {
                        println!("{}: Nothing to be done for '{}'.", self.progname, target);
                    }
                }
            }
        }

        // Track intermediate targets for deletion.
        for t in &intermediate_built {
            if !self.intermediate_built.contains(t) {
                self.intermediate_built.push(t.clone());
            }
        }
        self.delete_intermediate_files();

        Ok(())
    }

    /// Build the global environment ops list for worker threads.
    /// Returns a Vec of (name, Some(value)) to set or (name, None) to remove.
    fn build_env_ops_for_workers(&self) -> Vec<(String, Option<String>)> {
        use crate::types::VarOrigin;
        let mut ops: Vec<(String, Option<String>)> = Vec::new();
        for (name, var) in &self.db.variables {
            if name == "MAKELEVEL" { continue; } // set separately
            let always_export = matches!(name.as_str(), "MAKEFLAGS" | "MAKE" | "MAKECMDGOALS");
            let was_from_env = self.db.env_var_names.contains(name.as_str());
            let should_export = !var.is_private && (always_export || match var.export {
                Some(true) => true,
                Some(false) => false,
                None => {
                    if self.db.unexport_all {
                        was_from_env && var.origin == VarOrigin::Environment
                    } else {
                        self.db.export_all || was_from_env
                    }
                }
            });
            if should_export {
                let value = self.state.expand(&var.value);
                ops.push((name.clone(), Some(value)));
            } else {
                ops.push((name.clone(), None));
            }
        }
        ops
    }

    /// Build a Job from a TargetPlan for dispatch to a worker thread.
    /// Returns None if the target doesn't need rebuilding (will be marked Done).
    fn build_job_from_plan(
        &self,
        target: &str,
        env_ops: &[(String, Option<String>)],
        shell: &str,
        shell_flags: &str,
        makelevel: &str,
        gnumakeflags_was_set: bool,
        one_shell: bool,
        delete_on_error: bool,
    ) -> Option<parallel::Job> {
        let plans = self.parallel_plans.as_ref()?;
        let plan = plans.get(target)?;

        if plan.recipe.is_empty() {
            return None; // no recipe
        }

        // Determine effective shell/flags for this target.
        // (target-specific SHELL vars were baked into auto_vars during resolve)
        let eff_shell = plan.auto_vars.get("SHELL").map(|s| s.as_str()).unwrap_or(shell);
        let eff_flags = plan.auto_vars.get(".SHELLFLAGS").map(|s| s.as_str()).unwrap_or(shell_flags);

        // Pre-expand recipe lines using the stored auto_vars.
        // The recipe lines in TargetPlan are already expanded (done during collect_plans).
        let pre_expanded: Vec<(usize, String, Vec<String>)> = plan.recipe.iter().map(|(ln, line)| {
            let sub_lines = parallel::split_recipe_sub_lines_standalone(line);
            (*ln, line.clone(), sub_lines)
        }).collect();

        Some(parallel::Job {
            target: plan.target.clone(),
            pre_expanded,
            source_file: plan.source_file.clone(),
            shell: eff_shell.to_string(),
            shell_flags: eff_flags.to_string(),
            is_silent_target: self.db.is_silent_target(target),
            silent: self.silent,
            ignore_errors: self.ignore_errors,
            dry_run: self.dry_run,
            touch: self.touch,
            trace: self.trace,
            one_shell,
            delete_on_error,
            is_precious: self.db.is_precious(target),
            progname: self.progname.clone(),
            makelevel: makelevel.to_string(),
            env_ops: env_ops.to_vec(),
            extra_exports: plan.extra_exports.clone(),
            extra_unexports: plan.extra_unexports.clone(),
            gnumakeflags_was_set,
        })
    }

    /// Collect TargetPlans for all targets reachable from `roots` without executing recipes.
    /// This is Phase 1 of the parallel build: sequential graph resolution.
    ///
    /// Strategy: we leverage the existing build_target() machinery, but in a "plan collection"
    /// mode where execute_recipe() stores the plan instead of running shell commands.
    /// We set `self.collect_plans_mode = true` and `self.pending_plans = Some(map)` before
    /// calling build_targets_sequential(), then extract the collected plans.
    fn collect_plans(
        &mut self,
        roots: &[String],
    ) -> Result<HashMap<String, parallel::TargetPlan>, String> {
        // Enable plan collection mode.
        self.collect_plans_mode = true;
        self.pending_plans = Some(HashMap::new());

        // Run the sequential build in dry-run-like mode (it won't actually execute
        // recipes because execute_recipe checks collect_plans_mode and stores plans).
        // We need this to fully resolve dependencies, second expansion, pattern rules, etc.
        let result = self.build_targets_sequential(roots);

        // Disable plan collection mode and extract plans.
        self.collect_plans_mode = false;
        let plans = self.pending_plans.take().unwrap_or_default();

        // Reset built/building state so the actual parallel execution starts fresh.
        // (The sequential "dry run" marked targets as built — we need to undo that.)
        self.built.clear();
        self.building.clear();
        self.building_stack.clear();
        self.failed_targets.clear();
        self.errors.clear();
        self.any_recipe_ran = false;
        self.grouped_covered.clear();
        self.intermediate_built.clear();

        // If the sequential pass failed (e.g., missing rule), propagate the error.
        // But note: in collect_plans_mode, errors from missing recipes are soft
        // (we still want to continue to collect what we can).
        // For now, propagate hard errors (missing files with no rule).
        match result {
            Err(e) if !e.is_empty() => return Err(e),
            _ => {}
        }

        Ok(plans)
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

        // Already failed (in keep-going mode)?  Don't retry.
        if self.failed_targets.contains(target) {
            return Err(String::new());
        }

        // If this target was "imagined" as updated during the include phase (sv 61226),
        // treat it as if it was already built (not actually rebuilt = no file created).
        // This prevents re-running the recipe that already ran during include processing.
        if self.state.include_imagined.contains(target) {
            self.built.insert(target.to_string(), false);
            return Ok(false);
        }

        // Cycle detection
        if self.building.contains(target) {
            // The "requester" is the most recent target on the building_stack:
            // GNU Make prints "Circular A <- B" where A is the target currently
            // being built (last on the stack) and B is the already-in-progress target.
            let requester = self.building_stack.last()
                .map(|s| s.as_str())
                .unwrap_or(target);
            eprintln!("{}: Circular {} <- {} dependency dropped.", self.progname, requester, target);
            return Ok(false);
        }
        self.building.insert(target.to_string());
        self.building_stack.push(target.to_string());

        let result = self.build_target_inner(target);

        self.building_stack.pop();
        self.building.remove(target);

        match &result {
            Ok(rebuilt) => {
                self.built.insert(target.to_string(), *rebuilt);
            }
            Err(_) => {
                // Record this target as failed so we don't retry it in keep-going mode.
                self.failed_targets.insert(target.to_string());
            }
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
        if let Some(ref rules) = rules {
            if !rules.is_empty() {
                // Check if these are double-colon rules.  Target-specific variable
                // placeholders (single-colon, no recipe) may appear first in the list
                // before the actual :: rules, so check any() rather than first().
                let is_double_colon = rules.iter().any(|r| r.is_double_colon);
                if is_double_colon {
                    // For double-colon rules, each rule is independent.
                    // If a rule has grouped_siblings, treat it as its own grouped invocation.
                    // This handles `a b c&::; X` + `c d e&::; Y` where c has two groups.
                    let rules_clone = rules.clone();
                    let any_rule_is_grouped = rules_clone.iter().any(|r| !r.grouped_siblings.is_empty());
                    if any_rule_is_grouped {
                        let mut any_rebuilt = false;
                        for rule in &rules_clone {
                            // Compute this rule's siblings (not yet built, not already building)
                            let rule_siblings: Vec<String> = rule.grouped_siblings.iter()
                                .filter(|s| !self.built.contains_key(s.as_str())
                                         && !self.building.contains(s.as_str()))
                                .cloned()
                                .collect();
                            let result = self.build_with_rules_grouped(
                                target, std::slice::from_ref(rule), is_phony, &rule_siblings
                            );
                            match result {
                                Ok(rebuilt) => { if rebuilt { any_rebuilt = true; } }
                                Err(e) => return Err(e),
                            }
                        }
                        return Ok(any_rebuilt);
                    }
                }
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
            if self.collect_plans_mode {
                self.record_leaf_plan(target, is_phony);
            }
            return Ok(false);
        }

        // Try VPATH
        if let Some(_found) = self.find_in_vpath(target) {
            if self.collect_plans_mode {
                self.record_leaf_plan(target, is_phony);
            }
            return Ok(false);
        }

        // A phony target with no recipe and no file is still "successfully built" - it's
        // a no-op target. This allows .PHONY targets without recipes to be prerequisites.
        if is_phony {
            if self.collect_plans_mode {
                self.record_leaf_plan(target, is_phony);
            }
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
                if self.collect_plans_mode {
                    self.pending_plan_prereqs = Vec::new();
                    self.pending_plan_order_only = Vec::new();
                }
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
        // Use any() instead of first() to handle the case where a target-specific
        // variable placeholder (single-colon) appears before the actual :: rules.
        if rules.iter().any(|r| r.is_double_colon) {
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
            self.apply_target_vars_to_auto_vars(&collected_target_vars.0, &mut base_auto_vars);

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

        // Apply shuffle to prerequisite ordering (unless .NOTPARALLEL is set).
        if self.shuffle.is_some() && !self.db.not_parallel {
            all_prereqs = self.shuffle_list(all_prereqs);
            all_order_only = self.shuffle_list(all_order_only);
            se_expanded_prereqs = self.shuffle_list(se_expanded_prereqs);
            se_expanded_order_only = self.shuffle_list(se_expanded_order_only);
        }

        // Pre-check: if the target exists and none of the prereqs (by effective mtime, which
        // accounts for deleted intermediates) are newer than the target, skip rebuilding.
        // This handles the case where intermediate files were deleted after a previous build:
        // they should not cause an unnecessary rebuild if their sources are still old.
        // Only applicable for non-phony targets with no SE prereqs and no always-make.
        //
        // When the recipe is empty (no recipe from explicit rules), we also peek at the
        // pattern rule's prerequisites to include them in the check, so that
        // .NOTINTERMEDIATE pattern-rule prereqs correctly trigger a rebuild.
        if !is_phony && !self.always_make
            && se_prereq_texts.is_empty() && se_order_only_texts.is_empty() {
            if let Some(target_time) = get_mtime(target).or_else(|| {
                self.find_in_vpath(target).and_then(|f| get_mtime(&f))
            }) {
                // Start with the explicitly-collected prereqs.
                let mut check_prereqs: Vec<String> = all_prereqs.clone();

                // If there is no recipe from explicit rules yet, peek at the pattern rule
                // to include its prereqs in the check (read-only, no building).
                if recipe.is_empty() {
                    if let Some((pat_rule, stem)) = self.find_pattern_rule_inner(target, &all_prereqs) {
                        for p in &pat_rule.prerequisites {
                            if p != ".WAIT" {
                                check_prereqs.push(subst_stem_in_prereq(p, &stem));
                            }
                        }
                    }
                }

                let any_prereq_newer = check_prereqs.iter().any(|p| {
                    if p == ".WAIT" { return false; }
                    if self.what_if.iter().any(|w| w == p) { return true; }
                    // Also check VPATH-resolved path against what-if list
                    if let Some(ref vp) = self.find_in_vpath(p) {
                        if self.what_if.iter().any(|w| w == vp) { return true; }
                    }
                    if self.db.is_phony(p) { return true; }
                    // Use effective_mtime to handle deleted intermediates
                    match self.effective_mtime(p, 0) {
                        Some(pt) => pt > target_time,
                        None => {
                            // File doesn't exist and has no known sources to determine mtime.
                            // For secondary/intermediate files whose sources are all up to date
                            // (effective_mtime could not be determined), we can skip.
                            // But for explicitly-mentioned files and .NOTINTERMEDIATE files
                            // that are absent, we MUST build (they are "infinitely new").
                            // Also treat any non-existent file with a rule as needing rebuild:
                            // the rule will be run, and the target may then be out of date.
                            if self.db.is_secondary(p) && !self.db.is_notintermediate(p) {
                                return false; // secondary missing file: skip
                            }
                            if self.db.is_intermediate(p) && !self.db.is_notintermediate(p) {
                                return false; // deleted intermediate: skip (sources up to date)
                            }
                            // Regular file that doesn't exist: must be built, treat as newer.
                            true
                        }
                    }
                });
                if !any_prereq_newer {
                    return Ok(false);
                }
            }
        }

        // Push this target's "for_prereqs" vars onto the inheritance stack.
        // Use for_prereqs (index 1) so that private target-specific vars don't block
        // ancestor non-private vars from propagating to prerequisites.
        let my_target_vars = self.collect_target_vars(target);
        self.inherited_vars_stack.push(my_target_vars.1);

        let mut any_prereq_rebuilt = false;
        let mut prereq_errors = Vec::new();

        // Step 0: build .EXTRA_PREREQS first (before regular prereqs).
        // Extra prereqs are built before regular prereqs but are excluded from
        // automatic variables ($^, $<, $+, $?, $|).
        let extra_prereqs = self.get_extra_prereqs(target);
        for prereq in &extra_prereqs {
            match self.build_target(prereq) {
                Ok(rebuilt) => {
                    if rebuilt { any_prereq_rebuilt = true; }
                }
                Err(e) => {
                    let is_new = e.starts_with("No rule to make target '") && !e.contains(", needed by '");
                    let propagated = if is_new {
                        let base = e.trim_end_matches(".  Stop.").trim_end_matches(".");
                        format!("{}, needed by '{}'.  Stop.", base, target)
                    } else {
                        e
                    };
                    if self.keep_going {
                        if is_new { self.print_error_keep_going(&propagated); }
                        prereq_errors.push(propagated);
                    } else {
                        self.inherited_vars_stack.pop();
                        return Err(propagated);
                    }
                }
            }
        }

        // Step 1: build non-SE normal prereqs
        for prereq in all_prereqs.clone() {
            match self.build_target(&prereq) {
                Ok(rebuilt) => {
                    if rebuilt { any_prereq_rebuilt = true; }
                }
                Err(e) => {
                    let is_new = e.starts_with("No rule to make target '") && !e.contains(", needed by '");
                    let propagated = if is_new {
                        let base = e.trim_end_matches(".  Stop.").trim_end_matches(".");
                        format!("{}, needed by '{}'.  Stop.", base, target)
                    } else {
                        e
                    };
                    if self.keep_going {
                        // Only print at the first occurrence (when we just added "needed by").
                        if is_new { self.print_error_keep_going(&propagated); }
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
                    let is_new = e.starts_with("No rule to make target '") && !e.contains(", needed by '");
                    let propagated = if is_new {
                        let base = e.trim_end_matches(".  Stop.").trim_end_matches(".");
                        format!("{}, needed by '{}'.  Stop.", base, target)
                    } else {
                        e
                    };
                    if self.keep_going {
                        if is_new { self.print_error_keep_going(&propagated); }
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
                    .map(|p| subst_stem_in_prereq(p, &stem))
                    .collect();
                let mut pat_order_only: Vec<String> = pattern_rule.order_only_prerequisites.iter()
                    .map(|p| subst_stem_in_prereq(p, &stem))
                    .collect();

                // Handle second expansion for the pattern rule.
                // Auto vars are built from the ALREADY-accumulated explicit prereqs
                // (all_prereqs at this point), giving $+ the value from the explicit
                // rule(s) - which is what GNU Make uses for $+ in SE pattern rules.
                if pattern_rule.second_expansion_prereqs.is_some() || pattern_rule.second_expansion_order_only.is_some() {
                    let oo_refs: Vec<&str> = all_order_only.iter().map(|s| s.as_str()).collect();
                    let mut pat_se_auto_vars = self.make_auto_vars(target, &all_prereqs, &oo_refs, &stem);
                    let collected_target_vars = self.collect_target_vars(target);
                    self.apply_target_vars_to_auto_vars(&collected_target_vars.0, &mut pat_se_auto_vars);

                    if let Some(ref text) = pattern_rule.second_expansion_prereqs {
                        let stem_subst = subst_stem_in_se_text(text, &stem);
                        let (normal, oo) = self.second_expand_prereqs(&stem_subst, &pat_se_auto_vars, target);
                        pat_prereqs.extend(normal);
                        pat_order_only.extend(oo);
                    }
                    if let Some(ref text) = pattern_rule.second_expansion_order_only {
                        let stem_subst = subst_stem_in_se_text(text, &stem);
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
                // In plan collection mode, still record this target's dependency info.
                if self.collect_plans_mode {
                    let oo_refs: Vec<&str> = all_order_only.iter().map(|s| s.as_str()).collect();
                    let auto_vars_for_plan = self.make_auto_vars(target, &all_prereqs, &oo_refs, "");
                    let plan = parallel::TargetPlan {
                        target: target.to_string(),
                        prerequisites: all_prereqs.clone(),
                        order_only: all_order_only.clone(),
                        recipe: Vec::new(),
                        source_file: String::new(),
                        auto_vars: auto_vars_for_plan,
                        is_phony,
                        needs_rebuild: false, // no recipe = no rebuild
                        grouped_primary: None,
                        grouped_siblings: Vec::new(),
                        extra_exports: HashMap::new(),
                        extra_unexports: Vec::new(),
                        is_intermediate: self.db.is_intermediate(target),
                    };
                    if let Some(ref mut plans) = self.pending_plans {
                        plans.insert(target.to_string(), plan);
                    }
                }
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

        // Determine if we need to rebuild.
        // Include extra_prereqs in the rebuild check (they count for mtime comparison)
        // but they are NOT passed to make_auto_vars (excluded from $^, $<, etc.).
        let all_prereqs_for_rebuild = {
            let mut v = extra_prereqs.clone();
            v.extend(all_prereqs.clone());
            v
        };
        let needs_rebuild = if self.always_make || is_phony {
            true
        } else {
            self.needs_rebuild(target, &all_prereqs_for_rebuild, any_prereq_rebuilt)
        };

        // In plan collection mode, record a plan for this target regardless of needs_rebuild.
        // This ensures ALL targets (including those with no recipe or no rebuild) are in
        // the dependency graph so the parallel scheduler can track completion ordering.
        if self.collect_plans_mode {
            let oo_refs: Vec<&str> = all_order_only.iter().map(|s| s.as_str()).collect();
            let auto_vars_for_plan = self.make_auto_vars(target, &all_prereqs, &oo_refs, &pattern_stem);
            let plan = parallel::TargetPlan {
                target: target.to_string(),
                prerequisites: all_prereqs.clone(),
                order_only: all_order_only.clone(),
                recipe: Vec::new(), // recipe stored by execute_recipe; empty if not needed
                source_file: recipe_source_file.clone(),
                auto_vars: auto_vars_for_plan,
                is_phony,
                needs_rebuild,
                grouped_primary: None,
                grouped_siblings: Vec::new(),
                extra_exports: self.compute_target_exports(target),
                extra_unexports: self.compute_target_unexports(target).into_iter().collect(),
                is_intermediate: self.db.is_intermediate(target),
            };
            if let Some(ref mut plans) = self.pending_plans {
                plans.insert(target.to_string(), plan);
            }
        }

        if !needs_rebuild {
            return Ok(false);
        }

        // Set up automatic variables (extra_prereqs are excluded from auto vars).
        let oo_refs: Vec<&str> = all_order_only.iter().map(|s| s.as_str()).collect();
        let mut auto_vars = self.make_auto_vars(target, &all_prereqs, &oo_refs, &pattern_stem);

        // Merge target-specific and pattern-specific variables into auto_vars,
        // respecting command-line variable priority and override semantics.
        let collected_target_vars = self.collect_target_vars(target);
        self.apply_target_vars_to_auto_vars(&collected_target_vars.0, &mut auto_vars);

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
        // In plan collection mode, set prereqs so execute_recipe can update the plan
        // with the expanded recipe (the plan was already recorded above, but execute_recipe
        // will update it with the actual recipe lines).
        if self.collect_plans_mode {
            self.pending_plan_prereqs = all_prereqs.clone();
            self.pending_plan_order_only = all_order_only.clone();
        }
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
            self.apply_target_vars_to_auto_vars(&collected_target_vars.0, &mut base_auto_vars);

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
        self.inherited_vars_stack.push(my_target_vars.1);

        let mut any_prereq_rebuilt = false;
        let mut prereq_errors = Vec::new();

        for prereq in all_prereqs.clone() {
            match self.build_target(&prereq) {
                Ok(rebuilt) => {
                    if rebuilt { any_prereq_rebuilt = true; }
                }
                Err(e) => {
                    let is_new = e.starts_with("No rule to make target '") && !e.contains(", needed by '");
                    let propagated = if is_new {
                        let base = e.trim_end_matches(".  Stop.").trim_end_matches(".");
                        format!("{}, needed by '{}'.  Stop.", base, target)
                    } else {
                        e
                    };
                    if self.keep_going {
                        if is_new { self.print_error_keep_going(&propagated); }
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
                    let is_new = e.starts_with("No rule to make target '") && !e.contains(", needed by '");
                    let propagated = if is_new {
                        let base = e.trim_end_matches(".  Stop.").trim_end_matches(".");
                        format!("{}, needed by '{}'.  Stop.", base, target)
                    } else {
                        e
                    };
                    if self.keep_going {
                        if is_new { self.print_error_keep_going(&propagated); }
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
                    .map(|p| subst_stem_in_prereq(p, &stem))
                    .collect();
                let mut pat_order_only: Vec<String> = pattern_rule.order_only_prerequisites.iter()
                    .map(|p| subst_stem_in_prereq(p, &stem))
                    .collect();

                if pattern_rule.second_expansion_prereqs.is_some() || pattern_rule.second_expansion_order_only.is_some() {
                    let oo_refs: Vec<&str> = all_order_only.iter().map(|s| s.as_str()).collect();
                    let mut pat_se_auto_vars = self.make_auto_vars(target, &all_prereqs, &oo_refs, &stem);
                    let collected_target_vars = self.collect_target_vars(target);
                    self.apply_target_vars_to_auto_vars(&collected_target_vars.0, &mut pat_se_auto_vars);

                    if let Some(ref text) = pattern_rule.second_expansion_prereqs {
                        let stem_subst = subst_stem_in_se_text(text, &stem);
                        let (normal, oo) = self.second_expand_prereqs(&stem_subst, &pat_se_auto_vars, target);
                        pat_prereqs.extend(normal);
                        pat_order_only.extend(oo);
                    }
                    if let Some(ref text) = pattern_rule.second_expansion_order_only {
                        let stem_subst = subst_stem_in_se_text(text, &stem);
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
        // For grouped targets, also rebuild if any sibling is missing or out of date.
        let needs_rebuild = if self.always_make || is_phony {
            true
        } else if self.needs_rebuild(target, &all_prereqs, any_prereq_rebuilt) {
            true
        } else {
            // Check if any sibling needs rebuilding (missing or older than a prereq).
            grouped_siblings.iter().any(|sib| {
                let sib_rules = self.db.rules.get(sib.as_str()).cloned().unwrap_or_default();
                let sib_prereqs: Vec<String> = sib_rules.iter()
                    .flat_map(|r| r.prerequisites.iter().cloned())
                    .collect();
                self.needs_rebuild(sib, &sib_prereqs, any_prereq_rebuilt)
            })
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
        self.apply_target_vars_to_auto_vars(&collected_target_vars.0, &mut auto_vars);

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
        if self.collect_plans_mode {
            self.pending_plan_prereqs = all_prereqs.clone();
            self.pending_plan_order_only = all_order_only.clone();
        }
        let result = self.execute_recipe(target, &recipe, &recipe_source_file, &auto_vars, is_phony);
        self.target_extra_exports.clear();
        self.target_extra_unexports.clear();

        let rebuilt = result.as_ref().copied().unwrap_or(false);
        for (sibling, _) in &sibling_rules {
            // Mark siblings as "covered" (not independently rebuilt) so that
            // when they are requested as top-level targets, "Nothing to be done" is printed.
            self.grouped_covered.insert(sibling.clone());
            self.built.insert(sibling.clone(), false);
        }
        let _ = rebuilt; // primary target's rebuilt status is propagated by build_target()

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
            self.inherited_vars_stack.push(my_target_vars.1);

            let mut any_prereq_rebuilt = false;
            let mut prereq_errors = Vec::new();

            for prereq in &prereqs {
                match self.build_target(prereq) {
                    Ok(rebuilt) => {
                        if rebuilt { any_prereq_rebuilt = true; }
                    }
                    Err(e) => {
                        let is_new = e.starts_with("No rule to make target '") && !e.contains(", needed by '");
                        let propagated = if is_new {
                            let base = e.trim_end_matches(".  Stop.").trim_end_matches(".");
                            format!("{}, needed by '{}'.  Stop.", base, target)
                        } else {
                            e
                        };
                        if self.keep_going {
                            if is_new { self.print_error_keep_going(&propagated); }
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
            self.apply_target_vars_to_auto_vars(&collected_target_vars.0, &mut auto_vars);

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
            if self.collect_plans_mode {
                self.pending_plan_prereqs = prereqs.clone();
                self.pending_plan_order_only = order_only.clone();
            }
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
        // For grouped pattern rules (`a.% b.%&: ; recipe`): compute concrete sibling targets
        // by substituting % with the stem, then filter out the current target and already-built ones.
        let concrete_grouped_siblings: Vec<String> = rule.grouped_siblings.iter()
            .map(|pat| pat.replace('%', stem))
            .filter(|s| s != target
                    && !self.built.contains_key(s.as_str())
                    && !self.building.contains(s.as_str()))
            .collect();

        // For multi-target pattern rules (e.g. `%.h %.c: %.in`): compute "also_make" targets.
        // These are the concrete sibling targets (other target patterns with same stem).
        // When the recipe runs for `target`, all also_make siblings are considered built too.
        // This is different from grouped targets (&:): here there's no coordination,
        // we just mark them as covered after the recipe runs.
        // Only applies when there are multiple targets AND no grouped_siblings (not &:).
        let also_make_siblings: Vec<String> = if rule.grouped_siblings.is_empty() && rule.targets.len() > 1 {
            rule.targets.iter()
                .filter(|pat| {
                    // Only include patterns that match target (same stem)
                    match_pattern(pat, target).is_none()
                })
                .map(|pat| replace_first_percent(pat, stem))
                .filter(|s| s != target)
                .collect()
        } else {
            Vec::new()
        };

        // Expand pattern prerequisites using the stem.
        // For normal (non-SE) prerequisites, substitute % with the stem.
        // .WAIT markers are filtered since they are ordering hints, not real targets.
        let mut prereqs: Vec<String> = rule.prerequisites.iter()
            .filter(|p| p.as_str() != ".WAIT")
            .map(|p| subst_stem_in_prereq(p, stem))
            .collect();

        // Also expand any explicit prerequisites that came from `build_target_inner`
        // combining explicit rules with this pattern rule.
        // (Already handled via all_prereqs in build_target_inner for explicit rules.)

        // Handle second-expansion prerequisites for pattern rules.
        let mut order_only: Vec<String> = rule.order_only_prerequisites.iter()
            .filter(|p| p.as_str() != ".WAIT")
            .map(|p| subst_stem_in_prereq(p, stem))
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
                // Substitute first % per word in the raw text for the stem before SE expanding
                let stem_subst = subst_stem_in_se_text(text, stem);
                let (normal, oo) = self.second_expand_prereqs(&stem_subst, &base_auto_vars, target);
                prereqs.extend(normal);
                order_only.extend(oo);
            }
            if let Some(ref text) = rule.second_expansion_order_only {
                let stem_subst = subst_stem_in_se_text(text, stem);
                let (normal, oo) = self.second_expand_prereqs(&stem_subst, &base_auto_vars, target);
                order_only.extend(normal);
                order_only.extend(oo);
            }
            // Restore file context after pattern rule SE expansion.
            *self.state.current_file.borrow_mut() = saved_file;
            *self.state.current_line.borrow_mut() = saved_line;
        }

        // Apply shuffle to pattern rule prerequisites.
        if self.shuffle.is_some() && !self.db.not_parallel {
            prereqs = self.shuffle_list(prereqs);
            order_only = self.shuffle_list(order_only);
        }

        // Build prerequisites
        // Push target vars onto stack for inheritance by prerequisites.
        let my_target_vars = self.collect_target_vars(target);
        self.inherited_vars_stack.push(my_target_vars.1);

        let mut any_rebuilt = false;

        // Step 0: build .EXTRA_PREREQS first.
        let extra_prereqs = self.get_extra_prereqs(target);
        for prereq in &extra_prereqs {
            match self.build_target(prereq) {
                Ok(rebuilt) => {
                    if rebuilt { any_rebuilt = true; }
                }
                Err(e) => {
                    let propagated = if e.starts_with("No rule to make target '") && !e.contains(", needed by '") {
                        let base = e.trim_end_matches(".  Stop.").trim_end_matches(".");
                        format!("{}, needed by '{}'.  Stop.", base, target)
                    } else {
                        e
                    };
                    if self.keep_going {
                        // keep going: ignore error and continue
                    } else {
                        self.inherited_vars_stack.pop();
                        return Err(propagated);
                    }
                }
            }
        }

        for prereq in prereqs.clone() {
            match self.build_target(&prereq) {
                Ok(rebuilt) => {
                    if rebuilt { any_rebuilt = true; }
                }
                Err(e) => {
                    if e.starts_with("No rule to make target '") && !e.contains(", needed by '") && !rule.recipe.is_empty() && !rule.is_compat {
                        // GNU Make behavior: if a prerequisite doesn't exist and has
                        // no rule, but the parent target HAS a recipe, just consider
                        // the parent out of date. This handles auto-generated dependency
                        // files that list system headers as prerequisites.
                        // Only applies to DIRECT "no rule" errors (without "needed by"),
                        // not to errors propagated from deeper in the dependency chain,
                        // and not to compatibility rules (which propagate errors).
                        any_rebuilt = true;
                    } else {
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
        }

        for prereq in order_only.clone() {
            let _ = self.build_target(&prereq);
        }

        self.inherited_vars_stack.pop();

        // Include extra_prereqs in rebuild check but not in auto vars.
        let prereqs_for_rebuild = {
            let mut v = extra_prereqs.clone();
            v.extend(prereqs.clone());
            v
        };
        let needs_rebuild = if self.always_make || is_phony {
            true
        } else if self.needs_rebuild(target, &prereqs_for_rebuild, any_rebuilt) {
            true
        } else {
            // For grouped pattern rules, also rebuild if any concrete sibling is missing.
            concrete_grouped_siblings.iter().any(|sib| {
                self.needs_rebuild(sib, &[], false)
            })
        };

        if !needs_rebuild {
            // Mark grouped pattern siblings as covered (up to date).
            for sib in &concrete_grouped_siblings {
                self.grouped_covered.insert(sib.clone());
                self.built.insert(sib.clone(), false);
            }
            // Mark also_make siblings as covered (up to date).
            for sib in &also_make_siblings {
                self.grouped_covered.insert(sib.clone());
                self.built.insert(sib.clone(), false);
            }
            return Ok(false);
        }

        let oo_refs: Vec<&str> = order_only.iter().map(|s| s.as_str()).collect();
        let mut auto_vars = self.make_auto_vars(target, &prereqs, &oo_refs, stem);

        // Apply target-specific and pattern-specific variables
        let collected_target_vars = self.collect_target_vars(target);
        self.apply_target_vars_to_auto_vars(&collected_target_vars.0, &mut auto_vars);

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
        if self.collect_plans_mode {
            self.pending_plan_prereqs = prereqs.clone();
            self.pending_plan_order_only = order_only.clone();
        }
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
            // For multi-target pattern rules: mark also_make siblings as built and
            // potentially intermediate. These siblings were produced by the same recipe.
            // GNU Make tracks also_make siblings BEFORE the primary target in the
            // intermediate deletion list (so they are removed first).
            for sib in &also_make_siblings {
                // Mark as built/covered so we don't run the recipe again for them.
                self.grouped_covered.insert(sib.clone());
                self.built.insert(sib.clone(), true);

                // Track intermediate status for also_make siblings.
                if self.db.is_intermediate(sib) {
                    if !self.intermediate_built.contains(sib) {
                        self.intermediate_built.push(sib.clone());
                    }
                } else if !self.db.is_precious(sib)
                    && !self.db.is_notintermediate(sib)
                    && !self.db.is_secondary(sib)
                {
                    let is_explicit = self.top_level_targets.contains(sib.as_str())
                        || self.db.is_explicitly_mentioned(sib);
                    if !is_explicit {
                        if !self.intermediate_built.contains(sib) {
                            self.intermediate_built.push(sib.clone());
                        }
                    }
                }
            }

            // Now track the primary target as intermediate (after siblings).
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
        } else {
            // Even if recipe failed or didn't rebuild, mark also_make siblings as covered
            // so we don't try to rebuild them separately.
            for sib in &also_make_siblings {
                self.grouped_covered.insert(sib.clone());
                self.built.insert(sib.clone(), false);
            }
        }

        // Check if peer targets were actually updated (emit warning if not).
        // Per GNU Make: if a multi-target pattern rule ran AND the primary target now exists
        // on disk (was actually created/touched), but a peer target doesn't exist, warn.
        // Do NOT warn if the primary target also doesn't exist (recipe didn't create any files).
        if let Ok(true) = &result {
            let primary_exists = Path::new(target).exists();
            if primary_exists {
                for sib in &also_make_siblings {
                    if !Path::new(sib.as_str()).exists() {
                        // Peer target wasn't created: warn
                        let loc = if !rule.source_file.is_empty() && rule.lineno > 0 {
                            format!("{}:{}: ", rule.source_file, rule.lineno)
                        } else {
                            String::new()
                        };
                        eprintln!("{}warning: pattern recipe did not update peer target '{}'.", loc, sib);
                    }
                }
            }
        }

        // Mark grouped pattern siblings as covered by this recipe run.
        for sib in &concrete_grouped_siblings {
            self.grouped_covered.insert(sib.clone());
            self.built.insert(sib.clone(), false);
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
        // Collect all matching candidates, then pick the one with the shortest stem.
        let mut pass1_candidates: Vec<(Rule, String)> = Vec::new();
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
                            let resolved = subst_stem_in_prereq(p, &stem);
                            let ok = Path::new(&resolved).exists()
                                || self.db.is_phony(&resolved)
                                || self.db.rules.contains_key(&resolved)
                                || explicit_prereqs.iter().any(|ep| ep == &resolved)
                                || self.find_in_vpath(&resolved).is_some()
                                // A prerequisite currently being built (cycle) counts as
                                // available in pass 1; the cycle will be detected and dropped
                                // gracefully when we actually try to build it.
                                || self.building.contains(&resolved);
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
                        pass1_candidates.push(((**rule).clone(), stem));
                        break; // Only take first matching pattern_target per rule
                    } else if found_compat && compat_rule.is_none() {
                        compat_rule = Some(((**rule).clone(), stem.clone()));
                    }
                }
            }
        }
        if !pass1_candidates.is_empty() {
            // Pick the candidate with the shortest stem (GNU Make "shortest stem" rule).
            let best = pass1_candidates.into_iter()
                .min_by_key(|(_, stem)| stem.len())
                .unwrap();
            return Some(best);
        }

        // Pass 2: prereqs can be built via chaining. Terminal rules skipped.
        let mut pass2_candidates: Vec<(Rule, String)> = Vec::new();
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
                            let resolved = subst_stem_in_prereq(p, &stem);
                            let ok = Path::new(&resolved).exists()
                                || self.db.rules.contains_key(&resolved)
                                || self.db.is_phony(&resolved)
                                || explicit_prereqs.iter().any(|ep| ep == &resolved)
                                || self.find_pattern_rule_exists(&resolved)
                                || self.find_in_vpath(&resolved).is_some()
                                || self.building.contains(&resolved);
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
                        pass2_candidates.push(((**rule).clone(), stem));
                        break; // Only take first matching pattern_target per rule
                    } else if found_compat && compat_rule.is_none() {
                        compat_rule = Some(((**rule).clone(), stem.clone()));
                    }
                }
            }
        }
        if !pass2_candidates.is_empty() {
            // Pick the candidate with the shortest stem (GNU Make "shortest stem" rule).
            let best = pass2_candidates.into_iter()
                .min_by_key(|(_, stem)| stem.len())
                .unwrap();
            return Some(best);
        }

        // Mark the compat rule as a compatibility rule so that
        // build_with_pattern_rule knows to propagate errors from missing prereqs.
        if let Some((ref mut rule, _)) = compat_rule {
            rule.is_compat = true;
        }
        compat_rule
    }

    /// Check if `target` can be built by any pattern rule (recursive/intermediate context).
    /// Non-terminal match-anything rules (%) are skipped: they cannot create intermediates.
    fn find_pattern_rule_exists(&self, target: &str) -> bool {
        self.find_pattern_rule_exists_inner(target, &mut std::collections::HashSet::new())
    }

    fn find_pattern_rule_exists_inner(&self, target: &str, visited: &mut HashSet<String>) -> bool {
        // If this target is currently being built, treat it as available (cycle case).
        if self.building.contains(target) {
            return true;
        }
        // Prevent infinite recursion from circular dependencies.
        if visited.contains(target) {
            return false;
        }
        visited.insert(target.to_string());

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
                            let resolved = subst_stem_in_prereq(p, &stem);
                            Path::new(&resolved).exists()
                                || self.db.rules.contains_key(&resolved)
                                || self.db.is_phony(&resolved)
                                || self.find_in_vpath(&resolved).is_some()
                                || self.building.contains(&resolved)
                                || self.find_pattern_rule_exists_inner(&resolved, visited)
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
    /// Returns two maps:
    ///   0: for_recipe  — vars for this target's own recipe expansion (includes private vars)
    ///   1: for_prereqs — vars to pass to prerequisites (private vars replaced by their
    ///                    pre-override inherited values, so that `b: private F = b` does
    ///                    NOT block `a: F = a` from reaching `c` through `b`)
    /// Pattern-specific variables are matched with shortest-stem semantics.
    fn collect_target_vars(&self, target: &str) -> (HashMap<String, (String, bool, bool)>, HashMap<String, (String, bool, bool)>) {
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
                VarFlavor::Recursive | VarFlavor::PosixSimple => {
                    // Store raw value for second-pass expansion
                    let raw = psv.var.value.clone();
                    let idx = staging.len();
                    staging.push((psv.var_name.clone(), raw, false, psv.is_override, VarFlavor::Recursive));
                    staging_idx.insert(psv.var_name.clone(), idx);
                }
            }
        }

        // Snapshot the staging state before step 2 (target-specific vars).
        // This is used to build the "for_prereqs" result: when a target-specific private
        // var overrides an inherited value, the inherited value should still pass through
        // to prerequisites (GNU Make private semantics: only blocks THIS target's override).
        let pre_step2_snap: HashMap<String, (String, bool)> = staging_idx.iter()
            .map(|(name, &i)| {
                let (_, val, _is_expanded, is_override, _) = &staging[i];
                // For snapshot we just want to know the key existed and its is_override
                (name.clone(), (val.clone(), *is_override))
            })
            .collect();
        // Also record which vars are private at the END of step 1. Pattern-specific private
        // vars should NOT pass through their values to prerequisites (they are private at step 1).
        // Only TARGET-specific private vars (introduced in step 2) should use the pre-step-2 passthrough.
        let step1_private_flags: HashSet<String> = private_flags.clone();

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
                        VarFlavor::Recursive | VarFlavor::PosixSimple => {
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

        // Build for_prereqs: like result, but for private vars that were introduced/overridden
        // in STEP 2 (target-specific), use the pre-step-2 (inherited/pattern) value instead.
        // Pattern-specific private vars (set in step 1) should NOT pass through at all.
        // Target-specific private vars (set in step 2) that override an existing value should
        // let the pre-override value pass through to prerequisites.
        let mut for_prereqs: HashMap<String, (String, bool, bool)> = HashMap::new();
        for (name, (val, is_override, is_priv)) in &result {
            if *is_priv {
                if step1_private_flags.contains(name.as_str()) {
                    // Private at step 1 (pattern-specific) → don't include in for_prereqs at all.
                    // The pattern-specific private variable should not propagate.
                } else {
                    // Private at step 2 (target-specific, not step 1).
                    // For prereqs, use the pre-step-2 value if it existed.
                    if let Some((pre_val, pre_is_override)) = pre_step2_snap.get(name) {
                        // Carry through the pre-override value, marking it as non-private.
                        for_prereqs.insert(name.clone(), (pre_val.clone(), *pre_is_override, false));
                    }
                    // If no pre-step-2 entry: don't include in for_prereqs (new private var).
                }
            } else {
                for_prereqs.insert(name.clone(), (val.clone(), *is_override, false));
            }
        }

        (result, for_prereqs)
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
        // If target exists, use its actual mtime — but check if what-if or rule prereqs
        // would cause a rebuild; if so, treat this target as infinitely new.
        if let Some(t) = get_mtime(target).or_else(|| self.find_in_vpath(target).and_then(|f| get_mtime(&f))) {
            // Check explicit rules for this target
            if let Some(rules) = self.db.rules.get(target) {
                for rule in rules {
                    for prereq in &rule.prerequisites {
                        // If a prereq is what-if (or its VPATH-resolved path is), target would rebuild
                        if self.what_if.iter().any(|w| w == prereq) {
                            return Some(SystemTime::now());
                        }
                        if let Some(ref vp) = self.find_in_vpath(prereq) {
                            if self.what_if.iter().any(|w| w == vp) {
                                return Some(SystemTime::now());
                            }
                        }
                        // If a prereq's effective mtime is newer than target, target would rebuild
                        if let Some(pt) = self.effective_mtime(prereq, depth + 1) {
                            if pt > t {
                                return Some(SystemTime::now());
                            }
                        }
                    }
                }
            }
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
                .map(|p| subst_stem_in_prereq(p, &stem))
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

        // -W/--what-if: if any prereq is in the what_if list, treat it as infinitely new.
        // Also check the VPATH-resolved path (e.g., prereq "x" found as "x-dir/x" via VPATH,
        // and "-W x-dir/x" was given).
        for prereq in prereqs {
            if self.what_if.iter().any(|w| w == prereq) {
                return true;
            }
            if let Some(ref vp) = self.find_in_vpath(prereq) {
                if self.what_if.iter().any(|w| w == vp) {
                    return true;
                }
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
                        // Secondary files that don't exist don't trigger rebuilds,
                        // UNLESS they are also marked .NOTINTERMEDIATE (which overrides).
                        if self.db.is_secondary(prereq) && !self.db.is_notintermediate(prereq) {
                            continue;
                        }
                        // A prereq that was visited and its recipe actually ran (commands
                        // were executed) but the file still doesn't exist always triggers
                        // a rebuild.  GNU Make treats such prerequisites as "newer than
                        // everything".
                        if self.built.get(prereq.as_str()) == Some(&true) {
                            return true;
                        }
                        // If the prereq is explicitly mentioned (not intermediate), a
                        // missing file is treated as infinitely new: always rebuild.
                        // Intermediate files that are absent use effective_mtime instead.
                        if self.db.is_explicitly_mentioned(prereq) {
                            return true;
                        }
                        // For non-existent non-phony prereqs (potentially intermediate files
                        // that were deleted), compute effective mtime from their sources.
                        // If sources are all older than the target, no rebuild needed.
                        // Exception: .NOTINTERMEDIATE files are "real" files; when they
                        // don't exist (no effective mtime), treat as needing rebuild.
                        match self.effective_mtime(prereq, 0) {
                            Some(eff_t) if eff_t > target_time => return true,
                            None if self.db.is_notintermediate(prereq) => return true,
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

    /// Resolve `.EXTRA_PREREQS` for a given target.
    /// Target-specific `.EXTRA_PREREQS` takes priority over the global value.
    /// The value is variable-expanded and wildcard-expanded.
    /// Returns a deduplicated list of extra prerequisite names.
    fn get_extra_prereqs(&self, target: &str) -> Vec<String> {
        // Use collect_target_vars to get the fully-expanded target-specific vars
        // (which also handles inheritance, pattern-specific vars, etc.).
        let collected = self.collect_target_vars(target);
        // The value from collect_target_vars is already fully expanded.
        let expanded = if let Some((val, _, _)) = collected.0.get(".EXTRA_PREREQS") {
            val.clone()
        } else {
            // Fall back to global .EXTRA_PREREQS.
            match self.db.variables.get(".EXTRA_PREREQS") {
                Some(v) => {
                    if v.flavor == VarFlavor::Simple {
                        v.value.clone()
                    } else {
                        self.state.expand(&v.value)
                    }
                }
                None => return Vec::new(),
            }
        };

        if expanded.trim().is_empty() {
            return Vec::new();
        }

        // Split into tokens and apply wildcard expansion.
        let mut result = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for token in expanded.split_whitespace() {
            // Apply wildcard expansion if the token contains glob characters.
            if token.contains('*') || token.contains('?') || token.contains('[') {
                let mut matched = Vec::new();
                if let Ok(paths) = ::glob::glob(token) {
                    for entry in paths.flatten() {
                        matched.push(entry.to_string_lossy().to_string());
                    }
                }
                matched.sort();
                if matched.is_empty() {
                    // No matches: treat as literal (like GNU Make does).
                    if seen.insert(token.to_string()) {
                        result.push(token.to_string());
                    }
                } else {
                    for m in matched {
                        if seen.insert(m.clone()) {
                            result.push(m);
                        }
                    }
                }
            } else {
                if seen.insert(token.to_string()) {
                    result.push(token.to_string());
                }
            }
        }
        result
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
                        // prereq doesn't exist as file: include if prereq was visited
                        // (had a rule but didn't create a file — counts as always newer)
                        self.built.contains_key(p.as_str())
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
        // ------------------------------------------------------------------
        // Plan collection mode: store a TargetPlan instead of running the recipe.
        // This is used during parallel graph resolution (Phase 1 of -j N builds).
        // ------------------------------------------------------------------
        if self.collect_plans_mode {
            // Expand recipe lines now (on main thread, with full state access).
            // This is the ONLY place where expansion happens for the parallel path.
            let expanded_recipe: Vec<(usize, String)> = recipe.iter().map(|(lineno, line)| {
                *self.state.current_file.borrow_mut() = source_file.to_string();
                *self.state.current_line.borrow_mut() = *lineno;
                let preprocessed = preprocess_recipe_bsnl(line);
                let expanded = self.state.expand_with_auto_vars(&preprocessed, auto_vars);
                // Join sub-lines into a single string; the worker will re-split them.
                let sub_lines = split_recipe_sub_lines(&expanded);
                let rejoined = sub_lines.join("\n");
                (*lineno, rejoined)
            }).collect();

            // Clear the pending prereqs (they were used by build_with_rules to pre-record
            // the plan; here we update the plan's recipe field).
            let _ = std::mem::take(&mut self.pending_plan_prereqs);
            let _ = std::mem::take(&mut self.pending_plan_order_only);

            // Update the existing plan (created in build_with_rules) with the expanded recipe.
            // If no plan exists yet (e.g., called from .DEFAULT or pattern rules), create one.
            if let Some(ref mut plans) = self.pending_plans {
                if let Some(plan) = plans.get_mut(target) {
                    plan.recipe = expanded_recipe;
                    plan.needs_rebuild = true;
                    plan.auto_vars = auto_vars.clone();
                    plan.extra_exports = self.target_extra_exports.clone();
                    plan.extra_unexports = self.target_extra_unexports.iter().cloned().collect();
                } else {
                    // Plan not pre-created (e.g. pattern rule or .DEFAULT); create it now.
                    let plan = parallel::TargetPlan {
                        target: target.to_string(),
                        prerequisites: std::mem::take(&mut self.pending_plan_prereqs),
                        order_only: std::mem::take(&mut self.pending_plan_order_only),
                        recipe: expanded_recipe,
                        source_file: source_file.to_string(),
                        auto_vars: auto_vars.clone(),
                        is_phony: self.db.is_phony(target),
                        needs_rebuild: true,
                        grouped_primary: None,
                        grouped_siblings: Vec::new(),
                        extra_exports: self.target_extra_exports.clone(),
                        extra_unexports: self.target_extra_unexports.iter().cloned().collect(),
                        is_intermediate: self.db.is_intermediate(target),
                    };
                    plans.insert(target.to_string(), plan);
                }
            }

            // Mark as ran so the build graph continues correctly.
            self.any_recipe_ran = true;
            return Ok(true);
        }

        // With --trace, print "file:line: update target 'X' due to: reason" before executing.
        if self.trace && !recipe.is_empty() {
            let (lineno, _) = &recipe[0];
            // make_location already appends ": ", so use it directly without adding another ":"
            let loc = make_location(source_file, *lineno);
            let reason = if self.db.is_phony(target) {
                "target is .PHONY"
            } else if !Path::new(target).exists() {
                "target does not exist"
            } else {
                "target is out of date"
            };
            eprintln!("{}update target '{}' due to: {}", loc, target, reason);
        }
        // --debug=j (jobs debug) output: announce that a child job is being queued.
        // GNU Make outputs "Putting child PID ... on the chain." here.
        if self.debug_flag("j") && !recipe.is_empty() {
            println!("Putting child 0x0 ({}) PID 0 on the chain.", target);
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
            //   - Prefix chars (@, -, +) are stripped from ALL recipe lines when building
            //     the script (so they don't appear as invalid shell commands).
            //   - Echo behaviour and error-ignore are controlled by the FIRST recipe
            //     line's prefix only; inner-line prefix chars don't affect behavior.
            //   - The last recipe lineno is used for error messages.

            let mut script = String::new();
            let mut first_line_silent = false;
            let mut first_line_ignore = false;
            let mut is_first = true;
            let mut last_lineno: usize = 0;

            for (lineno, line) in recipe {
                last_lineno = *lineno;
                // Update current_file/current_line so that errors during expansion
                // (e.g. from $(word ...) or $(wordlist ...)) report the correct location.
                *self.state.current_file.borrow_mut() = source_file.to_string();
                *self.state.current_line.borrow_mut() = *lineno;
                // Pre-process: collapse \<newline> inside $(…)/${…} references
                let preprocessed = preprocess_recipe_bsnl(line);
                let expanded = self.state.expand_with_auto_vars(&preprocessed, auto_vars);
                // Strip @-+ prefixes from ALL lines for the script.
                // First line: also record behavioral flags (silent, ignore errors).
                if is_first {
                    let (_d, ls, li, _lf) = parse_recipe_prefix(&expanded);
                    first_line_silent = ls;
                    first_line_ignore = li;
                    is_first = false;
                }
                let cmd_line = strip_recipe_prefixes(&expanded);
                script.push_str(&cmd_line);
                script.push('\n');
            }

            let effective_silent = first_line_silent || self.silent || is_silent_target;
            let effective_ignore = first_line_ignore || self.ignore_errors;

            if !effective_silent {
                // Echo lines with @-+ prefixes stripped from ALL lines.
                for (_lineno, line) in recipe {
                    let preprocessed = preprocess_recipe_bsnl(line);
                    let expanded = self.state.expand_with_auto_vars(&preprocessed, auto_vars);
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
                // Parse shell_flags respecting shell-like single-quote quoting.
                // E.g. `.SHELLFLAGS = -w -E 'use warnings;' -E` produces 4 separate args.
                let flags: Vec<String> = parse_shell_flags(self.shell_flags);
                let mut child = Command::new(self.shell);
                for flag in &flags {
                    child.arg(flag);
                }
                // Script is the final argument (after any flags from .SHELLFLAGS).
                // Trim trailing newlines to avoid embedded newlines when interpreter
                // treats the arg as a filename (e.g. perl without -e/-E flags).
                child.arg(script.trim_end_matches('\n'));
                child.env("MAKELEVEL", self.get_makelevel());
                self.setup_exports(&mut child);
                let status = child.status();

                match status {
                    Ok(s) if !s.success() => {
                        let code = s.code().unwrap_or(1);
                        if effective_ignore {
                            let loc = make_location(source_file, last_lineno);
                            eprintln!("{}: [{}{}] Error {} (ignored)", self.progname, loc, target, code);
                        } else {
                            let loc = make_location(source_file, last_lineno);
                            eprintln!("{}: *** [{}{}] Error {}", self.progname, loc, target, code);
                            // Delete target on error only if .DELETE_ON_ERROR is set.
                            let delete_on_error = self.db.special_targets
                                .contains_key(&SpecialTarget::DeleteOnError);
                            if delete_on_error && !self.db.is_precious(target) && !self.db.is_phony(target) {
                                if Path::new(target).exists() {
                                    eprintln!("{}: *** Deleting file '{}'", self.progname, target);
                                    let _ = fs::remove_file(target);
                                }
                            }
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

        // GNU Make pre-expands all recipe lines before executing any of them.
        // This means $(shell ...) and other make functions in recipe lines are
        // evaluated before any shell commands from the recipe run.  For example,
        // if recipe line 2 has $(shell bad-cmd) and recipe line 1 has echo hi,
        // the error from $(shell bad-cmd) appears BEFORE "hi" is printed.
        // Pre-expand all recipe lines first.
        let pre_expanded: Vec<(usize, String, Vec<String>)> = recipe.iter().map(|(lineno, line)| {
            *self.state.current_file.borrow_mut() = source_file.to_string();
            *self.state.current_line.borrow_mut() = *lineno;
            let preprocessed = preprocess_recipe_bsnl(line);
            let expanded = self.state.expand_with_auto_vars(&preprocessed, auto_vars);
            let sub_lines = split_recipe_sub_lines(&expanded);
            (*lineno, line.clone(), sub_lines)
        }).collect();

        // Execute each recipe line separately.
        // Track whether any actual shell commands were executed.
        let mut any_cmd_ran = false;
        for (lineno, line, sub_lines) in &pre_expanded {
            let lineno = *lineno;
            // Update current_file/current_line for error reporting during execution.
            *self.state.current_file.borrow_mut() = source_file.to_string();
            *self.state.current_line.borrow_mut() = lineno;
            // The expanded value is already in sub_lines (pre-expanded above).
            // We still need the expanded string for prefix-flag extraction.
            // Reconstruct it from sub_lines.
            let expanded = sub_lines.join("\n");

            // Extract prefix flags (@, -, +) from the ORIGINAL recipe line (before expansion).
            // These flags propagate to ALL sub-lines from the expansion.
            // For example, `@$(MULTI_LINE_VAR)` silences every line in the expansion,
            // not just the first one.
            let (_outer_display, outer_silent, outer_ignore, outer_force) = parse_recipe_prefix(line);

            // sub_lines were already computed during pre-expansion above.
            // Each sub-line is an independent recipe command.
            'sub_line_loop: for sub_line in sub_lines {
                let (display_line, line_silent, ignore_error, force_sub) = parse_recipe_prefix(sub_line);
                // Outer flags (from the original recipe line before expansion) propagate to
                // all sub-lines.  This handles `@$(MULTI)` where MULTI has multiple lines.
                let force = force_sub || outer_force;

                // With --trace or --dry-run (-n), the @ prefix does NOT suppress echoing.
                // GNU Make prints all commands in -n mode regardless of @.
                let at_silent = if self.trace || self.dry_run { false } else { line_silent || outer_silent };
                let effective_silent = at_silent || self.silent || is_silent_target;
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
                        let loc = make_location(source_file, lineno);
                        if effective_ignore {
                            eprintln!("{}: [{}{}] Error {} (ignored)", self.progname, loc, target, code);
                        } else {
                            eprintln!("{}: *** [{}{}] Error {}", self.progname, loc, target, code);
                            // Delete target on error only if .DELETE_ON_ERROR is set
                            // and the target is not .PRECIOUS.
                            let delete_on_error = self.db.special_targets
                                .contains_key(&SpecialTarget::DeleteOnError);
                            if delete_on_error && !self.db.is_precious(target) && !self.db.is_phony(target) {
                                if Path::new(target).exists() {
                                    eprintln!("{}: *** Deleting file '{}'", self.progname, target);
                                    let _ = fs::remove_file(target);
                                }
                            }
                            return Err(String::new());
                        }
                    }
                    Err(e) => {
                        if effective_ignore {
                            let loc = make_location(source_file, lineno);
                            eprintln!("{}: [{}{}] Error: {} (ignored)", self.progname, loc, target, e);
                        } else {
                            let loc = make_location(source_file, lineno);
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
        // A "real" recipe either:
        //   (a) has at least one non-empty line after stripping whitespace, OR
        //   (b) has an inline recipe marker (semicolon in the rule line), even
        //       if the text after the semicolon is empty (`target: ;`).
        // GNU Make treats `target: ;` as having a recipe, so it prints
        // "'target' is up to date" rather than "Nothing to be done".
        let rule_has_recipe = |rule: &crate::types::Rule| -> bool {
            if rule.has_inline_recipe_marker {
                return true;
            }
            rule.recipe.iter().any(|(_, line)| {
                let stripped = strip_recipe_prefixes(line);
                !stripped.trim().is_empty()
            })
        };
        if let Some(rules) = self.db.rules.get(target) {
            for rule in rules {
                if rule_has_recipe(rule) {
                    return true;
                }
            }
        }
        // Check pattern rules
        if let Some((rule, _)) = self.find_pattern_rule(target) {
            if rule_has_recipe(&rule) {
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
            // Private global variables are NOT exported to recipe shells.
            // (They may still be exported to top-level $(shell) calls via shell_exec_with_env.)
            // When `unexport` (all) was seen, the default changes: don't export unless
            // explicitly marked as exported (Some(true)) or always-export (MAKEFLAGS etc).
            // Env vars that were not explicitly overridden by the makefile still need
            // their original value in the environment, but `unexport` should suppress them
            // unless they were explicitly `export`ed.
            let should_export = !var.is_private && (always_export || match var.export {
                Some(true) => true,
                Some(false) => false,
                None => {
                    if self.db.unexport_all {
                        // Global unexport: only export if var originated from env AND
                        // was not changed by the makefile (still Environment origin).
                        // If the makefile explicitly set the var, it must have been
                        // explicitly `export`ed to be exported.
                        was_from_env && var.origin == VarOrigin::Environment
                    } else {
                        self.db.export_all || was_from_env
                    }
                }
            });
            if should_export {
                let value = self.state.expand(&var.value);
                cmd.env(name, &value);
            } else {
                // Ensure the child does not see this variable from the inherited
                // environment. This covers:
                // - explicitly unexported variables (export = Some(false))
                // - private global variables (not inherited by recipes)
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
        // If GNUMAKEFLAGS was originally set in the environment, export it as empty
        // so child processes see it (but cleared to prevent flag duplication).
        if self.state.args.gnumakeflags_was_set {
            cmd.env("GNUMAKEFLAGS", "");
        }
    }

    /// Compute the set of variables that should be exported to the shell for `target`
    /// due to target-specific or pattern-specific `export` declarations.
    /// Returns a map from variable name to its expanded value.
    fn compute_target_exports(&self, target: &str) -> HashMap<String, String> {
        // Use the collect_target_vars result to find the final values of all
        // applicable target-specific and pattern-specific variables.
        let (target_vars, _) = self.collect_target_vars(target);
        let mut exports: HashMap<String, String> = HashMap::new();
        // Track vars that are explicitly unexported for this target.
        let mut unexports: HashSet<String> = HashSet::new();

        // Helper: determine if the global var should be exported.
        let global_should_export = |var_name: &str| -> bool {
            if matches!(var_name, "MAKEFLAGS" | "MAKE" | "MAKECMDGOALS") {
                return true;
            }
            let was_from_env = self.db.env_var_names.contains(var_name);
            if let Some(gvar) = self.db.variables.get(var_name) {
                // Private global variables are not exported to child recipe shells.
                if gvar.is_private {
                    return false;
                }
                match gvar.export {
                    Some(true) => true,
                    Some(false) => false,
                    None => {
                        if self.db.unexport_all {
                            was_from_env && gvar.origin == VarOrigin::Environment
                        } else {
                            self.db.export_all || was_from_env
                        }
                    }
                }
            } else {
                if self.db.unexport_all {
                    false
                } else {
                    self.db.export_all || was_from_env
                }
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

    /// Return true if the given debug flag character (e.g. "b", "j") is active.
    /// Active means it was specified via -d, --debug=FLAG, or MAKEFLAGS.
    fn debug_flag(&self, flag: &str) -> bool {
        self.state.args.debug_short
            || self.state.args.debug.iter().any(|d| {
                d == flag
                    || d == "a" || d == "all"
                    || (flag == "b" && (d == "basic"))
                    || (flag == "j" && (d == "jobs"))
            })
    }

    /// Print a keep-going error message to stderr.
    /// Formats it as: `progname: *** message.`
    fn print_error_keep_going(&self, err: &str) {
        // Strip " Stop." suffix if present (we're not stopping).
        let msg = if let Some(stripped) = err.strip_suffix("  Stop.") {
            stripped.to_string()
        } else {
            err.to_string()
        };
        // Ensure message ends with a period.
        let msg = msg.trim_end_matches('.');
        eprintln!("{}: *** {}.", self.progname, msg);
    }

    /// Shuffle a list of prerequisites according to the current shuffle mode.
    /// Returns the list in the new order.
    fn shuffle_list(&mut self, mut list: Vec<String>) -> Vec<String> {
        match &self.shuffle {
            None | Some(ShuffleMode::Identity) => list,
            Some(ShuffleMode::Reverse) => {
                list.reverse();
                list
            }
            Some(ShuffleMode::Random) | Some(ShuffleMode::Seeded(_)) => {
                // Simple xorshift64 PRNG for portable shuffle
                let seed = &mut self.shuffle_seed;
                let n = list.len();
                for i in (1..n).rev() {
                    // Advance xorshift64
                    let mut s = *seed;
                    if s == 0 { s = 1; }
                    s ^= s << 13;
                    s ^= s >> 7;
                    s ^= s << 17;
                    *seed = s;
                    let j = (s as usize) % (i + 1);
                    list.swap(i, j);
                }
                list
            }
        }
    }

    /// Record a minimal TargetPlan for a leaf target (file exists, phony-no-recipe, VPATH).
    /// Called during plan collection mode to ensure these targets appear in the dependency graph.
    fn record_leaf_plan(&mut self, target: &str, is_phony: bool) {
        if let Some(ref mut plans) = self.pending_plans {
            if !plans.contains_key(target) {
                plans.insert(target.to_string(), parallel::TargetPlan {
                    target: target.to_string(),
                    prerequisites: Vec::new(),
                    order_only: Vec::new(),
                    recipe: Vec::new(),
                    source_file: String::new(),
                    auto_vars: HashMap::new(),
                    is_phony,
                    needs_rebuild: false,
                    grouped_primary: None,
                    grouped_siblings: Vec::new(),
                    extra_exports: HashMap::new(),
                    extra_unexports: Vec::new(),
                    is_intermediate: false,
                });
            }
        }
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

/// Parse a .SHELLFLAGS value into individual arguments, respecting single-quote quoting.
/// E.g. `-w -E 'use warnings FATAL => "all";' -E` → ["-w", "-E", "use warnings...", "-E"]
/// Unquoted tokens are split on whitespace; single-quoted strings are a single token.
fn parse_shell_flags(flags: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut in_single_quote = false;
    let bytes = flags.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let ch = bytes[i] as char;
        if in_single_quote {
            if ch == '\'' {
                in_single_quote = false;
            } else {
                current.push(ch);
            }
        } else if ch == '\'' {
            in_single_quote = true;
        } else if ch == ' ' || ch == '\t' {
            if !current.is_empty() {
                result.push(current.clone());
                current.clear();
            }
        } else {
            current.push(ch);
        }
        i += 1;
    }
    if !current.is_empty() {
        result.push(current);
    }
    result
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

/// Substitute the stem for the FIRST `%` in a prerequisite word (GNU Make rule).
/// Any subsequent `%` characters in the same word are left as-is.
/// This follows GNU Make's behavior where only the first `%` in each word of a
/// pattern rule prerequisite is replaced by the stem.
fn replace_first_percent(word: &str, stem: &str) -> String {
    if let Some(pos) = word.find('%') {
        let mut result = String::with_capacity(word.len() + stem.len());
        result.push_str(&word[..pos]);
        result.push_str(stem);
        result.push_str(&word[pos+1..]);
        result
    } else {
        word.to_string()
    }
}

/// Apply `%` → stem substitution to a prerequisite string, word by word.
/// Only the first `%` in each whitespace-delimited word is replaced.
/// For non-SE prerequisites (stored as already-split individual words),
/// this simply calls `replace_first_percent`.
fn subst_stem_in_prereq(prereq: &str, stem: &str) -> String {
    replace_first_percent(prereq, stem)
}

/// Apply `%` → stem substitution to a raw SE prerequisite text.
/// GNU Make rule: in SE prerequisite text, replace the first `%` in each
/// whitespace-delimited word.  Whitespace inside function calls (e.g. between
/// arguments in `$(wordlist 1, 99, %.1 %.2)`) still acts as a word separator,
/// so `%.1` and `%.2` are treated as separate words even though they are
/// arguments to the same function.
fn subst_stem_in_se_text(text: &str, stem: &str) -> String {
    let mut result = String::with_capacity(text.len() + stem.len() * 4);
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let n = chars.len();

    while i < n {
        // Copy any leading whitespace verbatim.
        if chars[i].is_whitespace() {
            result.push(chars[i]);
            i += 1;
            continue;
        }

        // Collect one word (any non-whitespace run) and replace only the first %.
        let mut first_percent_replaced = false;
        while i < n && !chars[i].is_whitespace() {
            let c = chars[i];
            if c == '%' && !first_percent_replaced {
                result.push_str(stem);
                first_percent_replaced = true;
            } else {
                result.push(c);
            }
            i += 1;
        }
    }

    result
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

/// Check if the given shell path is a Bourne-compatible shell.
/// This mirrors GNU Make's is_bourne_compatible_shell() check.
/// Only strips @-+ recipe prefixes in ONESHELL mode for Bourne-compatible shells;
/// for non-standard shells (perl, python, etc.) the prefixes are left in place
/// because they may be valid syntax in those languages.
fn is_bourne_compatible_shell(shell: &str) -> bool {
    const UNIX_SHELLS: &[&str] = &["sh", "bash", "dash", "ksh", "rksh", "zsh", "ash"];
    // Get the basename of the shell path
    let basename = std::path::Path::new(shell)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    UNIX_SHELLS.contains(&basename)
}

/// Pre-process a recipe line before variable expansion.
///
/// GNU Make (job.c) collapses backslash-newline sequences that appear INSIDE
/// variable or function references (`$(...)` / `${...}`) before expanding them.
/// The rule: inside a `$(…)` block, `\<newline>` (and any following whitespace
/// and preceding whitespace) is replaced by a single space.  Outside such blocks
/// the `\<newline>` is left intact so the shell can handle the continuation.
///
/// This function implements that pre-processing pass.
fn preprocess_recipe_bsnl(line: &str) -> String {
    if !line.contains('\n') {
        // No embedded newlines – nothing to do.
        return line.to_string();
    }
    let bytes = line.as_bytes();
    let mut result = String::with_capacity(line.len());
    let mut i = 0;
    let mut depth: i32 = 0;  // nesting depth inside $(…) / ${…}

    while i < bytes.len() {
        let ch = bytes[i];
        if ch == b'$' && i + 1 < bytes.len() && (bytes[i + 1] == b'(' || bytes[i + 1] == b'{') {
            depth += 1;
            result.push(ch as char);
            result.push(bytes[i + 1] as char);
            i += 2;
            continue;
        }
        if depth > 0 && (ch == b')' || ch == b'}') {
            depth -= 1;
            result.push(ch as char);
            i += 1;
            continue;
        }
        if depth > 0 && ch == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
            // Inside a variable/function reference: count consecutive backslashes.
            let mut nb = 0usize;
            let mut j = i;
            while j < bytes.len() && bytes[j] == b'\\' {
                nb += 1;
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'\n' && nb % 2 == 1 {
                // Odd number of backslashes followed by newline → collapse.
                // Emit (nb-1)/2 literal backslashes (each pair → one backslash).
                for _ in 0..(nb / 2) {
                    result.push('\\');
                }
                // Strip trailing whitespace already in result (before the backslash run)
                while result.ends_with(|c: char| c == ' ' || c == '\t') {
                    result.pop();
                }
                // Skip backslash(es) + newline + leading whitespace on next line
                i = j + 1; // skip newline
                while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                    i += 1;
                }
                result.push(' ');
            } else {
                // Even number of backslashes or no newline: just push the backslash
                result.push(ch as char);
                i += 1;
            }
            continue;
        }
        result.push(ch as char);
        i += 1;
    }
    result
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
        Some(pos) => {
            if pos == 0 {
                // Root dir: the dir of "/foo" is "/"
                "/".to_string()
            } else {
                path[..pos].to_string()
            }
        }
        None => ".".to_string(),
    }
}

fn file_of(path: &str) -> String {
    match path.rfind('/') {
        Some(pos) => path[pos+1..].to_string(),
        None => path.to_string(),
    }
}

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
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::sync::mpsc;
use std::time::SystemTime;

pub struct Executor<'a> {
    db: &'a MakeDatabase,
    state: &'a MakeState,
    jobs: usize,
    /// Maximum load average for -l; None if not specified.
    load_average: Option<f64>,
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
    /// Each entry: HashMap<var_name, (value, is_override, is_private, export_status)>
    /// where export_status is Some(true)=exported, Some(false)=unexported, None=use global default.
    inherited_vars_stack: Vec<HashMap<String, (String, bool, bool, Option<bool>)>>,
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
    /// .WAIT groups for the current target (set before execute_recipe in collect_plans_mode).
    /// Non-empty only when the target's prerequisites contain .WAIT markers.
    pending_plan_wait_groups: Vec<Vec<String>>,
    /// When true, skip applying .EXTRA_PREREQS for the current target.
    /// Set when building a target that is itself an extra prerequisite, to prevent
    /// recursive/circular application of .EXTRA_PREREQS.
    skip_extra_prereqs: bool,
    /// Maps -lname prerequisites to their resolved library paths (e.g., -l1 → a1/lib1.a).
    /// Used to substitute the resolved path in $^ and $< auto-variable expansion.
    lib_search_results: HashMap<String, String>,
    /// Extra "explicitly mentioned" targets discovered during SE expansion at build time.
    /// SE-expanded prerequisites that are NOT derived from the stem (i.e., the SE text
    /// had no '%' to substitute) are treated as explicitly mentioned (not intermediate).
    /// We can't write to db.explicitly_mentioned (immutable ref), so we track them here.
    se_explicitly_mentioned: HashSet<String>,
    /// sv 62706: Extra SE prereq texts to expand for vpath-merged targets.
    /// When a local target is merged with its vpath-resolved counterpart, the local
    /// rule's SE prereq texts are stored here (keyed by vpath target name) so that
    /// build_with_rules can process them alongside the vpath target's own SE texts.
    /// These are the raw SE texts (same format as Rule::second_expansion_prereqs).
    vpath_extra_se_texts: HashMap<String, Vec<String>>,
    /// Targets whose SE side effects have already been fired during the implicit-rule
    /// search phase (via trigger_prereq_se_side_effects).  When build_with_pattern_rule
    /// encounters a target in this set it skips re-firing the SE expansion, preventing
    /// duplicate $(info ...) output.  Entries are removed after the skip so that a
    /// genuine re-build of the same target in a later invocation does fire SE.
    se_search_fired: HashSet<String>,
}

impl<'a> Executor<'a> {
    pub fn new(
        db: &'a MakeDatabase,
        state: &'a MakeState,
        jobs: usize,
        load_average: Option<f64>,
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
            load_average,
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
            pending_plan_wait_groups: Vec::new(),
            skip_extra_prereqs: false,
            lib_search_results: HashMap::new(),
            se_explicitly_mentioned: HashSet::new(),
            vpath_extra_se_texts: HashMap::new(),
            se_search_fired: HashSet::new(),
        }
    }

    /// Check if a target/file is "explicitly mentioned" — either in the static database
    /// or in SE-expanded prerequisites discovered at build time.
    fn is_explicitly_mentioned(&self, name: &str) -> bool {
        self.db.is_explicitly_mentioned(name) || self.se_explicitly_mentioned.contains(name)
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
                        // PHONY targets always get "Nothing to be done" (never "is up to date").
                        let has_recipe = !is_grouped_covered
                            && !self.db.is_phony(target)
                            && self.target_has_recipe(target);
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
                        // Clean up intermediate files even on error.
                        // Skip during collect_plans dry-run (no files were actually created).
                        if !self.collect_plans_mode {
                            self.delete_intermediate_files();
                        }
                        return Err(e);
                    }
                }
            }
        }

        // Delete intermediate files that were built during this run.
        // Skip this during collect_plans dry-run (collect_plans_mode=true) since no
        // files were actually created and the parallel executor will handle deletion.
        if !self.collect_plans_mode {
            self.delete_intermediate_files();
        }

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
            self.load_average,
            sched_plans,
            job_tx,
            result_rx,
            self.keep_going,
            self.progname.clone(),
        );

        scheduler.find_initial_ready(targets);

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
                        scheduler.states.insert(target.clone(), TargetState::Running);
                        scheduler.running_count += 1;
                        scheduler.send_job(j);
                    }
                    None => {
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
                    // PHONY targets always get "Nothing to be done" (never "is up to date").
                    let has_recipe = !self.db.is_phony(target) && self.target_has_recipe(target);
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
            // MAKE_RESTARTS must not be exported to child makes (only relevant for re-exec).
            if name == "MAKE_RESTARTS" {
                ops.push((name.clone(), None));
                continue;
            }
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

        // Re-compute exports here (in the main thread, sequentially) so that recursive
        // variables like `export HI = $(shell $($@.CMD))` are evaluated with $@ set to
        // the current target. This matches GNU Make behavior where the main thread evaluates
        // the recipe environment just before forking the recipe subprocess.
        let mut extra_exports = self.compute_target_exports(target);
        let extra_unexports = self.compute_target_unexports(target).into_iter().collect::<Vec<_>>();

        // Also re-evaluate globally exported recursive variables that reference $@ (or other
        // auto-vars that vary per-target). Override the pre-computed env_ops values.
        // Build auto_vars with $@ = target.
        let mut at_var_ctx: HashMap<String, String> = HashMap::new();
        at_var_ctx.insert("@".to_string(), target.to_string());
        for (name, var) in &self.db.variables {
            if var.flavor == VarFlavor::Recursive && var.value.contains("$@") {
                // Check if this variable is exported.
                let should_export = !var.is_private && match var.export {
                    Some(true) => true,
                    Some(false) => false,
                    None => self.db.export_all || self.db.env_var_names.contains(name.as_str()),
                };
                if should_export {
                    let val = self.state.expand_with_auto_vars(&var.value, &at_var_ctx);
                    extra_exports.insert(name.clone(), val);
                }
            }
        }

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
            extra_exports,
            extra_unexports,
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
        // Collect in REVERSE build order: intermediates built last (closest to the final
        // target) are deleted first, matching GNU Make's deletion output order.
        let to_delete: Vec<String> = self.intermediate_built.iter().rev()
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
        // Exclude files that also exist in VPATH: these were "promoted" from VPATH to a
        // local copy (VPATH+ rename behavior) and should not be deleted.
        let existing: Vec<String> = to_delete.iter()
            .filter(|t| {
                if !Path::new(t.as_str()).exists() { return false; }
                // Skip deletion if this target also exists under a VPATH directory:
                // the local copy is the updated authoritative version.
                if self.find_in_vpath(t).is_some() { return false; }
                true
            })
            .cloned()
            .collect();
        // GNU Make prints ONE rm command with all files.
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
        // Handle -lname library prerequisites: search for libname.a or libname.so
        // in VPATH directories. If found, redirect the build to the resolved path.
        // If not found on disk, fall through to check explicit/pattern rules (user may
        // have defined -l% pattern rules to handle library building).
        if let Some(lib_name) = target.strip_prefix("-l") {
            if let Some(resolved) = self.find_library(lib_name) {
                // Mark the resolved path as already built (it's a found file, no recipe).
                self.built.insert(resolved.clone(), false);
                // Also mark the original -lname as built, mapping to the resolved file.
                // We do this by re-inserting the target in the parent prerequisite list;
                // the caller sees Ok(false) meaning "found, no rebuild needed".
                // But we need the parent to use the resolved path for $^ — GNU Make
                // actually substitutes -lname → resolved path in the prerequisite list.
                // Store the mapping for auto-var expansion.
                self.lib_search_results.insert(target.to_string(), resolved);
                return Ok(false);
            }
            // Library not found on disk — check if libname.a has its own recipe.
            // If libname.a has explicit rules with a recipe, AND -lname would have a
            // recipe via a pattern rule (like -l%: lib%.a ;), then -lname "resolves"
            // to libname.a. Warn that -lname's recipe is ignored and redirect.
            // This matches GNU Make's sv 54549 behavior.
            let lib_a = format!("lib{}.a", lib_name);
            if let Some(lib_a_rules) = self.db.rules.get(&lib_a) {
                let lib_a_has_recipe = lib_a_rules.iter().any(|r| !r.recipe.is_empty());
                if lib_a_has_recipe {
                    // Check if -lname would have a recipe via a pattern rule.
                    if let Some((pattern_rule, _stem)) = self.find_pattern_rule(target) {
                        if !pattern_rule.recipe.is_empty() {
                            // Find source info from the pattern rule.
                            let src = &pattern_rule.source_file;
                            let recipe_lineno = pattern_rule.recipe.first()
                                .map(|(l, _)| *l)
                                .unwrap_or(pattern_rule.lineno);
                            eprintln!("{}:{}: Recipe was specified for file '{}' at {}:{},",
                                src, recipe_lineno, target, src, recipe_lineno);
                            eprintln!("{}:{}: but '{}' is now considered the same file as '{}'.",
                                src, recipe_lineno, target, lib_a);
                            eprintln!("{}:{}: Recipe for '{}' will be ignored in favor of the one for '{}'.",
                                src, recipe_lineno, target, lib_a);
                            // Redirect to libname.a
                            return self.build_target(&lib_a);
                        }
                    }
                }
            }
            // Fall through to check explicit/pattern rules.
        }

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
                // sv 62650: If VPATH resolves this target to a path that also has rules
                // with a recipe, AND the local target also has a recipe, then the VPATH
                // version takes precedence. Warn and redirect to the VPATH path.
                // Only applies to non-double-colon rules (single-colon, explicit target rules).
                let local_has_recipe = rules.iter().any(|r| !r.recipe.is_empty() && !r.is_double_colon);
                if local_has_recipe {
                    if let Some(vpath_resolved) = self.find_in_vpath(target) {
                        if vpath_resolved != target {
                            if let Some(vpath_rules) = self.db.rules.get(&vpath_resolved) {
                                let vpath_has_recipe = vpath_rules.iter().any(|r| !r.recipe.is_empty());
                                if vpath_has_recipe {
                                    // Find source info for the local rule with a recipe
                                    let local_rule_with_recipe = rules.iter()
                                        .find(|r| !r.recipe.is_empty() && !r.is_double_colon);
                                    if let Some(local_rule) = local_rule_with_recipe {
                                        let src = &local_rule.source_file;
                                        let recipe_lineno = local_rule.recipe.first()
                                            .map(|(l, _)| *l)
                                            .unwrap_or(local_rule.lineno);
                                        eprintln!("{}:{}: Recipe was specified for file '{}' at {}:{},",
                                            src, recipe_lineno, target, src, recipe_lineno);
                                        eprintln!("{}:{}: but '{}' is now considered the same file as '{}'.",
                                            src, recipe_lineno, target, vpath_resolved);
                                        eprintln!("{}:{}: Recipe for '{}' will be ignored in favor of the one for '{}'.",
                                            src, recipe_lineno, target, vpath_resolved);
                                    }
                                    // sv 62706: The local rule's recipe is ignored, but its
                                    // SE prereq text must still be evaluated (for side-effects
                                    // like $(info ...)).  Store the local SE texts keyed by
                                    // the vpath target name so that build_with_rules will
                                    // process them AFTER the vpath target's own SE texts,
                                    // preserving the correct output order.
                                    let local_se_texts: Vec<(String, String)> = rules.iter()
                                        .filter(|r| !r.is_double_colon
                                            && r.second_expansion_prereqs.is_some())
                                        .filter_map(|r| r.second_expansion_prereqs.as_ref()
                                            .map(|t| (t.clone(), r.static_stem.clone())))
                                        .collect();
                                    if !local_se_texts.is_empty() {
                                        let vr = vpath_resolved.clone();
                                        // Store (se_text, local_target, stem) for later use.
                                        // Encode as "target\x00stem\x00text" to carry all context.
                                        let encoded: Vec<String> = local_se_texts.iter()
                                            .map(|(text, stem)| {
                                                format!("{}\x00{}\x00{}", target, stem, text)
                                            })
                                            .collect();
                                        self.vpath_extra_se_texts.entry(vr)
                                            .or_default()
                                            .extend(encoded);
                                    }
                                    let vr2 = vpath_resolved.clone();
                                    return self.build_target(&vr2);
                                }
                            }
                        }
                    }
                }

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

        // Check if VPATH resolves the target to a path with an explicit rule.
        // This must happen BEFORE pattern rule lookup: when `vpa/foo.x` has an
        // explicit rule and VPATH contains `vpa`, that explicit VPATH rule takes
        // priority over any matching pattern rule.
        if let Some(found) = self.find_in_vpath(target) {
            if found != target && self.db.rules.contains_key(&found) {
                return self.build_target(&found);
            }
        }

        // Try pattern rules
        if let Some((pattern_rule, stem)) = self.find_pattern_rule(target) {
            return self.build_with_pattern_rule(target, &pattern_rule, &stem, is_phony);
        }

        // Check if file exists (no rule needed).
        // When -L/--check-symlink-times is set, also treat dangling symlinks as "existing"
        // (we use the symlink's own mtime rather than following it to its target).
        let target_exists = if self.state.args.check_symlink_times {
            Path::new(target).symlink_metadata().is_ok()
        } else {
            Path::new(target).exists()
        };
        if target_exists {
            if self.collect_plans_mode {
                self.record_leaf_plan(target, is_phony);
            }
            return Ok(false);
        }

        // Try VPATH (file exists in a vpath directory, no explicit rule needed)
        if let Some(found) = self.find_in_vpath(target) {
            if found == target {
                // Shouldn't happen, but guard against infinite recursion.
                if self.collect_plans_mode {
                    self.record_leaf_plan(target, is_phony);
                }
                return Ok(false);
            }
            // File exists in a vpath directory (no rule needed), treat as up-to-date.
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
                    self.pending_plan_wait_groups = Vec::new();
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
        // '|' may appear as a standalone token OR embedded within a word (e.g. p1|p2
        // from a macro expansion). In both cases, everything after '|' is order-only.
        let mut normal = Vec::new();
        let mut order_only = Vec::new();
        let mut is_order_only = false;
        for token in expanded.split_whitespace() {
            if token.is_empty() { continue; }
            if token == ".WAIT" { continue; } // filter .WAIT markers
            if token == "|" {
                is_order_only = true;
                continue;
            }
            // Handle '|' embedded within a token (e.g. "p1|p2" from macro expansion).
            if token.contains('|') {
                let parts: Vec<&str> = token.splitn(2, '|').collect();
                let before = parts[0];
                let after = parts[1];
                if !before.is_empty() && before != ".WAIT" {
                    if is_order_only {
                        order_only.push(before.to_string());
                    } else {
                        normal.push(before.to_string());
                    }
                }
                is_order_only = true;
                if !after.is_empty() && after != ".WAIT" {
                    order_only.push(after.to_string());
                }
                continue;
            }
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
        // SE texts paired with the per-rule static stem (for correct $* expansion when
        // multiple static pattern rules match the same target with different stems).
        let mut se_prereq_texts: Vec<(String, String)> = Vec::new();
        let mut se_order_only_texts: Vec<(String, String)> = Vec::new();
        let mut recipe = Vec::new();
        let mut recipe_source_file = String::new();

        for rule in rules {
            all_prereqs.extend(rule.prerequisites.clone());
            all_order_only.extend(rule.order_only_prerequisites.clone());
            if let Some(ref text) = rule.second_expansion_prereqs {
                se_prereq_texts.push((text.clone(), rule.static_stem.clone()));
            }
            if let Some(ref text) = rule.second_expansion_order_only {
                se_order_only_texts.push((text.clone(), rule.static_stem.clone()));
            }
            if !rule.recipe.is_empty() {
                recipe = rule.recipe.clone();
                recipe_source_file = rule.source_file.clone();
            }
        }

        // In collect_plans_mode (parallel build), extract .WAIT groups before filtering.
        // These groups encode the ordering constraint: targets in group N+1 must not start
        // until all targets in group N (and the barrier sentinel for group N) are Done.
        // This is computed from non-SE prereqs here; SE prereqs are handled below.
        let mut wait_groups = if self.collect_plans_mode {
            extract_wait_groups(&all_prereqs)
        } else {
            Vec::new()
        };

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
        // Accumulate raw SE tokens (with .WAIT) for wait_groups extraction in parallel mode.
        let mut se_raw_tokens_for_wait: Vec<String> = Vec::new();

        if !se_prereq_texts.is_empty() || !se_order_only_texts.is_empty() {
            // The global static_stem (last rule's stem) is used for $* in the auto-var
            // base, but each rule's SE text is expanded with that rule's own stem so
            // that $$* refers to the correct per-rule match.
            let global_stem = rules.iter()
                .rev()
                .find(|r| !r.static_stem.is_empty())
                .map(|r| r.static_stem.clone())
                .unwrap_or_default();
            let oo_refs: Vec<&str> = auto_var_order_only.iter().map(|s| s.as_str()).collect();

            let collected_target_vars = self.collect_target_vars(target);

            for (text, rule_stem) in &se_prereq_texts {
                // Use per-rule stem for $* so that each static pattern rule's $$*
                // expands to its own match stem, not the global (last) stem.
                let effective_stem = if rule_stem.is_empty() { &global_stem } else { rule_stem };
                let mut rule_auto_vars = self.make_auto_vars(target, &auto_var_prereqs, &oo_refs, effective_stem);
                // $? is always empty in second-expansion context.
                rule_auto_vars.insert("?".to_string(), String::new());
                self.apply_target_vars_to_auto_vars(&collected_target_vars.0, &mut rule_auto_vars);
                let (normal, oo) = self.second_expand_prereqs(text, &rule_auto_vars, target);
                se_expanded_prereqs.extend(normal);
                se_expanded_order_only.extend(oo);
                // Collect raw SE tokens (including .WAIT) for wait_groups extraction.
                // Include both normal and order-only tokens, treating '|' as transparent
                // so that .WAIT before order-only prereqs creates proper ordering.
                if self.collect_plans_mode {
                    *self.state.in_second_expansion.borrow_mut() = true;
                    let raw_expanded = self.state.expand_with_auto_vars(text, &rule_auto_vars);
                    *self.state.in_second_expansion.borrow_mut() = false;
                    for token in raw_expanded.split_whitespace() {
                        if token == "|" { continue; } // skip '|' but keep processing
                        se_raw_tokens_for_wait.push(token.to_string());
                    }
                }
            }
            for (text, rule_stem) in &se_order_only_texts {
                let effective_stem = if rule_stem.is_empty() { &global_stem } else { rule_stem };
                let mut rule_auto_vars = self.make_auto_vars(target, &auto_var_prereqs, &oo_refs, effective_stem);
                rule_auto_vars.insert("?".to_string(), String::new());
                self.apply_target_vars_to_auto_vars(&collected_target_vars.0, &mut rule_auto_vars);
                let (normal, oo) = self.second_expand_prereqs(text, &rule_auto_vars, target);
                se_expanded_order_only.extend(normal);
                se_expanded_order_only.extend(oo);
            }
        }

        // sv 62706: process extra SE texts from vpath-merged local rules.
        // These carry side-effects (e.g. $(info ...)) and their prereq results are
        // also appended, matching GNU Make's behaviour of second-expanding both rules.
        if let Some(extra_encoded) = self.vpath_extra_se_texts.remove(target) {
            for encoded in &extra_encoded {
                // Each entry is "local_target\x00stem\x00se_text"
                let mut parts = encoded.splitn(3, '\x00');
                let local_target = parts.next().unwrap_or(target);
                let stem = parts.next().unwrap_or("");
                let text = parts.next().unwrap_or("");
                if !text.is_empty() {
                    let oo_refs: Vec<&str> = Vec::new();
                    let empty_prereqs: Vec<String> = Vec::new();
                    let mut av = self.make_auto_vars(local_target, &empty_prereqs, &oo_refs, stem);
                    av.insert("?".to_string(), String::new());
                    let (normal, oo) = self.second_expand_prereqs(text, &av, local_target);
                    se_expanded_prereqs.extend(normal);
                    se_expanded_order_only.extend(oo);
                }
            }
        }

        // Update wait_groups to include any .WAIT markers from SE-expanded normal prereqs.
        // This handles the case where all prereqs come from second expansion (e.g.
        // `all: $$(pre)` where `pre = .WAIT pre1 .WAIT pre2`).
        if self.collect_plans_mode && se_raw_tokens_for_wait.iter().any(|t| t == ".WAIT") {
            // Build combined list: non-SE prereqs (already filtered) + SE raw tokens.
            // For wait_groups, we construct a unified token list. Non-SE prereqs have
            // no .WAIT (already filtered above), so they form one big group before
            // the SE groups. If both non-SE and SE prereqs have content, combine them.
            let mut combined_for_wait = all_prereqs.clone(); // non-SE (no .WAIT)
            combined_for_wait.extend(se_raw_tokens_for_wait); // SE tokens (with .WAIT)
            wait_groups = extract_wait_groups(&combined_for_wait);
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

        // Glob-expand wildcard patterns in non-SE prerequisites.
        // GNU Make expands wildcards in targets/prerequisites at read time; we do it
        // here before building so that patterns like a.t* expand to real files.
        all_prereqs = Self::glob_expand_prereqs(all_prereqs);
        all_order_only = Self::glob_expand_prereqs(all_order_only);

        // Apply shuffle to prerequisite ordering (unless .NOTPARALLEL is set).
        // GNU Make shuffles regular and order-only prerequisites together as one combined
        // list, then splits the result back into regular/order-only by original membership.
        // This ensures the shuffled execution order interleaves both categories, matching
        // GNU Make behavior for --shuffle=reverse (e.g. `a_: b_ c_ | d_ e_` reversed
        // gives `e_ d_ c_ b_` not `c_ b_` then `e_ d_`).
        //
        // `combined_build_order`: if Some, contains the interleaved execution order
        // (tagged with is_regular) that steps 1 and 2 below should use instead of
        // the separate all_prereqs/all_order_only loops.
        let mut combined_build_order: Option<Vec<(String, bool)>> = None;
        if self.shuffle.is_some() && !self.db.not_parallel {
            // Tag each prereq with whether it's regular (true) or order-only (false).
            let mut tagged: Vec<(String, bool)> = all_prereqs.drain(..)
                .map(|p| (p, true))
                .chain(all_order_only.drain(..).map(|p| (p, false)))
                .collect();
            // Shuffle by extracting names, shuffling them, then re-applying the permutation
            // to the tagged list (preserving the regular/order-only flag with each element).
            let names: Vec<String> = tagged.iter().map(|(s, _)| s.clone()).collect();
            let shuffled_names = self.shuffle_list(names);
            // Reconstruct with correct flags: match each shuffled name back to its
            // first remaining tagged entry (handles duplicates correctly).
            let mut new_tagged: Vec<(String, bool)> = Vec::with_capacity(tagged.len());
            for name in shuffled_names {
                let pos = tagged.iter().position(|(s, _)| s == &name).unwrap_or(0);
                new_tagged.push(tagged.remove(pos));
            }
            // Split back into regular and order-only (still in original declaration order
            // for auto vars, but `combined_build_order` holds the interleaved order).
            for (name, is_regular) in &new_tagged {
                if *is_regular {
                    all_prereqs.push(name.clone());
                } else {
                    all_order_only.push(name.clone());
                }
            }
            combined_build_order = Some(new_tagged);

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
            if let Some(target_time) = self.file_mtime(target).or_else(|| {
                self.find_in_vpath(target).and_then(|f| self.file_mtime(&f))
            }) {
                // Start with the explicitly-collected prereqs.
                let mut check_prereqs: Vec<String> = all_prereqs.clone();

                // If there is no recipe from explicit rules yet, peek at the pattern rule
                // to include its prereqs in the check (read-only, no building).
                // If the pattern rule has SE prereqs, we cannot determine up-to-date status
                // without performing the expansion (the SE might produce explicitly-mentioned
                // files that need building).  In that case skip the precheck entirely.
                let mut pat_rule_has_se = false;
                if recipe.is_empty() {
                    let pat_find = self.find_pattern_rule_inner(target, &all_prereqs);
                    if let Some((pat_rule, stem)) = pat_find {
                        if pat_rule.second_expansion_prereqs.is_some()
                            || pat_rule.second_expansion_order_only.is_some()
                        {
                            // Can't precheck without SE expansion — fall through to full build.
                            pat_rule_has_se = true;
                        } else {
                            let matched_pt_pre: String = pat_rule.targets.iter()
                                .find(|pt| match_pattern(pt, target).as_deref() == Some(stem.as_str()))
                                .cloned()
                                .unwrap_or_else(|| "%".to_string());
                            for p in &pat_rule.prerequisites {
                                if p != ".WAIT" {
                                    check_prereqs.push(subst_stem_in_prereq_dir(p, &stem, &matched_pt_pre));
                                }
                            }
                        }
                    }
                }

                if pat_rule_has_se {
                    // Skip precheck: pattern rule has SE prereqs that we can't evaluate here.
                    // Fall through to the full build path which handles SE expansion.
                } else {

                // Collect skipped intermediates so we can visit them for their
                // PHONY/order-only prereqs even when the target is up to date.
                let mut skipped_intermediates: Vec<String> = Vec::new();
                let any_prereq_newer = check_prereqs.iter().any(|p| {
                    if p == ".WAIT" { return false; }
                    if self.what_if.iter().any(|w| normalize_path(w) == normalize_path(p)) { return true; }
                    // Also check VPATH-resolved path against what-if list
                    if let Some(ref vp) = self.find_in_vpath(p) {
                        if self.what_if.iter().any(|w| normalize_path(w) == normalize_path(vp)) { return true; }
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
                                skipped_intermediates.push(p.clone());
                                return false; // secondary missing file: skip
                            }
                            if self.db.is_intermediate(p) && !self.db.is_notintermediate(p) {
                                skipped_intermediates.push(p.clone());
                                return false; // deleted intermediate: skip (sources up to date)
                            }
                            // Regular file that doesn't exist: must be built, treat as newer.
                            true
                        }
                    }
                });
                if !any_prereq_newer {
                    // Target is up to date relative to its normal prerequisites.
                    // However, order-only prerequisites must still be built even when
                    // the target doesn't need rebuilding.  This is crucial for PHONY
                    // order-only prereqs (like an output directory or a "baz: touch baz"
                    // rule) which should always execute regardless of the target's state.
                    // GNU Make always builds order-only prereqs before deciding whether
                    // to rebuild the target.
                    //
                    // Also visit skipped intermediates so that any PHONY order-only
                    // prereqs within them still run (e.g. `intermed: | phony`).
                    // IMPORTANT: We do NOT rebuild the intermediate itself (it was skipped
                    // because its normal prereqs don't cause a rebuild of the parent).
                    // We only run its order-only PHONY prereqs.

                    // Collect phony order-only prereqs from skipped intermediates so we
                    // can build them AND include them in the parallel plan for this target.
                    let mut transitive_oo_phony: Vec<String> = Vec::new();
                    for skipped in &skipped_intermediates {
                        let oo_items: Vec<String> = self.db.rules.get(skipped.as_str())
                            .map(|rules| rules.iter()
                                .flat_map(|r| r.order_only_prerequisites.iter().cloned())
                                .collect())
                            .unwrap_or_default();
                        for oo_p in oo_items {
                            if !transitive_oo_phony.contains(&oo_p) {
                                transitive_oo_phony.push(oo_p.clone());
                            }
                            let _ = self.build_target(&oo_p);
                        }
                    }
                    for prereq in all_order_only.clone() {
                        let _ = self.build_target(&prereq);
                    }

                    // In collect_plans_mode (parallel path), create a plan for this target
                    // so the BFS can reach the transitive PHONY order-only prereqs and run them,
                    // AND so the dependency chain is preserved for targets that are up-to-date
                    // but have prerequisites that other targets depend on (e.g. `2.a: 1.c` where
                    // 2.a is up-to-date but 2.b must still wait for 1.c to be built).
                    if self.collect_plans_mode {
                        // Combine direct order-only prereqs with transitive ones from skipped intermediates.
                        let mut plan_order_only = all_order_only.clone();
                        for p in &transitive_oo_phony {
                            if !plan_order_only.contains(p) {
                                plan_order_only.push(p.clone());
                            }
                        }
                        // Always create a plan entry (even when target is up-to-date and has no
                        // order-only prereqs) so that the parallel scheduler correctly tracks this
                        // target as a dependency that must complete before its dependents run.
                        // Without this, targets like `2.a: 1.c` (where 2.a is already up-to-date)
                        // would not appear in the plan, and targets depending on 2.a (e.g. 2.b)
                        // would start without waiting for 1.c to complete.
                        if !all_prereqs.is_empty() || !plan_order_only.is_empty() {
                            if let Some(ref mut plans) = self.pending_plans {
                                if !plans.contains_key(target) {
                                    // Compute effective_wait_groups for .NOTPARALLEL: target
                                    let effective_wg_uptodate = if wait_groups.is_empty()
                                        && self.db.not_parallel_targets.contains(target)
                                        && all_prereqs.len() > 1
                                    {
                                        all_prereqs.iter().map(|p| vec![p.clone()]).collect()
                                    } else {
                                        wait_groups.clone()
                                    };
                                    plans.insert(target.to_string(), parallel::TargetPlan {
                                        target: target.to_string(),
                                        prerequisites: all_prereqs.clone(),
                                        order_only: plan_order_only,
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
                                        wait_groups: effective_wg_uptodate,
                                        intermediate_also_make: Vec::new(),
                                    });
                                }
                            }
                        }
                    }

                    return Ok(false);
                }
                } // end else (pat_rule_has_se == false)
            }
        }

        // Push this target's "for_prereqs" vars onto the inheritance stack.
        // Use for_prereqs (index 1) so that private target-specific vars don't block
        // ancestor non-private vars from propagating to prerequisites.
        let my_target_vars = self.collect_target_vars(target);
        self.inherited_vars_stack.push(my_target_vars.1);

        let mut any_prereq_rebuilt = false;
        let mut prereq_errors = Vec::new();

        // Resolve .EXTRA_PREREQS.
        // Global extra prereqs are built BEFORE regular prereqs.
        // Target-specific extra prereqs are built AFTER regular prereqs.
        // When this target is itself being built as an extra prereq (skip_extra_prereqs==true),
        // skip .EXTRA_PREREQS to prevent circular/recursive application.
        let (extra_prereqs, extra_prereqs_target_specific) = if self.skip_extra_prereqs {
            (Vec::new(), false)
        } else {
            self.get_extra_prereqs(target)
        };

        // Build a helper closure for building extra prereqs with skip=true.
        // We do this inline since Rust closures can't borrow self mutably more than once.

        // Step 0: build GLOBAL .EXTRA_PREREQS (before regular prereqs).
        if !extra_prereqs_target_specific {
            let prev_skip = self.skip_extra_prereqs;
            self.skip_extra_prereqs = true;
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
                            self.skip_extra_prereqs = prev_skip;
                            self.inherited_vars_stack.pop();
                            return Err(propagated);
                        }
                    }
                }
            }
            self.skip_extra_prereqs = prev_skip;
        }

        // Step 0.5: if there is no recipe from explicit rules, peek at the matching
        // pattern rule to get its prereqs and build them BEFORE explicit prereqs.
        // GNU Make always builds pattern-rule prereqs first, then explicit prereqs.
        //
        // For SE pattern rules, we perform the SE expansion HERE (using all_prereqs as
        // the basis for $^/$+/$< — the list is known before building) and CACHE the
        // result. The pattern rule block later uses this cached expansion to avoid
        // double-expansion (which would trigger $(info ...) side effects twice).
        //
        // pre_pat_se_expansion: if non-None, means we already SE-expanded the pattern
        // rule's prereqs and the pattern rule block should skip its own SE expansion.
        let mut pre_pat_se_expansion: Option<(Vec<String>, Vec<String>)> = None;
        let pre_pat_prereqs: Vec<String> = if recipe.is_empty() {
            if let Some((pat_rule, stem)) = self.find_pattern_rule_inner(target, &all_prereqs) {
                let matched_pt: String = pat_rule.targets.iter()
                    .find(|pt| match_pattern(pt, target).as_deref() == Some(stem.as_str()))
                    .cloned()
                    .unwrap_or_else(|| "%".to_string());
                // Collect prereqs with literals (no %) first, then pattern prereqs.
                // GNU Make builds literal prereqs before pattern prereqs (sv 60435 test 22).
                let mut prereqs: Vec<String> = {
                    let mut lit: Vec<String> = Vec::new();
                    let mut pat: Vec<String> = Vec::new();
                    for p_str in pat_rule.prerequisites.iter() {
                        if p_str.as_str() == ".WAIT" { continue; }
                        let expanded = subst_stem_in_prereq_dir(p_str, &stem, &matched_pt);
                        if p_str.contains('%') { pat.push(expanded); } else { lit.push(expanded); }
                    }
                    lit.extend(pat);
                    lit
                };
                // For SE pattern rules, also compute the SE expansion now (using current
                // all_prereqs for auto vars) so we can build those prereqs first.
                if pat_rule.second_expansion_prereqs.is_some() || pat_rule.second_expansion_order_only.is_some() {
                    let oo_refs: Vec<&str> = all_order_only.iter().map(|s| s.as_str()).collect();
                    let mut pat_se_auto_vars = self.make_auto_vars(target, &all_prereqs, &oo_refs, &stem);
                    pat_se_auto_vars.insert("?".to_string(), String::new());
                    let collected_target_vars = self.collect_target_vars(target);
                    self.apply_target_vars_to_auto_vars(&collected_target_vars.0, &mut pat_se_auto_vars);
                    let mut se_normal: Vec<String> = Vec::new();
                    let mut se_oo: Vec<String> = Vec::new();
                    if let Some(ref text) = pat_rule.second_expansion_prereqs.clone() {
                        let stem_subst = subst_stem_in_se_text_dir(text, &stem, &matched_pt);
                        let (normal, oo) = self.second_expand_prereqs(&stem_subst, &pat_se_auto_vars, target);
                        se_normal.extend(normal.iter().cloned());
                        se_oo.extend(oo.iter().cloned());
                        prereqs.extend(normal);
                    }
                    if let Some(ref text) = pat_rule.second_expansion_order_only.clone() {
                        let stem_subst = subst_stem_in_se_text_dir(text, &stem, &matched_pt);
                        let (normal, oo) = self.second_expand_prereqs(&stem_subst, &pat_se_auto_vars, target);
                        se_oo.extend(normal);
                        se_oo.extend(oo);
                    }
                    pre_pat_se_expansion = Some((se_normal, se_oo));
                }
                prereqs
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        // Build pattern-rule prereqs first (before explicit prereqs).
        // When shuffle is active, skip prereqs that are already in all_prereqs
        // (they will be built in the correct shuffled order by step 1 below).
        let mut pre_pat_built: std::collections::HashSet<String> = std::collections::HashSet::new();
        for prereq in &pre_pat_prereqs {
            if self.shuffle.is_some() && !self.db.not_parallel && all_prereqs.contains(prereq) {
                pre_pat_built.insert(prereq.clone());
                continue;
            }
            if !pre_pat_built.contains(prereq) {
                match self.build_target(prereq) {
                    Ok(rebuilt) => { if rebuilt { any_prereq_rebuilt = true; } }
                    Err(e) => {
                        let is_new = e.starts_with("No rule to make target '") && !e.contains(", needed by '");
                        let propagated = if is_new {
                            let base = e.trim_end_matches(".  Stop.").trim_end_matches(".");
                            format!("{}, needed by '{}'.  Stop.", base, target)
                        } else { e };
                        if self.keep_going {
                            if is_new { self.print_error_keep_going(&propagated); }
                            prereq_errors.push(propagated);
                        } else {
                            self.inherited_vars_stack.pop();
                            return Err(propagated);
                        }
                    }
                }
                pre_pat_built.insert(prereq.clone());
            }
        }

        // Step 1+2: build non-SE normal prereqs and order-only prereqs.
        // When shuffle is active, use the combined_build_order that interleaves regular
        // and order-only in the shuffled order (GNU Make behavior: both are shuffled together).
        // When no shuffle, use the original separate loops (regular first, then order-only).
        if let Some(ref combined_order) = combined_build_order {
            for (prereq, is_regular) in combined_order.clone() {
                if is_regular {
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
                } else {
                    // order-only: errors are ignored
                    let _ = self.build_target(&prereq);
                }
            }
        } else {
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
        // Use clone() here so se_expanded_prereqs/se_expanded_order_only remain available
        // for the auto-var computation below (which needs the original SE-expanded list).
        all_prereqs.extend(se_expanded_prereqs.clone());
        all_order_only.extend(se_expanded_order_only.clone());
        all_prereqs.retain(|p| p != ".WAIT");
        all_order_only.retain(|p| p != ".WAIT");

        // (Step 5 for target-specific EXTRA_PREREQS is deferred until after pattern rule
        // prereqs are built, so that pattern rule prereqs come before extra prereqs.)

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
                // Find the matched pattern target for dir-aware stem substitution.
                let matched_pt: String = pattern_rule.targets.iter()
                    .find(|pt| match_pattern(pt, target).as_deref() == Some(stem.as_str()))
                    .cloned()
                    .unwrap_or_else(|| "%".to_string());
                // Add the pattern rule's prerequisites/order-only to our lists and build them.
                let mut pat_prereqs: Vec<String> = pattern_rule.prerequisites.iter()
                    .map(|p| subst_stem_in_prereq_dir(p, &stem, &matched_pt))
                    .collect();
                // Glob-expand wildcard patterns in pattern rule prereqs (e.g. %.t* -> a.three a.two).
                pat_prereqs = Self::glob_expand_prereqs(pat_prereqs);
                let mut pat_order_only: Vec<String> = pattern_rule.order_only_prerequisites.iter()
                    .map(|p| subst_stem_in_prereq_dir(p, &stem, &matched_pt))
                    .collect();
                pat_order_only = Self::glob_expand_prereqs(pat_order_only);

                // Handle second expansion for the pattern rule.
                // If Step 0.5 already performed the SE expansion (and cached the result),
                // reuse it to avoid double-expansion (which would trigger $(info ...) twice).
                // Otherwise, perform the SE expansion now using the accumulated explicit prereqs.
                if pattern_rule.second_expansion_prereqs.is_some() || pattern_rule.second_expansion_order_only.is_some() {
                    if let Some((cached_normal, cached_oo)) = pre_pat_se_expansion.take() {
                        // Use cached expansion from Step 0.5 - already built, just add to lists.
                        // Also mark non-stem-derived prereqs as explicitly mentioned.
                        if let Some(ref text) = pattern_rule.second_expansion_prereqs {
                            if !se_text_has_percent(text) {
                                for p in &cached_normal {
                                    self.se_explicitly_mentioned.insert(p.clone());
                                }
                            }
                        }
                        pat_prereqs.extend(cached_normal);
                        pat_order_only.extend(cached_oo);
                    } else {
                        // SE expansion not yet done (Step 0.5 didn't apply) - do it now.
                        // Auto vars are built from the ALREADY-accumulated explicit prereqs
                        // (all_prereqs at this point), giving $+ the value from the explicit
                        // rule(s) - which is what GNU Make uses for $+ in SE pattern rules.
                        let oo_refs: Vec<&str> = all_order_only.iter().map(|s| s.as_str()).collect();
                        let mut pat_se_auto_vars = self.make_auto_vars(target, &all_prereqs, &oo_refs, &stem);
                        pat_se_auto_vars.insert("?".to_string(), String::new());
                        let collected_target_vars = self.collect_target_vars(target);
                        self.apply_target_vars_to_auto_vars(&collected_target_vars.0, &mut pat_se_auto_vars);

                        if let Some(ref text) = pattern_rule.second_expansion_prereqs {
                            let stem_subst = subst_stem_in_se_text_dir(text, &stem, &matched_pt);
                            let (normal, oo) = self.second_expand_prereqs(&stem_subst, &pat_se_auto_vars, target);
                            // Mark non-stem-derived SE prereqs as explicitly mentioned.
                            if !se_text_has_percent(text) {
                                for p in &normal {
                                    self.se_explicitly_mentioned.insert(p.clone());
                                }
                            }
                            pat_prereqs.extend(normal);
                            pat_order_only.extend(oo);
                        }
                        if let Some(ref text) = pattern_rule.second_expansion_order_only {
                            let stem_subst = subst_stem_in_se_text_dir(text, &stem, &matched_pt);
                            let (normal, oo) = self.second_expand_prereqs(&stem_subst, &pat_se_auto_vars, target);
                            if !se_text_has_percent(text) {
                                for p in &normal {
                                    self.se_explicitly_mentioned.insert(p.clone());
                                }
                            }
                            pat_order_only.extend(normal);
                            pat_order_only.extend(oo);
                        }
                    }
                }

                // Build each unique pattern-rule prereq once, but add ALL occurrences
                // (including duplicates) to all_prereqs so that $+ is computed correctly.
                // The pattern rule's prereqs are prepended so they come first in $^/$+.

                // Apply GNU Make prereq ordering (same as build_with_pattern_rule):
                //  1. Standalone-explicit: explicitly mentioned, NOT sharing a rule with any intermediate.
                //  2. Non-explicit (intermediate): not explicitly mentioned.
                //  3. Shared-explicit: explicitly mentioned, but sharing a rule with an intermediate.
                // Must compute before the mutable borrow loop below.
                let pat_prereqs_build_order: Vec<String> = {
                    let standalone: Vec<String> = pat_prereqs.iter()
                        .filter(|p| {
                            self.is_explicitly_mentioned(p)
                                && !self.prereq_shares_rule_with_intermediate(p, &pat_prereqs)
                        })
                        .cloned()
                        .collect();
                    let intermediates_list: Vec<String> = pat_prereqs.iter()
                        .filter(|p| !self.is_explicitly_mentioned(p))
                        .cloned()
                        .collect();
                    let shared_explicit: Vec<String> = pat_prereqs.iter()
                        .filter(|p| {
                            self.is_explicitly_mentioned(p)
                                && self.prereq_shares_rule_with_intermediate(p, &pat_prereqs)
                        })
                        .cloned()
                        .collect();
                    let mut ordered = standalone;
                    ordered.extend(intermediates_list);
                    ordered.extend(shared_explicit);
                    ordered
                };

                let mut already_built: std::collections::HashSet<String> = std::collections::HashSet::new();
                // Also collect which prereqs were already in all_prereqs before pattern rule.
                for p in &all_prereqs {
                    already_built.insert(p.clone());
                }
                // Prepend pattern rule prereqs: they come first (pattern rule is primary).
                let orig_explicit_prereqs = all_prereqs.clone();
                all_prereqs.clear();
                // Build prereqs in ordered sequence (standalone-explicit first).
                for prereq in &pat_prereqs_build_order {
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
                }
                // all_prereqs must remain in DECLARATION ORDER for $^/$+ auto vars.
                for prereq in &pat_prereqs {
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
                    // Compute effective_wait_groups, accounting for .NOTPARALLEL: target.
                    let effective_wg_here = if wait_groups.is_empty()
                        && self.db.not_parallel_targets.contains(target)
                        && all_prereqs.len() > 1
                    {
                        all_prereqs.iter().map(|p| vec![p.clone()]).collect()
                    } else {
                        wait_groups.clone()
                    };
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
                        wait_groups: effective_wg_here,
                        intermediate_also_make: Vec::new(),
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

        // Step 5: build TARGET-SPECIFIC .EXTRA_PREREQS (after ALL prereqs including pattern rule).
        // This is done here (after both explicit and pattern rule prereqs are known and built)
        // so that the extra prereqs always come last in the build order, regardless of whether
        // the recipe came from an explicit rule or a pattern rule.
        let mut extra_prereq_errors: Vec<String> = Vec::new();
        if extra_prereqs_target_specific {
            let prev_skip = self.skip_extra_prereqs;
            self.skip_extra_prereqs = true;
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
                            extra_prereq_errors.push(propagated);
                        } else {
                            self.skip_extra_prereqs = prev_skip;
                            return Err(propagated);
                        }
                    }
                }
            }
            self.skip_extra_prereqs = prev_skip;
        }
        if !extra_prereq_errors.is_empty() {
            return Err(extra_prereq_errors.join("\n"));
        }

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
            // If this target is in .NOTPARALLEL: target_list, force sequential prereqs
            // by putting each prerequisite in its own wait group.
            let effective_wait_groups = if wait_groups.is_empty()
                && self.db.not_parallel_targets.contains(target)
                && all_prereqs.len() > 1
            {
                // Each prereq is its own group: [pre1], [pre2], [pre3], ...
                all_prereqs.iter().map(|p| vec![p.clone()]).collect()
            } else {
                wait_groups.clone()
            };
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
                wait_groups: effective_wait_groups,
                intermediate_also_make: Vec::new(),
            };
            if let Some(ref mut plans) = self.pending_plans {
                plans.insert(target.to_string(), plan);
            }
        }

        if !needs_rebuild {
            return Ok(false);
        }

        // Set up automatic variables (extra_prereqs are excluded from auto vars).
        // Use all_prereqs which by this point contains:
        //   - explicit prereqs + SE-expanded prereqs (always)
        //   - pattern rule prereqs prepended (if recipe came from a pattern rule)
        // This ensures $^ is non-empty when a pattern rule provides both prereqs and recipe
        // for an explicit target that had no prereqs of its own.
        let auto_vars_prereqs_for_recipe: Vec<String> = {
            let mut v = all_prereqs.clone();
            v.retain(|p| p != ".WAIT");
            v
        };
        // Use all_order_only for $| auto var: this includes the pattern rule's order-only
        // prerequisites (added at the pattern rule block above), not just the original
        // explicit-rule order-only prereqs (auto_var_order_only) or SE-expanded ones.
        let auto_vars_order_only_for_recipe: Vec<String> = {
            let mut v = all_order_only.clone();
            v.retain(|p| p != ".WAIT");
            v
        };
        let oo_refs: Vec<&str> = auto_vars_order_only_for_recipe.iter().map(|s| s.as_str()).collect();
        let mut auto_vars = self.make_auto_vars(target, &auto_vars_prereqs_for_recipe, &oo_refs, &pattern_stem);

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
            self.pending_plan_wait_groups = wait_groups.clone();
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
        let mut se_prereq_texts: Vec<(String, String)> = Vec::new();
        let mut se_order_only_texts: Vec<(String, String)> = Vec::new();
        let mut recipe = Vec::new();
        let mut recipe_source_file = String::new();

        for rule in rules {
            all_prereqs.extend(rule.prerequisites.clone());
            all_order_only.extend(rule.order_only_prerequisites.clone());
            if let Some(ref text) = rule.second_expansion_prereqs {
                se_prereq_texts.push((text.clone(), rule.static_stem.clone()));
            }
            if let Some(ref text) = rule.second_expansion_order_only {
                se_order_only_texts.push((text.clone(), rule.static_stem.clone()));
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
            let global_stem = rules.iter()
                .rev()
                .find(|r| !r.static_stem.is_empty())
                .map(|r| r.static_stem.clone())
                .unwrap_or_default();
            let oo_refs: Vec<&str> = auto_var_order_only.iter().map(|s| s.as_str()).collect();
            let collected_target_vars = self.collect_target_vars(target);

            for (text, rule_stem) in &se_prereq_texts {
                let effective_stem = if rule_stem.is_empty() { &global_stem } else { rule_stem };
                let mut rule_auto_vars = self.make_auto_vars(target, &auto_var_prereqs, &oo_refs, effective_stem);
                rule_auto_vars.insert("?".to_string(), String::new());
                self.apply_target_vars_to_auto_vars(&collected_target_vars.0, &mut rule_auto_vars);
                let (normal, oo) = self.second_expand_prereqs(text, &rule_auto_vars, target);
                se_expanded_prereqs.extend(normal);
                se_expanded_order_only.extend(oo);
            }
            for (text, rule_stem) in &se_order_only_texts {
                let effective_stem = if rule_stem.is_empty() { &global_stem } else { rule_stem };
                let mut rule_auto_vars = self.make_auto_vars(target, &auto_var_prereqs, &oo_refs, effective_stem);
                rule_auto_vars.insert("?".to_string(), String::new());
                self.apply_target_vars_to_auto_vars(&collected_target_vars.0, &mut rule_auto_vars);
                let (normal, oo) = self.second_expand_prereqs(text, &rule_auto_vars, target);
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
                let matched_pt_bwrp: String = pattern_rule.targets.iter()
                    .find(|pt| match_pattern(pt, target).as_deref() == Some(stem.as_str()))
                    .cloned()
                    .unwrap_or_else(|| "%".to_string());
                let mut pat_prereqs: Vec<String> = pattern_rule.prerequisites.iter()
                    .map(|p| subst_stem_in_prereq_dir(p, &stem, &matched_pt_bwrp))
                    .collect();
                let mut pat_order_only: Vec<String> = pattern_rule.order_only_prerequisites.iter()
                    .map(|p| subst_stem_in_prereq_dir(p, &stem, &matched_pt_bwrp))
                    .collect();

                if pattern_rule.second_expansion_prereqs.is_some() || pattern_rule.second_expansion_order_only.is_some() {
                    let oo_refs: Vec<&str> = all_order_only.iter().map(|s| s.as_str()).collect();
                    let mut pat_se_auto_vars = self.make_auto_vars(target, &all_prereqs, &oo_refs, &stem);
                    let collected_target_vars = self.collect_target_vars(target);
                    self.apply_target_vars_to_auto_vars(&collected_target_vars.0, &mut pat_se_auto_vars);

                    if let Some(ref text) = pattern_rule.second_expansion_prereqs {
                        let stem_subst = subst_stem_in_se_text_dir(text, &stem, &matched_pt_bwrp);
                        let (normal, oo) = self.second_expand_prereqs(&stem_subst, &pat_se_auto_vars, target);
                        pat_prereqs.extend(normal);
                        pat_order_only.extend(oo);
                    }
                    if let Some(ref text) = pattern_rule.second_expansion_order_only {
                        let stem_subst = subst_stem_in_se_text_dir(text, &stem, &matched_pt_bwrp);
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
            // Grouped targets don't support .WAIT groups; use empty.
            self.pending_plan_wait_groups = Vec::new();
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
        //
        // GNU Make semantics: all double-colon rules in the family are checked for
        // need-to-rebuild using the TARGET's STATE AT BUILD START, not after each
        // individual rule runs.  This ensures that if the target doesn't exist
        // initially, ALL rules run — even if an earlier rule in the family creates
        // the target and later rules would otherwise see it as "up to date".
        let initial_target_mtime: Option<SystemTime> = get_mtime(target);
        let target_initially_missing = initial_target_mtime.is_none();

        let mut any_rebuilt = false;
        // For collect_plans_mode: track virtual node keys for serializing rules.
        // rule_idx counts ALL rules; virtual_idx counts rules that actually need rebuilding.
        let mut rule_idx: usize = 0;
        // Name of the last virtual node added (so subsequent rules can depend on it).
        let mut last_virtual_key: Option<String> = None;

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
                let mut base_auto_vars = self.make_auto_vars(target, &empty_prereqs, &empty_oo, stem);
                base_auto_vars.insert("?".to_string(), String::new());

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
            } else if target_initially_missing {
                // Target didn't exist when we started building this double-colon family;
                // all rules in the family run regardless of post-rule target creation.
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

            // In collect_plans_mode (parallel build graph resolution), multiple
            // double-colon rules for the same target would overwrite each other's plan
            // if we called execute_recipe naively (plans are keyed by target name).
            //
            // Strategy: create a synthetic virtual node per rule ("target::0", "target::1",
            // etc.) and make the real "target" a zero-recipe completion marker that depends
            // on the last virtual node. Each virtual node depends on its rule's prerequisites
            // PLUS the previous virtual node (enforcing serial execution within the family).
            // This allows rule 0 (no prereqs) to start immediately while rule 1 waits for
            // both its prerequisites and rule 0 to finish — matching GNU Make semantics.
            if self.collect_plans_mode {
                let virtual_key = format!("{}::{}", target, rule_idx);
                rule_idx += 1;
                let rule_source = rule.source_file.clone();
                let expanded_lines: Vec<(usize, String)> = rule.recipe.iter().map(|(lineno, line)| {
                    *self.state.current_file.borrow_mut() = rule_source.clone();
                    *self.state.current_line.borrow_mut() = *lineno;
                    let preprocessed = preprocess_recipe_bsnl(line);
                    let expanded = self.state.expand_with_auto_vars(&preprocessed, &auto_vars);
                    let sub_lines = split_recipe_sub_lines(&expanded);
                    let rejoined = sub_lines.join("\n");
                    (*lineno, rejoined)
                }).collect();
                let extra_exports = self.target_extra_exports.clone();
                let extra_unexports: Vec<String> = self.target_extra_unexports.iter().cloned().collect();
                // Build the virtual node's prerequisites: rule's own prereqs + previous virtual node.
                let mut virtual_prereqs = prereqs.clone();
                if let Some(ref prev) = last_virtual_key {
                    if !virtual_prereqs.contains(prev) {
                        virtual_prereqs.push(prev.clone());
                    }
                }
                let virtual_plan = parallel::TargetPlan {
                    target: virtual_key.clone(),
                    prerequisites: virtual_prereqs,
                    order_only: order_only.clone(),
                    recipe: expanded_lines,
                    source_file: rule.source_file.clone(),
                    auto_vars: auto_vars.clone(),
                    is_phony,
                    needs_rebuild: true,
                    grouped_primary: None,
                    grouped_siblings: Vec::new(),
                    extra_exports,
                    extra_unexports,
                    is_intermediate: self.db.is_intermediate(target),
                    wait_groups: Vec::new(),
                    intermediate_also_make: Vec::new(),
                };
                if let Some(ref mut plans) = self.pending_plans {
                    plans.insert(virtual_key.clone(), virtual_plan);
                }
                last_virtual_key = Some(virtual_key);
                self.any_recipe_ran = true;
                any_rebuilt = true;
                self.target_extra_exports.clear();
                self.target_extra_unexports.clear();
                continue;
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

        // In collect_plans_mode, ensure this target has a plan entry in the plans map.
        // Without one, the parallel scheduler won't know about this target and
        // dependents will never be unblocked.
        if self.collect_plans_mode {
            if let Some(ref mut plans) = self.pending_plans {
                if let Some(last_key) = last_virtual_key {
                    // We created at least one virtual node. Create a completion-marker
                    // plan for the real target name that depends on the last virtual node.
                    // It has an empty recipe so the scheduler completes it instantly once
                    // its only prerequisite (the last virtual node) is done.
                    plans.insert(target.to_string(), parallel::TargetPlan {
                        target: target.to_string(),
                        prerequisites: vec![last_key],
                        order_only: Vec::new(),
                        recipe: Vec::new(),
                        source_file: String::new(),
                        auto_vars: HashMap::new(),
                        is_phony,
                        needs_rebuild: true,  // run through the scheduler (waits for prereqs)
                        grouped_primary: None,
                        grouped_siblings: Vec::new(),
                        extra_exports: HashMap::new(),
                        extra_unexports: Vec::new(),
                        is_intermediate: self.db.is_intermediate(target),
                        wait_groups: Vec::new(),
                        intermediate_also_make: Vec::new(),
                    });
                } else if !plans.contains_key(target) {
                    // No rules needed rebuilding. Record a no-op plan so the
                    // scheduler can mark this target Done and unblock dependents.
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
                        is_intermediate: self.db.is_intermediate(target),
                        wait_groups: Vec::new(),
                        intermediate_also_make: Vec::new(),
                    });
                }
            }
        }

        Ok(any_rebuilt)
    }

    /// sv 62706 ordering: Fire the SE side effects ($(info ...) etc.) for `target`'s
    /// implicit rule WITHOUT running the recipe.  This simulates the "implicit rule
    /// search" phase in GNU Make where SE prereqs are expanded as part of the search,
    /// before the actual build begins.
    ///
    /// Specifically: when building `hello.tsk` via `%.tsk: %.o $$(info ...)`, GNU Make
    /// searches for a rule to satisfy `hello.o` and during that search fires `hello.o`'s
    /// own SE expansion.  Only after that does it fire `hello.tsk`'s SE expansion during
    /// the build phase.  This function replicates that behaviour so that:
    ///
    ///   1. `hello.o`'s SE side effects fire (e.g. "second expansion of hello.o prereqs")
    ///   2. `hello.tsk`'s SE side effects fire (e.g. "second expansion of hello.tsk prereqs")
    ///   3. `hello.o`'s recipe runs ("hello.o")
    ///   4. `hello.tsk`'s recipe runs ("hello.tsk from hello.o")
    ///
    /// After this function marks `target` in `se_search_fired`, the next call to
    /// `build_with_pattern_rule` for `target` will skip re-firing the SE expansion
    /// (removing `target` from `se_search_fired` as it goes so that genuinely later
    /// re-builds still fire SE).
    fn trigger_prereq_se_side_effects(&mut self, target: &str) {
        // Only applies when the target has not been built yet.
        if self.built.contains_key(target) {
            return;
        }
        // Find the pattern rule for target.
        let pat = self.find_pattern_rule_inner(target, &[]);
        let (rule, stem) = match pat {
            Some(x) => x,
            None => return,
        };
        // Only relevant if the rule has SE prereqs.
        let se_text = match rule.second_expansion_prereqs.clone() {
            Some(t) => t,
            None => return,
        };
        // Find which pattern target matched (for stem-directory substitution).
        let matched_pat: String = rule.targets.iter()
            .find(|pt| match_pattern(pt, target).as_deref() == Some(stem.as_str()))
            .cloned()
            .unwrap_or_default();
        let stem_subst = subst_stem_in_se_text_dir(&se_text, &stem, &matched_pat);

        // Recursively fire SE for non-dollar words in this rule's SE text first.
        let pre_words = se_extract_non_dollar_words(&stem_subst);
        for word in pre_words {
            self.trigger_prereq_se_side_effects(&word);
        }

        // Now fire the SE expansion for this target for side effects only.
        // (We discard the results — we only care about $(info ...) / $(shell ...) etc.)
        let oo_refs: Vec<&str> = Vec::new();
        let auto_vars = self.make_auto_vars(target, &[], &oo_refs, &stem);
        self.second_expand_prereqs(&stem_subst, &auto_vars, target);

        // Mark this target so build_with_pattern_rule skips re-firing SE.
        self.se_search_fired.insert(target.to_string());
    }

    fn build_with_pattern_rule(&mut self, target: &str, rule: &Rule, stem: &str, is_phony: bool) -> Result<bool, String> {
        // Determine which pattern target in the rule matched `target` (needed for
        // GNU Make "stem directory" behaviour in prerequisite substitution).
        let matched_pattern_target: &str = rule.targets.iter()
            .find(|pt| match_pattern(pt, target).as_deref() == Some(stem))
            .map(|s| s.as_str())
            .unwrap_or("%"); // fallback

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
        // For normal (non-SE) prerequisites, substitute % with the stem using the
        // dir-aware rule: when stem has a directory but the matched pattern does not,
        // only the base stem is used for words where % is not the first character,
        // and the directory is prepended to the result.
        // Collect raw prereqs (with .WAIT) for wait_groups extraction before filtering.
        let prereqs_with_wait: Vec<String> = if self.collect_plans_mode {
            rule.prerequisites.iter()
                .map(|p| {
                    if p == ".WAIT" { ".WAIT".to_string() }
                    else { subst_stem_in_prereq_dir(p, stem, matched_pattern_target) }
                })
                .collect()
        } else {
            Vec::new()
        };
        // .WAIT markers are filtered since they are ordering hints, not real targets.
        let mut prereqs: Vec<String> = rule.prerequisites.iter()
            .filter(|p| p.as_str() != ".WAIT")
            .map(|p| subst_stem_in_prereq_dir(p, stem, matched_pattern_target))
            .collect();
        // Glob-expand wildcard patterns in the substituted prerequisites (e.g. a.t* → a.three a.two).
        prereqs = Self::glob_expand_prereqs(prereqs);

        // Also expand any explicit prerequisites that came from `build_target_inner`
        // combining explicit rules with this pattern rule.
        // (Already handled via all_prereqs in build_target_inner for explicit rules.)

        // Handle second-expansion prerequisites for pattern rules.
        let mut order_only: Vec<String> = rule.order_only_prerequisites.iter()
            .filter(|p| p.as_str() != ".WAIT")
            .map(|p| subst_stem_in_prereq_dir(p, stem, matched_pattern_target))
            .collect();
        order_only = Self::glob_expand_prereqs(order_only);

        // SE expansion is deferred until AFTER non-SE prereqs are built (see below).
        // This matches GNU Make behavior where $(info ...) in SE prereqs fires after
        // the immediately-available (non-SE) prerequisites have been visited.
        // `has_se` tracks whether we need to do SE expansion later.
        let has_se = rule.second_expansion_prereqs.is_some() || rule.second_expansion_order_only.is_some();

        // Save original prereq order before shuffling, so auto vars ($^, $<, etc.)
        // reflect the original declaration order (shuffle only affects BUILD order,
        // not the values of automatic variables — GNU Make behavior).
        // For SE rules this will be extended after SE expansion adds more prereqs.
        let mut prereqs_original_order = prereqs.clone();
        let mut order_only_original_order = order_only.clone();

        // Apply shuffle to pattern rule prerequisites.
        if self.shuffle.is_some() && !self.db.not_parallel {
            prereqs = self.shuffle_list(prereqs);
            order_only = self.shuffle_list(order_only);
        }

        // Pre-check: if target exists and none of the prerequisites are newer (by
        // effective mtime, which accounts for deleted intermediate files), return
        // immediately as "up to date".  This prevents unnecessarily rebuilding deleted
        // intermediate files when the final target is already fresh.
        //
        // GNU Make's rule: if the final target exists and its non-intermediate sources
        // haven't changed, intermediate files are NOT rebuilt.  Intermediate files that
        // don't exist are considered to have a "virtual" mtime equal to their sources.
        //
        // Skip this check when always_make (-B), the target is phony, the recipe is
        // empty (we can't decide without building), when in collect_plans_mode
        // (parallel build graph collects all dependencies regardless), or when the
        // rule has SE prerequisites that might expand to explicitly-mentioned files
        // (we cannot determine up-to-date status until SE is done and those files
        // are recognized as explicitly mentioned — sv 60188 subtest 2).
        if !is_phony && !self.always_make && !self.collect_plans_mode
            && !rule.second_expansion_prereqs.as_deref().map_or(false, se_text_has_non_pattern_word)
            && !rule.second_expansion_order_only.as_deref().map_or(false, se_text_has_non_pattern_word)
        {
            if let Some(target_time) = self.file_mtime(target).or_else(|| {
                self.find_in_vpath(target).and_then(|f| self.file_mtime(&f))
            }) {
                let any_prereq_newer = prereqs.iter().any(|p| {
                    if p == ".WAIT" { return false; }
                    if self.what_if.iter().any(|w| normalize_path(w) == normalize_path(p)) { return true; }
                    if let Some(ref vp) = self.find_in_vpath(p) {
                        if self.what_if.iter().any(|w| normalize_path(w) == normalize_path(vp)) { return true; }
                    }
                    if self.db.is_phony(p) { return true; }
                    // Check if the file actually exists on disk.
                    let file_exists = self.file_mtime(p).is_some()
                        || self.find_in_vpath(p).and_then(|f| self.file_mtime(&f)).is_some();
                    if !file_exists {
                        // File doesn't exist. Determine if it needs to be built:
                        if self.db.is_secondary(p) && !self.db.is_notintermediate(p) {
                            return false; // secondary missing file: skip
                        }
                        // Explicitly-mentioned (non-intermediate) non-existent files MUST be built.
                        // Exception: if the file is also declared .INTERMEDIATE, treat it as
                        // intermediate regardless of explicit mentions (sv 60188).
                        if (self.is_explicitly_mentioned(p) && !self.db.is_intermediate(p))
                            || self.db.is_notintermediate(p) {
                            return true;
                        }
                        // Intermediate file: use effective mtime of its sources.
                        return match self.effective_mtime(p, 0) {
                            Some(pt) => pt > target_time,
                            // No sources found: rebuild only if the intermediate has a
                            // buildable recipe (distinguishes empty-recipe sv 60188 files
                            // from normal intermediates like `foo.a: ; touch $@`).
                            None => self.intermediate_has_buildable_recipe(p),
                        };
                    }
                    // File exists: compare actual mtime (or effective mtime if file exists
                    // but has newer-source deps like what-if).
                    match self.effective_mtime(p, 0) {
                        Some(pt) => pt > target_time,
                        None => false,
                    }
                });
                if !any_prereq_newer {
                    // Build order-only prereqs if needed (they don't affect rebuild decision).
                    for prereq in &order_only {
                        let _ = self.build_target(prereq);
                    }
                    // Mark also-make siblings as covered (up to date).
                    for sib in &also_make_siblings {
                        self.grouped_covered.insert(sib.clone());
                        self.built.insert(sib.clone(), false);
                    }
                    for sib in &concrete_grouped_siblings {
                        self.grouped_covered.insert(sib.clone());
                        self.built.insert(sib.clone(), false);
                    }
                    return Ok(false);
                }
            }
        }

        // Build prerequisites
        // GNU Make order: "standalone explicit" prereqs (explicitly mentioned but NOT
        // sharing a pattern rule with any intermediate prereq) are built FIRST.
        // Intermediate prereqs (and explicit ones that share a rule with intermediates,
        // so they will be built as "also-make" siblings of the intermediate) come after.
        //
        // This must be computed before the mutable borrow below (inherited_vars_stack.push).
        let prereqs_ordered: Vec<String> = {
            // Three groups (GNU Make order):
            //  1. Standalone-explicit: explicitly mentioned, NOT sharing a rule with any intermediate.
            //     These are built FIRST (they must exist before intermediates can be checked).
            //  2. Non-explicit (intermediate): not explicitly mentioned.
            //     Built second, in original list order.
            //  3. Shared-explicit: explicitly mentioned, but SHARING a rule with an intermediate.
            //     Built last; typically they will already be built as also-make siblings of the
            //     intermediate and can be skipped.
            let standalone: Vec<String> = prereqs.iter()
                .filter(|p| {
                    self.is_explicitly_mentioned(p)
                        && !self.prereq_shares_rule_with_intermediate(p, &prereqs)
                })
                .cloned()
                .collect();
            let intermediates: Vec<String> = prereqs.iter()
                .filter(|p| !self.is_explicitly_mentioned(p))
                .cloned()
                .collect();
            let shared_explicit: Vec<String> = prereqs.iter()
                .filter(|p| {
                    self.is_explicitly_mentioned(p)
                        && self.prereq_shares_rule_with_intermediate(p, &prereqs)
                })
                .cloned()
                .collect();
            let mut ordered = standalone;
            ordered.extend(intermediates);
            ordered.extend(shared_explicit);
            ordered
        };

        // Push target vars onto stack for inheritance by prerequisites.
        let my_target_vars = self.collect_target_vars(target);
        self.inherited_vars_stack.push(my_target_vars.1);

        let mut any_rebuilt = false;

        // Resolve .EXTRA_PREREQS.
        // Global extra prereqs are built BEFORE regular prereqs.
        // Target-specific extra prereqs are built AFTER regular prereqs.
        // Skip when this target is itself an extra prereq (prevents circular application).
        let (extra_prereqs, extra_prereqs_target_specific) = if self.skip_extra_prereqs {
            (Vec::new(), false)
        } else {
            self.get_extra_prereqs(target)
        };

        // Step 0: build GLOBAL .EXTRA_PREREQS (before regular prereqs).
        if !extra_prereqs_target_specific {
            let prev_skip = self.skip_extra_prereqs;
            self.skip_extra_prereqs = true;
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
                            self.skip_extra_prereqs = prev_skip;
                            self.inherited_vars_stack.pop();
                            return Err(propagated);
                        }
                    }
                }
            }
            self.skip_extra_prereqs = prev_skip;
        }

        for prereq in prereqs_ordered {
            match self.build_target(&prereq) {
                Ok(rebuilt) => {
                    if rebuilt { any_rebuilt = true; }
                    // For terminal pattern rules (%::), prerequisites built via implicit
                    // (pattern) rules must ACTUALLY EXIST on disk — chaining through
                    // implicit rules that produce no file is not allowed.
                    // However, prerequisites with EXPLICIT rules are fine even if they
                    // don't create files (an explicit rule with empty recipe is valid).
                    if rule.is_terminal && !self.db.is_phony(&prereq)
                        && !self.db.rules.contains_key(prereq.as_str())
                    {
                        let exists = std::path::Path::new(&prereq).exists()
                            || self.find_in_vpath(&prereq).is_some();
                        if !exists {
                            let err = format!("No rule to make target '{}', needed by '{}'.  Stop.", prereq, target);
                            self.inherited_vars_stack.pop();
                            return Err(err);
                        }
                    }
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

        // sv 62706 ordering: if this target's SE was already fired during the implicit-rule
        // search phase (via trigger_prereq_se_side_effects called from a parent target's
        // pre-build step), skip re-firing it.  Remove from se_search_fired so that a
        // genuine re-build in a later invocation still fires SE.
        let se_was_prefired = self.se_search_fired.remove(target);

        // Perform second expansion NOW — after building non-SE prereqs — so that
        // $(info ...) and other side effects in SE text fire in the correct order:
        // each target's non-SE prerequisites (and their own SE expansions) are fully
        // processed before the current target's SE side effects appear.
        // This matches GNU Make behavior (sv 62706 ordering test).
        if has_se && !se_was_prefired {
            let oo_refs: Vec<&str> = order_only.iter().map(|s| s.as_str()).collect();
            let mut base_auto_vars = self.make_auto_vars(target, &prereqs, &oo_refs, stem);
            base_auto_vars.insert("?".to_string(), String::new());

            // For pattern rule SE expansion, GNU Make does not include a file:line prefix
            // in error messages (unlike explicit rule SE).  Temporarily clear the file context.
            let saved_file = self.state.current_file.borrow().clone();
            let saved_line = *self.state.current_line.borrow();
            *self.state.current_file.borrow_mut() = String::new();
            *self.state.current_line.borrow_mut() = 0;

            let mut se_added_prereqs: Vec<String> = Vec::new();
            let mut se_added_oo: Vec<String> = Vec::new();

            if let Some(ref text) = rule.second_expansion_prereqs.clone() {
                let stem_subst = subst_stem_in_se_text_dir(text, stem, matched_pattern_target);
                // sv 62706 ordering: before running the SE expansion for the current target
                // (which fires $(info ...) and other side effects), fire the SE side effects
                // for any "non-deferred" words — top-level tokens in the substituted text
                // that contain no '$'.  These are prerequisites that were NOT double-dollar-
                // escaped in the source (e.g. `%.o` in `%.tsk: %.o $$(info ...)`).
                //
                // GNU Make fires SE for these prerequisite targets during the IMPLICIT RULE
                // SEARCH phase (before building), not during the build phase.  We replicate
                // that by calling trigger_prereq_se_side_effects(), which expands the SE text
                // for side effects only (without running the recipe) and marks the target in
                // se_search_fired so that build_with_pattern_rule skips re-firing the SE when
                // it actually builds the target.
                //
                // This gives the correct ordering: prereq SE fires, then current-target SE
                // fires, then prereq recipe runs, then current-target recipe runs.
                let pre_prereqs: Vec<String> = se_extract_non_dollar_words(&stem_subst);
                for pre_prereq in pre_prereqs {
                    self.trigger_prereq_se_side_effects(&pre_prereq);
                }
                let (normal, oo) = self.second_expand_prereqs(&stem_subst, &base_auto_vars, target);
                // Mark SE prereqs as explicitly mentioned on a per-word basis.
                // Words without '%' produce explicitly-mentioned (non-intermediate) files.
                // Words with '%' produce stem-derived (intermediate) files.
                //
                // Three cases:
                // 1. Text has only '%'-words (all stem-derived): nothing explicitly mentioned.
                // 2. Text has BOTH '%'-words and non-'%'-words (mixed): re-expand only the
                //    non-pattern words to find which prereqs are explicitly mentioned.
                //    (Non-pattern words are simple variable refs, safe to expand again.)
                // 3. Text has only non-'%'-words: all prereqs are explicitly mentioned;
                //    mark them directly from `normal` (no re-expansion to avoid $(info ...)
                //    running twice when the text has side-effectful functions).
                if se_text_has_percent(text) && se_text_has_non_pattern_word(text) {
                    // Case 3 (mixed): expand only '%'-words with stem sub to find stem-derived set.
                    // Anything in `normal` NOT in that set is explicitly mentioned.
                    // This avoids re-running side-effect functions like $(info ...) in non-'%' words.
                    let pat_words = se_pattern_words(text);
                    let stem_derived: std::collections::HashSet<String> = if pat_words.is_empty() {
                        std::collections::HashSet::new()
                    } else {
                        let pat_text = pat_words.join(" ");
                        let pat_subst = subst_stem_in_se_text_dir(&pat_text, stem, matched_pattern_target);
                        let (pd_normal, _) = self.second_expand_prereqs(&pat_subst, &base_auto_vars, target);
                        pd_normal.into_iter().collect()
                    };
                    for p in &normal {
                        if !stem_derived.contains(p) {
                            self.se_explicitly_mentioned.insert(p.clone());
                        }
                    }
                } else if !se_text_has_percent(text) {
                    // Case 2: no '%' at all — every prereq is explicitly mentioned.
                    for p in &normal {
                        self.se_explicitly_mentioned.insert(p.clone());
                    }
                }
                // Case 1: all '%'-words — nothing is explicitly mentioned.
                se_added_prereqs.extend(normal);
                se_added_oo.extend(oo);
            }
            if let Some(ref text) = rule.second_expansion_order_only.clone() {
                let stem_subst = subst_stem_in_se_text_dir(text, stem, matched_pattern_target);
                let (normal, oo) = self.second_expand_prereqs(&stem_subst, &base_auto_vars, target);
                if se_text_has_percent(text) && se_text_has_non_pattern_word(text) {
                    let pat_words = se_pattern_words(text);
                    let stem_derived: std::collections::HashSet<String> = if pat_words.is_empty() {
                        std::collections::HashSet::new()
                    } else {
                        let pat_text = pat_words.join(" ");
                        let pat_subst = subst_stem_in_se_text_dir(&pat_text, stem, matched_pattern_target);
                        let (pd_normal, _) = self.second_expand_prereqs(&pat_subst, &base_auto_vars, target);
                        pd_normal.into_iter().collect()
                    };
                    for p in &normal {
                        if !stem_derived.contains(p) {
                            self.se_explicitly_mentioned.insert(p.clone());
                        }
                    }
                } else if !se_text_has_percent(text) {
                    for p in &normal {
                        self.se_explicitly_mentioned.insert(p.clone());
                    }
                }
                se_added_oo.extend(normal);
                se_added_oo.extend(oo);
            }

            // Restore file context after pattern rule SE expansion.
            *self.state.current_file.borrow_mut() = saved_file.clone();
            *self.state.current_line.borrow_mut() = saved_line;

            // Glob-expand wildcard patterns introduced by SE expansion.
            se_added_prereqs = Self::glob_expand_prereqs(se_added_prereqs);
            se_added_oo = Self::glob_expand_prereqs(se_added_oo);
            se_added_prereqs.retain(|p| p != ".WAIT");
            se_added_oo.retain(|p| p != ".WAIT");

            // sv 62706: Validate SE-generated prerequisites BEFORE building any of them.
            // GNU Make performs SE during its implicit rule SEARCH phase; if any SE-generated
            // prereq cannot be satisfied, the pattern rule is REJECTED at that point (before
            // any recipes have run).  jmake performs SE during the BUILD phase, so we must
            // simulate this by pre-checking all SE prereqs first.
            //
            // If every SE prereq is satisfiable (exists, has explicit rule, or has a
            // pattern rule), proceed to build.  If any SE prereq is NOT satisfiable, return
            // "No rule to make target '<parent>'" so the caller sees the parent as having no
            // applicable rule — matching GNU Make's implicit rule rejection behaviour.
            //
            // Only apply this to pattern rules (not compat rules) to avoid changing existing
            // explicit-rule SE behaviour.
            if !rule.is_compat {
                for se_prereq in &se_added_prereqs {
                    let satisfiable = Path::new(se_prereq).exists()
                        || self.db.rules.contains_key(se_prereq.as_str())
                        || self.db.is_phony(se_prereq)
                        || self.find_in_vpath(se_prereq).is_some()
                        || self.find_pattern_rule_exists(se_prereq)
                        || self.built.contains_key(se_prereq.as_str());
                    if !satisfiable {
                        // SE prereq can't be satisfied: reject the pattern rule.
                        // Return "No rule to make target '<parent>'" (no "needed by" suffix)
                        // so that the caller can propagate it correctly.
                        *self.state.current_file.borrow_mut() = saved_file.clone();
                        *self.state.current_line.borrow_mut() = saved_line;
                        self.inherited_vars_stack.pop();
                        return Err(format!("No rule to make target '{}'.  Stop.", target));
                    }
                }
            }

            // Build SE-expanded prerequisites.
            for se_prereq in &se_added_prereqs {
                match self.build_target(se_prereq) {
                    Ok(rebuilt) => {
                        if rebuilt { any_rebuilt = true; }
                    }
                    Err(e) => {
                        // SE prereq errors are propagated without the no-rule swallowing
                        // (SE-generated prereqs that don't exist ARE real errors).
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
            for se_oo in &se_added_oo {
                let _ = self.build_target(se_oo);
            }

            // Extend prereqs and order_only with SE results for auto vars and rebuild check.
            prereqs.extend(se_added_prereqs);
            order_only.extend(se_added_oo);
            prereqs = Self::glob_expand_prereqs(prereqs);
            order_only = Self::glob_expand_prereqs(order_only);

            // Update original-order lists to include SE prereqs for correct auto vars.
            prereqs_original_order = prereqs.clone();
            order_only_original_order = order_only.clone();
        }

        for prereq in order_only.clone() {
            let _ = self.build_target(&prereq);
        }

        // For also-make siblings (multi-target pattern rules, e.g. `%.t1 %.t2: ...`):
        // if a sibling target has explicit prerequisites from a separate rule in the
        // database, those prerequisites must be built before the shared recipe runs.
        // Example: `x.t2: dep` combined with `%.t1 %.t2:` — when building x.t1 via
        // the pattern rule, `dep` must be built as a prereq of the also-make sibling x.t2.
        for sib in &also_make_siblings {
            if let Some(sib_rules) = self.db.rules.get(sib).cloned() {
                for sib_rule in &sib_rules {
                    for sib_prereq in &sib_rule.prerequisites {
                        if sib_prereq == ".WAIT" { continue; }
                        match self.build_target(sib_prereq) {
                            Ok(rebuilt) => { if rebuilt { any_rebuilt = true; } }
                            Err(e) => {
                                if !e.starts_with("No rule to make target '") || e.contains(", needed by '") {
                                    self.inherited_vars_stack.pop();
                                    return Err(e);
                                }
                            }
                        }
                    }
                }
            }
        }

        // Step (after order-only): build TARGET-SPECIFIC .EXTRA_PREREQS (after regular prereqs).
        if extra_prereqs_target_specific {
            let prev_skip = self.skip_extra_prereqs;
            self.skip_extra_prereqs = true;
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
                            self.skip_extra_prereqs = prev_skip;
                            self.inherited_vars_stack.pop();
                            return Err(propagated);
                        }
                    }
                }
            }
            self.skip_extra_prereqs = prev_skip;
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

        // Use original (pre-shuffle) order for auto vars so $^, $<, etc. reflect
        // declaration order regardless of shuffle mode (GNU Make behavior).
        let oo_refs: Vec<&str> = order_only_original_order.iter().map(|s| s.as_str()).collect();
        let mut auto_vars = self.make_auto_vars(target, &prereqs_original_order, &oo_refs, stem);

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
            // If this target is in .NOTPARALLEL: target_list, force sequential prereqs.
            self.pending_plan_wait_groups = if self.db.not_parallel_targets.contains(target)
                && prereqs.len() > 1
            {
                // Each prereq in its own group to force sequential execution.
                prereqs.iter().map(|p| vec![p.clone()]).collect()
            } else if prereqs_with_wait.iter().any(|p| p == ".WAIT") {
                // Pattern rule has .WAIT markers in prerequisites: extract wait groups.
                extract_wait_groups(&prereqs_with_wait)
            } else {
                Vec::new()
            };
        }
        // Capture primary target mtime BEFORE running recipe, so we can detect if it changed.
        // The peer-target warning fires only when the primary was actually created/updated.
        let primary_mtime_before = self.file_mtime(target);
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

            // Compute the set of LITERAL (non-%) prereqs from the original pattern rule.
            // Only these make the invocation "explicit" for sv 60188 purposes.
            // Pattern prereqs like %.in do NOT count even if the expanded form (e.g. foo.in)
            // happens to be explicitly mentioned elsewhere.
            // Note: literal prereqs in a pattern rule have no % so they are already concrete.
            let literal_rule_prereqs: std::collections::HashSet<&str> = rule.prerequisites.iter()
                .filter(|p| !p.contains('%'))
                .map(|p| p.as_str())
                .collect();

            // Determine if the primary target is intermediate (without pushing to
            // intermediate_built yet — siblings must be pushed FIRST so that the
            // deletion order matches GNU Make: siblings are removed before primary).
            let is_target_intermediate;
            let primary_is_intermediate_explicit = self.db.is_intermediate(target);
            let primary_is_intermediate_implicit = if !primary_is_intermediate_explicit
                && !self.db.is_precious(target)
                && !self.db.is_notintermediate(target)
                && !self.db.is_secondary(target)
            {
                let is_explicit = self.top_level_targets.contains(target)
                    || self.is_explicitly_mentioned(target);
                !is_explicit
            } else {
                false
            };
            is_target_intermediate = primary_is_intermediate_explicit || primary_is_intermediate_implicit;

            // Mark all siblings as built/covered (order doesn't matter for these).
            for sib in &also_make_siblings {
                self.grouped_covered.insert(sib.clone());
                self.built.insert(sib.clone(), true);
            }
            // Push PRIMARY to intermediate_built BEFORE siblings.
            // Deletion iterates intermediate_built in REVERSE (.rev()), so to get
            // GNU Make's deletion order (siblings before primary), we push primary
            // first and siblings (forward declaration order) after.
            // Example: `%.1 %.15:`, primary a.1, siblings [a.15].
            //   Push a.1 → intermediate_built = [..., a.1]
            //   Push a.15 → intermediate_built = [..., a.1, a.15]
            //   Deletion .rev() → [a.15, a.1] → `rm a.15 a.1` ✓
            if is_target_intermediate {
                if !self.intermediate_built.contains(&target.to_string()) {
                    self.intermediate_built.push(target.to_string());
                }
            }
            // Now push siblings to intermediate_built in FORWARD declaration order.
            for sib in also_make_siblings.iter() {
                // Track intermediate status for also_make siblings.
                // A sibling is intermediate if:
                //   1. It is explicitly marked .INTERMEDIATE, OR
                //   2. The sibling is not explicitly mentioned in the makefile AND
                //      is not a top-level target AND is not precious/NOTINTERMEDIATE/SECONDARY
                // Note: explicitly mentioned = appears as target or prereq of any explicit rule.
                if self.db.is_intermediate(sib) {
                    if !self.intermediate_built.contains(sib) {
                        self.intermediate_built.push(sib.clone());
                    }
                } else if !self.db.is_precious(sib)
                    && !self.db.is_notintermediate(sib)
                    && !self.db.is_secondary(sib)
                {
                    // A sibling is intermediate independently of the primary target.
                    // It escapes intermediate status if it is explicitly mentioned
                    // in the makefile (as target or prereq of any rule) OR is a
                    // top-level target, OR if any LITERAL (non-%) prereq of the rule
                    // is explicitly mentioned (sv 60188: literal non-% prereqs of a
                    // pattern rule make the entire rule invocation "explicit").
                    let any_literal_prereq_explicit = literal_rule_prereqs.iter()
                        .any(|p| self.is_explicitly_mentioned(*p) && !self.db.is_intermediate(*p));
                    // Also: if any EXPANDED (stem-substituted) prereq is explicitly mentioned
                    // AND is NOT an explicit rule TARGET (only in explicitly_mentioned, not in
                    // db.rules), the invocation is also "explicit". This handles:
                    //   `%.z %.q: %.x; ...` with `unrelated: hello.x`
                    // → hello.x is explicitly mentioned (not a rule target) → hello.q not intermediate.
                    // Note: we exclude targets in db.rules to avoid falsely protecting intermediates
                    // when the prereq has its own explicit recipe (e.g. `foo.in: ; touch $@`).
                    let any_expanded_prereq_explicit_no_rule = prereqs.iter()
                        .any(|p| self.db.explicitly_mentioned.contains(p.as_str())
                            && !self.db.rules.contains_key(p.as_str())
                            && !self.db.is_intermediate(p));
                    let is_explicit = self.top_level_targets.contains(sib.as_str())
                        || self.is_explicitly_mentioned(sib)
                        || any_literal_prereq_explicit
                        || any_expanded_prereq_explicit_no_rule;
                    if !is_explicit {
                        if !self.intermediate_built.contains(sib) {
                            self.intermediate_built.push(sib.clone());
                        }
                    }
                }
            }
            // In collect_plans_mode, update the plan's is_intermediate flag now that we
            // know the runtime intermediate status (db.is_intermediate only covers explicit
            // .INTERMEDIATE declarations, not pattern-rule-built intermediates).
            if self.collect_plans_mode {
                if is_target_intermediate {
                    if let Some(ref mut plans) = self.pending_plans {
                        if let Some(plan) = plans.get_mut(target) {
                            plan.is_intermediate = true;
                        }
                    }
                }
                // For parallel mode: add intermediate siblings to the PRIMARY target's plan
                // via intermediate_also_make. When the primary completes with rebuilt=true,
                // the scheduler will add all entries from intermediate_also_make to
                // intermediate_built. This is necessary because siblings don't have their own
                // jobs and handle_completion is never called for them directly.
                let intermediate_sibs: Vec<String> = also_make_siblings.iter()
                    .filter(|sib| self.intermediate_built.contains(*sib))
                    .cloned()
                    .collect();
                if !intermediate_sibs.is_empty() {
                    if let Some(ref mut plans) = self.pending_plans {
                        if let Some(plan) = plans.get_mut(target) {
                            for sib in &intermediate_sibs {
                                if !plan.intermediate_also_make.contains(sib) {
                                    plan.intermediate_also_make.push(sib.clone());
                                }
                            }
                        }
                    }
                }
                // Also mark sibling plans as intermediate if they were pushed to intermediate_built.
                // If the sibling doesn't have a plan yet (it was only covered as a sibling,
                // never explicitly planned), create a minimal plan so the sequential
                // intermediate_built tracking works correctly.
                for sib in &also_make_siblings {
                    if self.intermediate_built.contains(sib) {
                        if let Some(ref mut plans) = self.pending_plans {
                            let plan = plans.entry(sib.clone()).or_insert_with(|| {
                                parallel::TargetPlan {
                                    target: sib.clone(),
                                    prerequisites: Vec::new(),
                                    order_only: Vec::new(),
                                    recipe: Vec::new(),
                                    source_file: String::new(),
                                    auto_vars: std::collections::HashMap::new(),
                                    is_phony: false,
                                    needs_rebuild: false,
                                    grouped_primary: None,
                                    grouped_siblings: Vec::new(),
                                    extra_exports: std::collections::HashMap::new(),
                                    extra_unexports: Vec::new(),
                                    is_intermediate: false,
                                    wait_groups: Vec::new(),
                                    intermediate_also_make: Vec::new(),
                                }
                            });
                            plan.is_intermediate = true;
                        }
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
        // Per GNU Make: warn only when the primary target was actually CREATED or UPDATED
        // (its mtime changed, or it didn't exist before and now exists).
        // Do NOT warn if the recipe ran but left the primary target unchanged (e.g., @echo).
        if let Ok(true) = &result {
            let primary_mtime_after = self.file_mtime(target);
            // Primary was "updated" if:
            //   - it didn't exist before (None → Some), OR
            //   - its mtime changed (before != after)
            let primary_updated = match (primary_mtime_before, primary_mtime_after) {
                (None, Some(_)) => true,                       // newly created
                (Some(before), Some(after)) => after != before, // mtime changed
                _ => false,
            };
            if primary_updated {
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
                            let resolved = subst_stem_in_prereq_dir(p, &stem, pattern_target);
                            // If the resolved prereq contains glob chars, check if any matching files exist.
                            let ok = if resolved.contains('*') || resolved.contains('?') || resolved.contains('[') {
                                ::glob::glob(&resolved)
                                    .ok()
                                    .map_or(false, |mut it| it.next().is_some())
                            } else {
                                Path::new(&resolved).exists()
                                || self.db.is_phony(&resolved)
                                || self.db.rules.contains_key(&resolved)
                                || explicit_prereqs.iter().any(|ep| ep == &resolved)
                                || self.find_in_vpath(&resolved).is_some()
                                // A prerequisite currently being built (cycle) counts as
                                // available in pass 1; the cycle will be detected and dropped
                                // gracefully when we actually try to build it.
                                || self.building.contains(&resolved)
                            };
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
                    } else if found_compat && compat_rule.is_none() && !rule.is_terminal {
                        // Use as compat (last-resort) candidate. Terminal rules are NOT
                        // used as compat candidates — they require prereqs to already exist.
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
                            let resolved = subst_stem_in_prereq_dir(p, &stem, pattern_target);
                            // If the resolved prereq contains glob chars, check if any matching files exist.
                            let ok = if resolved.contains('*') || resolved.contains('?') || resolved.contains('[') {
                                ::glob::glob(&resolved)
                                    .ok()
                                    .map_or(false, |mut it| it.next().is_some())
                            } else {
                                Path::new(&resolved).exists()
                                || self.db.rules.contains_key(&resolved)
                                || self.db.is_phony(&resolved)
                                || explicit_prereqs.iter().any(|ep| ep == &resolved)
                                || self.find_pattern_rule_exists(&resolved)
                                || self.find_in_vpath(&resolved).is_some()
                                || self.building.contains(&resolved)
                            };
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
        self.find_pattern_rule_exists_inner(target, &mut std::collections::HashSet::new(), 0)
    }

    fn find_pattern_rule_exists_inner(&self, target: &str, visited: &mut HashSet<String>, depth: usize) -> bool {
        // If this target is currently being built, treat it as available (cycle case).
        if self.building.contains(target) {
            return true;
        }
        // Prevent infinite recursion from circular dependencies.
        if visited.contains(target) {
            return false;
        }
        // Limit recursion depth to prevent infinite expansion when pattern rules
        // create unboundedly long target names (e.g. p%: p%1 → pre1 → pre11 → pre111 ...).
        if depth > 32 {
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
                            let resolved = subst_stem_in_prereq_dir(p, &stem, pattern_target);
                            Path::new(&resolved).exists()
                                || self.db.rules.contains_key(&resolved)
                                || self.db.is_phony(&resolved)
                                || self.find_in_vpath(&resolved).is_some()
                                || self.building.contains(&resolved)
                                // For terminal rules (%::), prerequisites cannot be
                                // further built via chaining - only on-disk/explicit.
                                // Allowing recursion here causes infinite expansion when
                                // a terminal rule's prereq pattern resolves to a longer name
                                // that re-matches the same terminal rule (e.g. %:: %.c → hello.c → hello.c.c → ...).
                                || (!rule.is_terminal && self.find_pattern_rule_exists_inner(&resolved, visited, depth + 1))
                        })
                    };
                    if prereqs_ok { return true; }
                }
            }
        }
        false
    }

    /// Check if `explicit_prereq` shares a pattern rule (same rule instance, same stem)
    /// with any non-explicitly-mentioned prereq in `all_prereqs`.
    ///
    /// Used to decide prereq build order in `build_with_pattern_rule`: if an explicitly-
    /// mentioned prereq shares a pattern rule with an intermediate (non-explicit) prereq,
    /// then the intermediate should be built first (as the primary trigger of the shared
    /// rule), and the explicit one will be built as an "also-make" sibling.
    ///
    /// If the explicit prereq does NOT share a rule with any intermediate, it is a
    /// "standalone explicit" prereq and should be built before the intermediates.
    fn prereq_shares_rule_with_intermediate(&self, explicit_prereq: &str, all_prereqs: &[String]) -> bool {
        if let Some((rule, stem)) = self.find_pattern_rule(explicit_prereq) {
            for other in all_prereqs {
                if other == explicit_prereq { continue; }
                // Only consider non-explicitly-mentioned prereqs (intermediates)
                if self.is_explicitly_mentioned(other) { continue; }
                // Check if `other` matches any target pattern in the SAME rule with the SAME stem
                for pat in &rule.targets {
                    if match_pattern(pat, other).as_deref() == Some(stem.as_str()) {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn find_in_vpath(&self, target: &str) -> Option<String> {
        // Search VPATH and vpath for the target.
        // A vpath-resolved path is valid if either:
        //   (a) the file exists on disk, OR
        //   (b) there is an explicit rule for the resolved path in the database.
        // This allows patterns like `vpath %.te vpath-d/` to resolve `fail.te`
        // to `vpath-d/fail.te` even when `vpath-d/fail.te` doesn't exist but has a rule.
        let has_rule = |path: &str| self.db.rules.contains_key(path);

        for (pattern, dirs) in &self.db.vpath {
            if vpath_pattern_matches(pattern, target) {
                for dir in dirs {
                    let candidate = dir.join(target);
                    let s = candidate.to_string_lossy().to_string();
                    if candidate.exists() || has_rule(&s) {
                        return Some(s);
                    }
                }
            }
        }
        for dir in &self.db.vpath_general {
            let candidate = dir.join(target);
            let s = candidate.to_string_lossy().to_string();
            if candidate.exists() || has_rule(&s) {
                return Some(s);
            }
        }

        // Also check VPATH variable
        if let Some(var) = self.db.variables.get("VPATH") {
            for dir in var.value.split(':') {
                let dir = dir.trim();
                if !dir.is_empty() {
                    let candidate = Path::new(dir).join(target);
                    let s = candidate.to_string_lossy().to_string();
                    if candidate.exists() || has_rule(&s) {
                        return Some(s);
                    }
                }
            }
        }

        None
    }

    /// Search for a library named `name` (from a `-lname` prerequisite).
    /// Uses `.LIBPATTERNS` to determine what filenames to look for, then searches
    /// in VPATH directories and the current directory.
    /// Returns the path to the found library if found.
    fn find_library(&self, name: &str) -> Option<String> {
        // Collect all VPATH directories to search.
        let mut search_dirs: Vec<std::path::PathBuf> = Vec::new();

        // From named vpath patterns
        for (pattern, dirs) in &self.db.vpath {
            for candidate_name in &[format!("lib{}.a", name), format!("lib{}.so", name)] {
                if vpath_pattern_matches(pattern, candidate_name)
                    || pattern == "%" || pattern == "*"
                {
                    for dir in dirs {
                        if !search_dirs.contains(dir) {
                            search_dirs.push(dir.clone());
                        }
                    }
                    break;
                }
            }
        }

        // From vpath_general (directories from bare `vpath % dir` or `vpath * dir`)
        for dir in &self.db.vpath_general {
            if !search_dirs.contains(dir) {
                search_dirs.push(dir.clone());
            }
        }

        // From VPATH variable
        if let Some(var) = self.db.variables.get("VPATH") {
            let vpath_expanded = self.state.expand(&var.value);
            for dir in vpath_expanded.split(':') {
                let dir = dir.trim();
                if !dir.is_empty() {
                    let pb = std::path::PathBuf::from(dir);
                    if !search_dirs.contains(&pb) {
                        search_dirs.push(pb);
                    }
                }
            }
        }

        // Get .LIBPATTERNS; fall back to default if not set.
        let lib_patterns_val = if let Some(var) = self.db.variables.get(".LIBPATTERNS") {
            self.state.expand(&var.value)
        } else {
            "lib%.so lib%.a".to_string()
        };

        // For each pattern in .LIBPATTERNS (in order), apply it to get the
        // candidate filename, then search in current dir and VPATH dirs.
        // GNU Make returns the first found match (earliest in .LIBPATTERNS order).
        for pattern in lib_patterns_val.split_ascii_whitespace() {
            // Pattern must contain '%'; if not, emit warning and skip.
            if !pattern.contains('%') {
                eprintln!("{}: .LIBPATTERNS element '{}' is not a pattern",
                    self.progname, pattern);
                continue;
            }
            // Replace '%' with the library name.
            let candidate = pattern.replace('%', name);

            // Search current directory first.
            if Path::new(&candidate).exists() {
                return Some(candidate);
            }
            // Then VPATH directories.
            for dir in &search_dirs {
                let full = dir.join(&candidate);
                if full.exists() {
                    return Some(full.to_string_lossy().to_string());
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
    fn collect_target_vars(&self, target: &str) -> (HashMap<String, (String, bool, bool)>, HashMap<String, (String, bool, bool, Option<bool>)>) {
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
            for (name, (val, is_override, is_private_flag, _export_status)) in inherited {
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
        // Include $@ = target so recursive variables using $@ (like `export HI = $(shell $($@.CMD))`)
        // are expanded with the correct target name.
        expansion_context.insert("@".to_string(), target.to_string());
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
        // Collect the effective export status for each variable for propagation to prerequisites.
        // export_status: Some(true)=exported, Some(false)=unexported, None=use global/inherited.
        // Priority: explicit TSV export flag > inherited export status from parent.
        let mut var_export_status: HashMap<String, Option<bool>> = HashMap::new();

        // Start with inherited export statuses from the parent.
        if let Some(inherited) = self.inherited_vars_stack.last() {
            for (name, (_, _, _, export_status)) in inherited {
                var_export_status.insert(name.clone(), *export_status);
            }
        }

        // Pattern-specific vars can set export status.
        for (_, _, psv) in &pattern_vars_with_stem {
            if psv.var.export.is_some() {
                var_export_status.insert(psv.var_name.clone(), psv.var.export);
            }
        }

        // Target-specific vars override the export status (if explicitly set).
        if let Some(rules) = self.db.rules.get(target) {
            for rule in rules {
                for (var_name, var) in &rule.target_specific_vars {
                    if var.export.is_some() {
                        var_export_status.insert(var_name.clone(), var.export);
                    }
                }
            }
        }

        let mut for_prereqs: HashMap<String, (String, bool, bool, Option<bool>)> = HashMap::new();
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
                        let export_st = var_export_status.get(name.as_str()).copied().flatten();
                        for_prereqs.insert(name.clone(), (pre_val.clone(), *pre_is_override, false, export_st));
                    }
                    // If no pre-step-2 entry: don't include in for_prereqs (new private var).
                }
            } else {
                // .EXTRA_PREREQS is a special variable that is NEVER inherited by
                // prerequisites.  GNU Make applies it only to the target that owns it,
                // not to that target's prerequisites.
                if name == ".EXTRA_PREREQS" {
                    // Do not propagate .EXTRA_PREREQS to prerequisites.
                } else {
                    let export_st = var_export_status.get(name.as_str()).copied().flatten();
                    for_prereqs.insert(name.clone(), (val.clone(), *is_override, false, export_st));
                }
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

    /// Check if an intermediate file has a buildable (non-empty) recipe.
    /// Used to distinguish between:
    ///   - `%.x: ;`  (empty recipe) — don't rebuild, sv 60188 behavior
    ///   - `foo.a: ; touch $@` (non-empty recipe) — rebuild needed
    fn intermediate_has_buildable_recipe(&self, target: &str) -> bool {
        // Check explicit rules first
        if let Some(rules) = self.db.rules.get(target) {
            if rules.iter().any(|r| !r.recipe.is_empty()) {
                return true;
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

    /// Compute the "effective mtime" of a target for the purpose of checking if a parent needs rebuild.
    /// For a target that doesn't exist but is intermediate/deletable, this computes the maximum
    /// mtime of the target's sources, representing "when would this have been built".
    /// Returns None if the target doesn't exist and has no known sources.
    fn effective_mtime(&self, target: &str, depth: usize) -> Option<SystemTime> {
        // Prevent infinite recursion
        if depth > 10 { return None; }
        // If target exists, use its actual mtime — but check if what-if or rule prereqs
        // would cause a rebuild; if so, treat this target as infinitely new.
        if let Some(t) = self.file_mtime(target).or_else(|| self.find_in_vpath(target).and_then(|f| self.file_mtime(&f))) {
            // Check explicit rules for this target
            if let Some(rules) = self.db.rules.get(target) {
                for rule in rules {
                    for prereq in &rule.prerequisites {
                        // If a prereq is what-if (or its VPATH-resolved path is), target would rebuild
                        if self.what_if.iter().any(|w| normalize_path(w) == normalize_path(prereq)) {
                            return Some(SystemTime::now());
                        }
                        if let Some(ref vp) = self.find_in_vpath(prereq) {
                            if self.what_if.iter().any(|w| normalize_path(w) == normalize_path(vp)) {
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
            } else {
                // No explicit rules: also check pattern rules so that a file found via
                // VPATH is considered out of date when its pattern-rule prerequisites are
                // newer.  Example: foo.b exists in VPATH but foo.c (its %.b:%.c source)
                // is newer — without this check effective_mtime would return the stale
                // mtime of foo.b and the parent would never be rebuilt.
                if let Some((rule, stem)) = self.find_pattern_rule(target) {
                    let matched_pt_em: String = rule.targets.iter()
                        .find(|pt| match_pattern(pt, target).as_deref() == Some(stem.as_str()))
                        .cloned()
                        .unwrap_or_else(|| "%".to_string());
                    for prereq_pat in &rule.prerequisites {
                        if prereq_pat.as_str() == ".WAIT" { continue; }
                        let prereq = subst_stem_in_prereq_dir(prereq_pat, &stem, &matched_pt_em);
                        if self.what_if.iter().any(|w| normalize_path(w) == normalize_path(&prereq)) {
                            return Some(SystemTime::now());
                        }
                        if let Some(ref vp) = self.find_in_vpath(&prereq) {
                            if self.what_if.iter().any(|w| normalize_path(w) == normalize_path(vp)) {
                                return Some(SystemTime::now());
                            }
                        }
                        if let Some(pt) = self.effective_mtime(&prereq, depth + 1) {
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
            let matched_pt_em: String = rule.targets.iter()
                .find(|pt| match_pattern(pt, target).as_deref() == Some(stem.as_str()))
                .cloned()
                .unwrap_or_else(|| "%".to_string());
            let max_prereq_time = rule.prerequisites.iter()
                .filter(|p| p.as_str() != ".WAIT")
                .map(|p| subst_stem_in_prereq_dir(p, &stem, &matched_pt_em))
                .filter_map(|p| self.effective_mtime(&p, depth + 1))
                .max();
            return max_prereq_time;
        }
        None
    }

    /// Get the mtime for a file, respecting the -L/--check-symlink-times flag.
    /// When check_symlink_times is true, returns the symlink's own mtime (lstat),
    /// which allows dangling symlinks to return Some() with their own mtime.
    /// When false (default), follows symlinks (stat), returning None for dangling symlinks.
    fn file_mtime(&self, path: &str) -> Option<SystemTime> {
        if self.state.args.check_symlink_times {
            get_mtime_symlink(path)
        } else {
            get_mtime(path)
        }
    }

    fn needs_rebuild(&self, target: &str, prereqs: &[String], any_prereq_rebuilt: bool) -> bool {
        if any_prereq_rebuilt {
            return true;
        }

        // -W/--what-if: if any prereq is in the what_if list, treat it as infinitely new.
        // Also check the VPATH-resolved path (e.g., prereq "x" found as "x-dir/x" via VPATH,
        // and "-W x-dir/x" was given).
        for prereq in prereqs {
            if self.what_if.iter().any(|w| normalize_path(w) == normalize_path(prereq)) {
                return true;
            }
            if let Some(ref vp) = self.find_in_vpath(prereq) {
                if self.what_if.iter().any(|w| normalize_path(w) == normalize_path(vp)) {
                    return true;
                }
            }
        }

        let target_time = match self.file_mtime(target) {
            Some(t) => t,
            None => return true, // Target doesn't exist
        };

        for prereq in prereqs {
            // Skip .WAIT markers (should already be filtered, but be safe)
            if prereq == ".WAIT" { continue; }

            let prereq_time = match self.file_mtime(prereq) {
                Some(t) => t,
                None => {
                    // Check VPATH
                    if let Some(found) = self.find_in_vpath(prereq) {
                        match self.file_mtime(&found) {
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
                        // Exception: if the prereq is .INTERMEDIATE, treat as intermediate
                        // even if it was also explicitly mentioned (sv 60188).
                        if self.is_explicitly_mentioned(prereq) && !self.db.is_intermediate(prereq) {
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

    /// Expand glob wildcards in a list of prerequisite tokens.
    /// Each token that contains `*`, `?`, or `[` is glob-expanded against the filesystem.
    /// If no files match, the literal token is kept (GNU Make behaviour).
    /// Tokens without wildcards are passed through unchanged.
    fn glob_expand_prereqs(prereqs: Vec<String>) -> Vec<String> {
        let mut result = Vec::with_capacity(prereqs.len());
        for token in prereqs {
            if token.contains('*') || token.contains('?') || token.contains('[') {
                let mut matched: Vec<String> = Vec::new();
                if let Ok(paths) = ::glob::glob(&token) {
                    for entry in paths.flatten() {
                        matched.push(entry.to_string_lossy().to_string());
                    }
                }
                matched.sort();
                if matched.is_empty() {
                    // No matches: keep the literal token (GNU Make keeps unmatched globs).
                    result.push(token);
                } else {
                    result.extend(matched);
                }
            } else {
                result.push(token);
            }
        }
        result
    }

    /// Resolve `.EXTRA_PREREQS` for a given target.
    /// Target-specific `.EXTRA_PREREQS` takes priority over the global value.
    /// The value is variable-expanded and wildcard-expanded.
    /// Returns `(prereqs, is_target_specific)` where `is_target_specific` is true
    /// when the value comes from a target-specific variable (not the global default).
    /// GNU Make builds target-specific extra prereqs AFTER regular prereqs, but
    /// global extra prereqs BEFORE regular prereqs.
    fn get_extra_prereqs(&self, target: &str) -> (Vec<String>, bool) {
        // Use collect_target_vars to get the fully-expanded target-specific vars
        // (which also handles inheritance, pattern-specific vars, etc.).
        let collected = self.collect_target_vars(target);
        // The value from collect_target_vars is already fully expanded.
        let (expanded, is_target_specific) = if let Some((val, _, _)) = collected.0.get(".EXTRA_PREREQS") {
            (val.clone(), true)
        } else {
            // Fall back to global .EXTRA_PREREQS.
            match self.db.variables.get(".EXTRA_PREREQS") {
                Some(v) => {
                    let val = if v.flavor == VarFlavor::Simple {
                        v.value.clone()
                    } else {
                        self.state.expand(&v.value)
                    };
                    (val, false)
                }
                None => return (Vec::new(), false),
            }
        };

        if expanded.trim().is_empty() {
            return (Vec::new(), is_target_specific);
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
        (result, is_target_specific)
    }

    fn make_auto_vars(&self, target: &str, prereqs: &[String], order_only: &[&str], stem: &str) -> HashMap<String, String> {
        let mut vars = HashMap::new();

        // $@ - target
        vars.insert("@".to_string(), target.to_string());

        // Helper: resolve a prerequisite to its actual path, checking:
        //   1. lib_search_results for -lname prerequisites
        //   2. Direct existence
        //   3. Whether the prereq's recipe actually ran this run (in which case use its
        //      local name, not any VPATH copy — the recipe targeted the local file)
        //   4. VPATH lookup
        let resolve_prereq = |p: &str| -> String {
            if let Some(resolved) = self.lib_search_results.get(p) {
                return resolved.clone();
            }
            if Path::new(p).exists() {
                return p.to_string();
            }
            // If the prereq's recipe actually ran this run (built == true), use its local
            // name even if the file doesn't exist (recipe ran but didn't create the file).
            if self.built.get(p) == Some(&true) {
                return p.to_string();
            }
            if let Some(found) = self.find_in_vpath(p) {
                found
            } else {
                p.to_string()
            }
        };

        // $< - first prerequisite
        let first_prereq = prereqs.first().map(|s| s.as_str()).unwrap_or_default();
        vars.insert("<".to_string(), resolve_prereq(first_prereq));

        // $^ - all prerequisites (no duplicates)
        let mut seen = HashSet::new();
        let unique_prereqs: Vec<String> = prereqs.iter()
            .filter(|p| seen.insert(p.to_string()))
            .map(|p| resolve_prereq(p))
            .collect();
        vars.insert("^".to_string(), unique_prereqs.join(" "));

        // $+ - all prerequisites (with duplicates)
        let all_prereqs: Vec<String> = prereqs.iter()
            .map(|p| resolve_prereq(p))
            .collect();
        vars.insert("+".to_string(), all_prereqs.join(" "));

        // $? - prerequisites that are newer than the target.
        // With -B (always_make), all prerequisites are considered newer.
        // Otherwise includes prereqs that:
        //   - exist on disk AND are newer than the target
        //   - target doesn't exist but prereq exists on disk
        //   - prereq doesn't exist on disk but was visited/built this make run and target doesn't exist
        let target_time = self.file_mtime(target);
        let newer: Vec<String> = prereqs.iter()
            .filter(|p| {
                if self.always_make {
                    // -B: all prerequisites are considered newer than the target
                    return true;
                }
                let resolved = resolve_prereq(p);
                let prereq_mtime = self.file_mtime(&resolved).or_else(|| {
                    self.find_in_vpath(p).and_then(|found| self.file_mtime(&found))
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
            .map(|p| resolve_prereq(p))
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
            // Collapse all backslash-newline continuations in the expanded text so
            // that `\<newline> ` (from `\<newline> $(.XY)` where .XY is empty) is
            // treated as empty, not as a `\` character.
            let collapsed = collapse_backslash_newlines(&cmd);
            if !collapsed.trim().is_empty() {
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

            // Update the existing plan (created in build_with_rules) with the expanded recipe.
            // If no plan exists yet (e.g., called from .DEFAULT or pattern rules), create one.
            if let Some(ref mut plans) = self.pending_plans {
                if let Some(plan) = plans.get_mut(target) {
                    // Plan was pre-created by build_with_rules; clear pending prereqs since
                    // they were already used during plan pre-creation.
                    let _ = std::mem::take(&mut self.pending_plan_prereqs);
                    let _ = std::mem::take(&mut self.pending_plan_order_only);
                    plan.recipe = expanded_recipe;
                    plan.needs_rebuild = true;
                    plan.auto_vars = auto_vars.clone();
                    plan.extra_exports = self.target_extra_exports.clone();
                    plan.extra_unexports = self.target_extra_unexports.iter().cloned().collect();
                    // Update wait_groups if we have pending ones (from build_with_rules).
                    let wg = std::mem::take(&mut self.pending_plan_wait_groups);
                    if !wg.is_empty() {
                        plan.wait_groups = wg;
                    }
                } else {
                    // Plan not pre-created (e.g. pattern rule or .DEFAULT); create it now.
                    // Use pending_plan_prereqs which was set by build_with_pattern_rule.
                    let prereqs_for_plan = std::mem::take(&mut self.pending_plan_prereqs);
                    let plan = parallel::TargetPlan {
                        target: target.to_string(),
                        prerequisites: prereqs_for_plan,
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
                        wait_groups: std::mem::take(&mut self.pending_plan_wait_groups),
                        intermediate_also_make: Vec::new(),
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
            //   - For Bourne-compatible shells: prefix chars (@, -, +) are stripped
            //     from each recipe line before building the script.
            //   - For non-Bourne shells (perl, python, etc.): prefix chars are NOT
            //     stripped because they may be valid syntax in those languages.
            //     GNU Make's rule: only strip for is_bourne_compatible_shell().
            //   - Echo behaviour and error-ignore are controlled by the FIRST recipe
            //     line's prefix only; inner-line prefix chars don't affect behavior.
            //   - The last recipe lineno is used for error messages.

            let bourne_shell = is_bourne_compatible_shell(self.shell);
            let mut script = String::new();
            let mut first_line_silent = false;
            let mut first_line_ignore = false;
            let mut is_first = true;
            let mut last_lineno: usize = 0;

            *self.state.in_recipe_execution.borrow_mut() = true;
            for (lineno, line) in recipe {
                last_lineno = *lineno;
                // Update current_file/current_line so that errors during expansion
                // (e.g. from $(word ...) or $(wordlist ...)) report the correct location.
                *self.state.current_file.borrow_mut() = source_file.to_string();
                *self.state.current_line.borrow_mut() = *lineno;
                // Pre-process: collapse \<newline> inside $(…)/${…} references
                let preprocessed = preprocess_recipe_bsnl(line);
                let expanded = self.state.expand_with_auto_vars(&preprocessed, auto_vars);
                // For the first line: record behavioral flags (silent, ignore errors).
                // The first line's prefix chars (@, -, +) are ALWAYS stripped:
                // GNU Make's start_job_command() strips them unconditionally
                // from the start of the recipe text before passing it to the shell.
                // Interior lines (2+) are stripped only for Bourne-compatible shells;
                // for non-Bourne shells (perl, python, etc.) they pass verbatim because
                // those chars may be valid syntax in the target language.
                let cmd_line = if is_first {
                    let (_d, ls, li, _lf) = parse_recipe_prefix(&expanded);
                    first_line_silent = ls;
                    first_line_ignore = li;
                    is_first = false;
                    strip_recipe_prefixes(&expanded)
                } else if bourne_shell {
                    strip_recipe_prefixes(&expanded)
                } else {
                    expanded
                };
                script.push_str(&cmd_line);
                script.push('\n');
            }
            *self.state.in_recipe_execution.borrow_mut() = false;

            let effective_silent = first_line_silent || self.silent || is_silent_target;
            let effective_ignore = first_line_ignore || self.ignore_errors;

            if !effective_silent {
                // Echo lines with @-+ prefixes stripped from ALL lines (for display).
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
        // Signal that we are inside recipe expansion so $(eval) can detect
        // and reject new prerequisite definitions (GNU Make bug #12124).
        *self.state.in_recipe_execution.borrow_mut() = true;
        let pre_expanded: Vec<(usize, String, Vec<String>)> = recipe.iter().map(|(lineno, line)| {
            *self.state.current_file.borrow_mut() = source_file.to_string();
            *self.state.current_line.borrow_mut() = *lineno;
            let preprocessed = preprocess_recipe_bsnl(line);
            let expanded = self.state.expand_with_auto_vars(&preprocessed, auto_vars);
            let sub_lines = split_recipe_sub_lines(&expanded);
            (*lineno, line.clone(), sub_lines)
        }).collect();
        *self.state.in_recipe_execution.borrow_mut() = false;

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
                    run_cmd_with_error_handling(c, &cmd, &self.progname)
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
                    run_cmd_with_error_handling(c, &cmd, &self.progname)
                };
                match child_status {
                    Ok(code) if code != 0 => {
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
            // MAKE_RESTARTS must NOT be exported to child makes.
            // It is only meaningful to the current process (counting re-execs).
            // Child makes start fresh with MAKE_RESTARTS=0 (unset).
            if name == "MAKE_RESTARTS" {
                cmd.env_remove(name);
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
            //
            // POSIX special case for SHELL: per POSIX, the value of SHELL in the makefile
            // has no effect on the shell used by subprocesses via the environment.  Only
            // when SHELL is explicitly `export`ed from the makefile does the shell see the
            // makefile's value.  Without explicit export, the child process should see the
            // inherited SHELL from the invoking environment (not the makefile override).
            // Therefore SHELL is never auto-exported even when it came from the environment,
            // unless it is explicitly `export`ed (var.export == Some(true)) or the global
            // export-all (.EXPORT_ALL_VARIABLES) is active.
            let shell_blocks_auto_export = name == "SHELL"
                && var.origin != VarOrigin::Environment
                && var.export != Some(true)
                && !self.db.export_all;
            let should_export = !var.is_private && !shell_blocks_auto_export && (always_export || match var.export {
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
            } else if shell_blocks_auto_export {
                // POSIX SHELL special case: the makefile overrode SHELL without explicit export.
                // Do NOT call cmd.env_remove here — let the child inherit the original SHELL
                // value from the parent process's own environment.  The makefile's SHELL value
                // is used by jmake to invoke the recipe shell, but the child process's $SHELL
                // env variable must remain whatever it was in the invoking environment.
                // (If we called env_remove, the child would see an empty SHELL.)
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

        // Collect explicit export/unexport decisions for this target (pattern + target-specific).
        // Used below to decide whether inherited export_status can be overridden.
        let mut explicit_exports: HashSet<String> = HashSet::new();
        for psv in &self.db.pattern_specific_vars {
            if match_pattern_simple(&psv.pattern, target).is_some() {
                if psv.var.export == Some(true) {
                    explicit_exports.insert(psv.var_name.clone());
                }
            }
        }
        if let Some(rules) = self.db.rules.get(target) {
            for rule in rules {
                for (var_name, var) in &rule.target_specific_vars {
                    if var.export == Some(true) {
                        explicit_exports.insert(var_name.clone());
                    }
                }
            }
        }

        // Also apply inherited export status from the parent target's for_prereqs stack.
        // When a parent target has `unexport VAR=val`, its for_prereqs carries
        // export_status=Some(false) for VAR. This suppresses VAR in the shell env
        // of this target's recipe, even if this target itself has no explicit unexport.
        // Similarly, `export VAR=val` in a parent propagates export_status=Some(true).
        if let Some(inherited) = self.inherited_vars_stack.last() {
            for (var_name, (_, _, _, export_status)) in inherited {
                match export_status {
                    Some(false) => {
                        // Only add to unexports if this target doesn't explicitly export it.
                        if !explicit_exports.contains(var_name.as_str()) {
                            unexports.insert(var_name.clone());
                        }
                    }
                    Some(true) => {
                        // Inherited explicit export: remove from unexports (unless target explicitly
                        // unexports it, which was already handled above).
                        if !unexports.contains(var_name.as_str()) {
                            // Mark as inherited-exported so the loop below includes this var.
                            explicit_exports.insert(var_name.clone());
                        }
                    }
                    None => {}
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
            // Check for explicit target-specific export (also covers inherited export status).
            let target_explicitly_exported = explicit_exports.contains(var_name.as_str());
            // POSIX special case for SHELL: target-specific SHELL without explicit export
            // must NOT be exported to the child's environment.  Only an explicit `export`
            // declaration causes the target-specific SHELL value to be exported.
            let target_shell_blocks_export = var_name == "SHELL"
                && !target_explicitly_exported
                && !self.db.export_all;
            if !target_shell_blocks_export && (target_explicitly_exported || global_should_export(var_name)) {
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

        // Also apply inherited unexport propagation from the parent target's for_prereqs stack.
        // When a parent target has `unexport VAR=val`, the for_prereqs entry carries
        // export_status=Some(false), which must suppress VAR in this target's recipe env
        // (even if this target itself has no explicit unexport for VAR).
        if let Some(inherited) = self.inherited_vars_stack.last() {
            for (var_name, (_, _, _, export_status)) in inherited {
                if *export_status == Some(false) && !unexports.contains(var_name.as_str()) {
                    // Check that this target doesn't explicitly re-export the var.
                    let target_explicitly_exports = self.db.rules.get(target)
                        .map(|rules| rules.iter().any(|r| r.target_specific_vars.iter().any(|(n, v)| n == var_name && v.export == Some(true))))
                        .unwrap_or(false)
                        || self.db.pattern_specific_vars.iter().any(|psv| {
                            match_pattern_simple(&psv.pattern, target).is_some()
                            && &psv.var_name == var_name
                            && psv.var.export == Some(true)
                        });
                    if !target_explicitly_exports {
                        unexports.insert(var_name.clone());
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
                    wait_groups: Vec::new(),
                    intermediate_also_make: Vec::new(),
                });
            }
        }
    }
}

/// Format a "file:line: " location prefix for error messages.
/// If source_file is empty or lineno is 0, returns an empty string.
/// Extract ".WAIT groups" from a prerequisite list that may contain ".WAIT" markers.
///
/// Normalize a file path for what-if comparison by stripping a leading `./`.
/// This allows `-W ./foo` to match `foo` and vice versa.
#[inline]
fn normalize_path(p: &str) -> &str {
    p.strip_prefix("./").unwrap_or(p)
}

/// Returns a Vec of groups, where each group is the list of prereqs between two consecutive
/// .WAIT markers (or between a .WAIT and the start/end of the list).  An empty Vec is
/// returned when there are no .WAIT markers (the caller should treat the list as a single
/// group with no ordering constraints).
///
/// Example: [A, B, .WAIT, C, D, .WAIT, E] → [[A,B], [C,D], [E]]
fn extract_wait_groups(prereqs: &[String]) -> Vec<Vec<String>> {
    if !prereqs.iter().any(|p| p == ".WAIT") {
        return Vec::new();
    }
    let mut groups: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for p in prereqs {
        if p == ".WAIT" {
            groups.push(std::mem::take(&mut current));
        } else {
            current.push(p.clone());
        }
    }
    groups.push(current);
    groups
}

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

/// Returns true if the SE text (before stem substitution) contains any `%` character
/// that would be replaced by stem substitution.  Used to determine if SE-expanded
/// prerequisites are "stem-derived" (intermediate) or not (explicitly mentioned).
fn se_text_has_percent(text: &str) -> bool {
    text.contains('%')
}

/// Extract "non-dollar" words from an SE text that has already had stem substituted.
/// A "non-dollar" word is a top-level whitespace-delimited token that contains no `$`.
/// These words correspond to prerequisites that were NOT double-dollar-escaped in the
/// original source (e.g. `hello.o` from `%.tsk: %.o $$(info ...)` with stem `hello`).
/// They should be built BEFORE the SE expansion runs, so their own SE side effects
/// (like $(info ...) in their pattern rules) fire before the current target's SE.
fn se_extract_non_dollar_words(text: &str) -> Vec<String> {
    let mut result = Vec::new();
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    while i < n {
        // Skip leading whitespace.
        while i < n && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if i >= n { break; }
        // Collect one top-level word, tracking nesting depth.
        let word_start = i;
        let mut depth: i32 = 0;
        let mut has_dollar = false;
        while i < n {
            let b = bytes[i];
            if (b == b' ' || b == b'\t') && depth == 0 { break; }
            if b == b'$' {
                has_dollar = true;
                if i + 1 < n && (bytes[i+1] == b'(' || bytes[i+1] == b'{') {
                    depth += 1;
                    i += 2;
                    continue;
                }
            }
            if (b == b')' || b == b'}') && depth > 0 {
                depth -= 1;
            }
            i += 1;
        }
        if !has_dollar && i > word_start {
            let word = &text[word_start..i];
            if !word.is_empty() && word != "|" {
                result.push(word.to_string());
            }
        }
    }
    result
}

/// Returns true if the SE text contains ANY word (top-level space-separated
/// token, respecting `$(...)` groups) that does NOT contain `%`.
/// Such words expand to files that are NOT stem-derived, i.e. "explicitly
/// mentioned" files that must not be treated as intermediate.
///
/// This is used to decide whether to skip the up-to-date precheck:
/// if ALL words have `%`, SE can only produce stem-derived (intermediate)
/// prereqs, and the precheck can still run safely.  If any word lacks `%`,
/// the SE might produce explicitly-mentioned files, and the precheck must
/// be skipped to allow those files to trigger a rebuild.
fn se_text_has_non_pattern_word(text: &str) -> bool {
    // Walk through top-level words, skipping $()/\${} groups.
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    let mut in_word = false;
    let mut word_has_percent = false;

    while i < n {
        match bytes[i] {
            b' ' | b'\t' | b'\n' => {
                // End of word
                if in_word && !word_has_percent {
                    return true; // Found a word without %
                }
                in_word = false;
                word_has_percent = false;
                i += 1;
            }
            b'$' => {
                in_word = true;
                if i + 1 < n {
                    match bytes[i + 1] {
                        b'(' | b'{' => {
                            // Skip past the entire $()/\${} group
                            let open = bytes[i + 1];
                            let close = if open == b'(' { b')' } else { b'}' };
                            let mut depth = 1;
                            i += 2;
                            while i < n && depth > 0 {
                                if bytes[i] == open { depth += 1; }
                                else if bytes[i] == close { depth -= 1; }
                                i += 1;
                            }
                        }
                        _ => { i += 2; } // $x single-char variable
                    }
                } else {
                    i += 1;
                }
            }
            b'%' => {
                in_word = true;
                word_has_percent = true;
                i += 1;
            }
            _ => {
                in_word = true;
                i += 1;
            }
        }
    }
    // Check last word
    if in_word && !word_has_percent {
        return true;
    }
    false
}

/// Collects all top-level words from the SE text that do NOT contain `%`.
/// These are words that are NOT stem-derived, i.e. they will expand to
/// explicitly-mentioned files regardless of what the stem is.
///
/// Used to mark the right subset of SE-expanded prerequisites as
/// "explicitly mentioned" (i.e. non-intermediate) in the database.
fn se_non_pattern_words(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut result = Vec::new();
    let mut i = 0;
    let mut word_start = 0;
    let mut in_word = false;
    let mut word_has_percent = false;

    let mut flush_word = |start: usize, end: usize, has_pct: bool, result: &mut Vec<String>| {
        if !has_pct && end > start {
            result.push(text[start..end].to_string());
        }
    };

    while i < n {
        match bytes[i] {
            b' ' | b'\t' | b'\n' => {
                if in_word {
                    flush_word(word_start, i, word_has_percent, &mut result);
                }
                in_word = false;
                word_has_percent = false;
                i += 1;
            }
            b'$' => {
                if !in_word {
                    word_start = i;
                    in_word = true;
                }
                if i + 1 < n {
                    match bytes[i + 1] {
                        b'(' | b'{' => {
                            let open = bytes[i + 1];
                            let close = if open == b'(' { b')' } else { b'}' };
                            let mut depth = 1;
                            i += 2;
                            while i < n && depth > 0 {
                                if bytes[i] == open { depth += 1; }
                                else if bytes[i] == close { depth -= 1; }
                                i += 1;
                            }
                        }
                        _ => { i += 2; }
                    }
                } else {
                    i += 1;
                }
            }
            b'%' => {
                if !in_word {
                    word_start = i;
                    in_word = true;
                }
                word_has_percent = true;
                i += 1;
            }
            _ => {
                if !in_word {
                    word_start = i;
                    in_word = true;
                }
                i += 1;
            }
        }
    }
    if in_word {
        flush_word(word_start, n, word_has_percent, &mut result);
    }
    result
}

/// Collects all top-level words from the SE text that contain `%`.
/// These are words that ARE stem-derived.  Complement of `se_non_pattern_words`.
///
/// Used in the mixed-word case to compute the stem-derived prereq set without
/// re-running side-effect functions ($(info ...) etc.) in non-`%` words.
fn se_pattern_words(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut result = Vec::new();
    let mut i = 0;
    let mut word_start = 0;
    let mut in_word = false;
    let mut word_has_percent = false;

    while i < n {
        match bytes[i] {
            b' ' | b'\t' | b'\n' => {
                if in_word && word_has_percent {
                    result.push(text[word_start..i].to_string());
                }
                in_word = false;
                word_has_percent = false;
                i += 1;
            }
            b'$' => {
                if !in_word { word_start = i; in_word = true; }
                if i + 1 < n {
                    match bytes[i + 1] {
                        b'(' | b'{' => {
                            let open = bytes[i + 1];
                            let close = if open == b'(' { b')' } else { b'}' };
                            let mut depth = 1;
                            i += 2;
                            while i < n && depth > 0 {
                                if bytes[i] == open { depth += 1; }
                                else if bytes[i] == close { depth -= 1; }
                                i += 1;
                            }
                        }
                        _ => { i += 2; }
                    }
                } else { i += 1; }
            }
            b'%' => {
                if !in_word { word_start = i; in_word = true; }
                word_has_percent = true;
                i += 1;
            }
            _ => {
                if !in_word { word_start = i; in_word = true; }
                i += 1;
            }
        }
    }
    if in_word && word_has_percent {
        result.push(text[word_start..n].to_string());
    }
    result
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
    // When substituting the stem into SE text, escape any '$' in the stem so
    // that subsequent expansion doesn't interpret them as variable references
    // (GNU Make "triple-expansion prevention": stems like "oo$ba" must survive
    // second expansion as literal "oo$ba", not become "oo" + expand("$ba")).
    let escaped_stem: String = stem.replace('$', "$$");
    let stem_to_use = escaped_stem.as_str();

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

        // Collect one top-level word, respecting $(...)/${...} groups so that
        // whitespace inside function calls doesn't split words.
        // Within a word, replace only the first '%' (at ANY depth level, including
        // inside function calls like $(%_a) → $(x_a) when stem=x).
        // GNU Make substitutes % in SE text at all depths, not just top level.
        let mut depth: i32 = 0;
        let mut first_percent_replaced = false;
        while i < n {
            let c = chars[i];
            // Whitespace at top level ends the word.
            if c.is_whitespace() && depth == 0 { break; }
            // Whitespace inside a function call (depth > 0) is also a word boundary
            // for % substitution.  Reset the flag and copy the whitespace verbatim.
            if c.is_whitespace() && depth > 0 {
                first_percent_replaced = false;
                result.push(c);
                i += 1;
                continue;
            }
            // Track $(  and ${  depth.
            if c == '$' && i + 1 < n && (chars[i + 1] == '(' || chars[i + 1] == '{') {
                depth += 1;
                result.push(c);
                i += 1;
                result.push(chars[i]);
                i += 1;
                continue;
            }
            // Closing ) or } at depth > 0.
            if (c == ')' || c == '}') && depth > 0 {
                depth -= 1;
                result.push(c);
                i += 1;
                continue;
            }
            // '%' at ANY depth: substitute stem (only once per whitespace-delimited word).
            if c == '%' && !first_percent_replaced {
                result.push_str(stem_to_use);
                first_percent_replaced = true;
                i += 1;
                continue;
            }
            result.push(c);
            i += 1;
        }
    }

    result
}

/// GNU Make "stem directory" behaviour: when a pattern like `%.x` (no directory
/// in the pattern) matches a target like `lib/bye.x`, the full stem is `lib/bye`.
/// However when substituting into prerequisites, only the *base* part of the stem
/// (`bye`) is used for words where `%` is **not** the first character, and the
/// directory prefix (`lib/`) is prepended to the resulting word.  For words where
/// `%` IS the first character, the full stem (including directory) is used directly.
/// Words with no `%` are left unchanged — no directory prepending.
///
/// This function returns `(dir_part, base_stem)`.  The special treatment is only
/// applied when the matched pattern target (`pattern_target`) does NOT contain a
/// `/` and the stem DOES (meaning the directory came from the target, not from the
/// pattern prefix).
fn stem_dir_parts<'a>(pattern_target: &str, full_stem: &'a str) -> (&'a str, &'a str) {
    if !pattern_target.contains('/') {
        if let Some(slash_pos) = full_stem.rfind('/') {
            let dir = &full_stem[..slash_pos + 1]; // e.g. "lib/"
            let base = &full_stem[slash_pos + 1..]; // e.g. "bye"
            return (dir, base);
        }
    }
    ("", full_stem)
}

/// Apply `%` → stem substitution to a single prerequisite word using GNU Make's
/// "stem directory" rule.  When `%` is the first character of the word, use the
/// full stem (dir + base); otherwise use only the base stem and prepend the dir.
/// Words without `%` are returned unchanged.
fn subst_stem_in_prereq_dir(prereq: &str, full_stem: &str, pattern_target: &str) -> String {
    let (dir, base) = stem_dir_parts(pattern_target, full_stem);
    if dir.is_empty() {
        replace_first_percent(prereq, full_stem)
    } else if let Some(pct_pos) = prereq.find('%') {
        if pct_pos == 0 {
            // % at start: use the full stem (dir + base).
            replace_first_percent(prereq, full_stem)
        } else {
            // % not at start: substitute base stem then prepend dir.
            let result = replace_first_percent(prereq, base);
            format!("{}{}", dir, result)
        }
    } else {
        // No % in prereq: use as-is (no dir prepend).
        prereq.to_string()
    }
}

/// Dir-aware version of `subst_stem_in_se_text`.  Applies the same "stem directory"
/// treatment as `subst_stem_in_prereq_dir` but processes a multi-word raw SE text.
/// Respects $(...)/${...} grouping so whitespace inside function calls doesn't
/// split words.  The `dir` is added via $(addprefix ...) wrapping so that it
/// applies to ALL words produced by a function call, not just the raw token.
fn subst_stem_in_se_text_dir(text: &str, full_stem: &str, pattern_target: &str) -> String {
    let (dir, base) = stem_dir_parts(pattern_target, full_stem);
    if dir.is_empty() {
        return subst_stem_in_se_text(text, full_stem);
    }
    // Escape '$' in stems to prevent triple-expansion.
    let escaped_full: String = full_stem.replace('$', "$$");
    let escaped_base: String = base.replace('$', "$$");

    let mut result = String::with_capacity(text.len() + full_stem.len() * 4);
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let n = chars.len();
    while i < n {
        if chars[i].is_whitespace() {
            result.push(chars[i]);
            i += 1;
            continue;
        }
        // Collect one top-level word (respecting $(...)/${...} nesting depth).
        // Scan through ALL characters (including inside function calls) to find the FIRST '%'.
        // The dir-logic is based on the POSITION of % relative to the start of the word.
        let word_start = i;
        let mut depth: i32 = 0;
        // pct_abs: absolute index of first '%' in the word (at any depth level).
        let mut pct_abs: Option<usize> = None;
        let mut j = i;
        while j < n {
            let c = chars[j];
            if c.is_whitespace() && depth == 0 { break; }
            if c == '$' && j + 1 < n && (chars[j + 1] == '(' || chars[j + 1] == '{') {
                depth += 1;
                j += 2;
                continue;
            }
            if (c == ')' || c == '}') && depth > 0 {
                depth -= 1;
                j += 1;
                continue;
            }
            if c == '%' && pct_abs.is_none() {
                pct_abs = Some(j);
            }
            j += 1;
        }
        // The word is chars[word_start..j].
        if let Some(pct_pos) = pct_abs {
            // Word contains '%' (possibly inside a function call).
            // Determine dir-logic based on position of '%' relative to the word start.
            // If '%' is the very first character of the word: use full stem, no dir prepend.
            // Otherwise: use base stem and apply dir prepend.
            let use_full = pct_pos == word_start;
            let stem_for_subst = if use_full { escaped_full.as_str() } else { escaped_base.as_str() };
            // Determine if wrapping with addprefix is needed.
            // Wrapping is needed when: !use_full AND the word is a function call
            // (starts with '$(' or '${'), because dir-prepend must apply to each expanded word.
            let needs_addprefix = !use_full && word_start < n
                && chars[word_start] == '$'
                && word_start + 1 < n
                && (chars[word_start + 1] == '(' || chars[word_start + 1] == '{');
            if needs_addprefix {
                result.push_str("$(addprefix ");
                result.push_str(dir);
                result.push(',');
            } else if !use_full {
                result.push_str(dir);
            }
            // Copy word chars, substituting '%' with the chosen stem (first once per
            // whitespace-delimited word, including words inside function calls).
            let mut replaced = false;
            let mut copy_depth: i32 = 0;
            let mut k = word_start;
            while k < j {
                let c = chars[k];
                // Track depth in the copy loop.
                if c == '$' && k + 1 < j && (chars[k + 1] == '(' || chars[k + 1] == '{') {
                    copy_depth += 1;
                    result.push(c);
                    k += 1;
                    result.push(chars[k]);
                    k += 1;
                    continue;
                }
                if (c == ')' || c == '}') && copy_depth > 0 {
                    copy_depth -= 1;
                    result.push(c);
                    k += 1;
                    continue;
                }
                // Whitespace inside a function call: reset the replaced flag (new word boundary).
                if c.is_whitespace() && copy_depth > 0 {
                    replaced = false;
                    result.push(c);
                    k += 1;
                    continue;
                }
                if c == '%' && !replaced {
                    result.push_str(stem_for_subst);
                    replaced = true;
                } else {
                    result.push(c);
                }
                k += 1;
            }
            if needs_addprefix {
                result.push(')');
            }
        } else {
            // No '%' in word: copy as-is (no dir prepend).
            for k in word_start..j {
                result.push(chars[k]);
            }
        }
        i = j;
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

/// Get mtime for symlink checking: with -L, use the latest mtime between the
/// symlink itself and its target (recursively), as GNU Make does.
/// For dangling symlinks (target doesn't exist), returns the symlink's own mtime
/// so the dependent target can still be built.
fn get_mtime_symlink(path: &str) -> Option<SystemTime> {
    // Get the symlink's own mtime via lstat.
    let sym_meta = fs::symlink_metadata(path).ok()?;
    let sym_mtime = sym_meta.modified().ok()?;

    // If it's not a symlink, just return the file's mtime.
    if !sym_meta.file_type().is_symlink() {
        return Some(sym_mtime);
    }

    // It's a symlink: read where it points.
    if let Ok(target) = fs::read_link(path) {
        // Resolve the target path relative to the parent directory of 'path'.
        let resolved = if target.is_absolute() {
            target
        } else if let Some(parent) = Path::new(path).parent() {
            parent.join(&target)
        } else {
            target
        };
        // Recursively get the target's mtime (handles symlink chains).
        // If the target doesn't exist (dangling), fall through and use sym_mtime.
        if let Some(target_mtime) = get_mtime_symlink(resolved.to_str().unwrap_or("")) {
            // Use the MAX of symlink mtime and target mtime.
            return Some(sym_mtime.max(target_mtime));
        }
    }

    // Dangling symlink or can't read link: return symlink's own mtime.
    Some(sym_mtime)
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

/// Extract the program name (first word) from a shell command line.
fn extract_cmd_name(cmd: &str) -> &str {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return "";
    }
    let end = trimmed.find(|c: char| c.is_ascii_whitespace()).unwrap_or(trimmed.len());
    &trimmed[..end]
}

/// Run a command through the shell, capturing stderr to intercept and reformat
/// "command not found" / "permission denied" errors to match GNU Make's output format.
///
/// Returns `Ok(exit_code)` where exit_code is normalized:
/// - 126/127 for exec errors become 127 (GNU Make normalizes both to 127)
/// - Other codes are returned as-is
/// Returns `Err` if the process could not be spawned.
fn run_cmd_with_error_handling(
    mut cmd: Command,
    recipe_cmd: &str,
    progname: &str,
) -> std::io::Result<i32> {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;

    // Pipe stderr so we can filter the shell's own "command not found" / "inaccessible or not
    // found" messages (emitted by shells like mksh/dash before exiting with code 127/126).
    // We emit all other stderr lines immediately, line-by-line, to preserve real-time ordering
    // for programs that write to stderr while running (e.g. recursive make -w).
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn()?;

    // Drain stderr in the current thread (stdout is still inherited = real-time).
    let stderr_pipe = child.stderr.take().unwrap();
    let reader = BufReader::new(stderr_pipe);
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        // Suppress lines that are the shell's own exec error messages.
        // The shell emits these when exit code is 127 (not found) or 126 (permission/exec).
        // GNU Make prints its own formatted version of these errors, so we suppress the
        // shell's version to avoid duplication.
        // Forms include:
        //   "/bin/sh: cmd: inaccessible or not found"   (mksh, 127)
        //   "mksh: cmd: not found"                      (mksh, 127)
        //   "sh: cmd: command not found"                (dash, 127)
        //   "/bin/sh: cmd: can't execute: Permission denied"  (mksh, 126)
        //   "/bin/sh: cmd: can't execute: Is a directory"     (mksh, 126)
        //   "sh: cmd: Permission denied"                (dash, 126)
        let is_shell_exec_error = line.ends_with(": inaccessible or not found")
            || line.ends_with(": not found")
            || line.ends_with(": command not found")
            || line.contains(": can't execute: ")
            || (line.ends_with(": Permission denied") && !line.starts_with("jmake:"))
            || (line.ends_with(": Is a directory") && !line.starts_with("jmake:"));
        if !is_shell_exec_error {
            eprintln!("{}", line);
        }
    }

    let status = child.wait()?;
    let code = status.code().unwrap_or(1);

    // If exit code is 127 (command not found) or 126 (permission/exec error),
    // check if the first word of the command exists and print our own GNU Make-style error.
    // GNU Make normalizes both to exit 127 when it handles the error itself via exec.
    if (code == 127 || code == 126) && !recipe_cmd.trim().is_empty() {
        let cmd_name = extract_cmd_name(recipe_cmd);
        if !cmd_name.is_empty() {
            // For commands with a path component (contain '/'), check the filesystem
            // directly to distinguish ENOENT vs EACCES.
            // For bare command names (no '/'), the shell searches PATH to find them.
            // We cannot replicate that search here (PATH may have been changed by the
            // makefile itself), so we rely on the shell exit code:
            //   127 = command not found (ENOENT equivalent) → "No such file or directory"
            //   126 = command found but not executable (EACCES)  → "Permission denied"
            let err_msg = if cmd_name.contains('/') {
                if !Path::new(cmd_name).exists() {
                    Some("No such file or directory")
                } else {
                    Some("Permission denied")
                }
            } else {
                // Bare command name: use exit code to determine error message.
                if code == 127 {
                    Some("No such file or directory")
                } else {
                    Some("Permission denied")
                }
            };
            if let Some(msg) = err_msg {
                eprintln!("{}: {}: {}", progname, cmd_name, msg);
                return Ok(127);
            }
        }
    }

    Ok(code)
}

/// Collapse all `\<newline>` sequences in `s` unconditionally (at all depths).
/// Used to determine whether an expanded recipe line is truly empty: after
/// expansion, `\<newline> ` should be treated as an empty (whitespace-only)
/// string rather than containing a `\` character.
fn collapse_backslash_newlines(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut result = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
            // Skip the backslash, newline, and any leading whitespace on next line
            i += 2;
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
            // Replace with a single space (GNU Make semantics)
            result.push(' ');
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }
    result
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

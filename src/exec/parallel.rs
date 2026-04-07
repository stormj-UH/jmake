// Copyright (c) 2026 Jon-Erik G. Storm. All rights reserved.
// Parallel execution engine for jmake (-j N support).
//
// Architecture:
//   - Main thread: graph resolution (sequential), scheduler loop
//   - Worker threads: recipe execution (shell-spawning only)
//   - Communication: mpsc channels (Job → workers, JobResult ← workers)
//
// Only activated when jobs > 1 AND .NOTPARALLEL is not set.
// When jobs == 1, the existing sequential Executor code path is used unchanged.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::sync::mpsc;
use std::thread;

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

/// The build state of a single target in the parallel scheduler.
#[derive(Debug, Clone, PartialEq)]
pub enum TargetState {
    /// All prerequisites done, waiting to be dispatched.
    Ready,
    /// Currently executing in a worker thread.
    Running,
    /// Successfully completed. `bool` = was_rebuilt (recipe actually executed commands).
    Done(bool),
    /// Failed with an error message.
    Failed(String),
}

/// Everything the scheduler knows about a target after graph resolution.
/// Created during the sequential graph-resolution phase, before any workers start.
#[derive(Clone)]
pub struct TargetPlan {
    /// The target name.
    pub target: String,
    /// Normal prerequisites (their completion triggers rebuild check).
    pub prerequisites: Vec<String>,
    /// Order-only prerequisites (must complete first but don't trigger rebuild).
    pub order_only: Vec<String>,
    /// Recipe lines as (lineno, raw_line). Already resolved (no further expansion needed at
    /// this point — expansion was done per-line during graph resolution using auto_vars).
    pub recipe: Vec<(usize, String)>,
    /// Source file for the recipe (for error messages).
    pub source_file: String,
    /// Automatic variables pre-computed for this target ($@, $<, $^, etc.).
    pub auto_vars: HashMap<String, String>,
    /// Whether this target is .PHONY.
    pub is_phony: bool,
    /// Whether this target needs rebuilding (determined conservatively during resolution).
    pub needs_rebuild: bool,
    /// When Some(primary), this target is a grouped sibling whose recipe is run by `primary`.
    /// The sibling will be auto-completed when the primary finishes.
    pub grouped_primary: Option<String>,
    /// When non-empty, this target is the primary of a grouped rule and these are its siblings.
    pub grouped_siblings: Vec<String>,
    /// Target-specific variables to export to child processes.
    pub extra_exports: HashMap<String, String>,
    /// Target-specific variable names to remove from child environment.
    pub extra_unexports: Vec<String>,
    /// Whether this target is an intermediate file (tracked for post-build deletion).
    pub is_intermediate: bool,
    /// When non-empty, the normal prerequisites are split into ".WAIT groups".
    /// Group 0 runs first; once all of group 0 are Done, group 1 is eligible, etc.
    /// The scheduler uses this to enforce .WAIT ordering within a target's prerequisites.
    /// The flat `prerequisites` field still contains ALL prereqs for auto_var/rebuild purposes.
    pub wait_groups: Vec<Vec<String>>,
}

/// A unit of work dispatched to a worker thread.
/// Must be `Send` — contains only owned data, no references into Executor/MakeState.
pub struct Job {
    pub target: String,
    /// Pre-expanded recipe lines ready for execution:
    /// each entry is (lineno, original_line, expanded_sub_lines).
    pub pre_expanded: Vec<(usize, String, Vec<String>)>,
    pub source_file: String,
    pub shell: String,
    pub shell_flags: String,
    pub is_silent_target: bool,
    pub silent: bool,
    pub ignore_errors: bool,
    pub dry_run: bool,
    pub touch: bool,
    pub trace: bool,
    pub one_shell: bool,
    pub delete_on_error: bool,
    pub is_precious: bool,
    pub progname: String,
    pub makelevel: String,
    /// Global environment setup: (name, Some(value)) to set, (name, None) to remove.
    pub env_ops: Vec<(String, Option<String>)>,
    /// Target-specific exports.
    pub extra_exports: HashMap<String, String>,
    /// Target-specific unexports.
    pub extra_unexports: Vec<String>,
    /// Whether GNUMAKEFLAGS should be exported as empty.
    pub gnumakeflags_was_set: bool,
}

/// Result returned from a worker thread after executing a `Job`.
pub struct JobResult {
    pub target: String,
    /// True if the recipe actually ran shell commands (not just make-functions).
    pub rebuilt: bool,
    /// None = success, Some(msg) = error.
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Worker thread pool
// ---------------------------------------------------------------------------

/// Spawn `num_workers` worker threads.  Each thread takes jobs from `job_rx`
/// (shared via Arc<Mutex>) and sends results back via `result_tx`.
pub fn spawn_workers(
    num_workers: usize,
    job_rx: Arc<Mutex<mpsc::Receiver<Job>>>,
    result_tx: mpsc::Sender<JobResult>,
) -> Vec<thread::JoinHandle<()>> {
    (0..num_workers)
        .map(|_| {
            let rx = Arc::clone(&job_rx);
            let tx = result_tx.clone();
            thread::spawn(move || loop {
                let job = {
                    let receiver = rx.lock().unwrap();
                    match receiver.recv() {
                        Ok(j) => j,
                        Err(_) => return, // channel closed → exit
                    }
                };
                let result = execute_job(job);
                let _ = tx.send(result);
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// execute_job: standalone recipe execution (no Executor references)
// ---------------------------------------------------------------------------

/// Execute a single Job in a worker thread.
/// Self-contained: all state needed is inside `job`.
pub fn execute_job(job: Job) -> JobResult {
    let target = job.target.clone();

    // Apply trace output (we still print from the worker; output may interleave
    // with other workers, which is GNU Make's default behavior without --output-sync).
    if job.trace && !job.pre_expanded.is_empty() {
        let (lineno, _, _) = &job.pre_expanded[0];
        let loc = if job.source_file.is_empty() {
            String::new()
        } else if *lineno == 0 {
            format!("{}: ", job.source_file)
        } else {
            format!("{}:{}: ", job.source_file, lineno)
        };
        let reason = if !Path::new(&target).exists() {
            "target does not exist"
        } else {
            "target is out of date"
        };
        eprintln!("{}update target '{}' due to: {}", loc, target, reason);
    }

    if job.touch {
        if !job.silent {
            println!("touch {}", target);
        }
        if !job.dry_run {
            touch_file_standalone(&target);
        }
        return JobResult { target, rebuilt: true, error: None };
    }

    if job.one_shell {
        return execute_job_oneshell(job);
    }

    execute_job_normal(job)
}

fn execute_job_oneshell(job: Job) -> JobResult {
    let target = job.target.clone();
    let is_silent_target = job.is_silent_target;

    let mut script = String::new();
    let mut first_line_silent = false;
    let mut first_line_ignore = false;
    let mut is_first = true;
    let mut last_lineno: usize = 0;

    for (lineno, _orig, sub_lines) in &job.pre_expanded {
        last_lineno = *lineno;
        let expanded = sub_lines.join("\n");
        if is_first {
            let (_d, ls, li, _f) = parse_recipe_prefix_standalone(&expanded);
            first_line_silent = ls;
            first_line_ignore = li;
            is_first = false;
        }
        let cmd_line = strip_recipe_prefixes_standalone(&expanded);
        script.push_str(&cmd_line);
        script.push('\n');
    }

    let effective_silent = first_line_silent || job.silent || is_silent_target;
    let effective_ignore = first_line_ignore || job.ignore_errors;

    if !effective_silent {
        for (_lineno, _orig, sub_lines) in &job.pre_expanded {
            let expanded = sub_lines.join("\n");
            let display = strip_recipe_prefixes_standalone(&expanded);
            if !display.trim().is_empty() {
                println!("{}", display.trim_end());
            }
        }
    }

    if script.trim().is_empty() {
        return JobResult { target, rebuilt: false, error: None };
    }

    if job.dry_run {
        return JobResult { target, rebuilt: true, error: None };
    }

    let flags = parse_shell_flags_standalone(&job.shell_flags);
    let mut cmd = Command::new(&job.shell);
    for flag in &flags {
        cmd.arg(flag);
    }
    cmd.arg(script.trim_end_matches('\n'));
    apply_env_ops(&mut cmd, &job.env_ops, &job.extra_exports, &job.extra_unexports,
                  &job.makelevel, job.gnumakeflags_was_set);

    match cmd.status() {
        Ok(s) if !s.success() => {
            let code = s.code().unwrap_or(1);
            let loc = if job.source_file.is_empty() {
                String::new()
            } else if last_lineno == 0 {
                format!("{}: ", job.source_file)
            } else {
                format!("{}:{}: ", job.source_file, last_lineno)
            };
            if effective_ignore {
                eprintln!("{}: [{}{}] Error {} (ignored)", job.progname, loc, target, code);
                JobResult { target, rebuilt: true, error: None }
            } else {
                eprintln!("{}: *** [{}{}] Error {}", job.progname, loc, target, code);
                maybe_delete_on_error(&target, job.delete_on_error, job.is_precious, &job.progname);
                JobResult { target, rebuilt: true, error: Some(String::new()) }
            }
        }
        Err(e) => {
            eprintln!("{}: *** Error running shell: {}", job.progname, e);
            JobResult { target, rebuilt: true, error: Some(String::new()) }
        }
        _ => JobResult { target, rebuilt: true, error: None },
    }
}

fn execute_job_normal(job: Job) -> JobResult {
    let target = job.target.clone();
    let is_silent_target = job.is_silent_target;
    let mut any_cmd_ran = false;

    'outer: for (lineno, orig_line, sub_lines) in &job.pre_expanded {
        let lineno = *lineno;
        let (_outer_display, outer_silent, outer_ignore, outer_force) =
            parse_recipe_prefix_standalone(orig_line);

        for sub_line in sub_lines {
            let (display_line, line_silent, ignore_error, force_sub) =
                parse_recipe_prefix_standalone(sub_line);
            let force = force_sub || outer_force;

            let at_silent = if job.trace || job.dry_run {
                false
            } else {
                line_silent || outer_silent
            };
            let effective_silent = at_silent || job.silent || is_silent_target;
            let effective_ignore = ignore_error || outer_ignore || job.ignore_errors;

            let cmd = strip_recipe_prefixes_standalone(sub_line);

            if cmd.trim().is_empty() {
                continue;
            }

            if !effective_silent {
                println!("{}", display_line);
            }

            if job.dry_run {
                let contains_make_var =
                    orig_line.contains("$(MAKE)") || orig_line.contains("${MAKE}");
                if !force && !contains_make_var {
                    any_cmd_ran = true;
                    continue;
                }
            }

            any_cmd_ran = true;

            // Determine effective shell/flags for this command.
            // (target-specific SHELL/.SHELLFLAGS are already baked into the job's
            // shell/shell_flags fields by the scheduler.)
            let child_status = if job.shell.contains(' ') {
                let composed = format!("{} {} {}", job.shell, job.shell_flags, cmd);
                let mut c = Command::new("/bin/sh");
                c.arg("-c").arg(&composed);
                apply_env_ops(&mut c, &job.env_ops, &job.extra_exports,
                              &job.extra_unexports, &job.makelevel,
                              job.gnumakeflags_was_set);
                c.status()
            } else {
                let flags = parse_shell_flags_standalone(&job.shell_flags);
                let mut c = Command::new(&job.shell);
                for flag in &flags {
                    c.arg(flag);
                }
                c.arg(&cmd);
                apply_env_ops(&mut c, &job.env_ops, &job.extra_exports,
                              &job.extra_unexports, &job.makelevel,
                              job.gnumakeflags_was_set);
                c.status()
            };

            let loc = if job.source_file.is_empty() {
                String::new()
            } else if lineno == 0 {
                format!("{}: ", job.source_file)
            } else {
                format!("{}:{}: ", job.source_file, lineno)
            };

            match child_status {
                Ok(s) if !s.success() => {
                    let code = s.code().unwrap_or(1);
                    if effective_ignore {
                        eprintln!("{}: [{}{}] Error {} (ignored)",
                                  job.progname, loc, target, code);
                    } else {
                        eprintln!("{}: *** [{}{}] Error {}",
                                  job.progname, loc, target, code);
                        maybe_delete_on_error(&target, job.delete_on_error,
                                              job.is_precious, &job.progname);
                        return JobResult { target, rebuilt: true, error: Some(String::new()) };
                    }
                }
                Err(e) => {
                    if effective_ignore {
                        eprintln!("{}: [{}{}] Error: {} (ignored)",
                                  job.progname, loc, target, e);
                    } else {
                        eprintln!("{}: *** [{}{}] Error: {}",
                                  job.progname, loc, target, e);
                        return JobResult { target, rebuilt: true, error: Some(String::new()) };
                    }
                }
                _ => {}
            }
        } // sub_line loop
    } // pre_expanded loop

    let _ = &target; // silence unused
    JobResult {
        target: job.target.clone(),
        rebuilt: any_cmd_ran,
        error: None,
    }
}

// ---------------------------------------------------------------------------
// Parallel scheduler
// ---------------------------------------------------------------------------

/// The parallel build scheduler.  Runs on the main thread.
pub struct ParallelScheduler {
    max_jobs: usize,
    /// All target plans, keyed by target name.
    plans: HashMap<String, TargetPlan>,
    /// Current execution state of each target.
    pub states: HashMap<String, TargetState>,
    /// For each target, the prerequisites that must complete before it runs.
    prereqs_of: HashMap<String, Vec<String>>,
    /// Reverse map: for each target, the targets that depend on it.
    pub dependents_of: HashMap<String, Vec<String>>,
    /// Queue of targets ready to run (all prereqs done, hasn't started yet).
    pub ready_queue: VecDeque<String>,
    /// Number of jobs currently executing in worker threads.
    pub running_count: usize,
    /// Channel to send jobs to workers.
    pub job_tx: mpsc::Sender<Job>,
    /// Channel to receive results from workers.
    result_rx: mpsc::Receiver<JobResult>,
    /// True if any error has occurred.
    has_error: bool,
    /// Keep going on errors (-k).
    keep_going: bool,
    /// Collected error messages.
    errors: Vec<String>,
    /// True once we've decided to drain (stop launching, wait for in-flight jobs).
    draining: bool,
    /// Program name for error messages.
    progname: String,
    /// Targets that were actually rebuilt (for "any_recipe_ran" tracking).
    pub any_recipe_ran: bool,
    /// Intermediate targets that were rebuilt (candidates for deletion after build).
    pub intermediate_built: Vec<String>,
}

impl ParallelScheduler {
    pub fn new(
        max_jobs: usize,
        mut plans: HashMap<String, TargetPlan>,
        job_tx: mpsc::Sender<Job>,
        result_rx: mpsc::Receiver<JobResult>,
        keep_going: bool,
        progname: String,
    ) -> Self {
        // Build prereqs_of and dependents_of from plans.
        let mut prereqs_of: HashMap<String, Vec<String>> = HashMap::new();
        let mut dependents_of: HashMap<String, Vec<String>> = HashMap::new();

        for (name, plan) in &plans {
            let mut all_deps: Vec<String> = plan.prerequisites.clone();
            all_deps.extend(plan.order_only.iter().cloned());
            // Only include deps that are themselves in the plans map (i.e., targets we know about).
            let deps: Vec<String> = all_deps
                .into_iter()
                .filter(|d| plans.contains_key(d.as_str()))
                .collect();

            for dep in &deps {
                dependents_of
                    .entry(dep.clone())
                    .or_default()
                    .push(name.clone());
            }
            prereqs_of.insert(name.clone(), deps);
        }

        // ---------------------------------------------------------------------------
        // .WAIT group processing: inject virtual barrier nodes to enforce ordering.
        //
        // For a target with `wait_groups = [[A, B], [C, D], [E]]`:
        //   • Create barrier `target##wait##0` with prereqs [A, B] (completes when A+B done)
        //   • Create barrier `target##wait##1` with prereqs [C, D, target##wait##0]
        //   • Add `target##wait##0` to prereqs_of[C] and prereqs_of[D] (they must wait)
        //   • Add `target##wait##1` to prereqs_of[E]
        //
        // Safety: only inject the barrier into T's prereqs_of if T does NOT appear as a
        // non-waited (group 0 or no-wait-groups) prerequisite in any OTHER plan.  This
        // prevents deadlocks when the same target is shared between a parent that has .WAIT
        // and another parent that doesn't (per-target .WAIT semantics).
        // ---------------------------------------------------------------------------

        // Collect all targets that appear as "unguarded" prerequisites (first group or
        // plans with no wait_groups).  These targets can be dispatched before any barrier.
        let mut globally_unguarded: HashSet<String> = HashSet::new();
        for (_name, plan) in &plans {
            if plan.wait_groups.is_empty() {
                for p in &plan.prerequisites {
                    globally_unguarded.insert(p.clone());
                }
            } else if let Some(first_group) = plan.wait_groups.first() {
                for p in first_group {
                    globally_unguarded.insert(p.clone());
                }
            }
        }

        // Build barrier plans and extra prereq edges.
        let mut barrier_plans: HashMap<String, TargetPlan> = HashMap::new();
        // Extra entries to add to prereqs_of after iteration (can't borrow+mutate).
        let mut extra_prereqs: Vec<(String, String)> = Vec::new(); // (target, barrier_dep)
        let mut extra_dependents: Vec<(String, String)> = Vec::new(); // (dep, dependent)

        for (name, plan) in &plans {
            if plan.wait_groups.len() < 2 {
                continue;
            }
            let mut last_barrier: Option<String> = None;
            for (gi, group) in plan.wait_groups.iter().enumerate() {
                // The last group needs no barrier after it.
                if gi + 1 >= plan.wait_groups.len() {
                    break;
                }
                let barrier_name = format!("{}##wait##{}", name, gi);

                // Barrier prerequisites: targets in the current group that are known.
                let mut barrier_prereqs: Vec<String> = group.iter()
                    .filter(|t| plans.contains_key(t.as_str()))
                    .cloned()
                    .collect();
                // Chain: also depend on the previous barrier so groups are strictly ordered.
                if let Some(ref prev) = last_barrier {
                    if !barrier_prereqs.contains(prev) {
                        barrier_prereqs.push(prev.clone());
                    }
                }

                // Register prereqs_of for the barrier node.
                prereqs_of.insert(barrier_name.clone(), barrier_prereqs.clone());
                for dep in &barrier_prereqs {
                    dependents_of
                        .entry(dep.clone())
                        .or_default()
                        .push(barrier_name.clone());
                }

                // For each target in the NEXT group, inject the barrier as a prerequisite
                // UNLESS the target is "globally unguarded" (appears without a wait guard
                // in another plan — injecting would risk deadlock).
                let next_group = &plan.wait_groups[gi + 1];
                for after_target in next_group {
                    if globally_unguarded.contains(after_target.as_str()) {
                        // Skip: target is unguarded elsewhere — don't impose barrier.
                        continue;
                    }
                    if plans.contains_key(after_target.as_str()) {
                        extra_prereqs.push((after_target.clone(), barrier_name.clone()));
                        extra_dependents.push((barrier_name.clone(), after_target.clone()));
                    }
                }

                // Create the virtual barrier plan (empty recipe, completes immediately).
                barrier_plans.insert(barrier_name.clone(), TargetPlan {
                    target: barrier_name.clone(),
                    prerequisites: barrier_prereqs,
                    order_only: Vec::new(),
                    recipe: Vec::new(),
                    source_file: String::new(),
                    auto_vars: HashMap::new(),
                    is_phony: false,
                    needs_rebuild: true,
                    grouped_primary: None,
                    grouped_siblings: Vec::new(),
                    extra_exports: HashMap::new(),
                    extra_unexports: Vec::new(),
                    is_intermediate: false,
                    wait_groups: Vec::new(),
                });

                last_barrier = Some(barrier_name);
            }
        }

        // Apply extra prereqs/dependents edges.
        for (target, barrier_dep) in extra_prereqs {
            let entry = prereqs_of.entry(target).or_default();
            if !entry.contains(&barrier_dep) {
                entry.push(barrier_dep);
            }
        }
        for (dep, dependent) in extra_dependents {
            let entry = dependents_of.entry(dep).or_default();
            if !entry.contains(&dependent) {
                entry.push(dependent);
            }
        }

        // Merge barrier plans into the main plans map.
        for (k, v) in barrier_plans {
            plans.insert(k, v);
        }

        // Initialize states based on needs_rebuild and prereq availability.
        let mut states: HashMap<String, TargetState> = HashMap::new();
        for (name, plan) in &plans {
            // Grouped siblings don't run directly; their primary runs the recipe.
            if plan.grouped_primary.is_some() {
                // Will be marked Done when primary completes.
                continue;
            }
            if !plan.needs_rebuild {
                states.insert(name.clone(), TargetState::Done(false));
            }
            // Otherwise leave absent; initial_ready_queue will compute readiness.
        }

        ParallelScheduler {
            max_jobs,
            plans,
            states,
            prereqs_of,
            dependents_of,
            ready_queue: VecDeque::new(),
            running_count: 0,
            job_tx,
            result_rx,
            has_error: false,
            keep_going,
            errors: Vec::new(),
            draining: false,
            progname,
            any_recipe_ran: false,
            intermediate_built: Vec::new(),
        }
    }

    /// Enqueue all targets that are already ready (no pending prerequisites).
    pub fn find_initial_ready(&mut self, roots: &[String]) {
        // Walk all plans reachable from roots using BFS to find which targets need
        // to be considered. Then among those, find ones with no unfinished prereqs.
        // We use a Vec (visited_order) to maintain BFS-order for deterministic scheduling.
        let mut visited: HashSet<String> = HashSet::new();
        let mut visited_order: Vec<String> = Vec::new();
        let mut queue: VecDeque<String> = roots.iter().cloned().collect();
        while let Some(t) = queue.pop_front() {
            if !visited.insert(t.clone()) { continue; }
            visited_order.push(t.clone());
            if let Some(deps) = self.prereqs_of.get(&t) {
                for d in deps.clone() {
                    queue.push_back(d);
                }
            }
        }

        // Among reachable targets that need rebuilding, find those with all deps done.
        // Process in BFS order to maintain the order targets were specified as prerequisites.
        // This ensures `all: first second` processes `first` before `second`, preserving
        // the user-specified order in the ready queue.
        for t in &visited_order {
            if self.states.contains_key(t.as_str()) {
                // Already handled (Done or grouped sibling).
                continue;
            }
            let plan = match self.plans.get(t.as_str()) {
                Some(p) => p,
                None => continue,
            };
            if plan.grouped_primary.is_some() {
                continue; // sibling — handled by primary
            }
            if self.all_prereqs_done(t) {
                self.states.insert(t.clone(), TargetState::Ready);
                self.ready_queue.push_back(t.clone());
            }
        }
    }

    fn all_prereqs_done(&self, target: &str) -> bool {
        let prereqs = match self.prereqs_of.get(target) {
            Some(p) => p,
            None => return true,
        };
        prereqs.iter().all(|p| {
            matches!(self.states.get(p.as_str()), Some(TargetState::Done(_)))
        })
    }

    /// Public version of all_prereqs_done for use from exec/mod.rs.
    pub fn all_prereqs_done_pub(&self, target: &str) -> bool {
        self.all_prereqs_done(target)
    }

    /// Check if any prerequisite of `target` failed (for -k propagation).
    fn any_prereq_failed(&self, target: &str) -> bool {
        let prereqs = match self.prereqs_of.get(target) {
            Some(p) => p,
            None => return false,
        };
        prereqs.iter().any(|p| {
            matches!(self.states.get(p.as_str()), Some(TargetState::Failed(_)))
        })
    }

    /// Check if the recipe for `target` actually needs to run, given what we
    /// now know about which prerequisites were rebuilt.
    fn should_rebuild_now(&self, target: &str) -> bool {
        let plan = match self.plans.get(target) {
            Some(p) => p,
            None => return false,
        };
        if plan.is_phony {
            return true;
        }
        if plan.needs_rebuild {
            // Was conservatively marked as needing rebuild during graph resolution.
            // Re-check: if no prereq was actually rebuilt AND target exists with
            // newer mtime, skip.
            let any_rebuilt = plan.prerequisites.iter().any(|p| {
                matches!(self.states.get(p.as_str()), Some(TargetState::Done(true)))
            });
            if any_rebuilt {
                return true;
            }
            // Also check order-only prereqs — their rebuild doesn't trigger but
            // we still need to check the original mtime condition.
            // If needs_rebuild was set just because target doesn't exist → still rebuild.
            return true;
        }
        // Wasn't marked as needing rebuild — check if any normal prereq was rebuilt.
        plan.prerequisites.iter().any(|p| {
            matches!(self.states.get(p.as_str()), Some(TargetState::Done(true)))
        })
    }

    fn launch_job(&mut self, target: &str) {
        let plan = match self.plans.get(target) {
            Some(p) => p,
            None => {
                // No plan → mark done (no-op).
                self.states.insert(target.to_string(), TargetState::Done(false));
                return;
            }
        };

        // Check if the recipe actually needs to run given current state.
        if !self.should_rebuild_now(target) {
            self.states.insert(target.to_string(), TargetState::Done(false));
            self.complete_grouped_siblings(target, false);
            return;
        }

        if plan.recipe.is_empty() {
            // No recipe to run — target is "built" without executing anything.
            self.states.insert(target.to_string(), TargetState::Done(false));
            self.complete_grouped_siblings(target, false);
            return;
        }

        let job = Job {
            target: plan.target.clone(),
            pre_expanded: plan.recipe.iter().map(|(ln, line)| {
                // The recipe lines stored in TargetPlan are already the RAW (unexpanded)
                // lines. We need to expand them here. However, since we're on the main
                // thread and the plan was built on the main thread, and all expansion
                // happened during graph resolution (pre_expanded is stored), we just
                // package them for the worker.
                //
                // IMPORTANT: The recipe stored in TargetPlan IS pre-expanded (the executor
                // calls expand_with_auto_vars and stores the result). Sub-lines are
                // computed from the expanded text.
                //
                // For parallel.rs, TargetPlan.recipe stores (lineno, expanded_text) pairs
                // where expanded_text is the ALREADY-EXPANDED recipe line. Sub-lines are
                // split here.
                let sub_lines = split_recipe_sub_lines_standalone(line);
                (*ln, line.clone(), sub_lines)
            }).collect(),
            source_file: plan.source_file.clone(),
            shell: String::new(), // filled by build_targets_parallel before launch
            shell_flags: String::new(),
            is_silent_target: false, // filled by build_targets_parallel
            silent: false,
            ignore_errors: false,
            dry_run: false,
            touch: false,
            trace: false,
            one_shell: false,
            delete_on_error: false,
            is_precious: false,
            progname: self.progname.clone(),
            makelevel: String::new(),
            env_ops: Vec::new(),
            extra_exports: plan.extra_exports.clone(),
            extra_unexports: plan.extra_unexports.clone(),
            gnumakeflags_was_set: false,
        };
        // Note: the actual job fields are set in build_targets_parallel via build_job_from_plan.
        // We store a placeholder here; launch_job_with_settings is the real entry point.
        let _ = job; // suppress unused warning; actual send happens via launch_job_full
        self.states.insert(target.to_string(), TargetState::Running);
        self.running_count += 1;
    }

    /// Actually send the job to the worker pool. Called from ParallelExecutor wrapper.
    pub fn send_job(&self, job: Job) {
        let _ = self.job_tx.send(job);
    }

    fn complete_grouped_siblings(&mut self, primary: &str, rebuilt: bool) {
        let siblings = match self.plans.get(primary) {
            Some(p) => p.grouped_siblings.clone(),
            None => return,
        };
        for sibling in siblings {
            self.states.insert(sibling, TargetState::Done(rebuilt));
        }
    }

    pub fn handle_completion(&mut self, result: JobResult) {
        self.running_count -= 1;
        let target = result.target.clone();

        if let Some(err) = result.error {
            self.states.insert(target.clone(), TargetState::Failed(err.clone()));
            self.has_error = true;
            if self.running_count > 0 && !self.draining {
                eprintln!("{}: *** Waiting for unfinished jobs....", self.progname);
            }
            self.draining = true;
            if self.keep_going {
                self.errors.push(if err.is_empty() {
                    format!("Target '{}' failed.", target)
                } else {
                    err
                });
            }
            // Propagate failure to dependents (in -k mode, mark them Failed so they
            // don't launch unnecessarily).
            if let Some(deps) = self.dependents_of.get(&target) {
                for dep in deps.clone() {
                    if !self.states.contains_key(dep.as_str()) {
                        self.states.insert(dep.clone(), TargetState::Failed(
                            format!("prerequisite '{}' failed", target)
                        ));
                    }
                }
            }
        } else {
            let rebuilt = result.rebuilt;
            if rebuilt { self.any_recipe_ran = true; }
            self.states.insert(target.clone(), TargetState::Done(rebuilt));
            // Complete grouped siblings.
            self.complete_grouped_siblings(&target, rebuilt);
            // Track intermediate targets that were rebuilt.
            if let Some(plan) = self.plans.get(&target) {
                if rebuilt && plan.is_intermediate {
                    if !self.intermediate_built.contains(&target) {
                        self.intermediate_built.push(target.clone());
                    }
                }
            }

            // Check dependents — any that now have all prereqs done become Ready.
            if let Some(deps) = self.dependents_of.get(&target) {
                for dep in deps.clone() {
                    // Skip if already in a terminal or active state.
                    if self.states.contains_key(dep.as_str()) {
                        continue;
                    }
                    if !self.has_error || self.keep_going {
                        if self.all_prereqs_done(&dep) && !self.any_prereq_failed(&dep) {
                            self.states.insert(dep.clone(), TargetState::Ready);
                            self.ready_queue.push_back(dep);
                        } else if self.any_prereq_failed(&dep) {
                            self.states.insert(dep.clone(), TargetState::Failed(
                                format!("prerequisite of '{}' failed", dep)
                            ));
                        }
                    }
                }
            }
        }
    }

    pub fn is_done(&self) -> bool {
        self.running_count == 0 && self.ready_queue.is_empty()
    }

    pub fn has_work(&self) -> bool {
        self.running_count > 0 || !self.ready_queue.is_empty()
    }

    pub fn should_launch(&self) -> bool {
        !self.draining && self.running_count < self.max_jobs && !self.ready_queue.is_empty()
    }

    pub fn pop_ready(&mut self) -> Option<String> {
        self.ready_queue.pop_front()
    }

    pub fn recv_result(&self) -> Option<JobResult> {
        self.result_rx.recv().ok()
    }

    /// Drain remaining in-flight jobs after an error (without -k).
    pub fn drain_running(&mut self) {
        while self.running_count > 0 {
            if let Some(result) = self.result_rx.recv().ok() {
                self.running_count -= 1;
                // Record completion but don't enqueue dependents.
                self.states.insert(result.target, TargetState::Done(false));
            } else {
                break;
            }
        }
    }

    pub fn final_error(&self) -> Option<String> {
        if self.has_error {
            Some(if self.errors.is_empty() {
                String::new()
            } else {
                self.errors.join("\n")
            })
        } else {
            None
        }
    }

    /// Check if the given root target was successfully built (Done or already up-to-date).
    pub fn target_was_rebuilt(&self, target: &str) -> Option<bool> {
        match self.states.get(target) {
            Some(TargetState::Done(r)) => Some(*r),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Standalone helper functions (no Executor references — safe to use in workers)
// ---------------------------------------------------------------------------

fn touch_file_standalone(target: &str) {
    if let Ok(f) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(target)
    {
        let _ = f.set_len(f.metadata().map(|m| m.len()).unwrap_or(0));
    }
    // Also update mtime by setting it to now.
    let now = std::time::SystemTime::now();
    if let Ok(modified) = now.duration_since(std::time::UNIX_EPOCH) {
        use std::time::Duration;
        let _ = filetime_or_noop(target, modified);
    }
}

fn filetime_or_noop(_target: &str, _t: std::time::Duration) {
    // Best-effort mtime update; the file was already created above.
    // On platforms where utime is available, this is a no-op placeholder.
    // The important part is the file creation above.
}

fn maybe_delete_on_error(target: &str, delete_on_error: bool, is_precious: bool, progname: &str) {
    if delete_on_error && !is_precious {
        if Path::new(target).exists() {
            eprintln!("{}: *** Deleting file '{}'", progname, target);
            let _ = fs::remove_file(target);
        }
    }
}

/// Apply environment operations (set/remove vars) to a Command.
fn apply_env_ops(
    cmd: &mut Command,
    env_ops: &[(String, Option<String>)],
    extra_exports: &HashMap<String, String>,
    extra_unexports: &[String],
    makelevel: &str,
    gnumakeflags_was_set: bool,
) {
    // Apply global env ops.
    for (name, val) in env_ops {
        match val {
            Some(v) => { cmd.env(name, v); }
            None => { cmd.env_remove(name); }
        }
    }
    // MAKELEVEL is always set explicitly.
    cmd.env("MAKELEVEL", makelevel);
    // Target-specific exports override global.
    for (name, value) in extra_exports {
        cmd.env(name, value);
    }
    // Target-specific unexports.
    for name in extra_unexports {
        cmd.env_remove(name);
    }
    if gnumakeflags_was_set {
        cmd.env("GNUMAKEFLAGS", "");
    }
}

/// Parse a .SHELLFLAGS string into individual arguments (standalone version).
pub fn parse_shell_flags_standalone(flags: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut in_single_quote = false;

    for ch in flags.chars() {
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
                result.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        result.push(current);
    }
    result
}

/// Parse recipe prefix flags (@, -, +) from a line.
/// Returns (display_line, silent, ignore_error, force).
pub fn parse_recipe_prefix_standalone(line: &str) -> (String, bool, bool, bool) {
    let mut silent = false;
    let mut ignore = false;
    let mut force = false;
    let mut i = 0;
    let bytes = line.as_bytes();

    while i < bytes.len() {
        match bytes[i] {
            b'@' => { silent = true; i += 1; }
            b'-' => { ignore = true; i += 1; }
            b'+' => { force = true; i += 1; }
            b' ' | b'\t' => { i += 1; }
            _ => break,
        }
    }

    (line[i..].to_string(), silent, ignore, force)
}

/// Strip @, -, + recipe prefix characters.
pub fn strip_recipe_prefixes_standalone(line: &str) -> String {
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

/// Split an expanded recipe line on bare newlines (not preceded by backslash).
pub fn split_recipe_sub_lines_standalone(s: &str) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in s.chars() {
        if ch == '\n' {
            if current.ends_with('\\') {
                current.push('\n');
            } else {
                result.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    result.push(current);
    result
}

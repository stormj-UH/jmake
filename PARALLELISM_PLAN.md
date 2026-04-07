# Parallelism (-j) Implementation Plan for jmake

## Current State

The `-j` flag is parsed by `src/cli.rs` and stored in `MakeArgs::jobs` (default 1). The
`Executor` struct receives the value but never uses it. All recipe execution in
`src/exec/mod.rs` is sequential: `build_target()` recursively builds each prerequisite
in order, then runs the recipe. The `built` HashMap tracks completed targets; `building`
HashSet detects cycles.

Additionally, `build_makeflags()` in `src/eval/mod.rs` does NOT emit `-jN` into the
`MAKEFLAGS` variable, which means sub-makes do not inherit the parallelism level.

The `MakeDatabase` already has `not_parallel: bool` (set when `.NOTPARALLEL` is seen)
and `.WAIT` is recognized as `SpecialTarget::Wait`. However `.WAIT` markers are stripped
early and never used for synchronization.

12 of 13 parallelism tests fail because all recipes execute sequentially regardless of
`-j` value.

---

## Architecture Decision: std::thread + channel-based scheduler

Use a dedicated **scheduler thread** with a pool of **worker threads** (`std::thread`).
Do NOT use rayon (its work-stealing model is a poor fit for process-spawning jobs) or
async (unnecessary complexity for CPU-bound-process spawning). The design:

```
                         +-----------------------+
                         |   Scheduler (main)    |
                         |                       |
                         |  - Dependency graph   |
                         |  - Ready queue        |
                         |  - Job slot counter   |
                         +-----------+-----------+
                                     |
               +---------------------+---------------------+
               |                     |                     |
        +------v------+      +------v------+      +------v------+
        | Worker T1   |      | Worker T2   |      | Worker TN   |
        | spawn shell |      | spawn shell |      | spawn shell |
        +------+------+      +------+------+      +------+------+
               |                     |                     |
               +---------------------+---------------------+
                                     |
                         +-----------v-----------+
                         | Completion channel    |
                         | (target, result)      |
                         +-----------------------+
```

The scheduler runs in the main thread. When `jobs == 1` (or `.NOTPARALLEL`), the
current sequential `Executor` code path is used unchanged -- no threads are spawned.
This is critical for correctness: the entire parallel code path is **only** activated
when `jobs > 1`.

---

## Phase 1: Basic Parallel Execution (Thread Pool, No Jobserver)

### 1.1 New File: `src/exec/parallel.rs`

Create a new module alongside the existing `src/exec/mod.rs`. The sequential executor
remains untouched; the parallel executor wraps it.

### 1.2 Data Structures

```rust
// src/exec/parallel.rs

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, Condvar};
use std::sync::mpsc;
use std::thread;

/// Represents the state of a target in the parallel build
#[derive(Debug, Clone, PartialEq)]
enum TargetState {
    /// Not yet visited
    Unknown,
    /// Dependencies are being resolved (cycle detection)
    Resolving,
    /// Waiting for prerequisites to complete
    Waiting,
    /// Ready to execute (all prereqs done)
    Ready,
    /// Currently executing in a worker thread
    Running,
    /// Successfully completed; bool = was_rebuilt
    Done(bool),
    /// Failed with error
    Failed(String),
}

/// A unit of work to be executed by a worker thread.
/// Contains everything needed to run a recipe WITHOUT referencing the
/// Executor (which is not Send).
struct Job {
    target: String,
    /// The shell command lines to execute (pre-expanded recipe lines).
    /// Each entry: (line_number, raw_line_text, sub_lines_after_expansion)
    recipe_lines: Vec<(usize, String, Vec<String>)>,
    source_file: String,
    auto_vars: HashMap<String, String>,
    is_phony: bool,
    /// Shell program path
    shell: String,
    /// Shell flags string
    shell_flags: String,
    /// Environment variables to set on the child process
    env_vars: HashMap<String, String>,
    /// Environment variables to remove from the child process
    env_remove: Vec<String>,
    /// MAKELEVEL value for the child
    makelevel: String,
    /// Executor settings needed for recipe execution
    silent: bool,
    ignore_errors: bool,
    dry_run: bool,
    touch: bool,
    trace: bool,
    one_shell: bool,
    is_silent_target: bool,
    is_precious: bool,
    progname: String,
}

/// Result of executing a Job
struct JobResult {
    target: String,
    /// true if the target was rebuilt (recipe ran), false otherwise
    rebuilt: bool,
    /// None = success, Some(msg) = failure
    error: Option<String>,
    /// Captured stdout lines (for --output-sync)
    stdout: Vec<String>,
    /// Captured stderr lines (for --output-sync)
    stderr: Vec<String>,
}

/// The parallel build scheduler
struct ParallelScheduler {
    /// Number of job slots
    max_jobs: usize,
    /// Current state of every target
    target_states: HashMap<String, TargetState>,
    /// For each target, the set of prerequisites that must complete first
    prereqs_of: HashMap<String, Vec<String>>,
    /// Reverse dependency map: for each target, the set of targets waiting on it
    dependents_of: HashMap<String, Vec<String>>,
    /// Targets that are ready to execute (all prereqs done)
    ready_queue: VecDeque<String>,
    /// Number of currently running jobs
    running_count: usize,
    /// Channel to send jobs to workers
    job_sender: mpsc::Sender<Job>,
    /// Channel to receive results from workers
    result_receiver: mpsc::Receiver<JobResult>,
    /// Whether any error has occurred (for early termination without -k)
    has_error: bool,
    /// Keep going on errors
    keep_going: bool,
    /// Errors collected in keep-going mode
    errors: Vec<String>,
}
```

### 1.3 Dependency Graph Construction

Before launching parallel execution, we must walk the dependency graph to discover
all targets and their prerequisites. This is done **sequentially** in the scheduler
thread, producing a DAG.

```rust
impl ParallelScheduler {
    /// Walk the dependency graph starting from `roots`, resolving implicit rules,
    /// second expansion, etc. Populates prereqs_of, dependents_of, and
    /// target_states. Returns the set of all targets.
    ///
    /// This is the "planning" phase -- no recipes are executed.
    fn resolve_graph(
        &mut self,
        roots: &[String],
        db: &MakeDatabase,
        state: &MakeState,
        // ... other Executor-like params for needs_rebuild, find_pattern_rule, etc.
    ) -> Result<(), String>;
}
```

**Key insight**: The current `build_target_inner()` interleaves graph resolution with
recipe execution. For parallelism, these must be separated:

1. **Graph resolution phase** (sequential): Walk targets, resolve implicit rules,
   perform second expansion, determine which targets need rebuilding. Record the
   prerequisite relationships.

2. **Execution phase** (parallel): Feed ready targets to workers, process completions,
   enqueue newly-ready targets.

The graph resolution phase needs to reuse much of the logic from `build_target_inner()`,
`build_with_rules()`, `build_with_pattern_rule()`, and `find_pattern_rule()`. The
cleanest approach is to add a `plan_only: bool` parameter or create a separate
`resolve_target()` method that does everything `build_target_inner()` does EXCEPT call
`execute_recipe()`. Instead it records the recipe, auto_vars, and prerequisite list
for later parallel execution.

#### Proposed approach: `TargetPlan` struct

```rust
/// Everything needed to execute a target's recipe, computed during graph resolution.
struct TargetPlan {
    target: String,
    prerequisites: Vec<String>,       // normal prereqs (must complete before recipe)
    order_only_prereqs: Vec<String>,   // order-only (must complete, don't trigger rebuild)
    recipe: Vec<(usize, String)>,      // recipe lines
    source_file: String,
    auto_vars: HashMap<String, String>,
    is_phony: bool,
    needs_rebuild: bool,               // determined during resolution
    grouped_siblings: Vec<String>,     // other targets built by same recipe
    // target-specific variable exports
    extra_exports: HashMap<String, String>,
    extra_unexports: HashSet<String>,
}
```

Add a method to `Executor`:

```rust
impl<'a> Executor<'a> {
    /// Resolve the dependency graph for `targets` without executing any recipes.
    /// Returns a map from target name to its TargetPlan.
    /// Targets that don't need rebuilding are still present (with needs_rebuild=false)
    /// so that dependents can check completion.
    pub fn resolve_graph(&mut self, targets: &[String]) -> Result<HashMap<String, TargetPlan>, String>;
}
```

### 1.4 Worker Thread Pool

```rust
fn spawn_workers(
    num_workers: usize,
    job_receiver: Arc<Mutex<mpsc::Receiver<Job>>>,
    result_sender: mpsc::Sender<JobResult>,
) -> Vec<thread::JoinHandle<()>> {
    (0..num_workers).map(|_| {
        let rx = Arc::clone(&job_receiver);
        let tx = result_sender.clone();
        thread::spawn(move || {
            loop {
                let job = {
                    let receiver = rx.lock().unwrap();
                    match receiver.recv() {
                        Ok(job) => job,
                        Err(_) => return, // channel closed, exit
                    }
                };
                let result = execute_job(job);
                let _ = tx.send(result);
            }
        })
    }).collect()
}
```

The `execute_job()` function is a **standalone function** (not a method on Executor)
that takes a `Job` struct and returns a `JobResult`. It contains the shell-spawning
logic currently in `execute_recipe()`, extracted to be `Send`-safe (no references to
Executor, MakeState, or MakeDatabase).

```rust
/// Execute a single job in a worker thread. Self-contained: all data needed
/// is in the Job struct. Returns the result.
fn execute_job(job: Job) -> JobResult;
```

### 1.5 Scheduler Main Loop

```rust
impl ParallelScheduler {
    fn run(&mut self) -> Result<(), String> {
        // Enqueue all initially-ready targets (those with no unfinished prereqs)
        self.find_ready_targets();

        loop {
            // If no targets are running and no targets are ready, we're done
            // (or deadlocked, which is a bug)
            if self.running_count == 0 && self.ready_queue.is_empty() {
                break;
            }

            // Launch jobs up to the max_jobs limit
            while self.running_count < self.max_jobs && !self.ready_queue.is_empty() {
                if self.has_error && !self.keep_going {
                    break; // stop launching new jobs on error
                }
                let target = self.ready_queue.pop_front().unwrap();
                self.launch_job(&target);
                self.running_count += 1;
            }

            // Wait for a job to complete
            match self.result_receiver.recv() {
                Ok(result) => self.handle_completion(result),
                Err(_) => break, // all workers exited
            }
        }

        // Wait for all in-flight jobs to finish (for error case with -k)
        // ...

        if self.has_error {
            Err(self.errors.join("\n"))
        } else {
            Ok(())
        }
    }

    fn handle_completion(&mut self, result: JobResult) {
        self.running_count -= 1;
        let target = &result.target;

        if let Some(err) = result.error {
            self.target_states.insert(target.clone(), TargetState::Failed(err.clone()));
            self.has_error = true;
            if self.keep_going {
                self.errors.push(err);
            }
        } else {
            self.target_states.insert(target.clone(), TargetState::Done(result.rebuilt));
        }

        // Flush output (for --output-sync, print captured stdout/stderr now)
        // ...

        // Check if any dependents are now ready
        if let Some(deps) = self.dependents_of.get(target) {
            for dep in deps.clone() {
                if self.all_prereqs_done(&dep) {
                    self.target_states.insert(dep.clone(), TargetState::Ready);
                    self.ready_queue.push_back(dep);
                }
            }
        }
    }

    fn all_prereqs_done(&self, target: &str) -> bool {
        if let Some(prereqs) = self.prereqs_of.get(target) {
            prereqs.iter().all(|p| {
                matches!(
                    self.target_states.get(p),
                    Some(TargetState::Done(_))
                )
            })
        } else {
            true
        }
    }
}
```

### 1.6 Integration Point: `Executor::build_targets()`

Modify `build_targets()` in `src/exec/mod.rs` to dispatch:

```rust
pub fn build_targets(&mut self, targets: &[String]) -> Result<(), String> {
    if self.jobs <= 1 || self.db.not_parallel {
        // Existing sequential code path, unchanged
        return self.build_targets_sequential(targets);
    }
    // Parallel path
    self.build_targets_parallel(targets)
}
```

The current `build_targets()` body becomes `build_targets_sequential()`. The new
`build_targets_parallel()` method:

1. Calls `resolve_graph()` to build the DAG and TargetPlans
2. Creates the ParallelScheduler
3. Spawns worker threads
4. Runs the scheduler loop
5. Joins worker threads
6. Handles "is up to date" / "Nothing to be done" messages for top-level targets
7. Deletes intermediate files

### 1.7 MAKEFLAGS: Include -jN

In `src/eval/mod.rs`, `build_makeflags_from_args()`, add `-jN` to the long options:

```rust
// After the include_dirs, load_average, output_sync sections:
if self.args.jobs > 1 {
    long_parts.push(format!("-j{}", self.args.jobs));
}
```

GNU Make includes `-j` in MAKEFLAGS with the jobserver file descriptors
(`--jobserver-auth=R,W`). For Phase 1, simply passing `-jN` is sufficient and matches
what the tests expect.

### 1.8 Extracting `execute_job()` from `execute_recipe()`

The existing `execute_recipe()` method (line 2249) is tightly coupled to `&mut self`.
For parallel execution, we need a standalone version. Steps:

1. Extract the shell-spawning logic into a free function `run_shell_command()`:
   ```rust
   fn run_shell_command(
       shell: &str,
       shell_flags: &str,
       cmd: &str,
       env_vars: &HashMap<String, String>,
       env_remove: &[String],
       makelevel: &str,
   ) -> Result<std::process::ExitStatus, std::io::Error>;
   ```

2. Build `execute_job()` on top of `run_shell_command()`, handling the recipe line
   iteration, prefix parsing, echo/silent logic, error handling, and
   `.ONESHELL` mode.

3. The sequential `execute_recipe()` continues to work as before (no breakage).

### 1.9 needs_rebuild in the Graph Resolution Phase

The `needs_rebuild()` check currently depends on `self.built` (which tracks whether
a prerequisite was actually rebuilt). In the parallel model, during graph resolution
we don't yet know if a prerequisite will be rebuilt. Two approaches:

**Conservative (recommended for Phase 1):** During graph resolution, mark every target
with a non-empty recipe and at least one prerequisite as "potentially needs rebuild"
(`needs_rebuild = true`). Then during execution, the worker checks the actual file
timestamps and skips execution if the target is up-to-date. This may launch some
unnecessary jobs but is safe.

**Accurate (deferred):** During the execution phase, after a prerequisite completes,
re-evaluate `needs_rebuild` for the dependent before launching it. This requires the
scheduler to have access to the mtime-checking logic.

For Phase 1, use the conservative approach. The `TargetPlan::needs_rebuild` field is
computed based on file timestamps and phony status (same logic as current
`needs_rebuild()` but using only file-system state, not `self.built`). During execution,
re-check after all prereqs are done.

### 1.10 Error Handling: Stop on First Error vs -k

When a job fails:

- **Without -k**: Set `has_error = true`, stop launching new jobs, but wait for all
  currently-running jobs to finish. Print "Waiting for unfinished jobs..." (matches
  GNU Make behavior from test #7). Then return the error.

- **With -k**: Record the error, continue launching jobs for targets that don't depend
  on the failed target. Targets that depend on the failed target are marked as
  `Failed` without being launched.

```rust
fn handle_completion(&mut self, result: JobResult) {
    // ...
    if result.error.is_some() && !self.keep_going {
        // Print "Waiting for unfinished jobs...." if there are still running jobs
        if self.running_count > 0 {
            eprintln!("{}: *** Waiting for unfinished jobs....", self.progname);
        }
        // Don't launch new jobs, but keep draining the result channel
        self.draining = true;
    }
}
```

---

## Phase 2: Output Synchronization (--output-sync)

### 2.1 Output Capture

When `--output-sync` is specified (or `-O`), each job's stdout and stderr must be
captured and printed atomically after the job completes, preventing interleaving.

Modify `execute_job()` to capture output:

```rust
fn execute_job(job: Job) -> JobResult {
    if job.output_sync {
        // Use Command::output() or pipe stdout/stderr to capture
        let child = Command::new(&job.shell)
            .args(...)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        // Read output, wait for exit
        // Return captured stdout/stderr in JobResult
    } else {
        // Direct to inherited stdout/stderr (current behavior)
    }
}
```

### 2.2 Output Sync Modes

GNU Make supports four modes:

- `none` (default): No synchronization (output interleaves)
- `line`: Not commonly used; buffer per line
- `target`: Buffer all output for each target, print when target completes
- `recurse`: Like `target` but also groups recursive make output

For Phase 2, implement `target` mode (the most common and what tests exercise).

### 2.3 Scheduler Output Flushing

In `handle_completion()`, after processing the result, flush the captured output:

```rust
fn handle_completion(&mut self, result: JobResult) {
    // ... state updates ...

    // Flush captured output atomically
    if self.output_sync {
        for line in &result.stdout {
            println!("{}", line);
        }
        for line in &result.stderr {
            eprintln!("{}", line);
        }
    }
}
```

### 2.4 Add `--output-sync` to `Job` struct

```rust
struct Job {
    // ... existing fields ...
    output_sync: bool,  // true if output should be captured
}
```

---

## Phase 3: .NOTPARALLEL, .WAIT Support

### 3.1 .NOTPARALLEL

Already partially implemented: `MakeDatabase::not_parallel` is set to `true` when
`.NOTPARALLEL:` (with no prerequisites) is encountered. The dispatch in
`build_targets()` checks this.

GNU Make 4.4 also supports `.NOTPARALLEL: target1 target2` which disables parallelism
only for the listed targets' prerequisite builds. To support this:

- Check `special_targets[NotParallel]` for the set of target names
- If the set is empty, `.NOTPARALLEL` applies globally (current behavior)
- If non-empty, only those targets' prerequisite graphs are built sequentially

```rust
// In resolve_graph or the scheduler:
fn is_not_parallel(&self, target: &str) -> bool {
    if self.db.not_parallel {
        return true; // global .NOTPARALLEL
    }
    self.db.special_targets
        .get(&SpecialTarget::NotParallel)
        .map_or(false, |set| set.contains(target))
}
```

When a target has `.NOTPARALLEL`, its prerequisites are built sequentially (as if
`-j1` for that subtree). Implementation: when the scheduler is about to launch
prerequisites for a `.NOTPARALLEL` target, it serializes them by adding artificial
dependencies (each prereq depends on the previous one).

### 3.2 .WAIT in Prerequisite Lists

`.WAIT` creates synchronization barriers in prerequisite lists. For example:

```makefile
all: a b .WAIT c d
```

means: build `a` and `b` (possibly in parallel), wait for both to finish, then
build `c` and `d` (possibly in parallel).

**Current code** strips `.WAIT` markers early and discards them. For parallel support,
we need to preserve them during graph resolution.

#### Changes needed:

1. **Stop stripping `.WAIT` markers in `build_with_rules()` and related functions
   during graph resolution.** Instead, use them to split prerequisite lists into
   "waves":

   ```rust
   fn split_into_waves(prereqs: &[String]) -> Vec<Vec<String>> {
       let mut waves: Vec<Vec<String>> = vec![Vec::new()];
       for p in prereqs {
           if p == ".WAIT" {
               waves.push(Vec::new());
           } else {
               waves.last_mut().unwrap().push(p.clone());
           }
       }
       waves
   }
   ```

2. **In the dependency graph**, insert synthetic barrier nodes between waves:

   ```
   For "all: a b .WAIT c d":
     a -> barrier_all_1
     b -> barrier_all_1
     barrier_all_1 -> c
     barrier_all_1 -> d
     c -> all
     d -> all
   ```

   This ensures `c` and `d` don't start until both `a` and `b` are done.

3. **In the TargetPlan**, store prerequisites as waves:

   ```rust
   struct TargetPlan {
       // ...
       prereq_waves: Vec<Vec<String>>,  // each inner vec runs in parallel; waves are sequential
   }
   ```

### 3.3 Preserving .WAIT markers

Modify `build_with_rules_prereqs()` and `build_with_rules_grouped()` to preserve
`.WAIT` markers when `self.jobs > 1`. When `jobs == 1`, continue stripping them
(sequential path is unaffected).

In `resolve_graph()`, when building the dependency edges for a target, process
prerequisites wave-by-wave. Within each wave, edges go directly from prereqs to the
target (or next barrier). Between waves, a barrier dependency is inserted.

---

## Phase 4: Jobserver Protocol for Recursive Make

### 4.1 Background

When make invokes sub-makes via `$(MAKE)`, the sub-make should participate in the
same job pool to avoid oversubscription. GNU Make uses a "jobserver" protocol:

- A pipe (or named pipe on Windows) is created by the top-level make
- Job tokens are written to the pipe (N-1 tokens for -jN; the parent holds one)
- Before starting a job, a sub-make reads a token from the pipe
- After a job finishes, the token is written back
- The pipe FDs are passed via `MAKEFLAGS` as `--jobserver-auth=R,W`

### 4.2 Simplified Approach for Phase 4

For the initial implementation, use a pipe-based jobserver:

```rust
// src/exec/jobserver.rs

use std::os::unix::io::{RawFd, FromRawFd, AsRawFd};
use std::fs::File;
use std::io::{Read, Write};

pub struct Jobserver {
    read_fd: RawFd,
    write_fd: RawFd,
    /// True if this process created the jobserver (top-level make)
    is_owner: bool,
}

impl Jobserver {
    /// Create a new jobserver with `n` tokens (for -jN, create N-1 tokens)
    pub fn new(n: usize) -> std::io::Result<Self>;

    /// Inherit a jobserver from parent make (parse --jobserver-auth=R,W from MAKEFLAGS)
    pub fn from_makeflags(makeflags: &str) -> Option<Self>;

    /// Acquire a job token (blocks until one is available)
    pub fn acquire(&self) -> std::io::Result<()>;

    /// Release a job token
    pub fn release(&self) -> std::io::Result<()>;

    /// Get the --jobserver-auth string for MAKEFLAGS
    pub fn auth_string(&self) -> String;
}
```

### 4.3 Integration with MAKEFLAGS

When the jobserver is active, include `--jobserver-auth=R,W` in MAKEFLAGS instead of
(or in addition to) `-jN`. Sub-makes will detect the jobserver and use it instead of
creating their own.

```rust
// In build_makeflags_from_args():
if let Some(ref js) = self.jobserver {
    long_parts.push(format!("-j{}", self.args.jobs));
    long_parts.push(format!("--jobserver-auth={}", js.auth_string()));
} else if self.args.jobs > 1 {
    long_parts.push(format!("-j{}", self.args.jobs));
}
```

### 4.4 Sub-make Detection

When jmake starts, check MAKEFLAGS for `--jobserver-auth=`. If present:
- Do NOT create a new jobserver
- Inherit the parent's jobserver
- Use `acquire()`/`release()` around each job instead of a local thread pool

### 4.5 FD Management

The pipe FDs must NOT be closed across exec (set `FD_CLOEXEC` only on non-jobserver
FDs). When spawning sub-makes, ensure the jobserver FDs are passed through. For
non-make child processes, the FDs should be close-on-exec to avoid leaking.

```rust
// Before spawning a $(MAKE) child:
// - Do NOT set FD_CLOEXEC on jobserver FDs
// - Include --jobserver-auth=R,W in MAKEFLAGS

// Before spawning a non-$(MAKE) child:
// - Set FD_CLOEXEC on jobserver FDs (or don't pass them)
```

### 4.6 Fallback: Simple -j Pass-through

If the full jobserver protocol is too complex for the initial implementation, a simpler
approach:

- Top-level make creates a pipe jobserver and passes `--jobserver-auth` in MAKEFLAGS
- Sub-makes that see `--jobserver-auth` use it
- Sub-makes that don't see it fall back to `-jN` from MAKEFLAGS

This is what the tests actually exercise (tests 5 and 12-13 involve recursive make
with `-j`). The jobserver ensures the total number of concurrent jobs across all
sub-makes stays within the `-jN` limit.

---

## Phase 5: Deferred Needs-Rebuild Check (Accuracy Improvement)

After Phase 1 is working, improve the `needs_rebuild` evaluation:

Instead of computing `needs_rebuild` during graph resolution (when prerequisite rebuild
status is unknown), compute it **in the scheduler** after all prerequisites are done
and before launching the job:

```rust
fn should_launch_job(&self, target: &str, plan: &TargetPlan) -> bool {
    if plan.is_phony || plan.always_make {
        return true;
    }
    // Check if any prereq was rebuilt
    let any_rebuilt = plan.prerequisites.iter().any(|p| {
        matches!(self.target_states.get(p), Some(TargetState::Done(true)))
    });
    if any_rebuilt {
        return true;
    }
    // Check file timestamps
    self.needs_rebuild_by_mtime(&plan.target, &plan.prerequisites)
}
```

If `should_launch_job()` returns false, the target is immediately marked as
`Done(false)` without launching a worker, and its dependents are checked.

---

## Key Implementation Details

### Thread Safety: What is Send/Sync

- `MakeDatabase` and `MakeState` are NOT `Send` (contain `RefCell`, `IndexMap`, etc.)
- The `Job` struct MUST be `Send` -- it contains only owned data (`String`, `HashMap`,
  `Vec`)
- All `Executor` methods that touch `MakeState` (for variable expansion) must run in
  the main thread during graph resolution
- Worker threads only run `execute_job()` which spawns shell processes

### Avoiding Deadlocks

1. The scheduler loop is single-threaded (no locks needed for graph state)
2. Workers communicate only via channels (no shared mutable state)
3. The only shared state is the job channel (`mpsc` is designed for this)
4. Cycle detection during graph resolution prevents circular dependencies

### Output Ordering Without --output-sync

When `--output-sync` is NOT specified, output from parallel jobs is naturally
interleaved (each shell process inherits stdout/stderr). This matches GNU Make
behavior and is what the parallelism tests expect.

### Grouped Targets (&:)

A grouped target rule `a b c &: prereqs; recipe` means the recipe runs once and
produces all three targets. In the parallel scheduler:

1. During graph resolution, all grouped siblings share the same `TargetPlan`
2. When any one of the group is ready, the recipe runs once
3. All siblings are marked as `Done` when the recipe completes
4. If another target depends on a sibling, it sees the sibling as Done

Implementation: use a `grouped_primary: Option<String>` field in `TargetPlan`. If
set, this target is a sibling whose recipe is run by the primary. The scheduler
only launches the primary's job; siblings are auto-completed when the primary finishes.

### Signal Handling

The existing signal handler in `src/signal_handler.rs` should be checked. When a
parallel make receives SIGINT:
1. Kill all running child processes
2. Wait for workers to finish
3. Clean up (delete targets being built, unless `.PRECIOUS`)
4. Exit with signal status

---

## File-by-File Change Summary

### New files:
- `src/exec/parallel.rs` -- ParallelScheduler, Job, JobResult, execute_job(), worker pool
- `src/exec/jobserver.rs` (Phase 4) -- Jobserver pipe protocol

### Modified files:
- `src/exec/mod.rs`:
  - Add `mod parallel;` and `mod jobserver;`
  - Add `build_targets_parallel()` method to Executor
  - Add `resolve_graph()` method to Executor
  - Rename current `build_targets()` body to `build_targets_sequential()`
  - Add dispatch in `build_targets()` based on `self.jobs`
  - Extract `run_shell_command()` as a standalone function
  - Add `TargetPlan` struct

- `src/eval/mod.rs`:
  - In `build_makeflags_from_args()`: add `-jN` to long_parts when jobs > 1
  - In `build_makeflags_from_args()`: add `--jobserver-auth=R,W` when jobserver active (Phase 4)

- `src/cli.rs`:
  - Add parsing for `--jobserver-auth=R,W` (Phase 4)
  - Handle `-j` without a number (means unlimited jobs; treat as large number or thread-per-target)

- `src/types.rs`:
  - No structural changes needed for Phase 1
  - Phase 3: may need to store `.WAIT` positions in Rule.prerequisites differently

### Files NOT modified:
- `src/parser/` -- parsing is unchanged
- `src/functions/` -- function expansion is unchanged
- `src/database.rs` -- re-export is unchanged

---

## Testing Strategy

### Phase 1 validation:
Run the parallelism test suite:
```
docker exec jmake-build bash -c "cd /tmp/make-4.4.1 && ./jmake-test.sh features/parallelism"
```

Key tests and what they exercise:
1. Tests 1-3: Basic -j4 with file/wait synchronization helpers
2. Test 4: Parallel included file building
3. Test 5: Recursive make with jobserver
4. Test 6: Exported variables with $(shell) in parallel
5. Test 7: Error handling -- stop without -k, wait for running jobs
6. Test 8: Two failing parallel jobs
7. Test 9: Intermediate/phony targets with -j
8. Test 10: MAKEFLAGS with -jN causing parallel execution
9. Test 11: Intermediate/secondary file handling with -j2
10. Test 12: Jobserver preservation across re-exec
11. Test 13: Sub-make re-exec with jobserver

### Manual smoke tests:
- `make -j4` on a real project with independent targets
- Verify `-j1` behavior is identical to current (no regressions)
- Verify `.NOTPARALLEL:` disables parallelism
- Verify `-k` with parallel failures

---

## Implementation Order for Sonnet Workers

### Worker A: Phase 1a -- MAKEFLAGS and infrastructure
1. Add `-jN` to `build_makeflags_from_args()` in `src/eval/mod.rs`
2. Create `src/exec/parallel.rs` with `Job`, `JobResult`, `TargetPlan` structs
3. Extract `run_shell_command()` from `execute_recipe()` as a standalone function
4. Implement `execute_job()` in `parallel.rs`
5. Write unit tests for `execute_job()` and `run_shell_command()`

### Worker B: Phase 1b -- Graph resolution
1. Add `resolve_graph()` method to Executor (or a new `GraphResolver` struct)
2. Refactor `build_target_inner()` to separate "plan" from "execute" logic
3. Handle implicit rule resolution in the planning phase
4. Handle second expansion in the planning phase
5. Handle grouped targets in the planning phase
6. Write tests for graph resolution on sample Makefiles

### Worker C: Phase 1c -- Scheduler and integration
1. Implement `ParallelScheduler` with ready queue and main loop
2. Implement worker thread pool with channel communication
3. Add `build_targets_parallel()` to Executor
4. Add dispatch in `build_targets()` based on jobs count
5. Handle error reporting ("Waiting for unfinished jobs...")
6. Handle "is up to date" / "Nothing to be done" messages
7. Integration test with the full test suite

### Worker D: Phases 2-3
1. Implement output capture in `execute_job()` for `--output-sync`
2. Implement `.WAIT` preservation and wave-based scheduling
3. Implement per-target `.NOTPARALLEL`

### Worker E: Phase 4
1. Implement `src/exec/jobserver.rs` (pipe-based jobserver)
2. Parse `--jobserver-auth` from MAKEFLAGS
3. Integrate jobserver with scheduler (acquire/release around jobs)
4. Pass jobserver FDs to sub-makes
5. Test recursive make scenarios

---

## Risk Areas and Mitigations

1. **Graph resolution may miss dynamic dependencies**: Some targets are only discovered
   during recipe execution (e.g., generated include files). Mitigation: re-run graph
   resolution after include file rebuilds (existing re-exec mechanism handles this).

2. **Race conditions in file timestamp checks**: Two parallel jobs may check/create
   the same intermediate file. Mitigation: the graph ensures no two jobs write the
   same file (prerequisite ordering prevents this).

3. **$(shell ...) during graph resolution**: Second expansion may call `$(shell ...)`.
   This is fine since graph resolution runs in the main thread with access to
   `MakeState`.

4. **Large dependency graphs**: For projects with thousands of targets, the graph
   resolution phase must be efficient. Use `HashMap` with pre-allocated capacity.

5. **SIGINT handling**: Must kill all child processes and clean up. The existing
   signal handler needs to be extended to track PIDs of running children across
   all worker threads.

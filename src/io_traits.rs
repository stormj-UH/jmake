// (c) 2026 Jon-Erik G. Storm, Inc., a California Corporation,
// doing business as LAVA GOAT SOFTWARE. All rights reserved.
// SPDX-License-Identifier: MIT

//! Boundary traits that isolate the pure build-logic core from OS side-effects.
//!
//! The build core (eval, exec, expand, functions, parser) must never reach
//! directly into the OS.  Instead it calls through these traits, which are
//! injected at construction time.  This makes the boundary testable and
//! auditable: a test double can implement `MakeFs` + `MakeShell` and exercise
//! the core without touching the real filesystem or spawning real processes.
//!
//! # Production path
//!
//! [`RealFs`] and [`RealShell`] delegate to `std::fs` and `std::process::Command`
//! exactly as the code did before this boundary was introduced.  Wiring them in
//! (see `exec/mod.rs` and `main.rs`) is zero-cost at run time.
//!
//! # Boundary rules
//!
//! The core MUST NOT:
//! - call `std::fs::metadata`, `std::fs::read_to_string`, `std::fs::remove_file`,
//!   `glob::glob`, or any other filesystem function directly.
//! - call `std::process::Command::new` for recipe execution or `$(shell …)` capture.
//! - read environment variables after `MakeState::new` is called (env is captured
//!   once into `EnvConfig` — see `eval::env`).
//! - call `std::time::SystemTime::now` for mtime purposes (go through `MakeFs`).
//!
//! Everything else (string manipulation, HashMap/HashSet operations, integer
//! arithmetic, panic-safety bookkeeping) is pure and may remain inline.

use std::io;
use std::path::Path;
use std::time::SystemTime;

// ---------------------------------------------------------------------------
// MakeFs — filesystem operations needed by the build system
// ---------------------------------------------------------------------------

/// Filesystem operations required by the build core.
///
/// Every call that touches the real on-disk filesystem from within the
/// executor or evaluator must go through this trait so that the boundary
/// is explicit and injectable.
// #[allow(dead_code)] on the trait itself covers trait methods not yet wired at
// all call sites.  This is the expected state during incremental migration.
#[allow(dead_code)]
pub trait MakeFs {
    /// Returns `true` if `path` exists (regular file, directory, or symlink).
    fn file_exists(&self, path: &Path) -> bool;

    /// Returns the modification time of `path`, or an error if the file does
    /// not exist or its mtime cannot be read.
    fn file_mtime(&self, path: &Path) -> io::Result<SystemTime>;

    /// Returns the modification time of `path` for symlink-aware builds (`-L`).
    ///
    /// For a symlink, returns the maximum mtime between the symlink itself and
    /// its target (recursively), matching GNU Make's `-L` behaviour.  For
    /// dangling symlinks returns the symlink's own mtime.  For non-symlinks
    /// behaves identically to [`MakeFs::file_mtime`].
    fn file_mtime_symlink(&self, path: &Path) -> io::Result<SystemTime>;

    /// Read the entire contents of `path` into a `String`.
    ///
    /// TODO(pure-core): wire once `eval/mod.rs` std::fs::read_to_string calls are migrated.
    fn read_to_string(&self, path: &Path) -> io::Result<String>;

    /// Expand `pattern` against the filesystem and return all matching paths.
    ///
    /// Returns an empty `Vec` when the pattern matches nothing (the caller
    /// is responsible for the "keep literal" fallback where GNU Make requires it).
    fn glob(&self, pattern: &str) -> Vec<String>;

    /// Delete `path`.  Errors are silently ignored by most callers (matching
    /// GNU Make behaviour), but the `Result` is returned for those that care.
    fn remove_file(&self, path: &Path) -> io::Result<()>;
}

// ---------------------------------------------------------------------------
// MakeShell — shell execution needed by recipes
// ---------------------------------------------------------------------------

/// Shell execution required by the build core.
///
/// Recipes and `$(shell …)` calls go through this trait so that both can be
/// substituted with test doubles without touching real processes.
// #[allow(dead_code)] on the trait itself covers trait methods not yet wired at
// all call sites.  This is the expected state during incremental migration.
#[allow(dead_code)]
pub trait MakeShell {
    /// Run an already-expanded command through the shell.
    ///
    /// `shell`  — the shell binary (e.g. `/bin/sh`).
    /// `flags`  — zero or more pre-split `.SHELLFLAGS` tokens (e.g. `["-e", "-c"]`).
    ///            The implementation appends `script` as the final argument after these
    ///            flags, matching GNU Make's argument layout.
    /// `script` — the command string to run (passed as a single argument).
    /// `env`    — extra `(name, value)` pairs to inject into the child's environment
    ///            (in addition to the process's inherited environment).
    ///
    /// Returns the process exit code as `Ok(code)`, or `Err` on spawn/wait failure.
    fn run_recipe_line(
        &self,
        shell: &str,
        flags: &[String],
        script: &str,
        env: &[(String, String)],
    ) -> io::Result<i32>;

    /// Run `line` through `shell -c line` and capture its stdout as a `String`.
    ///
    /// Used for `$(shell …)` expansion.  Trailing newlines are NOT stripped
    /// here; the caller is responsible for normalisation.
    fn capture_output(&self, line: &str, shell: &str) -> io::Result<String>;
}

// ---------------------------------------------------------------------------
// RealFs — production implementation backed by std::fs + glob
// ---------------------------------------------------------------------------

/// Production [`MakeFs`] that delegates to `std::fs` and the `glob` crate.
pub struct RealFs;

impl MakeFs for RealFs {
    fn file_exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn file_mtime(&self, path: &Path) -> io::Result<SystemTime> {
        std::fs::metadata(path)?.modified()
    }

    fn file_mtime_symlink(&self, path: &Path) -> io::Result<SystemTime> {
        real_mtime_symlink(path)
    }

    // TODO(pure-core): called once eval/mod.rs read_to_string calls are migrated.
    #[allow(dead_code)]
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn glob(&self, pattern: &str) -> Vec<String> {
        match ::glob::glob(pattern) {
            Ok(paths) => {
                let mut out = Vec::new();
                for entry in paths.flatten() {
                    if let Some(s) = entry.to_str() {
                        out.push(s.to_owned());
                    }
                }
                out
            }
            Err(_) => Vec::new(),
        }
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
}

/// Walk a (possibly symlinked) path and return the maximum mtime across
/// the symlink chain.  Mirrors the logic previously inlined in `exec/mod.rs`.
fn real_mtime_symlink(path: &Path) -> io::Result<SystemTime> {
    let meta = std::fs::symlink_metadata(path)?;
    let own_mtime = meta.modified()?;

    if !meta.file_type().is_symlink() {
        return Ok(own_mtime);
    }

    let target = std::fs::read_link(path)?;
    let resolved = if target.is_absolute() {
        target
    } else if let Some(parent) = path.parent() {
        parent.join(&target)
    } else {
        target
    };

    // Recursively follow the chain; ignore errors from dangling symlinks.
    match real_mtime_symlink(&resolved) {
        Ok(target_mtime) => Ok(own_mtime.max(target_mtime)),
        Err(_) => Ok(own_mtime), // dangling symlink — use the symlink's own mtime
    }
}

// ---------------------------------------------------------------------------
// RealShell — production implementation backed by std::process::Command
// ---------------------------------------------------------------------------

/// Production [`MakeShell`] that spawns real child processes.
pub struct RealShell;

impl MakeShell for RealShell {
    // TODO(pure-core): called once exec/mod.rs Command::new recipe-execution sites are migrated.
    #[allow(dead_code)]
    fn run_recipe_line(
        &self,
        shell: &str,
        flags: &[String],
        script: &str,
        env: &[(String, String)],
    ) -> io::Result<i32> {
        use std::process::{Command, Stdio};

        let mut cmd = Command::new(shell);
        for flag in flags {
            cmd.arg(flag);
        }
        cmd.arg(script);
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.stderr(Stdio::inherit());
        let status = cmd.spawn()?.wait()?;
        Ok(status.code().unwrap_or(1))
    }

    // TODO(pure-core): called once eval/mod.rs $(shell …) invocations are migrated.
    #[allow(dead_code)]
    fn capture_output(&self, line: &str, shell: &str) -> io::Result<String> {
        use std::process::Command;

        let out = Command::new(shell)
            .arg("-c")
            .arg(line)
            .output()?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

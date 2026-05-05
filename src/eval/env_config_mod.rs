// (c) 2026 Jon-Erik G. Storm, Inc., a California Corporation,
// doing business as LAVA GOAT SOFTWARE. All rights reserved.
// SPDX-License-Identifier: MIT

//! Environment variable initialization — captured once at startup.
//!
//! # Motivation
//!
//! The pure-core boundary requires that env var reads are not scattered throughout
//! the build logic.  Instead:
//!
//! 1. `EnvConfig::from_process_env()` is called **once** in `main`, before
//!    `MakeState::new`.  It snapshots everything jmake needs from the process
//!    environment into owned `String` fields.
//! 2. The resulting `EnvConfig` is passed into `MakeState::new` and stored on
//!    `MakeState`.
//! 3. Every place that previously called `std::env::var("FOO")` inside the core
//!    reads from `state.env_config.foo` instead.
//!
//! This makes the environment a parameter rather than a global, which is both
//! testable and auditable.
//!
//! # Migration status
//!
//! The fields below cover the env vars used most broadly.  Additional vars
//! (e.g. `JMAKE_ENTERING_PRINTED`, `MAKEFILES`) are still read inline at their
//! use sites; those can be migrated to `EnvConfig` in follow-up passes.
//! TODO(pure-core): migrate remaining scattered env::var calls into EnvConfig.

use std::collections::HashMap;

/// Snapshot of the process environment captured once at process startup.
///
/// After `MakeState::new` has been called the core MUST NOT call `std::env::var`
/// or `std::env::vars` — it should read from this struct instead.
#[derive(Clone, Debug)]
pub struct EnvConfig {
    /// Value of `MAKELEVEL` at startup, or `"0"` if unset.
    /// Used to initialise the `MAKELEVEL` variable and build prognames.
    pub makelevel: String,

    /// Value of `MAKE_RESTARTS` at startup, or `""` if unset.
    /// Tracks how many times the top-level make has re-exec'd itself.
    pub make_restarts: String,

    /// True when `JMAKE_TEST_MODE=1` was set at startup.
    /// Enables byte-identical GNU Make 4.4.1 output impersonation.
    pub test_mode: bool,

    /// Value of `TMPDIR` at startup, or `"/tmp"` if unset.
    /// Used to create temp files for `-f-` (stdin makefile) handling.
    pub tmpdir: String,

    /// Value of `PWD` at startup, or `""` if unset.
    /// Used by `logical_cwd()` to return the logical working directory
    /// rather than the canonical path from `getcwd()`.
    pub pwd: String,

    /// Full process environment snapshot as key→value pairs.
    ///
    /// Used by `init_variables` to seed Make variables from the environment.
    /// Storing a snapshot here means the core never needs to call `env::vars()`
    /// again after startup.
    pub all_vars: HashMap<String, String>,
}

impl EnvConfig {
    /// Capture the current process environment into an `EnvConfig`.
    ///
    /// Call this **once**, in `main`, before constructing `MakeState`.
    pub fn from_process_env() -> Self {
        let all_vars: HashMap<String, String> = std::env::vars().collect();

        let makelevel = all_vars
            .get("MAKELEVEL")
            .cloned()
            .unwrap_or_else(|| "0".to_string());

        let make_restarts = all_vars
            .get("MAKE_RESTARTS")
            .cloned()
            .unwrap_or_default();

        let test_mode = all_vars
            .get("JMAKE_TEST_MODE")
            .map(|v| v.trim() == "1")
            .unwrap_or(false);

        let tmpdir = all_vars
            .get("TMPDIR")
            .cloned()
            .unwrap_or_else(|| "/tmp".to_string());

        let pwd = all_vars
            .get("PWD")
            .cloned()
            .unwrap_or_default();

        EnvConfig {
            makelevel,
            make_restarts,
            test_mode,
            tmpdir,
            pwd,
            all_vars,
        }
    }

    /// Convenience: numeric MAKELEVEL (0 if unparseable).
    pub fn makelevel_num(&self) -> u32 {
        self.makelevel.parse().unwrap_or(0)
    }
}

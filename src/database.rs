// (c) 2026 Jon-Erik G. Storm, Inc., a California Corporation,
// doing business as LAVA GOAT SOFTWARE. All rights reserved.
// SPDX-License-Identifier: MIT

//! Rule and variable database for jmake.
//!
//! This module is a thin facade over [`crate::types::MakeDatabase`], re-exporting
//! it under the `database` namespace so that other modules can import it without
//! needing to reach into `types` directly.
//!
//! The actual implementation — fields, methods, and all query helpers such as
//! [`MakeDatabase::is_phony`] and [`MakeDatabase::is_precious`] — lives in
//! [`crate::types`].  This separation keeps the type definitions in one place
//! while giving the database a distinct module identity in the crate hierarchy.
//!
//! # Population
//!
//! [`crate::eval::MakeState`] owns and mutates the database during makefile
//! parsing.  Once parsing is complete the database is treated as read-only
//! by the executor.
//!
//! # Thread safety
//!
//! See [`crate::types::MakeDatabase`] — the database is not `Sync` and is
//! accessed only from the main thread.

pub use crate::types::MakeDatabase;

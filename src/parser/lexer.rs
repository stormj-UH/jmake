// (c) 2026 Jon-Erik G. Storm, Inc., a California Corporation,
// doing business as LAVA GOAT SOFTWARE. All rights reserved.
// SPDX-License-Identifier: MIT

//! Low-level lexer utilities for Makefile parsing.
//!
//! This module is reserved for character-level helpers that sit below the
//! structural parser in `parser/mod.rs`.  Currently it is a stub; the
//! practical tokenisation routines (`split_filenames`, `strip_comment`,
//! `find_semicolon`, etc.) live inline in `parser/mod.rs` because they are
//! tightly coupled to the structural parser's logic.
//!
//! Future refactoring may migrate those helpers here to improve separation of
//! concerns.  Until then this module exists as a named boundary so that
//! imports stay consistent across the codebase.
//!
//! # Thread safety
//!
//! This module contains no mutable state.  All functions (when added) will be
//! pure, stateless, and safe to call from any context.

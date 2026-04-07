// Copyright (c) 2026 Jon-Erik G. Storm. All rights reserved.
// Makefile parser - lexing and parsing of Makefile syntax

mod lexer;
mod directives;

pub use lexer::*;
pub use directives::*;

use crate::types::*;
use crate::eval::MakeState;
use std::path::{Path, PathBuf};
use std::fs;
use std::io::{self, BufRead};

pub struct Parser {
    pub lines: Vec<String>,
    pub pos: usize,
    pub filename: PathBuf,
    pub lineno: usize,
    pub in_recipe: bool,
    pub in_define: bool,
    pub define_name: String,
    pub define_flavor: VarFlavor,
    pub define_override: bool,
    pub define_export: bool,
    pub define_lines: Vec<String>,
    pub define_lineno: usize,  // line number where define started
    /// Nesting depth for nested define/endef blocks within the body.
    /// When in_define and we see another 'define', depth increments.
    /// When we see 'endef', depth decrements; only when depth==0 does the define end.
    pub define_depth: usize,
    pub conditional_stack: Vec<ConditionalState>,
    /// True when `.POSIX:` has been seen; affects backslash-newline collapsing.
    pub posix_mode: bool,
    /// Current `.RECIPEPREFIX` character (None means tab is the prefix).
    /// Used by `next_logical_line` to correctly handle backslash-newline continuation
    /// in recipe lines that use a custom prefix character.
    pub recipe_prefix: Option<char>,
}

#[derive(Debug, Clone)]
pub struct ConditionalState {
    pub active: bool,       // is the current branch active?
    pub seen_true: bool,    // have we seen a true branch yet?
    pub in_else: bool,      // are we in the else branch?
}

impl Parser {
    pub fn new(filename: PathBuf) -> Self {
        Parser {
            lines: Vec::new(),
            pos: 0,
            filename,
            lineno: 0,
            in_recipe: false,
            in_define: false,
            define_name: String::new(),
            define_flavor: VarFlavor::Recursive,
            define_override: false,
            define_export: false,
            define_lines: Vec::new(),
            define_lineno: 0,
            define_depth: 0,
            conditional_stack: Vec::new(),
            posix_mode: false,
            recipe_prefix: None,
        }
    }

    pub fn load_file(&mut self) -> io::Result<()> {
        let content = fs::read_to_string(&self.filename)?;
        // Strip UTF-8 BOM if present
        let content = content.strip_prefix('\u{FEFF}').unwrap_or(&content);
        self.lines = content.lines().map(String::from).collect();
        self.pos = 0;
        self.lineno = 0;
        Ok(())
    }

    pub fn load_string(&mut self, content: &str) {
        // Strip UTF-8 BOM if present
        let content = content.strip_prefix('\u{FEFF}').unwrap_or(content);
        self.lines = content.lines().map(String::from).collect();
        self.pos = 0;
        self.lineno = 0;
    }

    pub fn is_conditionally_active(&self) -> bool {
        self.conditional_stack.iter().all(|c| c.active)
    }

    pub fn next_logical_line(&mut self) -> Option<(String, usize)> {
        if self.pos >= self.lines.len() {
            return None;
        }

        let start_lineno = self.pos + 1;
        let mut line = self.lines[self.pos].clone();
        self.pos += 1;

        // Handle backslash line continuations
        // For recipe lines (tab-prefixed or custom-prefix), preserve the backslash-newline
        // so the shell can handle continuation. For other lines, collapse to a single space.
        // EXCEPTION: For inline recipes (lines of the form "target:;recipe"), once
        // we have accumulated past the `;`, we must also preserve \<newline> so the
        // shell can handle multi-line inline recipes correctly (GNU Make 4.4 behavior).
        while line.ends_with('\\') && self.pos < self.lines.len() {
            // Check if this is a recipe line (tab prefix or custom .RECIPEPREFIX)
            let is_recipe_line = line.starts_with('\t') || self.recipe_prefix
                .map(|p| p != '\t' && line.starts_with(p))
                .unwrap_or(false);
            if is_recipe_line {
                // Recipe line: preserve \<newline> for the shell.
                // GNU Make strips at most ONE leading prefix character from the continuation
                // line (it is the recipe prefix character). Any other leading
                // whitespace (spaces) must be preserved so the shell receives
                // the correct text (e.g., "foo\<NL>    bar" should remain as
                // "foo\<NL>    bar" so the shell can join them correctly).
                line.push('\n');
                let next = &self.lines[self.pos];
                // Strip one leading recipe prefix character from continuation line
                let stripped = if line.starts_with('\t') && next.starts_with('\t') {
                    &next[1..]
                } else if let Some(p) = self.recipe_prefix {
                    if p != '\t' && next.starts_with(p) {
                        &next[p.len_utf8()..]
                    } else {
                        next
                    }
                } else {
                    next   // Preserve leading spaces; only strip one tab above
                };
                line.push_str(stripped);
            } else if line_has_inline_recipe(&line) {
                // Inline recipe (e.g. "target:;@echo fa\<nl>st"): preserve
                // the \<newline> so the shell can join lines itself, mirroring
                // how GNU Make 4.4+ passes inline recipe continuations to the
                // shell without collapsing them.
                // Strip exactly ONE leading tab from the continuation line
                // (the recipe-prefix character), but preserve any other
                // leading whitespace (spaces).
                line.push('\n');
                let next = &self.lines[self.pos];
                let stripped = if next.starts_with('\t') {
                    &next[1..]
                } else {
                    next
                };
                line.push_str(stripped);
            } else {
                line.pop(); // remove backslash
                // $\ (dollar immediately before backslash-newline): GNU Make treats
                // this as a concatenating continuation: the $ and \ are both removed
                // and the next line is trimmed and directly appended (no space).
                if line.ends_with('$') {
                    line.pop(); // remove the preceding $
                    line.push_str(self.lines[self.pos].trim_start());
                } else {
                    if !self.posix_mode {
                        // GNU Make non-POSIX: strip trailing whitespace before the space.
                        let trimmed_len = line.trim_end_matches(|c: char| c == ' ' || c == '\t').len();
                        line.truncate(trimmed_len);
                    }
                    line.push(' ');
                    line.push_str(self.lines[self.pos].trim_start());
                }
            }
            self.pos += 1;
        }

        // Handle the case where the line still ends with '\' at end-of-file.
        // For non-recipe, non-inline-recipe lines, a trailing '\' followed by
        // EOF is treated as a continuation to an empty next line: consume the
        // backslash and append a single space (matching GNU Make's
        // collapse_continuations behaviour where backslash-newline at EOF
        // still triggers the join, just with an empty continuation).
        let is_recipe_at_eof = line.starts_with('\t') || self.recipe_prefix
            .map(|p| p != '\t' && line.starts_with(p))
            .unwrap_or(false);
        if line.ends_with('\\') && !is_recipe_at_eof && !line_has_inline_recipe(&line) {
            line.pop(); // remove the trailing backslash
            if !self.posix_mode {
                let trimmed_len = line.trim_end_matches(|c: char| c == ' ' || c == '\t').len();
                line.truncate(trimmed_len);
            }
            line.push(' ');
            // No continuation content (EOF acts like empty next line)
        }

        self.lineno = start_lineno;
        Some((line, start_lineno))
    }

    pub fn parse_line(&self, line: &str, state: &MakeState) -> ParsedLine {
        let trimmed = line.trim_start();

        // Empty line
        if trimmed.trim_end().is_empty() {
            return ParsedLine::Empty;
        }

        // Comment line
        if trimmed.starts_with('#') {
            return ParsedLine::Comment;
        }

        // Recipe line: check for tab (always a recipe line when in recipe context,
        // and tab-prefixed lines are always recipes per GNU Make semantics)
        // Also support .RECIPEPREFIX for a custom prefix character.
        let recipe_prefix: char = state.db.variables.get(".RECIPEPREFIX")
            .and_then(|v| v.value.chars().next())
            .unwrap_or('\t');

        // A tab-prefixed line is always treated as a recipe line (GNU Make rule).
        // A custom RECIPEPREFIX line is also a recipe line.
        if line.starts_with('\t') || (recipe_prefix != '\t' && line.starts_with(recipe_prefix)) {
            // Strip exactly the one prefix character
            let stripped = &line[recipe_prefix.len_utf8()..];
            return ParsedLine::Recipe(stripped.to_string());
        }

        // Strip inline comments (not in recipe context).
        // For rule lines with an inline recipe (target: ; cmd), only strip
        // comments from the part before the semicolon — the recipe content
        // must not have its '#' characters treated as makefile comments.
        let effective = if let Some(semi_pos) = find_inline_recipe_semi_pos(trimmed) {
            let before_semi = strip_comment(&trimmed[..semi_pos]);
            // Append the semicolon and everything after (the recipe) verbatim.
            // Trailing whitespace after stripping is fine; preserve the recipe exactly.
            format!("{};{}", before_semi.trim_end(), &trimmed[semi_pos+1..])
        } else {
            strip_comment(trimmed)
        };

        // Conditional directives
        if let Some(cond) = parse_conditional(&effective) {
            return cond;
        }

        // endif
        if effective == "endif" || effective.starts_with("endif ") || effective.starts_with("endif\t") {
            return ParsedLine::Endif;
        }

        // else
        if effective == "else" || effective.starts_with("else ") || effective.starts_with("else\t") {
            let rest = effective.strip_prefix("else").unwrap().trim();
            if rest.is_empty() {
                return ParsedLine::Else(None);
            }
            if let Some(ParsedLine::Conditional(kind)) = {
                let parsed = parse_conditional(rest);
                parsed
            } {
                return ParsedLine::Else(Some(kind));
            }
            return ParsedLine::Else(None);
        }

        // undefine directive (must be checked before define)
        // BUT: `undefine:` or `undefine : recipe` is a RULE target named "undefine",
        // not the undefine directive. Check that this is not a rule.
        // Also: `undefine = value` is a variable assignment where `undefine` is the
        // variable name, NOT the undefine directive.
        if effective.starts_with("undefine ") || effective.starts_with("override undefine ") {
            let is_override = effective.starts_with("override ");
            let rest = if is_override {
                effective.strip_prefix("override ").unwrap()
            } else {
                &effective
            };
            let name = rest.strip_prefix("undefine").unwrap().trim().to_string();
            // If the name starts with `:` (including `::` for double-colon rules),
            // this is actually a rule like `undefine: ;recipe`, not a directive.
            // If the name starts with an assignment operator (`=`, `:=`, `+=`, etc.),
            // this is a variable assignment like `undefine = value`, not a directive.
            let starts_with_assign_op = name.starts_with('=')
                || name.starts_with(":=")
                || name.starts_with("::=")
                || name.starts_with(":::=")
                || name.starts_with("+=")
                || name.starts_with("?=")
                || name.starts_with("!=");
            if !name.starts_with(':') && !starts_with_assign_op {
                return ParsedLine::Undefine { name, is_override };
            }
            // Fall through to rule/assignment parsing
        }

        // define directive (also handles "override define" and "export define")
        // BUT: "define = value" or "define := value" etc. is a regular variable assignment
        // where the variable name happens to be "define". If the first token after "define"
        // is itself an assignment operator, treat as a regular variable assignment.
        let is_define_directive = (effective.starts_with("define ") || effective == "define"
            || effective.starts_with("override define ")
            || effective.starts_with("export define "))
            && {
                // After stripping "define" (and optional override/export prefix),
                // check if what remains starts with an assignment operator (no variable name).
                let rest = if effective.starts_with("override define ") {
                    effective.strip_prefix("override define ").unwrap_or("").trim()
                } else if effective.starts_with("export define ") {
                    effective.strip_prefix("export define ").unwrap_or("").trim()
                } else {
                    effective.strip_prefix("define").unwrap_or("").trim()
                };
                // If rest starts with a pure assignment operator, not a define directive
                let assignment_ops = [":::=", "::=", "!=", "?=", "+=", ":=", "="];
                let starts_with_op = assignment_ops.iter().any(|op| {
                    rest.starts_with(op) && (rest.len() == op.len()
                        || rest[op.len()..].starts_with(' ')
                        || rest[op.len()..].starts_with('\t')
                        || rest[op.len()..].starts_with('\n'))
                });
                // If rest (the part after "define") starts with ':' not followed by '=' or ':',
                // it's a rule like "define : recipe", not a define directive.
                // But note: rest here is the part AFTER the "define" keyword AND any variable name.
                // When the full line is "define : recipe", rest after stripping "define" is ": recipe",
                // so we check if the stripped rest starts with a rule colon.
                // However, rest could also be "VAR_NAME" or "VAR_NAME =", so we only trigger this
                // when rest itself starts with ':' (meaning there's no variable name, just a colon).
                let starts_with_rule_colon = rest.starts_with(':')
                    && !rest.starts_with(":=")
                    && !rest.starts_with("::=")
                    && !rest.starts_with(":::=");
                !starts_with_op && !starts_with_rule_colon
            };
        if is_define_directive {
            return parse_define_start(&effective);
        }

        // endef
        if effective == "endef" {
            return ParsedLine::Endef;
        }

        // include / -include / sinclude
        // Handle both "include file" (with space) and bare "include" (no filenames = no-op)
        if effective.starts_with("include ") || effective == "include"
            || effective.starts_with("-include ") || effective == "-include"
            || effective.starts_with("sinclude ") || effective == "sinclude" {
            return parse_include(&effective);
        }

        // vpath directive
        if effective.starts_with("vpath ") || effective == "vpath" {
            return parse_vpath(&effective);
        }

        // export / unexport
        if effective.starts_with("export ") || effective == "export" {
            return parse_export(&effective, true);
        }
        if effective.starts_with("unexport ") || effective == "unexport" {
            return parse_export(&effective, false);
        }

        // Try to parse as variable assignment
        if let Some(va) = try_parse_variable_assignment(&effective) {
            return va;
        }

        // Try to parse as rule
        if let Some(rule) = try_parse_rule(&effective) {
            return rule;
        }

        // Unknown line - missing separator error
        // Check if this looks like a recipe line with spaces instead of a tab
        if line.starts_with("        ") && !line.trim().is_empty() {
            // 8 spaces at the start - did they mean a tab?
            // But only if we're NOT using a custom recipe prefix
            let recipe_prefix = state.db.variables.get(".RECIPEPREFIX")
                .and_then(|v| v.value.chars().next())
                .unwrap_or('\t');
            if recipe_prefix == '\t' {
                return ParsedLine::MissingSeparator(
                    "did you mean TAB instead of 8 spaces?".to_string()
                );
            }
        }
        if !effective.is_empty() {
            // A bare colon (empty target from variable expansion) is silently ignored
            if effective.starts_with(':') || effective.trim() == ":" {
                return ParsedLine::Empty;
            }
            // Check for ifeq/ifneq without whitespace
            if effective.starts_with("ifeq(") || effective.starts_with("ifneq(") {
                return ParsedLine::MissingSeparator(
                    "ifeq/ifneq must be followed by whitespace".to_string()
                );
            }
            return ParsedLine::MissingSeparator(String::new());
        }
        ParsedLine::Empty
    }
}

/// Return true if `line` contains an inline recipe: a bare `;` that is
/// preceded somewhere by a bare `:` (both outside variable references).
/// This is used by `next_logical_line` to decide whether to preserve
/// backslash-newline continuations (shell handles them) vs. collapse them
/// (make handles them).
fn line_has_inline_recipe(line: &str) -> bool {
    find_inline_recipe_semi_pos(line).is_some()
}

/// Returns the byte position of the `;` that begins an inline recipe in a rule line.
/// Returns None if there is no inline recipe.
/// An inline recipe `;` is one that appears after a rule colon (`:`) at $() depth 0,
/// not part of `:=` / `::=` / `:::=` / `::`, and with an unescaped colon.
fn find_inline_recipe_semi_pos(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut depth = 0i32;
    let mut found_colon = false;
    let mut in_assignment = false;  // true once we've seen an assignment op after the colon
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'$' if i + 1 < bytes.len() => {
                match bytes[i + 1] {
                    b'(' | b'{' => { depth += 1; i += 2; continue; }
                    _ => { i += 2; continue; }
                }
            }
            b'(' | b'{' if depth > 0 => depth += 1,
            b')' | b'}' if depth > 0 => depth -= 1,
            b':' if depth == 0 && !in_assignment => {
                // Skip \: (escaped colon)
                if i > 0 && bytes[i - 1] == b'\\' {
                    i += 1;
                    continue;
                }
                // Skip ::= and :=
                if i + 2 < bytes.len() && bytes[i+1] == b':' && bytes[i+2] == b'=' {
                    i += 1; continue;
                }
                if i + 1 < bytes.len() && bytes[i+1] == b'=' {
                    i += 1; continue;
                }
                found_colon = true;
            }
            b'=' if depth == 0 && found_colon && !in_assignment => {
                // We've seen an assignment operator after the rule colon.
                // This means the `;` is part of the value, not an inline recipe.
                in_assignment = true;
            }
            b'+' | b'?' | b'!' if depth == 0 && found_colon && !in_assignment => {
                // Check for +=, ?=, !=
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    in_assignment = true;
                }
            }
            b';' if depth == 0 && found_colon && !in_assignment => return Some(i),
            b'#' if depth == 0 => {
                // Comment: a '#' at top level starts a comment; no inline recipe
                // can appear after it (e.g. "target: # comment ; cmd" has no inline recipe).
                // Check for \# which is an escaped '#' (not a comment).
                if i > 0 && bytes[i - 1] == b'\\' {
                    // escaped '#' — not a comment, continue scanning
                } else {
                    return None;
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

pub fn strip_comment(line: &str) -> String {
    let mut result = String::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut depth = 0; // track $(...) and ${...} nesting

    while i < bytes.len() {
        let ch = bytes[i];
        if ch == b'\\' && i + 1 < bytes.len() {
            if depth == 0 {
                // At top level: \# means a literal '#' (backslash is consumed).
                // Any other \X: push both characters (backslash kept for shell/recipe use).
                if bytes[i + 1] == b'#' {
                    result.push('#');
                    i += 2;
                    continue;
                }
                result.push(ch as char);
                result.push(bytes[i + 1] as char);
                i += 2;
                continue;
            } else {
                // Inside $(...) / ${...}: push both characters verbatim.
                result.push(ch as char);
                result.push(bytes[i + 1] as char);
                i += 2;
                continue;
            }
        }
        if ch == b'$' && i + 1 < bytes.len() && (bytes[i + 1] == b'(' || bytes[i + 1] == b'{') {
            depth += 1;
            result.push(ch as char);
            i += 1;
            continue;
        }
        if depth > 0 && (ch == b')' || ch == b'}') {
            depth -= 1;
            result.push(ch as char);
            i += 1;
            continue;
        }
        if ch == b'#' && depth == 0 {
            break;
        }
        result.push(ch as char);
        i += 1;
    }

    result
}

pub fn try_parse_variable_assignment(line: &str) -> Option<ParsedLine> {
    let mut is_override = false;
    let mut is_export = false;
    let mut is_unexport = false;
    let mut is_private = false;
    let mut target: Option<String> = None;
    let mut work = line.to_string();

    // Check for override/export/unexport/private prefixes
    // Note: these are only keywords when followed by another keyword or a valid
    // variable name+operator (not when the word IS the variable name, e.g. "private = g").
    let is_keyword_prefix = |keyword: &str, rest: &str| -> bool {
        // After the keyword (with leading whitespace stripped), the remaining text
        // must NOT start with an assignment operator. If it does, the keyword is
        // the variable name itself (e.g., `private = g` sets var "private").
        let ops = [":::=", "::=", "!=", "?=", "+=", ":=", "="];
        let rest_trimmed = rest.trim_start();
        if rest_trimmed.is_empty() {
            return false; // Bare keyword with nothing after = not a keyword prefix
        }
        // If next token starts with an assignment op, this is the variable name
        for op in &ops {
            if rest_trimmed.starts_with(op) {
                return false; // "keyword =" → keyword is the variable name
            }
        }
        true
    };
    loop {
        let trimmed = work.trim_start();
        if trimmed.starts_with("override ") {
            let after = &trimmed["override ".len()..];
            if is_keyword_prefix("override", after) {
                is_override = true;
                work = after.to_string();
                continue;
            }
        }
        if trimmed.starts_with("export ") {
            let after = &trimmed["export ".len()..];
            if is_keyword_prefix("export", after) {
                is_export = true;
                work = after.to_string();
                continue;
            }
        }
        if trimmed.starts_with("unexport ") {
            let after = &trimmed["unexport ".len()..];
            if is_keyword_prefix("unexport", after) {
                is_unexport = true;
                work = after.to_string();
                continue;
            }
        }
        if trimmed.starts_with("private ") {
            let after = &trimmed["private ".len()..];
            if is_keyword_prefix("private", after) {
                is_private = true;
                work = after.to_string();
                continue;
            }
        }
        break;
    }

    let work = work.trim_start();

    // Check for target-specific variable: target: var = value
    // But don't confuse with rules - target-specific has a known assignment op after the colon part

    // Find assignment operator
    let ops = [":::=", "::=", "!=", "?=", "+=", ":=", "="];
    for op in &ops {
        if let Some(pos) = find_assignment_op(work, op) {
            let name = work[..pos].trim().to_string();
            let value = work[pos + op.len()..].trim_start().to_string();
            let flavor = match *op {
                "=" => VarFlavor::Recursive,
                ":=" | "::=" => VarFlavor::Simple,
                ":::=" => VarFlavor::PosixSimple,
                "+=" => VarFlavor::Append,
                "?=" => VarFlavor::Conditional,
                "!=" => VarFlavor::Shell,
                _ => VarFlavor::Recursive,
            };

            // Check if this is a target-specific variable
            // But NOT if the name contains `;` (which indicates an inline recipe)
            if let Some(colon_pos) = name.find(':') {
                let potential_target = name[..colon_pos].trim();
                let raw_var_part = name[colon_pos+1..].trim();
                // Strip override/export/unexport/private prefixes from the variable part
                let (inner_override, inner_export, inner_unexport, inner_private, potential_var) =
                    strip_var_prefixes(raw_var_part);
                let effective_override = is_override || inner_override;
                let effective_export = is_export || inner_export;
                let effective_unexport = is_unexport || inner_unexport;
                let effective_private = is_private || inner_private;
                if !potential_target.is_empty() && !potential_var.is_empty()
                    && !potential_var.contains('/')
                    && !potential_var.contains(';')
                    && !potential_target.contains(';')
                    && is_valid_variable_name(potential_var)
                {
                    return Some(ParsedLine::VariableAssignment {
                        name: potential_var.to_string(),
                        value,
                        flavor,
                        is_override: effective_override,
                        is_export: effective_export,
                        is_unexport: effective_unexport,
                        is_private: effective_private,
                        target: Some(potential_target.to_string()),
                    });
                }
            }

            // Don't treat as variable assignment if name contains `:` followed by `;`
            // That's a rule with an inline recipe like: target:;recipe=value
            if name.contains(':') && (name.contains(';') || name.contains('\t')) {
                // This is likely a rule, not a variable assignment
                return None;
            }

            // Valid variable name check (no colons or spaces in plain variable names;
            // colons indicate it could be a rule, spaces indicate multiple targets)
            if name.contains(':') {
                return None;
            }
            if name.contains(' ') || name.contains('\t') {
                // Name has whitespace but no colon. This could be:
                // (a) "x $X=" where $X expands to something with spaces → missing separator
                // (b) a rule like "ten one=two =: ; recipe" where the `=` in a target name
                //     was found first before the rule's `:`.
                // Distinguish: if there is a bare rule colon ANYWHERE in `work`,
                // this is a rule (case b). Return None so try_parse_rule can handle it.
                if find_rule_colon(work).is_some() {
                    return None;
                }
                return Some(ParsedLine::MissingSeparator(String::new()));
            }

            return Some(ParsedLine::VariableAssignment {
                name,
                value,
                flavor,
                is_override,
                is_export,
                is_unexport,
                is_private,
                target: None,
            });
        }
    }

    None
}

fn find_assignment_op(line: &str, op: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let op_bytes = op.as_bytes();
    let mut i = 0;
    let mut paren_depth = 0i32;

    while i + op_bytes.len() <= bytes.len() {
        match bytes[i] {
            b'(' | b'{' => paren_depth += 1,
            b')' | b'}' => paren_depth -= 1,
            b'$' => {
                i += 1;
                continue;
            }
            _ => {}
        }
        if paren_depth == 0 && &bytes[i..i+op_bytes.len()] == op_bytes {
            // For '=', make sure it's not part of ':=' or '!=' or '?=' or '+='
            if op == "=" && i > 0 {
                match bytes[i-1] {
                    b':' | b'!' | b'?' | b'+' => {
                        i += 1;
                        continue;
                    }
                    _ => {}
                }
            }
            // For '::=', make sure it's not part of ':::='
            if op == "::=" && i > 0 && bytes[i-1] == b':' {
                i += 1;
                continue;
            }
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Like `try_parse_rule` but skips target-specific-variable detection.
/// Use when the ORIGINAL (pre-expansion) line was confirmed to have no
/// literal assignment operator, so any `=` sign in the expanded line came
/// from variable expansion (e.g. `ten: one $(EQ) two` → `ten: one = two`).
/// In that case the line must be treated as a rule, not a TSV assignment.
pub fn try_parse_rule_force(line: &str) -> Option<ParsedLine> {
    try_parse_rule_inner(line, true)
}

pub fn try_parse_rule(line: &str) -> Option<ParsedLine> {
    try_parse_rule_inner(line, false)
}

fn try_parse_rule_inner(line: &str, skip_tsv: bool) -> Option<ParsedLine> {
    // Find the colon that separates targets from prerequisites.
    // Handles: target: prereqs, target:: prereqs (double-colon),
    //          target&: prereqs (grouped targets, GNU Make 4.3+),
    //          and static pattern rules: targets: target-pattern: prereq-patterns
    //          (and their double-colon variants).

    let colon_pos = find_rule_colon(line)?;
    let is_double_colon = line[colon_pos..].starts_with("::");

    // Check for grouped-target syntax: targets end with & before the colon.
    // "15.x 1.x&: prereqs" means both targets are grouped.
    // Strip the & from the targets string.
    let raw_targets_str = line[..colon_pos].trim();
    let (targets_str, _is_grouped) = if raw_targets_str.ends_with('&') {
        (&raw_targets_str[..raw_targets_str.len()-1], true)
    } else {
        (raw_targets_str, false)
    };

    let rest = if is_double_colon {
        &line[colon_pos+2..]
    } else {
        &line[colon_pos+1..]
    };

    // Detect static pattern rule: the text after the first colon (or ::)
    // contains a second bare colon, and the part before that second colon
    // (the target-pattern) contains a `%`.
    // Example:  "foo.o bar.o: %.o: %.c"
    //           rest = " %.o: %.c"  →  second colon found at "%.o"
    let rest_trimmed_raw = rest.trim();
    // Strip inline comments from the prerequisites portion (before any `;`).
    // For example "target: # comment ; cmd" or "target: dep # comment" should
    // have their comments stripped so `#` is not treated as a prerequisite name.
    // The inline recipe (after `;`) is NOT comment-stripped here — it is passed
    // to the shell verbatim.
    //
    // Important: we must find the inline-recipe `;` ONLY in the part BEFORE any
    // `#` comment.  A `;` inside a comment (e.g. "target: # foo ; bar") is not an
    // inline recipe separator.  Use find_inline_recipe_semi_pos (which is already
    // comment-aware) to locate the `;`, then strip the comment from the prereq part.
    let rest_trimmed_owned;
    let rest_trimmed = {
        // find_inline_recipe_semi_pos operates on the whole-line context, but here
        // we only have the rest (after the target colon).  We reconstruct a fake
        // line with a dummy "x:" prefix so the function sees a post-colon context.
        let fake_line = format!("x:{}", rest_trimmed_raw);
        let semi_in_rest = find_inline_recipe_semi_pos(&fake_line)
            .map(|pos| pos - 2); // subtract the length of "x:"
        if let Some(semi) = semi_in_rest {
            // Strip comments only from the prereq part; keep recipe verbatim.
            let prereq_stripped = strip_comment(&rest_trimmed_raw[..semi]);
            rest_trimmed_owned = format!("{};{}", prereq_stripped, &rest_trimmed_raw[semi+1..]);
            rest_trimmed_owned.as_str()
        } else {
            // No inline recipe: strip comments from the whole rest.
            rest_trimmed_owned = strip_comment(rest_trimmed_raw);
            rest_trimmed_owned.as_str()
        }
    };
    let rest_trimmed = rest_trimmed.trim();
    // Only search for the static-pattern-rule colon in the part before any
    // inline recipe (`;`).  Otherwise "%.elf: %.c ; :" would incorrectly
    // treat the `:` from the inline recipe as a static-pattern-rule separator.
    let rest_before_semi = match find_semicolon(rest_trimmed) {
        Some(semi) => &rest_trimmed[..semi],
        None => rest_trimmed,
    };
    if let Some(second_colon) = find_bare_colon(rest_before_semi) {
        let target_pattern_str = rest_before_semi[..second_colon].trim();
        if find_unescaped_percent(target_pattern_str).is_some() {
            let after_second = rest_trimmed[second_colon + 1..].trim();
            let targets: Vec<String> = split_filenames(targets_str);
            if !targets.is_empty() {
                // Normalize the target pattern through the same backslash processing
                // as split_filenames: `\\` → `\`, `\%` → `\%` (kept as-is).
                // This is required so that match_pattern can correctly align the
                // pattern prefix/suffix with the already-normalized target strings.
                let target_pattern_normalized: String = split_filenames(target_pattern_str)
                    .into_iter().next().unwrap_or_default();
                return Some(expand_static_pattern_rule(
                    targets,
                    &target_pattern_normalized,
                    after_second,
                    is_double_colon,
                ));
            }
        } else if !target_pattern_str.is_empty() {
            // A second bare colon was found but the target pattern contains no '%'.
            // This is a fatal error: "target pattern contains no '%'".
            return Some(ParsedLine::FatalError(
                "target pattern contains no '%'.  Stop.".to_string()
            ));
        }
    }

    // Check for target-specific variable assignment in rest, but only in the
    // part before any inline recipe (i.e., before a bare `;`).
    // e.g., "target: VAR = value" but NOT "target: ; @echo $(VAR=x)"
    // Skip this check when the caller knows the original (pre-expansion) line
    // had no literal `=`, meaning any `=` came from variable expansion.
    if !skip_tsv {
        let prereq_part = match find_semicolon(rest_trimmed) {
            Some(semi) => &rest_trimmed[..semi],
            None => rest_trimmed,
        };
        let ops = [":::=", "::=", "!=", "?=", "+=", ":=", "="];
        for op in &ops {
            if let Some(pos) = find_assignment_op(prereq_part.trim(), op) {
                let raw_var_name = prereq_part.trim()[..pos].trim();
                // Strip override/export/unexport/private prefixes from the variable name
                let (is_override, is_export, is_unexport, is_private, var_name) = strip_var_prefixes(raw_var_name);
                let var_value = prereq_part.trim()[pos + op.len()..].trim_start().to_string();
                if !var_name.is_empty() && is_valid_variable_name(var_name) {
                    let flavor = match *op {
                        "=" => VarFlavor::Recursive,
                        ":=" | "::=" | ":::=" => VarFlavor::Simple,
                        "+=" => VarFlavor::Append,
                        "?=" => VarFlavor::Conditional,
                        "!=" => VarFlavor::Shell,
                        _ => VarFlavor::Recursive,
                    };
                    return Some(ParsedLine::VariableAssignment {
                        name: var_name.to_string(),
                        value: var_value,
                        flavor,
                        is_override,
                        is_export,
                        is_unexport,
                        is_private,
                        target: Some(targets_str.to_string()),
                    });
                }
            }
        }
    }

    // Use split_filenames to handle escaped spaces (`\ `) and escaped colons (`\:`)
    let targets: Vec<String> = split_filenames(targets_str);
    if targets.is_empty() && !_is_grouped {
        return None;
    }

    // Split prerequisites and order-only prerequisites (after |), and extract
    // any inline recipe that appears after a bare `;`.
    // Use the comment-stripped version of rest for actual prereq splitting.
    let rest_trimmed2 = rest_trimmed;
    let (prereqs, order_only, inline_recipe) = split_prerequisites(rest_trimmed2);
    let is_pattern = targets.iter().any(|t| t.contains('%'));

    // Also compute the raw (unsplit) prerequisite text for second expansion.
    // This is the text before the `;` (if any) and before `|` for order-only.
    let (raw_prereq_text, raw_order_only_text) = {
        let prereq_part = match find_semicolon(rest_trimmed2) {
            Some(pos) => &rest_trimmed2[..pos],
            None => rest_trimmed2,
        };
        if let Some(pipe_pos) = find_pipe(prereq_part) {
            let normal = prereq_part[..pipe_pos].trim().to_string();
            let oo = prereq_part[pipe_pos + 1..].trim().to_string();
            (normal, oo)
        } else {
            (prereq_part.trim().to_string(), String::new())
        }
    };

    let mut rule = Rule::new();
    rule.targets = targets.clone();
    rule.prerequisites = prereqs;
    rule.order_only_prerequisites = order_only;
    rule.is_pattern = is_pattern;
    rule.is_double_colon = is_double_colon;
    // For grouped targets (`&:`), compute grouped_siblings = all targets except this one.
    // We store all targets here; register_rule will split them per target with proper siblings.
    // We re-use `grouped_siblings` as a temporary "all targets" store; register_rule converts it.
    if _is_grouped && targets.len() > 1 {
        // Store ALL targets in grouped_siblings (register_rule will remove the self target).
        rule.grouped_siblings = targets;
    }
    // Store the raw (unsplit) prerequisite text for second expansion, but ONLY
    // when the text contains '$' (i.e., has deferred variable references that
    // need build-time re-expansion).  Plain prereqs like "bar baz" need no SE.
    if raw_prereq_text.contains('$') {
        rule.second_expansion_prereqs = Some(raw_prereq_text);
    }
    if raw_order_only_text.contains('$') {
        rule.second_expansion_order_only = Some(raw_order_only_text);
    }

    // If there was an inline recipe after the `;`, add it as the first recipe line.
    // Line number will be stamped by the caller (process_parsed_lines) which has the lineno.
    if let Some(recipe_line) = inline_recipe {
        rule.has_inline_recipe_marker = true;
        rule.recipe.push((0, recipe_line));
    }

    Some(ParsedLine::Rule(rule))
}

/// Find the byte position of the first bare `:` in `s` that is NOT part of
/// the operator `::=`.  Variable references (`$(...)`, `${...}`, `$x`) are
/// skipped.  Returns `None` if no such colon exists.
///
/// Used to locate the second colon in a static pattern rule after the first
/// colon (and the potential `::`) have already been consumed by the caller.
fn find_bare_colon(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut paren_depth = 0i32;

    while i < bytes.len() {
        match bytes[i] {
            b'$' => {
                if i + 1 < bytes.len() {
                    match bytes[i + 1] {
                        b'(' | b'{' => {
                            paren_depth += 1;
                            i += 2;
                            continue;
                        }
                        _ => {
                            i += 2; // single-char reference like `$@`
                            continue;
                        }
                    }
                }
            }
            b'(' | b'{' if paren_depth > 0 => paren_depth += 1,
            b')' | b'}' if paren_depth > 0 => paren_depth -= 1,
            b':' if paren_depth == 0 => {
                // Skip \: (escaped colon — literal colon in a filename).
                // Count consecutive backslashes; if odd, colon is escaped.
                if i > 0 && bytes[i - 1] == b'\\' {
                    let mut nb = 0usize;
                    let mut j = i;
                    while j > 0 && bytes[j - 1] == b'\\' {
                        nb += 1;
                        j -= 1;
                    }
                    if nb % 2 == 1 {
                        i += 1;
                        continue;
                    }
                }
                // Skip ::= (it is a variable-assignment operator, not a rule separator)
                if i + 2 < bytes.len() && bytes[i + 1] == b':' && bytes[i + 2] == b'=' {
                    return None;
                }
                return Some(i);
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Unescape `\%` → `%` in a target name (file path) string.
/// In GNU Make, `\%` in a target name or static pattern rule target list
/// represents a literal `%` character.  This function converts stored
/// `\%` sequences back to plain `%` so the target can be looked up in the
/// rule database by its canonical (unescaped) name.
pub fn unescape_percent_in_target(s: &str) -> String {
    if !s.contains("\\%") {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut result = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'%' {
            // \% → % (unescape)
            result.push('%');
            i += 2;
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }
    result
}

/// Find the position of the first UNESCAPED `%` in a pattern string.
/// An unescaped `%` is one not preceded by an odd number of backslashes.
/// Returns `None` if no unescaped `%` exists.
fn find_unescaped_percent(pattern: &str) -> Option<usize> {
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            // Count consecutive backslashes
            let start = i;
            while i < bytes.len() && bytes[i] == b'\\' {
                i += 1;
            }
            let num_backslashes = i - start;
            if i < bytes.len() && bytes[i] == b'%' {
                if num_backslashes % 2 == 0 {
                    // Even number of backslashes: % is NOT escaped
                    return Some(i);
                } else {
                    // Odd number of backslashes: % IS escaped (literal)
                    i += 1; // skip the %
                }
            }
            // Otherwise continue (the backslashes escaped something else)
        } else if bytes[i] == b'%' {
            return Some(i);
        } else {
            i += 1;
        }
    }
    None
}

/// Match `target` against `pattern` (which contains at most one `%`) and
/// return the stem — the substring that `%` stands for — or `None` if the
/// target does not match.
///
/// GNU Make rules:
///   - In the pattern, `\%` is a literal `%` (escaped), and the first unescaped
///     `%` is the wildcard.
///   - The literal text before `%` must be a prefix of `target`.
///   - The literal text after  `%` must be a suffix of `target`.
///   - The prefix and suffix together must not exceed `target.len()`.
pub fn match_pattern(target: &str, pattern: &str) -> Option<String> {
    match find_unescaped_percent(pattern) {
        None => {
            // No wildcard — treat pattern as literal (but unescape \% → % for comparison).
            // The target also has \% stored as \%, so compare raw bytes.
            if target == pattern {
                Some(String::new())
            } else {
                None
            }
        }
        Some(pct) => {
            let prefix = &pattern[..pct];
            let suffix = &pattern[pct + 1..];

            if target.len() < prefix.len() + suffix.len() {
                return None;
            }
            if !target.starts_with(prefix) {
                return None;
            }
            let stem_end = target.len() - suffix.len();
            if !target[stem_end..].ends_with(suffix) {
                return None;
            }
            if stem_end < prefix.len() {
                return None;
            }
            Some(target[prefix.len()..stem_end].to_string())
        }
    }
}

/// Apply `%` → stem substitution to a raw SE prerequisite text (for static pattern rules).
/// GNU Make rule: in the SE prerequisite text, replace only the FIRST `%` in each
/// whitespace-delimited word.  Subsequent `%` characters in the same word are left as-is.
/// This is used when storing the SE text for static pattern rules at parse time.
pub fn subst_first_percent_per_word_in_se_text(text: &str, stem: &str) -> String {
    let mut result = String::with_capacity(text.len() + stem.len() * 4);
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let n = chars.len();
    while i < n {
        if chars[i].is_whitespace() {
            result.push(chars[i]);
            i += 1;
            continue;
        }
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

/// Replace the **first** occurrence of `%` in `pattern` with `stem`.
/// Subsequent `%` characters are left unchanged (GNU Make semantics).
/// Replace bare `$*` (dollar + asterisk, where the `*` is not part of a `$(`
/// or `${` expression) with the literal `stem` value.  This is used when storing
/// second-expansion prerequisite text for static pattern rules: the stem is known
/// at parse time and baking it in prevents the wrong stem being used after the
/// evaluator merges multiple static pattern rules into one rule object.
///
/// Only the bare two-character sequence `$*` is replaced.  `$(*)`, `$(*D)`,
/// `$(*F)` etc. are left unchanged (they are uncommon in static pattern rules and
/// handled at build time).
pub fn subst_dollar_star_in_se_text(text: &str, stem: &str) -> String {
    if !text.contains("$*") {
        return text.to_string();
    }
    let mut result = String::with_capacity(text.len() + stem.len() * 4);
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    while i < n {
        if bytes[i] == b'$' && i + 1 < n && bytes[i + 1] == b'*' {
            // Make sure this isn't `$(*` (i.e. `$` followed by `*` then `(`/`{`)
            // which would be part of a longer variable reference.
            let next_next = if i + 2 < n { bytes[i + 2] } else { 0 };
            if next_next != b'(' && next_next != b'{' {
                result.push_str(stem);
                i += 2; // skip `$*`
                continue;
            }
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

pub fn apply_stem(pattern: &str, stem: &str) -> String {
    match find_unescaped_percent(pattern) {
        None => pattern.to_string(),
        Some(pct) => {
            let mut result = String::with_capacity(pattern.len() + stem.len());
            result.push_str(&pattern[..pct]);
            result.push_str(stem);
            result.push_str(&pattern[pct + 1..]);
            result
        }
    }
}

/// Expand a static pattern rule into a [`ParsedLine::StaticPatternExpansion`].
///
/// For each target in `targets`:
///   1. Match against `target_pattern` to extract the stem.
///   2. Substitute the stem into each prerequisite pattern.
///   3. Build an explicit `Rule` for that (target, resolved prerequisites) pair.
///
/// Targets that do not match the target pattern are skipped (GNU Make ignores
/// them silently, just as unmatched implicit rules are ignored).
fn expand_static_pattern_rule(
    targets: Vec<String>,
    target_pattern: &str,
    prereq_str: &str,
    is_double_colon: bool,
) -> ParsedLine {
    // Determine raw texts for second expansion BEFORE splitting.
    // These are needed when the prerequisite pattern contains '$' (deferred refs).
    let (raw_prereq_text, raw_order_only_text) = {
        let prereq_part = match find_semicolon(prereq_str) {
            Some(pos) => &prereq_str[..pos],
            None => prereq_str,
        };
        if let Some(pipe_pos) = find_pipe(prereq_part) {
            let normal = prereq_part[..pipe_pos].trim().to_string();
            let oo = prereq_part[pipe_pos + 1..].trim().to_string();
            (normal, oo)
        } else {
            (prereq_part.trim().to_string(), String::new())
        }
    };

    let (prereq_patterns, order_only_patterns, inline_recipe) =
        split_prerequisites(prereq_str);

    let mut rules: Vec<Rule> = Vec::new();

    for target in targets {
        let stem = match match_pattern(&target, target_pattern) {
            Some(s) => s,
            None => continue, // target doesn't match pattern — skip
        };

        // Substitute the stem into every prerequisite pattern.
        // GNU Make filters out empty-string prerequisites that result from an
        // empty stem (e.g. `foo: foo%: %` with stem `` gives empty prereq `""`
        // which is silently discarded).
        // Also unescape \% → % in the resulting prerequisite names: patterns
        // like `the\%weird\_..._pattern%\.1` have \% for a literal % in the
        // prerequisite file name, which must be unescaped after stem substitution.
        let prereqs: Vec<String> = prereq_patterns
            .iter()
            .map(|p| unescape_percent_in_target(&apply_stem(p, &stem)))
            .filter(|p| !p.is_empty())
            .collect();

        let order_only: Vec<String> = order_only_patterns
            .iter()
            .map(|p| unescape_percent_in_target(&apply_stem(p, &stem)))
            .filter(|p| !p.is_empty())
            .collect();

        // Unescape \% → % in the target name: a static pattern rule target list may
        // contain \% to represent a literal % in a target file name.  The explicit
        // rule must be stored under the canonical (unescaped) name so that it can
        // be found when the target is looked up by its unescaped name.
        let canonical_target = unescape_percent_in_target(&target);

        let mut rule = Rule::new();
        rule.targets = vec![canonical_target.clone()];
        rule.prerequisites = prereqs;
        rule.order_only_prerequisites = order_only;
        // Static pattern rules create explicit (non-pattern) rules.
        rule.is_pattern = false;
        rule.is_double_colon = is_double_colon;
        // Store the stem so the executor can provide a correct `$*`.
        rule.static_stem = stem.clone();

        // Store raw SE texts for second expansion.
        // We store the raw prereq text with:
        //   1. The FIRST `%` per word replaced by the stem (pattern wildcard substitution).
        //   2. `$$*` replaced by the literal stem (so each static pattern rule's own stem
        //      is baked in at parse time, allowing correct `$*` expansion even when multiple
        //      static pattern rules for the same target are merged by the evaluator).
        // This ensures that `$$*` in each rule's SE text refers to THAT rule's stem,
        // not the globally-last static stem stored on the merged rule.
        // Other auto vars ($$@, $$<, etc.) are still expanded at build time.
        if raw_prereq_text.contains('$') {
            let raw_with_stem = subst_first_percent_per_word_in_se_text(&raw_prereq_text, &stem);
            // Replace `$*` (the stem auto-variable, already reduced from `$$*` by
            // first-expansion) with the literal stem so that each static pattern rule's
            // own stem is baked in at parse time.  This prevents the wrong stem from being
            // used when the evaluator merges multiple static pattern rules for the same
            // target and replaces the collective static_stem with the last rule's value.
            // Only the bare `$*` form is replaced here; `$(*D)` / `$(*F)` etc. are left
            // for second expansion at build time (they are uncommon in static pattern rules).
            let raw_with_star = subst_dollar_star_in_se_text(&raw_with_stem, &stem);
            rule.second_expansion_prereqs = Some(raw_with_star);
        }
        if raw_order_only_text.contains('$') {
            let raw_oo_with_stem = subst_first_percent_per_word_in_se_text(&raw_order_only_text, &stem);
            let raw_oo_with_star = subst_dollar_star_in_se_text(&raw_oo_with_stem, &stem);
            rule.second_expansion_order_only = Some(raw_oo_with_star);
        }

        // Inline recipe after `;` is propagated to every generated rule.
        // The caller will stamp the real line number (entry.0 == 0 is the cue).
        if let Some(ref recipe_line) = inline_recipe {
            rule.has_inline_recipe_marker = true;
            rule.recipe.push((0, recipe_line.clone()));
        }

        rules.push(rule);
    }

    ParsedLine::StaticPatternExpansion(rules)
}

/// Public wrapper around find_rule_colon for use in the eval loop.
pub fn find_rule_colon_pub(line: &str) -> Option<usize> {
    find_rule_colon(line)
}

fn find_rule_colon(line: &str) -> Option<usize> {
    // If the line has an inline recipe (bare `;`), it must be a rule, so `=` in
    // the targets portion should not cause an early bail-out.  Pre-check for a
    // bare semicolon so we know whether to honour the `=`-exit optimisation.
    let has_inline_recipe = find_semicolon(line).is_some();

    let bytes = line.as_bytes();
    let mut i = 0;
    let mut paren_depth = 0i32;

    while i < bytes.len() {
        match bytes[i] {
            b'$' => {
                // Skip variable references
                if i + 1 < bytes.len() {
                    match bytes[i+1] {
                        b'(' | b'{' => {
                            paren_depth += 1;
                            i += 2;
                            continue;
                        }
                        _ => {
                            i += 2;
                            continue;
                        }
                    }
                }
            }
            b'(' | b'{' if paren_depth > 0 => paren_depth += 1,
            b')' | b'}' if paren_depth > 0 => paren_depth -= 1,
            b':' if paren_depth == 0 => {
                // Skip \: (escaped colon - literal colon in target/prereq name)
                if i > 0 && bytes[i - 1] == b'\\' {
                    // Count consecutive backslashes before this ':'
                    let mut nb = 0usize;
                    let mut j = i;
                    while j > 0 && bytes[j - 1] == b'\\' {
                        nb += 1;
                        j -= 1;
                    }
                    if nb % 2 == 1 {
                        // Odd number of backslashes: the colon is escaped, skip it
                        i += 1;
                        continue;
                    }
                }
                // Make sure this isn't a drive letter (e.g., C:)
                if i == 1 && bytes[0].is_ascii_alphabetic() && i + 1 < bytes.len() && (bytes[i+1] == b'\\' || bytes[i+1] == b'/') {
                    i += 1;
                    continue;
                }
                // Check it's not ::= (which is a variable assignment)
                if i + 2 < bytes.len() && bytes[i+1] == b':' && bytes[i+2] == b'=' {
                    return None; // This is a ::= assignment, not a rule
                }
                return Some(i);
            }
            b'=' if paren_depth == 0 => {
                // If we hit = before :, this is likely a variable assignment.
                // However, when an inline recipe (`;`) is present the whole line
                // must be a rule – targets can legitimately contain `=` after
                // variable expansion (e.g. `one=two =:;@echo $@`).
                if !has_inline_recipe {
                    return None;
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn is_valid_variable_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(' ')
        && !name.contains('\t')
        && !name.contains('#')
}

/// Strip leading override/export/unexport/private prefixes from a variable name token.
/// Returns (is_override, is_export, is_unexport, is_private, remaining_name).
/// E.g. "override FOO" → (true, false, false, false, "FOO")
///      "export override BAR" → (true, true, false, false, "BAR")
///      "unexport BAZ" → (false, false, true, false, "BAZ")
pub fn strip_var_prefixes(s: &str) -> (bool, bool, bool, bool, &str) {
    let mut is_override = false;
    let mut is_export = false;
    let mut is_unexport = false;
    let mut is_private = false;
    let mut rest = s.trim();
    loop {
        if let Some(r) = rest.strip_prefix("override") {
            // Must be followed by whitespace (not end of string: "override" alone = var name)
            if r.starts_with(|c: char| c.is_ascii_whitespace()) {
                let trimmed = r.trim_start();
                // Also check: remaining text must not be empty and must not start with
                // an assignment operator (which would make "override" the var name).
                if !trimmed.is_empty() && !trimmed.starts_with(['=', '!', '?', '+', ':']) {
                    is_override = true;
                    rest = trimmed;
                    continue;
                }
            }
        }
        // Check 'unexport' before 'export' to avoid prefix match issues
        if let Some(r) = rest.strip_prefix("unexport") {
            if r.starts_with(|c: char| c.is_ascii_whitespace()) {
                let trimmed = r.trim_start();
                if !trimmed.is_empty() && !trimmed.starts_with(['=', '!', '?', '+', ':']) {
                    is_unexport = true;
                    rest = trimmed;
                    continue;
                }
            }
        }
        if let Some(r) = rest.strip_prefix("export") {
            if r.starts_with(|c: char| c.is_ascii_whitespace()) {
                let trimmed = r.trim_start();
                if !trimmed.is_empty() && !trimmed.starts_with(['=', '!', '?', '+', ':']) {
                    is_export = true;
                    rest = trimmed;
                    continue;
                }
            }
        }
        if let Some(r) = rest.strip_prefix("private") {
            if r.starts_with(|c: char| c.is_ascii_whitespace()) {
                let trimmed = r.trim_start();
                if !trimmed.is_empty() && !trimmed.starts_with(['=', '!', '?', '+', ':']) {
                    is_private = true;
                    rest = trimmed;
                    continue;
                }
            }
        }
        break;
    }
    (is_override, is_export, is_unexport, is_private, rest)
}

pub fn split_words(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}

/// Split a targets/prerequisites string into individual file names,
/// respecting backslash escaping of whitespace (`\ `, `\<tab>`) and
/// converting escape sequences to their literal equivalents:
///   `\:` → `:`   `\#` → `#`   `\ ` → ` ` (within a word)
///
/// Note: `\\` (two backslashes) is NOT collapsed — GNU Make preserves double
/// backslashes in file names.  Only `\ ` (backslash-space) is processed as an
/// escape sequence for whitespace.  `\%` is also kept verbatim here so that
/// callers can distinguish a literal `%` in a file name from the `%` pattern
/// wildcard; callers that need a canonical name should call
/// `unescape_percent_in_target` afterwards.
///
/// This mirrors GNU Make's `PARSE_FILE_SEQ` behaviour for target and
/// prerequisite lists.
pub fn split_filenames(s: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    let bytes = s.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let ch = bytes[i];
        if ch == b'\\' && i + 1 < bytes.len() {
            let next = bytes[i + 1];
            match next {
                // Escaped special chars: consume the backslash, keep the char
                b':' | b'#' => {
                    current.push(next as char);
                    i += 2;
                    continue;
                }
                b' ' | b'\t' => {
                    // Escaped whitespace: part of the current word
                    current.push(next as char);
                    i += 2;
                    continue;
                }
                _ => {
                    // All other backslash sequences (including `\\` and `\%`):
                    // keep both characters verbatim.  GNU Make does NOT collapse
                    // `\\` to `\` in file-name lists.
                    current.push('\\');
                    current.push(next as char);
                    i += 2;
                    continue;
                }
            }
        }
        if ch == b' ' || ch == b'\t' {
            // Unescaped whitespace: word separator
            if !current.is_empty() {
                result.push(current.clone());
                current.clear();
            }
        } else {
            current.push(ch as char);
        }
        i += 1;
    }
    if !current.is_empty() {
        result.push(current);
    }
    result
}

pub fn split_prerequisites(s: &str) -> (Vec<String>, Vec<String>, Option<String>) {
    // First, find a bare `;` (not inside variable references).
    // Everything before it is the prereq/order-only part; everything after is
    // the inline recipe.
    let semi_pos = find_semicolon(s);

    let (prereq_part, inline_recipe): (&str, Option<String>) = if let Some(pos) = semi_pos {
        let recipe_text = s[pos + 1..].trim_start().to_string();
        (&s[..pos], Some(recipe_text))
    } else {
        (s, None)
    };

    // Split on | for order-only prerequisites inside the prereq part.
    let mut prereqs = Vec::new();
    let mut order_only = Vec::new();

    if let Some(pipe_pos) = find_pipe(prereq_part) {
        let normal = &prereq_part[..pipe_pos];
        let oo = &prereq_part[pipe_pos + 1..];
        prereqs = split_filenames(normal.trim());
        order_only = split_filenames(oo.trim());
    } else {
        prereqs = split_filenames(prereq_part.trim());
    }

    (prereqs, order_only, inline_recipe)
}

/// Find the byte position of the first bare `;` in `s`, skipping over
/// `$(...)` / `${...}` variable references and single-char `$x` refs.
pub fn find_semicolon(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut paren_depth = 0i32;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'$' => {
                if i + 1 < bytes.len() {
                    match bytes[i + 1] {
                        b'(' | b'{' => {
                            paren_depth += 1;
                            i += 2;
                            continue;
                        }
                        _ => {
                            i += 2; // single-char variable reference like `$@`
                            continue;
                        }
                    }
                }
            }
            b'(' | b'{' if paren_depth > 0 => paren_depth += 1,
            b')' | b'}' if paren_depth > 0 => paren_depth -= 1,
            b';' if paren_depth == 0 => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

fn find_pipe(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut paren_depth = 0i32;
    for i in 0..bytes.len() {
        match bytes[i] {
            b'$' if i + 1 < bytes.len() && (bytes[i+1] == b'(' || bytes[i+1] == b'{') => {
                paren_depth += 1;
            }
            b'(' | b'{' if paren_depth > 0 => paren_depth += 1,
            b')' | b'}' if paren_depth > 0 => paren_depth -= 1,
            b'|' if paren_depth == 0 => return Some(i),
            _ => {}
        }
    }
    None
}

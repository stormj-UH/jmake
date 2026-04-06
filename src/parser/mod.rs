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
    pub conditional_stack: Vec<ConditionalState>,
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
            conditional_stack: Vec::new(),
        }
    }

    pub fn load_file(&mut self) -> io::Result<()> {
        let content = fs::read_to_string(&self.filename)?;
        self.lines = content.lines().map(String::from).collect();
        self.pos = 0;
        self.lineno = 0;
        Ok(())
    }

    pub fn load_string(&mut self, content: &str) {
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
        while line.ends_with('\\') && self.pos < self.lines.len() {
            line.pop(); // remove backslash
            line.push(' ');
            line.push_str(self.lines[self.pos].trim_start());
            self.pos += 1;
        }

        self.lineno = start_lineno;
        Some((line, start_lineno))
    }

    pub fn parse_line(&self, line: &str, state: &MakeState) -> ParsedLine {
        let trimmed = line.trim();

        // Empty line
        if trimmed.is_empty() {
            return ParsedLine::Empty;
        }

        // Comment line
        if trimmed.starts_with('#') {
            return ParsedLine::Comment;
        }

        // Recipe line (starts with tab)
        if line.starts_with('\t') || (line.starts_with(' ') && self.in_recipe) {
            return ParsedLine::Recipe(line[1..].to_string());
        }

        // Strip inline comments (not in recipe context)
        let effective = strip_comment(trimmed);

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

        // define directive
        if effective.starts_with("define ") || effective == "define" {
            return parse_define_start(&effective);
        }

        // endef
        if effective == "endef" {
            return ParsedLine::Endef;
        }

        // include / -include / sinclude
        if effective.starts_with("include ") || effective.starts_with("-include ") || effective.starts_with("sinclude ") {
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

        // Unknown line - treat as recipe continuation or error
        ParsedLine::Empty
    }
}

pub fn strip_comment(line: &str) -> String {
    let mut result = String::new();
    let mut chars = line.chars().peekable();
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if escaped {
            result.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            result.push(ch);
            continue;
        }
        if ch == '#' {
            break;
        }
        result.push(ch);
    }

    result.trim_end().to_string()
}

pub fn try_parse_variable_assignment(line: &str) -> Option<ParsedLine> {
    let mut is_override = false;
    let mut is_export = false;
    let mut is_private = false;
    let mut target: Option<String> = None;
    let mut work = line.to_string();

    // Check for override/export/private prefixes
    loop {
        let trimmed = work.trim_start();
        if trimmed.starts_with("override ") {
            is_override = true;
            work = trimmed["override ".len()..].to_string();
        } else if trimmed.starts_with("export ") {
            is_export = true;
            work = trimmed["export ".len()..].to_string();
        } else if trimmed.starts_with("private ") {
            is_private = true;
            work = trimmed["private ".len()..].to_string();
        } else {
            break;
        }
    }

    let work = work.trim();

    // Check for target-specific variable: target: var = value
    // But don't confuse with rules - target-specific has a known assignment op after the colon part

    // Find assignment operator
    let ops = ["::=", "!=", "?=", "+=", ":=", "="];
    for op in &ops {
        if let Some(pos) = find_assignment_op(work, op) {
            let name = work[..pos].trim().to_string();
            let value = work[pos + op.len()..].trim_start().to_string();
            let flavor = match *op {
                "=" => VarFlavor::Recursive,
                ":=" | "::=" => VarFlavor::Simple,
                "+=" => VarFlavor::Append,
                "?=" => VarFlavor::Conditional,
                "!=" => VarFlavor::Shell,
                _ => VarFlavor::Recursive,
            };

            // Check if this is a target-specific variable
            if let Some(colon_pos) = name.find(':') {
                let potential_target = name[..colon_pos].trim();
                let potential_var = name[colon_pos+1..].trim();
                if !potential_target.is_empty() && !potential_var.is_empty()
                    && !potential_target.contains('%')
                    && !potential_var.contains('/')
                {
                    return Some(ParsedLine::VariableAssignment {
                        name: potential_var.to_string(),
                        value,
                        flavor,
                        is_override,
                        is_export,
                        is_private,
                        target: Some(potential_target.to_string()),
                    });
                }
            }

            return Some(ParsedLine::VariableAssignment {
                name,
                value,
                flavor,
                is_override,
                is_export,
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
            return Some(i);
        }
        i += 1;
    }
    None
}

pub fn try_parse_rule(line: &str) -> Option<ParsedLine> {
    // Find the colon that separates targets from prerequisites
    // Must handle target: prereqs, target:: prereqs (double colon)
    // Also handle pattern rules with %

    let colon_pos = find_rule_colon(line)?;
    let is_double_colon = line[colon_pos..].starts_with("::");

    let targets_str = line[..colon_pos].trim();
    let rest = if is_double_colon {
        &line[colon_pos+2..]
    } else {
        &line[colon_pos+1..]
    };

    // Check for target-specific variable assignment in rest
    // e.g., "target: VAR = value"
    let ops = ["::=", "!=", "?=", "+=", ":=", "="];
    for op in &ops {
        if let Some(pos) = find_assignment_op(rest.trim(), op) {
            let var_name = rest.trim()[..pos].trim().to_string();
            let var_value = rest.trim()[pos + op.len()..].trim_start().to_string();
            if !var_name.is_empty() && is_valid_variable_name(&var_name) {
                let flavor = match *op {
                    "=" => VarFlavor::Recursive,
                    ":=" | "::=" => VarFlavor::Simple,
                    "+=" => VarFlavor::Append,
                    "?=" => VarFlavor::Conditional,
                    "!=" => VarFlavor::Shell,
                    _ => VarFlavor::Recursive,
                };
                return Some(ParsedLine::VariableAssignment {
                    name: var_name,
                    value: var_value,
                    flavor,
                    is_override: false,
                    is_export: false,
                    is_private: false,
                    target: Some(targets_str.to_string()),
                });
            }
        }
    }

    let targets: Vec<String> = split_words(targets_str);
    if targets.is_empty() {
        return None;
    }

    // Split prerequisites and order-only prerequisites (after |)
    let (prereqs, order_only) = split_prerequisites(rest.trim());
    let is_pattern = targets.iter().any(|t| t.contains('%'));

    let mut rule = Rule::new();
    rule.targets = targets;
    rule.prerequisites = prereqs;
    rule.order_only_prerequisites = order_only;
    rule.is_pattern = is_pattern;
    rule.is_double_colon = is_double_colon;

    Some(ParsedLine::Rule(rule))
}

fn find_rule_colon(line: &str) -> Option<usize> {
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
                // If we hit = before :, this is likely a variable assignment
                return None;
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
        && !name.contains('/')
        && !name.contains('\\')
}

pub fn split_words(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}

pub fn split_prerequisites(s: &str) -> (Vec<String>, Vec<String>) {
    // Split on | for order-only prerequisites
    // But be careful not to split inside variable references
    let mut prereqs = Vec::new();
    let mut order_only = Vec::new();

    if let Some(pipe_pos) = find_pipe(s) {
        let normal = &s[..pipe_pos];
        let oo = &s[pipe_pos+1..];
        prereqs = split_words(normal.trim());
        order_only = split_words(oo.trim());
    } else {
        prereqs = split_words(s);
    }

    // Handle semicolons in prerequisites - text after ; is a recipe line
    if let Some(last) = prereqs.last() {
        if let Some(semi_pos) = last.find(';') {
            let before = last[..semi_pos].to_string();
            // The rest after ; would be handled as an inline recipe
            if !before.is_empty() {
                let len = prereqs.len();
                prereqs[len - 1] = before;
            } else {
                prereqs.pop();
            }
        }
    }

    (prereqs, order_only)
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

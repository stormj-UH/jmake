// Copyright (c) 2026 Jon-Erik G. Storm. All rights reserved.
// Directive parsing (conditionals, include, vpath, export, define)

use crate::types::*;

/// Find the position of assignment operator `op` in `line`, respecting
/// parenthesis depth and `$` escaping (same logic as `find_assignment_op` in
/// `parser/mod.rs`).  Returns `None` if the operator is not found.
fn find_assignment_op_in(line: &str, op: &str) -> Option<usize> {
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
        if paren_depth == 0 && &bytes[i..i + op_bytes.len()] == op_bytes {
            // For bare '=', make sure it's not part of ':=', '!=', '?=', '+='
            if op == "=" && i > 0 {
                match bytes[i - 1] {
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

pub fn parse_conditional(line: &str) -> Option<ParsedLine> {
    let trimmed = line.trim();

    // Note: ifeq( and ifneq( (without whitespace) are intentionally NOT handled here.
    // GNU Make requires whitespace after ifeq/ifneq. "ifeq(" falls through to
    // MissingSeparator("ifeq/ifneq must be followed by whitespace") in parse_line.
    if trimmed.starts_with("ifeq ") || trimmed.starts_with("ifeq\t") {
        let rest = trimmed.strip_prefix("ifeq").unwrap().trim();
        if let Some((a, b)) = parse_conditional_args(rest) {
            return Some(ParsedLine::Conditional(ConditionalKind::Ifeq(a, b)));
        }
        // ifeq followed by whitespace but invalid args (e.g., "ifeq blah", "ifeq")
        return Some(ParsedLine::InvalidConditional);
    }

    if trimmed.starts_with("ifneq ") || trimmed.starts_with("ifneq\t") {
        let rest = trimmed.strip_prefix("ifneq").unwrap().trim();
        if let Some((a, b)) = parse_conditional_args(rest) {
            return Some(ParsedLine::Conditional(ConditionalKind::Ifneq(a, b)));
        }
        // ifneq followed by whitespace but invalid args (e.g., "ifneq blah", "ifneq")
        return Some(ParsedLine::InvalidConditional);
    }

    // Bare "ifeq" or "ifneq" with no following text: invalid syntax in conditional
    if trimmed == "ifeq" || trimmed == "ifneq" {
        return Some(ParsedLine::InvalidConditional);
    }

    if trimmed.starts_with("ifdef ") || trimmed.starts_with("ifdef\t") {
        let var = trimmed.strip_prefix("ifdef").unwrap().trim().to_string();
        return Some(ParsedLine::Conditional(ConditionalKind::Ifdef(var)));
    }

    if trimmed.starts_with("ifndef ") || trimmed.starts_with("ifndef\t") {
        let var = trimmed.strip_prefix("ifndef").unwrap().trim().to_string();
        return Some(ParsedLine::Conditional(ConditionalKind::Ifndef(var)));
    }

    None
}

pub fn parse_conditional_args(s: &str) -> Option<(String, String)> {
    let trimmed = s.trim();

    // Form 1: (arg1,arg2)
    if trimmed.starts_with('(') {
        let inner = &trimmed[1..];
        // Find matching closing paren, accounting for nested parens
        if let Some((a, b)) = split_conditional_parens(inner) {
            return Some((a, b));
        }
    }

    // Form 2: 'arg1' 'arg2' or "arg1" "arg2"
    if let Some((a, b)) = parse_quoted_conditional(trimmed) {
        return Some((a, b));
    }

    None
}

fn split_conditional_parens(s: &str) -> Option<(String, String)> {
    // Find the comma that splits the two args, accounting for nested variable refs.
    // We track nesting depth: `$(` or `${` opens a reference (depth += 1),
    // `)` or `}` closes a reference (depth -= 1 if depth > 0).
    // Commas at depth == 0 split the two arguments.
    let mut depth = 0i32;
    let bytes = s.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // `$(` or `${` starts a variable/function reference
        if bytes[i] == b'$' && i + 1 < bytes.len() && (bytes[i+1] == b'(' || bytes[i+1] == b'{') {
            depth += 1;
            i += 2; // skip both `$` and the opening delimiter
            continue;
        }
        match bytes[i] {
            b')' | b'}' if depth > 0 => depth -= 1,
            b',' if depth == 0 => {
                let a = s[..i].trim().to_string();
                let rest = &s[i+1..];
                // Find closing paren of the outer ifeq(...)
                if let Some(close) = find_closing_paren(rest) {
                    let b = rest[..close].trim().to_string();
                    return Some((a, b));
                }
            }
            b')' if depth == 0 => {
                // Shouldn't normally get here without finding comma first
                break;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn find_closing_paren(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    let bytes = s.as_bytes();
    for i in 0..bytes.len() {
        match bytes[i] {
            b'(' | b'{' => depth += 1,
            b')' if depth == 0 => return Some(i),
            b')' | b'}' => depth -= 1,
            _ => {}
        }
    }
    None
}

fn parse_quoted_conditional(s: &str) -> Option<(String, String)> {
    let trimmed = s.trim();
    let bytes = trimmed.as_bytes();

    if bytes.is_empty() {
        return None;
    }

    let quote = bytes[0];
    if quote != b'\'' && quote != b'"' {
        return None;
    }

    // Find end of first quoted string
    let mut i = 1;
    while i < bytes.len() && bytes[i] != quote {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    let a = trimmed[1..i].to_string();

    // Skip whitespace and find second quoted string
    i += 1;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }

    let quote2 = bytes[i];
    if quote2 != b'\'' && quote2 != b'"' {
        return None;
    }
    i += 1;
    let start = i;
    while i < bytes.len() && bytes[i] != quote2 {
        i += 1;
    }
    let b = trimmed[start..i].to_string();

    Some((a, b))
}

pub fn parse_include(line: &str) -> ParsedLine {
    let ignore_missing = line.starts_with("-include") || line.starts_with("sinclude");
    let rest = if line.starts_with("-include") {
        &line["-include".len()..]
    } else if line.starts_with("sinclude") {
        &line["sinclude".len()..]
    } else {
        &line["include".len()..]
    };

    let paths: Vec<String> = rest.split_whitespace().map(String::from).collect();

    ParsedLine::Include {
        paths,
        ignore_missing,
    }
}

pub fn parse_vpath(line: &str) -> ParsedLine {
    let rest = line.strip_prefix("vpath").unwrap().trim();

    if rest.is_empty() {
        return ParsedLine::VpathDirective {
            pattern: None,
            directories: Vec::new(),
        };
    }

    let mut parts = rest.splitn(2, |c: char| c.is_whitespace());
    let pattern = parts.next().map(String::from);
    let dirs = parts
        .next()
        .map(|d| {
            d.split(':')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    ParsedLine::VpathDirective {
        pattern,
        directories: dirs,
    }
}

pub fn parse_export(line: &str, is_export: bool) -> ParsedLine {
    let keyword = if is_export { "export" } else { "unexport" };
    let rest = line.strip_prefix(keyword).unwrap().trim();

    if rest.is_empty() {
        if is_export {
            return ParsedLine::ExportDirective {
                names: Vec::new(),
                export: true,
            };
        } else {
            return ParsedLine::UnExport {
                names: Vec::new(),
            };
        }
    }

    // Check if it's `export VAR = value` or even `export = value` (where the
    // variable name IS "export").
    let ops = [":::=", "::=", "!=", "?=", "+=", ":=", "="];
    for op in &ops {
        if let Some(pos) = find_assignment_op_in(rest, op) {
            let var_name_raw = rest[..pos].trim();
            // If the name portion is empty, the keyword itself is the variable name
            // (e.g. `export = 123` defines a variable literally named "export").
            let var_name = if var_name_raw.is_empty() {
                keyword.to_string()
            } else {
                var_name_raw.to_string()
            };
            let var_value = rest[pos + op.len()..].trim_start().to_string();
            let flavor = match *op {
                "=" => VarFlavor::Recursive,
                ":=" | "::=" => VarFlavor::Simple,
                ":::=" => VarFlavor::PosixSimple,
                "+=" => VarFlavor::Append,
                "?=" => VarFlavor::Conditional,
                "!=" => VarFlavor::Shell,
                _ => VarFlavor::Recursive,
            };
            return ParsedLine::VariableAssignment {
                name: var_name,
                value: var_value,
                flavor,
                is_override: false,
                is_export: is_export,
                is_unexport: !is_export,
                is_private: false,
                target: None,
            };
        }
    }

    let names: Vec<String> = rest.split_whitespace().map(String::from).collect();

    if is_export {
        ParsedLine::ExportDirective {
            names,
            export: true,
        }
    } else {
        ParsedLine::UnExport { names }
    }
}

pub fn parse_define_start(line: &str) -> ParsedLine {
    let mut is_override = false;
    let mut is_export = false;
    let mut work = line.trim().to_string();

    // Strip leading modifiers before "define"
    loop {
        if work.starts_with("override ") {
            is_override = true;
            work = work["override ".len()..].trim_start().to_string();
        } else if work.starts_with("export ") {
            is_export = true;
            work = work["export ".len()..].trim_start().to_string();
        } else {
            break;
        }
    }

    // Now strip "define"
    let rest = work.strip_prefix("define").unwrap_or("").trim();
    let work = rest.to_string();

    // Find assignment operator in the name part.
    // The define syntax is: define NAME [OP]
    // where OP is =, :=, ::=, :::=, +=, ?=, !=
    // The operator may be attached to the name or separated by whitespace.
    // Any text after the operator is "extraneous".
    let ops = [":::=", "::=", "!=", "?=", "+=", ":=", "="];
    let mut flavor = VarFlavor::Recursive;
    let mut name = work.trim().to_string();
    let mut has_extraneous = false;

    // Try to find the op using split on whitespace approach
    // The name and op may be adjacent (define multi=) or separated (define multi =)
    // We split on whitespace and look for an op token
    let tokens: Vec<&str> = work.split_whitespace().collect();
    if !tokens.is_empty() {
        // Check if first token ends with an op
        let first = tokens[0];
        let mut found = false;
        for op in &ops {
            if first.ends_with(op) {
                let var_name = first[..first.len() - op.len()].trim();
                name = var_name.to_string();
                flavor = op_to_flavor(op);
                // Any remaining tokens are extraneous
                if tokens.len() > 1 {
                    has_extraneous = true;
                }
                found = true;
                break;
            }
        }
        if !found && tokens.len() >= 2 {
            // Check if second token is an op
            let second = tokens[1];
            let mut found2 = false;
            for op in &ops {
                if second == *op {
                    name = first.to_string();
                    flavor = op_to_flavor(op);
                    // Any tokens after the op are extraneous
                    if tokens.len() > 2 {
                        has_extraneous = true;
                    }
                    found2 = true;
                    break;
                }
                // Op may be the start of second token (e.g., "=foo" → extraneous)
                if second.starts_with(op) && second.len() > op.len() {
                    name = first.to_string();
                    flavor = op_to_flavor(op);
                    has_extraneous = true;
                    found2 = true;
                    break;
                }
            }
            if !found2 {
                // No op found - just use whole work as name, recursive flavor
                name = work.trim().to_string();
            }
        }
    }

    ParsedLine::Define {
        name: name.trim().to_string(),
        flavor,
        is_override,
        is_export,
        has_extraneous,
    }
}

fn op_to_flavor(op: &str) -> VarFlavor {
    match op {
        "=" => VarFlavor::Recursive,
        ":=" | "::=" => VarFlavor::Simple,
        ":::=" => VarFlavor::PosixSimple,
        "+=" => VarFlavor::Append,
        "?=" => VarFlavor::Conditional,
        "!=" => VarFlavor::Shell,
        _ => VarFlavor::Recursive,
    }
}

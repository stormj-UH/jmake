// Copyright (c) 2026 Jon-Erik G. Storm. All rights reserved.
// Variable expansion engine

use crate::eval::MakeState;
use crate::functions;
use crate::types::*;
use std::collections::HashMap;

impl MakeState {
    /// Expand all variable references and function calls in a string
    pub fn expand(&self, input: &str) -> String {
        self.expand_with_auto_vars(input, &HashMap::new())
    }

    /// Expand with automatic variables ($@, $<, $^, etc.)
    pub fn expand_with_auto_vars(&self, input: &str, auto_vars: &HashMap<String, String>) -> String {
        let mut result = String::with_capacity(input.len());
        let bytes = input.as_bytes();
        let mut i = 0;

        while i < bytes.len() {
            if bytes[i] == b'$' && i + 1 < bytes.len() {
                match bytes[i + 1] {
                    b'$' => {
                        // $$ -> literal $
                        result.push('$');
                        i += 2;
                    }
                    b'(' | b'{' => {
                        // $(var) or ${var} or $(function args)
                        let close = if bytes[i + 1] == b'(' { b')' } else { b'}' };
                        let start = i + 2;
                        if let Some(end) = find_matching_close(bytes, start, bytes[i + 1], close) {
                            let content = &input[start..end];
                            let expanded = self.expand_reference(content, auto_vars);
                            result.push_str(&expanded);
                            i = end + 1;
                        } else {
                            // Unmatched paren - literal
                            result.push('$');
                            i += 1;
                        }
                    }
                    b'@' | b'<' | b'^' | b'+' | b'*' | b'?' | b'%' => {
                        // Single-character automatic variables
                        let var_name = (bytes[i + 1] as char).to_string();
                        if let Some(val) = auto_vars.get(&var_name) {
                            result.push_str(val);
                        } else if let Some(var) = self.db.variables.get(&var_name) {
                            result.push_str(&self.expand_var_value(var, auto_vars));
                        }
                        i += 2;
                    }
                    ch if ch.is_ascii_alphanumeric() || ch == b'_' => {
                        // Single character variable reference $X
                        let var_name = (ch as char).to_string();
                        if let Some(val) = auto_vars.get(&var_name) {
                            result.push_str(val);
                        } else if let Some(var) = self.db.variables.get(&var_name) {
                            result.push_str(&self.expand_var_value(var, auto_vars));
                        }
                        i += 2;
                    }
                    _ => {
                        result.push('$');
                        i += 1;
                    }
                }
            } else {
                result.push(bytes[i] as char);
                i += 1;
            }
        }

        result
    }

    fn expand_reference(&self, content: &str, auto_vars: &HashMap<String, String>) -> String {
        // Check for substitution reference: $(var:pattern=replacement)
        if let Some(colon_pos) = find_subst_ref_colon(content) {
            let var_name = &content[..colon_pos];
            let rest = &content[colon_pos + 1..];
            if let Some(eq_pos) = rest.find('=') {
                let pattern = &rest[..eq_pos];
                let replacement = &rest[eq_pos + 1..];
                return self.expand_substitution_ref(var_name, pattern, replacement, auto_vars);
            }
        }

        // Check for function call: $(function arg1,arg2,...)
        if let Some(space_pos) = content.find(|c: char| c.is_whitespace()) {
            let func_name = &content[..space_pos];
            let func_args_str = &content[space_pos + 1..];

            let builtins = functions::get_builtin_functions();
            if builtins.contains_key(func_name) {
                return self.expand_function(func_name, func_args_str, auto_vars);
            }
        }

        // Check for function call with comma args but no space (shouldn't normally happen, but...)
        // Most function calls have: $(func arg) format

        // Variable reference with possible D/F modifier
        // $(@D), $(@F), etc.
        if content.len() >= 2 {
            let first = &content[..content.len()-1];
            let modifier = &content[content.len()-1..];
            if (modifier == "D" || modifier == "F") && first.len() == 1 {
                let base_char = first;
                let base_val = if let Some(val) = auto_vars.get(base_char) {
                    val.clone()
                } else if let Some(var) = self.db.variables.get(base_char) {
                    self.expand_var_value(var, auto_vars)
                } else {
                    String::new()
                };

                return match modifier {
                    "D" => dir_part(&base_val),
                    "F" => file_part(&base_val),
                    _ => unreachable!(),
                };
            }
        }

        // Simple variable lookup
        let expanded_name = self.expand_with_auto_vars(content, auto_vars);
        if let Some(val) = auto_vars.get(&expanded_name) {
            return val.clone();
        }
        if let Some(var) = self.db.variables.get(&expanded_name) {
            return self.expand_var_value(var, auto_vars);
        }

        // Warn if requested
        if self.args.warn_undefined_variables {
            eprintln!("jmake: warning: undefined variable '{}'", expanded_name);
        }

        String::new()
    }

    fn expand_var_value(&self, var: &Variable, auto_vars: &HashMap<String, String>) -> String {
        match var.flavor {
            VarFlavor::Recursive => self.expand_with_auto_vars(&var.value, auto_vars),
            VarFlavor::Simple => var.value.clone(),
            _ => self.expand_with_auto_vars(&var.value, auto_vars),
        }
    }

    fn expand_substitution_ref(&self, var_name: &str, pattern: &str, replacement: &str, auto_vars: &HashMap<String, String>) -> String {
        let expanded_name = self.expand_with_auto_vars(var_name, auto_vars);
        let var_value = if let Some(val) = auto_vars.get(&expanded_name) {
            val.clone()
        } else if let Some(var) = self.db.variables.get(&expanded_name) {
            self.expand_var_value(var, auto_vars)
        } else {
            return String::new();
        };

        // Apply substitution
        let pattern = self.expand_with_auto_vars(pattern, auto_vars);
        let replacement = self.expand_with_auto_vars(replacement, auto_vars);

        // If pattern doesn't contain %, add % at beginning
        let full_pattern = if pattern.contains('%') {
            pattern
        } else {
            format!("%{}", pattern)
        };
        let full_replacement = if replacement.contains('%') {
            replacement
        } else {
            format!("%{}", replacement)
        };

        let words: Vec<&str> = var_value.split_whitespace().collect();
        let results: Vec<String> = words.iter()
            .map(|w| functions::patsubst_word(w, &full_pattern, &full_replacement))
            .collect();
        results.join(" ")
    }

    fn expand_function(&self, name: &str, args_str: &str, auto_vars: &HashMap<String, String>) -> String {
        let builtins = functions::get_builtin_functions();

        // Handle special functions that need access to state
        match name {
            "eval" => {
                let expanded = self.expand_with_auto_vars(args_str, auto_vars);
                // We can't mutate self here, so we'll need to handle this differently
                // For now, return empty and schedule eval
                // In practice, $(eval) needs special handling in the main loop
                return String::new();
            }
            "value" => {
                let var_name = args_str.trim();
                let expanded_name = self.expand_with_auto_vars(var_name, auto_vars);
                if let Some(var) = self.db.variables.get(&expanded_name) {
                    return var.value.clone(); // unexpanded
                }
                return String::new();
            }
            "origin" => {
                let var_name = args_str.trim();
                let expanded_name = self.expand_with_auto_vars(var_name, auto_vars);
                if let Some(var) = self.db.variables.get(&expanded_name) {
                    return match var.origin {
                        VarOrigin::Default => "default".into(),
                        VarOrigin::Environment => "environment".into(),
                        VarOrigin::File => "file".into(),
                        VarOrigin::CommandLine => "command line".into(),
                        VarOrigin::Override => "override".into(),
                        VarOrigin::Automatic => "automatic".into(),
                    };
                }
                return "undefined".into();
            }
            "flavor" => {
                let var_name = args_str.trim();
                let expanded_name = self.expand_with_auto_vars(var_name, auto_vars);
                if let Some(var) = self.db.variables.get(&expanded_name) {
                    return match var.flavor {
                        VarFlavor::Recursive => "recursive".into(),
                        VarFlavor::Simple => "simple".into(),
                        _ => "recursive".into(),
                    };
                }
                return "undefined".into();
            }
            "call" => {
                return self.expand_call(args_str, auto_vars);
            }
            "foreach" => {
                return self.expand_foreach(args_str, auto_vars);
            }
            "if" => {
                let args = split_function_args(args_str);
                if args.is_empty() { return String::new(); }
                let condition = self.expand_with_auto_vars(&args[0], auto_vars);
                if !condition.trim().is_empty() {
                    if args.len() > 1 {
                        return self.expand_with_auto_vars(&args[1], auto_vars);
                    }
                } else if args.len() > 2 {
                    return self.expand_with_auto_vars(&args[2], auto_vars);
                }
                return String::new();
            }
            "or" => {
                let args = split_function_args(args_str);
                for arg in &args {
                    let expanded = self.expand_with_auto_vars(arg, auto_vars);
                    if !expanded.trim().is_empty() {
                        return expanded;
                    }
                }
                return String::new();
            }
            "and" => {
                let args = split_function_args(args_str);
                let mut last = String::new();
                for arg in &args {
                    last = self.expand_with_auto_vars(arg, auto_vars);
                    if last.trim().is_empty() {
                        return String::new();
                    }
                }
                return last;
            }
            _ => {}
        }

        // Standard function handling
        let raw_args = split_function_args(args_str);
        let expanded_args: Vec<String> = raw_args.iter()
            .map(|a| self.expand_with_auto_vars(a, auto_vars))
            .collect();

        if let Some((handler, min_args, max_args)) = builtins.get(name) {
            let expand_fn = |s: &str| -> String {
                self.expand_with_auto_vars(s, auto_vars)
            };
            return handler(&expanded_args, &expand_fn);
        }

        String::new()
    }

    fn expand_call(&self, args_str: &str, auto_vars: &HashMap<String, String>) -> String {
        let args = split_function_args(args_str);
        if args.is_empty() {
            return String::new();
        }

        let var_name = self.expand_with_auto_vars(&args[0], auto_vars).trim().to_string();
        let var_value = if let Some(var) = self.db.variables.get(&var_name) {
            var.value.clone()
        } else {
            return String::new();
        };

        // Set up $1, $2, etc.
        let mut call_auto_vars = auto_vars.clone();
        call_auto_vars.insert("0".into(), var_name);
        for (i, arg) in args.iter().skip(1).enumerate() {
            let expanded = self.expand_with_auto_vars(arg, auto_vars);
            call_auto_vars.insert((i + 1).to_string(), expanded);
        }

        self.expand_with_auto_vars(&var_value, &call_auto_vars)
    }

    fn expand_foreach(&self, args_str: &str, auto_vars: &HashMap<String, String>) -> String {
        let args = split_function_args(args_str);
        if args.len() < 3 {
            return String::new();
        }

        let var = self.expand_with_auto_vars(&args[0], auto_vars).trim().to_string();
        let list = self.expand_with_auto_vars(&args[1], auto_vars);
        let body = &args[2];

        let words: Vec<&str> = list.split_whitespace().collect();
        let results: Vec<String> = words.iter().map(|word| {
            let mut loop_auto_vars = auto_vars.clone();
            // We need to temporarily set the variable
            // Since we can't mutate self, we'll do string replacement
            let substituted = body.replace(&format!("$({})", var), word)
                                 .replace(&format!("${{{}}}", var), word);
            self.expand_with_auto_vars(&substituted, &loop_auto_vars)
        }).collect();
        results.join(" ")
    }
}

fn find_matching_close(bytes: &[u8], start: usize, open: u8, close: u8) -> Option<usize> {
    let mut depth = 1;
    let mut i = start;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && (bytes[i+1] == b'(' || bytes[i+1] == b'{') {
            // Nested variable reference
            let nested_open = bytes[i+1];
            let nested_close = if nested_open == b'(' { b')' } else { b'}' };
            i += 2;
            let mut nested_depth = 1;
            while i < bytes.len() && nested_depth > 0 {
                if bytes[i] == nested_open { nested_depth += 1; }
                else if bytes[i] == nested_close { nested_depth -= 1; }
                if nested_depth > 0 { i += 1; }
            }
            if i < bytes.len() { i += 1; }
            continue;
        }
        if bytes[i] == open { depth += 1; }
        else if bytes[i] == close { depth -= 1; }
        if depth == 0 { return Some(i); }
        i += 1;
    }
    None
}

fn find_subst_ref_colon(content: &str) -> Option<usize> {
    // Find ':' that's a substitution reference, not inside a function call
    let bytes = content.as_bytes();
    let mut i = 0;

    // First check if this looks like a variable name followed by :pattern=replacement
    // It should NOT contain spaces (that would make it a function call)
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' => return None, // It's a function call, not subst ref
            b':' => return Some(i),
            b'$' => {
                // Skip variable references
                if i + 1 < bytes.len() && (bytes[i+1] == b'(' || bytes[i+1] == b'{') {
                    let close = if bytes[i+1] == b'(' { b')' } else { b'}' };
                    if let Some(end) = find_matching_close(bytes, i+2, bytes[i+1], close) {
                        i = end + 1;
                        continue;
                    }
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

pub fn split_function_args(s: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut depth = 0i32;

    while i < bytes.len() {
        match bytes[i] {
            b'$' if i + 1 < bytes.len() && (bytes[i+1] == b'(' || bytes[i+1] == b'{') => {
                depth += 1;
                current.push('$');
                current.push(bytes[i+1] as char);
                i += 2;
            }
            b'(' | b'{' if depth > 0 => {
                depth += 1;
                current.push(bytes[i] as char);
                i += 1;
            }
            b')' | b'}' if depth > 0 => {
                depth -= 1;
                current.push(bytes[i] as char);
                i += 1;
            }
            b',' if depth == 0 => {
                args.push(current.clone());
                current.clear();
                i += 1;
            }
            _ => {
                current.push(bytes[i] as char);
                i += 1;
            }
        }
    }
    args.push(current);
    args
}

fn dir_part(path: &str) -> String {
    let words: Vec<&str> = path.split_whitespace().collect();
    let results: Vec<String> = words.iter().map(|w| {
        match w.rfind('/') {
            Some(pos) => w[..=pos].to_string(),
            None => "./".to_string(),
        }
    }).collect();
    results.join(" ")
}

fn file_part(path: &str) -> String {
    let words: Vec<&str> = path.split_whitespace().collect();
    let results: Vec<String> = words.iter().map(|w| {
        match w.rfind('/') {
            Some(pos) => w[pos+1..].to_string(),
            None => w.to_string(),
        }
    }).collect();
    results.join(" ")
}

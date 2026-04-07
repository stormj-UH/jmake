// Copyright (c) 2026 Jon-Erik G. Storm. All rights reserved.
// Variable expansion engine

use crate::eval::MakeState;
use crate::eval::make_progname;
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
                            // Unmatched opening paren/brace: unterminated variable reference.
                            // If the content starts with a known function name, emit a
                            // function-specific error message (matching GNU Make behavior).
                            let partial = &input[start..];
                            let func_name = if let Some(sp) = partial.find(|c: char| c.is_whitespace() || c == ',') {
                                let candidate = &partial[..sp];
                                if functions::get_builtin_functions().contains_key(candidate) {
                                    Some(candidate.to_string())
                                } else {
                                    None
                                }
                            } else {
                                None
                            };
                            let close_char = if bytes[i + 1] == b'(' { "')'" } else { "'}'" };
                            let file = self.current_file.borrow();
                            let line = *self.current_line.borrow();
                            let location = if file.is_empty() {
                                None
                            } else if line == 0 {
                                Some(file.clone())
                            } else {
                                Some(format!("{}:{}", *file, line))
                            };
                            if let Some(fname) = func_name {
                                let msg = format!("unterminated call to function '{}': missing {}.",
                                    fname, close_char);
                                if let Some(loc) = location {
                                    eprintln!("{}: *** {}.  Stop.", loc, msg.trim_end_matches('.'));
                                } else {
                                    eprintln!("{}: *** {}.  Stop.", make_progname(), msg.trim_end_matches('.'));
                                }
                            } else if let Some(loc) = location {
                                eprintln!("{}: *** unterminated variable reference.  Stop.", loc);
                            } else {
                                eprintln!("{}: *** unterminated variable reference.  Stop.", make_progname());
                            }
                            std::process::exit(2);
                        }
                    }
                    b'@' | b'<' | b'^' | b'+' | b'*' | b'?' | b'%' | b'|' => {
                        // Single-character automatic variables (including $| for order-only)
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
                    b' ' | b'\t' | b'\n' | b'\r' => {
                        // $<space> is NOT a variable reference; keep the $ literal.
                        result.push('$');
                        i += 1;
                    }
                    _ => {
                        // $X where X is any other char (e.g., $., $/, $!, etc.)
                        // GNU Make expands these as single-char variable references.
                        let var_name = (bytes[i + 1] as char).to_string();
                        if let Some(val) = auto_vars.get(&var_name) {
                            result.push_str(val);
                        } else if let Some(var) = self.db.variables.get(&var_name) {
                            result.push_str(&self.expand_var_value(var, auto_vars));
                        }
                        // If not found, expand to empty string (no output)
                        i += 2;
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
        // GNU Make allows `\:` as the separator, with the `\` stripped from the var name.
        // e.g. $(@\:%=%.bar) has var_name="@", pattern="%", replacement="%.bar"
        if let Some(colon_pos) = find_subst_ref_colon(content) {
            let raw_var_name = &content[..colon_pos];
            // Strip trailing `\` from var name (backslash-quoted colon separator).
            let var_name = raw_var_name.trim_end_matches('\\');
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
        // .SHELLSTATUS is dynamically maintained from the last $(shell) call.
        // Before any $(shell ...) call, .SHELLSTATUS expands to empty string.
        if expanded_name == ".SHELLSTATUS" {
            return match *self.last_shell_status.borrow() {
                Some(status) => status.to_string(),
                None => String::new(),
            };
        }
        // .VARIABLES expands to a space-separated list of all currently defined variables.
        if expanded_name == ".VARIABLES" {
            let mut names: Vec<&str> = self.db.variables.keys().map(|k| k.as_str()).collect();
            names.sort();
            return names.join(" ");
        }
        if let Some(var) = self.db.variables.get(&expanded_name) {
            return self.expand_var_value(var, auto_vars);
        }

        // Warn if requested — but suppress warnings for GNU Make's special/auto-set
        // variables that are always "defined" (even if empty) in GNU Make.  Users should
        // not get warnings for referencing these standard variables.
        const BUILTIN_VARS: &[&str] = &[
            // Automatic variables (always set by make)
            ".VARIABLES", "MAKECMDGOALS", "MAKE_RESTARTS", "CURDIR",
            "GNUMAKEFLAGS", "MAKEFLAGS", "MFLAGS", "MAKE_COMMAND", "MAKE",
            "MAKEFILE_LIST", "MAKEOVERRIDES", "-*-command-variables-*-",
            ".RECIPEPREFIX", ".LOADED", ".FEATURES",
            "SHELL", ".SHELLFLAGS", "MAKE_TERMOUT", "MAKE_TERMERR",
            ".DEFAULT", ".DEFAULT_GOAL", "-*-eval-flags-*-", "SUFFIXES",
            "VPATH", "GPATH",
            // Additional special variables
            "MAKELEVEL", "MAKEFILES", "MAKEFILE", "MAKEINFO",
            ".LIBPATTERNS", ".DEFAULT_GOAL", "MAKE_VERSION",
            "MAKELEVEL", "MAKEFILE_LIST",
        ];
        let is_builtin = BUILTIN_VARS.contains(&expanded_name.as_str());
        if self.args.warn_undefined_variables && !is_builtin {
            // Use the outermost caller context (expansion_caller_stack) when available,
            // as it reflects the location in the user's makefile where the expansion
            // was triggered (not the variable's definition site).
            // Fall back to current_file/current_line if no caller stack entry.
            let loc = {
                let stack = self.expansion_caller_stack.borrow();
                if let Some((cf, cl)) = stack.first() {
                    if !cf.is_empty() && *cl > 0 {
                        format!("{}:{}: ", cf, cl)
                    } else {
                        String::new()
                    }
                } else {
                    let file = self.current_file.borrow();
                    let line = *self.current_line.borrow();
                    if !file.is_empty() && line > 0 {
                        format!("{}:{}: ", file, line)
                    } else {
                        String::new()
                    }
                }
            };
            if loc.is_empty() {
                eprintln!("jmake: warning: undefined variable '{}'", expanded_name);
            } else {
                eprintln!("{}warning: undefined variable '{}'", loc, expanded_name);
            }
        }

        String::new()
    }

    fn expand_var_value(&self, var: &Variable, auto_vars: &HashMap<String, String>) -> String {
        match var.flavor {
            VarFlavor::Recursive => {
                // Guard against infinite recursion: if this variable is already being expanded
                // (e.g. VARIABLE = $(eval VARIABLE := foo)$(VARIABLE) where eval hasn't run yet),
                // return empty string instead of recursing infinitely.
                // We key on (source_file, source_line, value) to uniquely identify the var instance.
                let recursion_key = if !var.source_file.is_empty() && var.source_line != 0 {
                    format!("{}:{}", var.source_file, var.source_line)
                } else {
                    var.value.clone()
                };
                {
                    let mut being_expanded = self.vars_being_expanded.borrow_mut();
                    if being_expanded.contains(&recursion_key) {
                        // Circular expansion detected — return empty to break the cycle.
                        return String::new();
                    }
                    being_expanded.insert(recursion_key.clone());
                }

                // For lazily-expanded variables, temporarily set current_file/current_line
                // to the variable's definition site so that errors (e.g. from $(word ...) or
                // $(wordlist ...)) report the location where the function was written, not
                // where the variable was referenced from.
                //
                // We push the caller's (file, line) onto the expansion_caller_stack so that
                // functions like $(error) and $(warning) — which should report the callsite
                // rather than the definition — can restore the outer context.
                let result = if !var.source_file.is_empty() && var.source_line != 0 {
                    let saved_file = self.current_file.borrow().clone();
                    let saved_line = *self.current_line.borrow();
                    self.expansion_caller_stack.borrow_mut().push((saved_file.clone(), saved_line));
                    *self.current_file.borrow_mut() = var.source_file.clone();
                    *self.current_line.borrow_mut() = var.source_line;
                    // Clone the value so we don't hold a reference into self.db.variables
                    // while expanding (eval may modify the variable database).
                    let value = var.value.clone();
                    let r = self.expand_with_auto_vars(&value, auto_vars);
                    self.expansion_caller_stack.borrow_mut().pop();
                    *self.current_file.borrow_mut() = saved_file;
                    *self.current_line.borrow_mut() = saved_line;
                    r
                } else {
                    let value = var.value.clone();
                    self.expand_with_auto_vars(&value, auto_vars)
                };

                self.vars_being_expanded.borrow_mut().remove(&recursion_key);
                result
            }
            VarFlavor::Simple | VarFlavor::PosixSimple => var.value.clone(),
            _ => {
                let value = var.value.clone();
                self.expand_with_auto_vars(&value, auto_vars)
            }
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
                // GNU Make does not allow eval to define prerequisites from within
                // a second expansion prereq context or from within a recipe (Savannah #12124).
                // It would modify the rule database in an unsafe context.
                let in_se = *self.in_second_expansion.borrow();
                let in_recipe = *self.in_recipe_execution.borrow();
                if in_se || in_recipe {
                    let expanded = self.expand_with_auto_vars(args_str, auto_vars);
                    if !expanded.is_empty() {
                        // Check if it looks like a rule/prerequisite definition.
                        // Variable assignments containing ':=' or '::=' are NOT rules.
                        // Only flag lines where ':' appears as a rule separator (not `:=`).
                        let looks_like_rule = expanded.lines().any(|line| {
                            let t = line.trim();
                            if t.is_empty() || t.starts_with('#') {
                                return false;
                            }
                            // Skip known non-rule directives
                            if t.starts_with("override ")
                                || t.starts_with("export ")
                                || t.starts_with("unexport ")
                                || t.starts_with("vpath ")
                                || t.starts_with("include ")
                                || t.starts_with("-include ")
                                || t.starts_with("sinclude ")
                                || t.starts_with("define ")
                                || t.starts_with("undefine ")
                            {
                                return false;
                            }
                            // Skip variable assignments: foo := ..., foo ?= ..., foo += ..., etc.
                            // A simple heuristic: if the first ':' in the line is immediately
                            // followed by '=' or ':', it's an assignment or double-colon rule
                            // being defined (double-colon rules ARE allowed in eval in recipes?).
                            // The real check: does this line define NEW prerequisites?
                            // We detect that by finding a ':' not followed by '=' or ':'.
                            if let Some(colon_pos) = t.find(':') {
                                let after = &t[colon_pos + 1..];
                                // :=, ::=, := are assignment operators — not rules
                                if after.starts_with('=') {
                                    return false;
                                }
                                // Check for simple assignment operators before the colon
                                // e.g. "VAR ?= val", "VAR += val", "VAR != cmd"
                                let before_colon = t[..colon_pos].trim_end();
                                if before_colon.ends_with('?')
                                    || before_colon.ends_with('+')
                                    || before_colon.ends_with('!')
                                {
                                    return false;
                                }
                                // ':' that is not part of an assignment → looks like a rule
                                return true;
                            }
                            false
                        });
                        if looks_like_rule {
                            let progname = crate::eval::make_progname();
                            eprintln!("{}: *** prerequisites cannot be defined in recipes.  Stop.", progname);
                            std::process::exit(2);
                        }
                    }
                    // When inside a recipe or second-expansion context, execute the eval
                    // immediately via the same unsafe path as the normal case (below).
                    // Variable assignments (the common case here) are safe to execute immediately.
                    let result = unsafe {
                        let self_ptr: *mut Self = self as *const Self as *mut Self;
                        (*self_ptr).eval_string(&expanded)
                    };
                    if let Err(e) = result {
                        if !e.is_empty() {
                            eprintln!("{}", e);
                        }
                    }
                } else {
                    let expanded = self.expand_with_auto_vars(args_str, auto_vars);
                    // Apply a second expansion pass for $(call)/$(foreach)/$(let) context vars.
                    // If we're inside a $(call), auto_vars contains $1, $2, etc.
                    // After the first expansion pass, `$$1` → `$1` (literal dollar-one).
                    // GNU Make resolves `$1` in the eval content using the call context.
                    // Similarly, if we're inside a $(foreach x,...) or $(let VAR,val,...),
                    // auto_vars contains the loop/let variable values. Any $(var) in the
                    // eval content (typically from $(value VAR) which returns unexpanded
                    // text) must be resolved with the current iteration value.
                    //
                    // We do a second expansion pass, but ONLY for the non-recipe parts
                    // of each line (recipe lines start with \t or come after `;`).
                    // This prevents let/foreach variable bindings from incorrectly
                    // expanding variable references that are meant to be evaluated at
                    // build time (e.g., $(AR) in a recipe when AR is bound by $(let)).
                    let final_content = if !auto_vars.is_empty() && expanded.contains('$') {
                        // Process line by line, expanding only non-recipe parts.
                        let lines: Vec<&str> = expanded.split('\n').collect();
                        let processed: Vec<String> = lines.iter().map(|line| {
                            if line.starts_with('\t') {
                                // Pure recipe line: do not expand
                                line.to_string()
                            } else if let Some(semi_pos) = crate::parser::find_semicolon(line) {
                                // Has inline recipe: expand only the header part
                                let header = &line[..semi_pos];
                                let recipe = &line[semi_pos..]; // includes the ';'
                                let expanded_header = if header.contains('$') {
                                    self.expand_with_auto_vars(header, auto_vars)
                                } else {
                                    header.to_string()
                                };
                                format!("{}{}", expanded_header, recipe)
                            } else {
                                // Non-recipe line: expand fully
                                if line.contains('$') {
                                    self.expand_with_auto_vars(line, auto_vars)
                                } else {
                                    line.to_string()
                                }
                            }
                        }).collect();
                        processed.join("\n")
                    } else {
                        expanded
                    };
                    // When inside a $(foreach)/$(call)/$(let) context (auto_vars is non-empty),
                    // execute eval IMMEDIATELY so that variable changes (e.g. `$(eval res:=...)`)
                    // are visible to subsequent loop iterations.
                    // When at the top level (auto_vars is empty), defer via eval_pending so
                    // that any current_rule pending in the outer process_parsed_lines loop is
                    // registered BEFORE the eval'd rules (preserving definition order for
                    // double-colon rules like `all:: ; @echo it` followed by `$(eval all:: ; @echo worked)`).
                    if !auto_vars.is_empty() {
                        // Inside call/foreach/let: execute immediately.
                        // SAFETY: eval_string only inserts/modifies entries in self.db.variables
                        // (via IndexMap); it does NOT remove entries nor reallocate while we hold
                        // a live &-reference into the map. We have exclusive logical access to self
                        // at this call site; no other thread is mutating self, and no live
                        // &-borrows into self.db.variables exist from the caller's stack frame.
                        let result = unsafe {
                            let self_ptr: *mut Self = self as *const Self as *mut Self;
                            (*self_ptr).eval_string(&final_content)
                        };
                        if let Err(e) = result {
                            if !e.is_empty() {
                                eprintln!("{}", e);
                            }
                        }
                    } else {
                        // Top-level (no call/foreach context): defer to eval_pending.
                        // This ensures the outer process_parsed_lines loop can flush
                        // current_rule BEFORE processing the eval'd content.
                        self.eval_pending.borrow_mut().push(final_content);
                    }
                }
                return String::new();
            }
            "info" => {
                let text = self.expand_with_auto_vars(args_str.trim_start(), auto_vars);
                println!("{}", text);
                return String::new();
            }
            "warning" => {
                let text = self.expand_with_auto_vars(args_str.trim_start(), auto_vars);
                // Use the outermost caller context (from expansion_caller_stack) when available,
                // so that $(warning) in a lazy variable reports the callsite (e.g. recipe line),
                // not the variable's definition line.
                let (file_str, line_num) = {
                    let stack = self.expansion_caller_stack.borrow();
                    if let Some((f, l)) = stack.first() {
                        (f.clone(), *l)
                    } else {
                        (self.current_file.borrow().clone(), *self.current_line.borrow())
                    }
                };
                if file_str.is_empty() {
                    eprintln!("{}", text);
                } else {
                    eprintln!("{}:{}: {}", file_str, line_num, text);
                }
                return String::new();
            }
            "error" => {
                let text = self.expand_with_auto_vars(args_str.trim_start(), auto_vars);
                // Use the outermost caller context (from expansion_caller_stack) when available,
                // so that $(error) in a lazy variable reports the callsite (e.g. recipe line),
                // not the variable's definition line.
                let (file_str, line_num) = {
                    let stack = self.expansion_caller_stack.borrow();
                    if let Some((f, l)) = stack.first() {
                        (f.clone(), *l)
                    } else {
                        (self.current_file.borrow().clone(), *self.current_line.borrow())
                    }
                };
                if file_str.is_empty() {
                    eprintln!("*** {}.  Stop.", text);
                } else {
                    eprintln!("{}:{}: *** {}.  Stop.", file_str, line_num, text);
                }
                std::process::exit(2);
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
                // Automatic variables ($@, $<, $^, etc.) are in auto_vars
                if auto_vars.contains_key(&expanded_name) {
                    return "automatic".into();
                }
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
            "let" => {
                let args = split_function_args_max(args_str, 3);
                if args.len() < 3 {
                    let file = self.current_file.borrow();
                    let line = *self.current_line.borrow();
                    let loc = if file.is_empty() { String::new() } else { format!("{}:{}: ", *file, line) };
                    eprintln!("{}*** insufficient number of arguments ({}) to function 'let'.  Stop.", loc, args.len());
                    std::process::exit(2);
                }
                let var_names_str = self.expand_with_auto_vars(&args[0], auto_vars);
                let var_names: Vec<&str> = var_names_str.split_whitespace().collect();
                let list_str = self.expand_with_auto_vars(&args[1], auto_vars);
                let body = &args[2];

                if var_names.is_empty() {
                    return self.expand_with_auto_vars(body, auto_vars);
                }

                // Bind words from list to variable names.
                // For variables before the last: use individual words (whitespace-split).
                // For the last variable: use the remaining raw text starting at the
                // beginning of that word, preserving trailing whitespace (GNU Make behavior).
                let words: Vec<&str> = list_str.split_whitespace().collect();
                let mut let_vars = auto_vars.clone();
                let num_vars = var_names.len();
                for (i, var) in var_names.iter().enumerate() {
                    let val = if i == num_vars - 1 {
                        // Last variable: find the start of word i in the raw string
                        // and take the rest of the string (preserving trailing whitespace).
                        if i < words.len() {
                            // Find the position of words[i] in list_str by scanning past
                            // the first `i` whitespace-delimited words.
                            let mut rest = list_str.as_str();
                            let mut skipped = 0;
                            while skipped < i {
                                rest = rest.trim_start();
                                // skip the next word
                                let word_end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
                                rest = &rest[word_end..];
                                skipped += 1;
                            }
                            // rest now starts at optional whitespace before words[i]
                            rest.trim_start().to_string()
                        } else {
                            String::new()
                        }
                    } else if i < words.len() {
                        words[i].to_string()
                    } else {
                        String::new()
                    };
                    let_vars.insert(var.to_string(), val);
                }
                return self.expand_with_auto_vars(body, &let_vars);
            }
            "if" => {
                let args = split_function_args_max(args_str, 3);
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
            "shell" => {
                let cmd = self.expand_with_auto_vars(args_str.trim_start(), auto_vars);
                // Execute with the makefile's exported environment so that
                // $(shell echo $$FOO) sees make-defined FOO, not just the process env.
                let (output, status) = self.shell_exec_with_env(&cmd);
                // Store exit code; variable lookup for .SHELLSTATUS checks this field.
                *self.last_shell_status.borrow_mut() = Some(status);
                return output;
            }
            "file" => {
                return self.expand_file_function(args_str, auto_vars);
            }
            _ => {}
        }

        // Validate word/wordlist/intcmp numeric arguments before dispatching
        if name == "word" || name == "wordlist" || name == "intcmp" {
            let max = match name { "word" => 2, "intcmp" => 5, _ => 3 };
            let raw_args = split_function_args_max(args_str, max);
            let expanded_args: Vec<String> = raw_args.iter()
                .map(|a| self.expand_with_auto_vars(a, auto_vars))
                .collect();
            let file = self.current_file.borrow();
            let line = *self.current_line.borrow();
            let loc = if file.is_empty() { String::new() } else { format!("{}:{}: ", file, line) };

            let validate_numeric_arg = |arg: &str, func_name: &str, ordinal: &str, allow_zero: bool| {
                let trimmed = arg.trim();
                if trimmed.is_empty() {
                    eprintln!("{}*** invalid {} argument to '{}' function: empty value.  Stop.", loc, ordinal, func_name);
                    std::process::exit(2);
                }
                match trimmed.parse::<i64>() {
                    Ok(n) if !allow_zero && n == 0 => {
                        // $(word) uses a special "must be greater than 0" message for zero;
                        // other functions (e.g. $(wordlist)) use the generic "invalid" message.
                        if func_name == "word" {
                            eprintln!("{}*** first argument to '{}' function must be greater than 0.  Stop.", loc, func_name);
                        } else {
                            eprintln!("{}*** invalid {} argument to '{}' function: '{}'.  Stop.", loc, ordinal, func_name, arg);
                        }
                        std::process::exit(2);
                    }
                    Ok(n) if n < 0 => {
                        eprintln!("{}*** invalid {} argument to '{}' function: '{}'.  Stop.", loc, ordinal, func_name, arg);
                        std::process::exit(2);
                    }
                    Err(_) => {
                        if trimmed.chars().all(|c| c.is_ascii_digit()) {
                            eprintln!("{}*** invalid {} argument to '{}' function: '{}' out of range.  Stop.", loc, ordinal, func_name, trimmed);
                        } else {
                            eprintln!("{}*** invalid {} argument to '{}' function: '{}'.  Stop.", loc, ordinal, func_name, arg);
                        }
                        std::process::exit(2);
                    }
                    _ => {}
                }
            };
            if name == "word" && expanded_args.len() >= 2 {
                validate_numeric_arg(&expanded_args[0], "word", "first", false);
            } else if name == "wordlist" && expanded_args.len() >= 3 {
                validate_numeric_arg(&expanded_args[0], "wordlist", "first", false);
                validate_numeric_arg(&expanded_args[1], "wordlist", "second", true);
            } else if name == "intcmp" && expanded_args.len() >= 2 {
                // intcmp uses "non-numeric" instead of "invalid" in error messages
                let validate_intcmp_arg = |arg: &str, ordinal: &str| {
                    let trimmed = arg.trim();
                    if trimmed.is_empty() {
                        eprintln!("{}*** non-numeric {} argument to 'intcmp' function: empty value.  Stop.", loc, ordinal);
                        std::process::exit(2);
                    }
                    // intcmp accepts negative numbers and leading +/-
                    let num_part = trimmed.strip_prefix('+').or_else(|| trimmed.strip_prefix('-')).unwrap_or(trimmed);
                    if num_part.is_empty() || !num_part.chars().all(|c| c.is_ascii_digit()) {
                        eprintln!("{}*** non-numeric {} argument to 'intcmp' function: '{}'.  Stop.", loc, ordinal, trimmed);
                        std::process::exit(2);
                    }
                    // intcmp supports arbitrary precision - no overflow check
                };
                validate_intcmp_arg(&expanded_args[0], "first");
                validate_intcmp_arg(&expanded_args[1], "second");
            }
        }

        // Standard function handling
        if let Some((handler, _min_args, max_args)) = builtins.get(name) {
            let raw_args = split_function_args_max(args_str, *max_args);
            let expanded_args: Vec<String> = raw_args.iter()
                .map(|a| self.expand_with_auto_vars(a, auto_vars))
                .collect();
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
            // Not a user-defined variable — fall through to calling as a builtin.
            // The remaining args are passed as a comma-separated args_str to expand_function.
            // We pass them unexpanded so that expand_function expands them in the normal way
            // (with access to the current auto_vars context).
            if args.len() > 1 {
                let builtin_args = args[1..].join(",");
                return self.expand_function(&var_name, &builtin_args, auto_vars);
            } else {
                return self.expand_function(&var_name, "", auto_vars);
            }
        };

        // Build a new call frame: inherit non-numeric auto_vars (automatic variables
        // like @, <, ^, etc.) but do NOT inherit numeric args ($1, $2, ...) from the
        // outer call context — those must be eclipsed by the explicit args we receive.
        // Unset positions are explicitly set to empty string.
        let mut call_auto_vars: HashMap<String, String> = auto_vars
            .iter()
            .filter(|(k, _)| k.parse::<u32>().is_err())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        call_auto_vars.insert("0".into(), var_name);

        // Expand each positional arg using the *outer* auto_vars context.
        let passed_count = args.len() - 1; // number of explicit args
        for (i, arg) in args.iter().skip(1).enumerate() {
            let expanded = self.expand_with_auto_vars(arg, auto_vars);
            call_auto_vars.insert((i + 1).to_string(), expanded);
        }
        // Explicitly clear any higher positions that were not passed, up to the
        // highest number used anywhere in practice (GNU Make supports $1..$9 directly;
        // beyond that via $(10) etc., but we clear up to a reasonable limit).
        for i in (passed_count + 1)..=9 {
            call_auto_vars.entry(i.to_string()).or_insert_with(String::new);
        }

        // Push the call context so that $(eval) inside the body can access $1, $2, etc.
        // This allows `$(eval undefine $$1)` inside a define called via $(call) to correctly
        // see the call arguments (since after $$1 → $1, the eval processes $1 in this context).
        self.call_context_stack.borrow_mut().push(call_auto_vars.clone());
        let result = self.expand_with_auto_vars(&var_value, &call_auto_vars);
        self.call_context_stack.borrow_mut().pop();
        result
    }

    fn expand_foreach(&self, args_str: &str, auto_vars: &HashMap<String, String>) -> String {
        let args = split_function_args(args_str);
        if args.len() < 3 {
            let file = self.current_file.borrow();
            let line = *self.current_line.borrow();
            let loc = if file.is_empty() { String::new() } else { format!("{}:{}: ", *file, line) };
            eprintln!("{}*** insufficient number of arguments ({}) to function 'foreach'.  Stop.", loc, args.len());
            std::process::exit(2);
        }

        let var = self.expand_with_auto_vars(&args[0], auto_vars).trim().to_string();
        let list = self.expand_with_auto_vars(&args[1], auto_vars);
        let body = &args[2];

        let words: Vec<&str> = list.split_whitespace().collect();
        let results: Vec<String> = words.iter().map(|word| {
            // Replace all forms of the variable reference in the body before expanding:
            //   $(var), ${var}, and $v (single-char only)
            let mut substituted = body.replace(&format!("$({})", var), word)
                                      .replace(&format!("${{{}}}", var), word);
            // Handle single-character variable form $v (only when var is exactly one char)
            if var.len() == 1 {
                substituted = substituted.replace(&format!("${}", var), word);
            }
            // Pass the current word in auto_vars so nested expansions that reference
            // the loop variable by name (after substitution) work correctly.
            let mut loop_auto_vars = auto_vars.clone();
            loop_auto_vars.insert(var.clone(), word.to_string());
            self.expand_with_auto_vars(&substituted, &loop_auto_vars)
        }).collect();
        results.join(" ")
    }

    fn expand_file_function(&self, args_str: &str, auto_vars: &HashMap<String, String>) -> String {
        use std::io::Write;

        // Format an IO error message the way GNU Make does: just the OS error
        // description without the " (os error N)" suffix that Rust appends.
        let fmt_io_err = |e: &std::io::Error| -> String {
            let s = e.to_string();
            // Rust formats IO errors as "Description (os error N)".
            // GNU Make just shows "Description".
            if let Some(pos) = s.rfind(" (os error ") {
                s[..pos].to_string()
            } else {
                s
            }
        };

        let file_loc = {
            let f = self.current_file.borrow();
            let l = *self.current_line.borrow();
            if f.is_empty() { String::new() } else { format!("{}:{}: ", f, l) }
        };

        let fatal = |msg: &str| -> ! {
            eprintln!("{}*** {}.  Stop.", file_loc, msg);
            std::process::exit(2);
        };

        // Split into at most 2 args: the op+filename part, and the optional text part.
        // We need to know if a comma was present (args.len() > 1) even when text is empty.
        let raw_args = split_function_args_max(args_str, 2);
        let has_text_arg = raw_args.len() > 1;

        // Expand the first arg (op + filename) and the text if present.
        let op_raw = self.expand_with_auto_vars(&raw_args[0], auto_vars);
        let op_str = op_raw.trim();

        // Determine operation and filename by parsing the operator prefix.
        // Must check ">>" before ">" because ">>" starts with ">".
        let (mode, filename) = if let Some(rest) = op_str.strip_prefix(">>") {
            (">>", rest.trim())
        } else if let Some(rest) = op_str.strip_prefix('>') {
            (">", rest.trim())
        } else if let Some(rest) = op_str.strip_prefix('<') {
            ("<", rest.trim())
        } else {
            // Invalid operation
            let loc2 = {
                let f = self.current_file.borrow();
                let l = *self.current_line.borrow();
                if f.is_empty() { String::new() } else { format!("{}:{}: ", f, l) }
            };
            eprintln!("{}*** file: invalid file operation: {}.  Stop.", loc2, op_str);
            std::process::exit(2);
        };

        if filename.is_empty() {
            let loc2 = {
                let f = self.current_file.borrow();
                let l = *self.current_line.borrow();
                if f.is_empty() { String::new() } else { format!("{}:{}: ", f, l) }
            };
            eprintln!("{}*** file: missing filename.  Stop.", loc2);
            std::process::exit(2);
        }

        match mode {
            "<" => {
                // Read mode: no text arg allowed
                if has_text_arg {
                    let loc2 = {
                        let f = self.current_file.borrow();
                        let l = *self.current_line.borrow();
                        if f.is_empty() { String::new() } else { format!("{}:{}: ", f, l) }
                    };
                    eprintln!("{}*** file: too many arguments.  Stop.", loc2);
                    std::process::exit(2);
                }
                // Read file; strip exactly one trailing newline (GNU Make behavior).
                let content = std::fs::read_to_string(filename).unwrap_or_default();
                if content.ends_with('\n') {
                    content[..content.len() - 1].to_string()
                } else {
                    content
                }
            }
            ">" => {
                // Write mode (overwrite).
                // If a comma was present (has_text_arg), write text + newline (unless text
                // already ends with newline).  If no comma, write an empty file.
                let text = if has_text_arg {
                    self.expand_with_auto_vars(&raw_args[1], auto_vars)
                } else {
                    // No comma: create empty file (zero bytes).
                    match std::fs::write(filename, b"") {
                        Err(e) => {
                            let loc2 = {
                                let f = self.current_file.borrow();
                                let l = *self.current_line.borrow();
                                if f.is_empty() { String::new() } else { format!("{}:{}: ", f, l) }
                            };
                            eprintln!("{}*** open: {}: {}.  Stop.", loc2, filename, fmt_io_err(&e));
                            std::process::exit(2);
                        }
                        Ok(_) => {}
                    }
                    return String::new();
                };
                // Determine content to write.
                let content = if text.ends_with('\n') {
                    text
                } else {
                    format!("{}\n", text)
                };
                match std::fs::write(filename, content.as_bytes()) {
                    Err(e) => {
                        let loc2 = {
                            let f = self.current_file.borrow();
                            let l = *self.current_line.borrow();
                            if f.is_empty() { String::new() } else { format!("{}:{}: ", f, l) }
                        };
                        eprintln!("{}*** open: {}: {}.  Stop.", loc2, filename, fmt_io_err(&e));
                        std::process::exit(2);
                    }
                    Ok(_) => {}
                }
                String::new()
            }
            ">>" => {
                // Append mode.
                // If a comma was present with empty or non-empty text, append text + newline
                // (unless text already ends with newline).
                // If no comma, do nothing (not even create the file).
                if !has_text_arg {
                    return String::new();
                }
                let text = self.expand_with_auto_vars(&raw_args[1], auto_vars);
                // With comma but empty text, still write a newline.
                let content = if text.ends_with('\n') {
                    text
                } else {
                    format!("{}\n", text)
                };
                match std::fs::OpenOptions::new().append(true).create(true).open(filename) {
                    Err(e) => {
                        let loc2 = {
                            let f = self.current_file.borrow();
                            let l = *self.current_line.borrow();
                            if f.is_empty() { String::new() } else { format!("{}:{}: ", f, l) }
                        };
                        eprintln!("{}*** open: {}: {}.  Stop.", loc2, filename, fmt_io_err(&e));
                        std::process::exit(2);
                    }
                    Ok(mut f) => {
                        let _ = f.write_all(content.as_bytes());
                    }
                }
                String::new()
            }
            _ => String::new(),
        }
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
    // Find ':' that's a substitution reference, not inside a function call.
    // GNU Make allows '\:' as the colon separator (the backslash is stripped
    // from the variable name). We find the ':' position (including after '\').
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
    split_function_args_max(s, usize::MAX)
}

pub fn split_function_args_max(s: &str, max_args: usize) -> Vec<String> {
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
            b',' if depth == 0 && args.len() + 1 < max_args => {
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
            Some(0) => "/".to_string(),
            Some(pos) => w[..pos].to_string(),
            None => ".".to_string(),
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

// Copyright (c) 2026 Jon-Erik G. Storm. All rights reserved.
// GNU Make built-in functions

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

pub type FnHandler = fn(args: &[String], expand: &dyn Fn(&str) -> String) -> String;

pub fn get_builtin_functions() -> HashMap<String, (FnHandler, usize, usize)> {
    // (handler, min_args, max_args) - max_args 0 means unlimited
    let mut map: HashMap<String, (FnHandler, usize, usize)> = HashMap::new();

    map.insert("subst".into(), (fn_subst as FnHandler, 3, 3));
    map.insert("patsubst".into(), (fn_patsubst as FnHandler, 3, 3));
    map.insert("strip".into(), (fn_strip as FnHandler, 1, 1));
    map.insert("findstring".into(), (fn_findstring as FnHandler, 2, 2));
    map.insert("filter".into(), (fn_filter as FnHandler, 2, 2));
    map.insert("filter-out".into(), (fn_filter_out as FnHandler, 2, 2));
    map.insert("sort".into(), (fn_sort as FnHandler, 1, 1));
    map.insert("word".into(), (fn_word as FnHandler, 2, 2));
    map.insert("wordlist".into(), (fn_wordlist as FnHandler, 3, 3));
    map.insert("words".into(), (fn_words as FnHandler, 1, 1));
    map.insert("firstword".into(), (fn_firstword as FnHandler, 1, 1));
    map.insert("lastword".into(), (fn_lastword as FnHandler, 1, 1));
    map.insert("dir".into(), (fn_dir as FnHandler, 1, 1));
    map.insert("notdir".into(), (fn_notdir as FnHandler, 1, 1));
    map.insert("suffix".into(), (fn_suffix as FnHandler, 1, 1));
    map.insert("basename".into(), (fn_basename as FnHandler, 1, 1));
    map.insert("addsuffix".into(), (fn_addsuffix as FnHandler, 2, 2));
    map.insert("addprefix".into(), (fn_addprefix as FnHandler, 2, 2));
    map.insert("join".into(), (fn_join as FnHandler, 2, 2));
    map.insert("wildcard".into(), (fn_wildcard as FnHandler, 1, 1));
    map.insert("realpath".into(), (fn_realpath as FnHandler, 1, 1));
    map.insert("abspath".into(), (fn_abspath as FnHandler, 1, 1));
    map.insert("if".into(), (fn_if as FnHandler, 2, 3));
    map.insert("or".into(), (fn_or as FnHandler, 1, 0));
    map.insert("and".into(), (fn_and as FnHandler, 1, 0));
    map.insert("foreach".into(), (fn_foreach as FnHandler, 3, 3));
    map.insert("file".into(), (fn_file as FnHandler, 1, 2));
    map.insert("call".into(), (fn_call as FnHandler, 1, 0));
    map.insert("value".into(), (fn_value as FnHandler, 1, 1));
    map.insert("eval".into(), (fn_eval as FnHandler, 1, 1));
    map.insert("origin".into(), (fn_origin as FnHandler, 1, 1));
    map.insert("flavor".into(), (fn_flavor as FnHandler, 1, 1));
    map.insert("shell".into(), (fn_shell as FnHandler, 1, 1));
    map.insert("error".into(), (fn_error as FnHandler, 1, 1));
    map.insert("warning".into(), (fn_warning as FnHandler, 1, 1));
    map.insert("info".into(), (fn_info as FnHandler, 1, 1));
    map.insert("guile".into(), (fn_guile as FnHandler, 1, 1));
    map.insert("let".into(), (fn_let as FnHandler, 3, 3));
    map.insert("intcmp".into(), (fn_intcmp as FnHandler, 2, 5));

    map
}

fn fn_subst(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let from = &args[0];
    let to = &args[1];
    let text = &args[2];
    text.replace(from.as_str(), to.as_str())
}

fn fn_patsubst(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let pattern = &args[0];
    let replacement = &args[1];
    let text = &args[2];

    let words: Vec<&str> = text.split_whitespace().collect();
    let results: Vec<String> = words.iter().map(|w| patsubst_word(w, pattern, replacement)).collect();
    results.join(" ")
}

pub fn patsubst_word(word: &str, pattern: &str, replacement: &str) -> String {
    if let Some(percent_pos) = pattern.find('%') {
        let prefix = &pattern[..percent_pos];
        let suffix = &pattern[percent_pos+1..];

        if word.starts_with(prefix) && word.ends_with(suffix) && word.len() >= prefix.len() + suffix.len() {
            let stem = &word[prefix.len()..word.len()-suffix.len()];
            return replacement.replace('%', stem);
        }
    } else {
        // No %, exact match
        if word == pattern {
            return replacement.to_string();
        }
    }
    word.to_string()
}

fn fn_strip(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let words: Vec<&str> = args[0].split_whitespace().collect();
    words.join(" ")
}

fn fn_findstring(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let find = &args[0];
    let text = &args[1];
    if text.contains(find.as_str()) {
        find.clone()
    } else {
        String::new()
    }
}

fn fn_filter(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let patterns: Vec<&str> = args[0].split_whitespace().collect();
    let words: Vec<&str> = args[1].split_whitespace().collect();
    let results: Vec<&str> = words.into_iter().filter(|w| {
        patterns.iter().any(|p| pattern_matches(p, w))
    }).collect();
    results.join(" ")
}

fn fn_filter_out(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let patterns: Vec<&str> = args[0].split_whitespace().collect();
    let words: Vec<&str> = args[1].split_whitespace().collect();
    let results: Vec<&str> = words.into_iter().filter(|w| {
        !patterns.iter().any(|p| pattern_matches(p, w))
    }).collect();
    results.join(" ")
}

/// Match a GNU Make filter pattern against a word.
///
/// In GNU Make filter patterns:
///  - `\%` is an escaped literal `%` (not a wildcard).
///  - `\\` is an escaped literal `\`.
///  - A bare `%` (not preceded by `\`) is the wildcard (matches any string).
///  - Only the FIRST unescaped `%` is treated as wildcard.
///
/// This function handles all three cases and correctly matches words against
/// patterns that contain literal `%` characters.
fn pattern_matches(pattern: &str, word: &str) -> bool {
    // Parse the pattern to find the first unescaped `%` (the wildcard).
    // Build a literal prefix and suffix (with backslash-escape processing).
    let pbytes = pattern.as_bytes();
    let mut prefix_lit = Vec::new(); // literal bytes before the wildcard (or the whole pattern)
    let mut wildcard_pos: Option<usize> = None; // index IN pbytes where `%` wildcard is
    let mut i = 0;
    while i < pbytes.len() {
        if pbytes[i] == b'\\' && i + 1 < pbytes.len() {
            match pbytes[i + 1] {
                b'%' => {
                    // \% → literal %
                    prefix_lit.push(b'%');
                    i += 2;
                }
                b'\\' => {
                    // \\ → literal \
                    prefix_lit.push(b'\\');
                    i += 2;
                }
                _ => {
                    // \ followed by anything else: keep the backslash and advance one
                    prefix_lit.push(b'\\');
                    i += 1;
                }
            }
        } else if pbytes[i] == b'%' {
            // Unescaped `%`: this is the wildcard.
            wildcard_pos = Some(i);
            i += 1;
            break;
        } else {
            prefix_lit.push(pbytes[i]);
            i += 1;
        }
    }

    if wildcard_pos.is_none() {
        // No wildcard: the pattern must match the word exactly (after unescape).
        // Build the full unescaped pattern.
        let mut full = prefix_lit;
        while i < pbytes.len() {
            if pbytes[i] == b'\\' && i + 1 < pbytes.len() {
                match pbytes[i + 1] {
                    b'%' => { full.push(b'%'); i += 2; }
                    b'\\' => { full.push(b'\\'); i += 2; }
                    _ => { full.push(b'\\'); i += 1; }
                }
            } else {
                full.push(pbytes[i]);
                i += 1;
            }
        }
        return word.as_bytes() == full.as_slice();
    }

    // Build the literal suffix (the part after the `%` wildcard).
    let mut suffix_lit = Vec::new();
    while i < pbytes.len() {
        if pbytes[i] == b'\\' && i + 1 < pbytes.len() {
            match pbytes[i + 1] {
                b'%' => { suffix_lit.push(b'%'); i += 2; }
                b'\\' => { suffix_lit.push(b'\\'); i += 2; }
                _ => { suffix_lit.push(b'\\'); i += 1; }
            }
        } else {
            suffix_lit.push(pbytes[i]);
            i += 1;
        }
    }

    let wbytes = word.as_bytes();
    let plen = prefix_lit.len();
    let slen = suffix_lit.len();

    if wbytes.len() < plen + slen {
        return false;
    }
    if &wbytes[..plen] != prefix_lit.as_slice() {
        return false;
    }
    if slen > 0 && &wbytes[wbytes.len() - slen..] != suffix_lit.as_slice() {
        return false;
    }
    true
}

fn fn_sort(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let mut words: Vec<&str> = args[0].split_whitespace().collect();
    words.sort();
    words.dedup();
    words.join(" ")
}

fn fn_word(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let n: usize = args[0].trim().parse().unwrap_or(0);
    let words: Vec<&str> = args[1].split_whitespace().collect();
    if n >= 1 && n <= words.len() {
        words[n-1].to_string()
    } else {
        String::new()
    }
}

fn fn_wordlist(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let s: usize = args[0].trim().parse().unwrap_or(0);
    let e: usize = args[1].trim().parse().unwrap_or(0);
    let words: Vec<&str> = args[2].split_whitespace().collect();
    if s >= 1 && s <= words.len() && e >= s {
        let end = e.min(words.len());
        words[s-1..end].join(" ")
    } else {
        String::new()
    }
}

fn fn_words(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let count = args[0].split_whitespace().count();
    count.to_string()
}

fn fn_firstword(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    args[0].split_whitespace().next().unwrap_or("").to_string()
}

fn fn_lastword(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    args[0].split_whitespace().last().unwrap_or("").to_string()
}

fn fn_dir(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let words: Vec<&str> = args[0].split_whitespace().collect();
    let results: Vec<String> = words.iter().map(|w| {
        match w.rfind('/') {
            Some(pos) => w[..=pos].to_string(),
            None => "./".to_string(),
        }
    }).collect();
    results.join(" ")
}

fn fn_notdir(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let words: Vec<&str> = args[0].split_whitespace().collect();
    let results: Vec<String> = words.iter().map(|w| {
        match w.rfind('/') {
            Some(pos) => w[pos+1..].to_string(),
            None => w.to_string(),
        }
    }).collect();
    results.join(" ")
}

fn fn_suffix(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let words: Vec<&str> = args[0].split_whitespace().collect();
    let results: Vec<String> = words.iter().filter_map(|w| {
        let name = match w.rfind('/') {
            Some(pos) => &w[pos+1..],
            None => w,
        };
        name.rfind('.').map(|pos| name[pos..].to_string())
    }).collect();
    results.join(" ")
}

fn fn_basename(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let words: Vec<&str> = args[0].split_whitespace().collect();
    let results: Vec<String> = words.iter().map(|w| {
        let name = match w.rfind('/') {
            Some(slash_pos) => {
                let dir = &w[..=slash_pos];
                let file = &w[slash_pos+1..];
                match file.rfind('.') {
                    Some(dot_pos) => format!("{}{}", dir, &file[..dot_pos]),
                    None => w.to_string(),
                }
            }
            None => {
                match w.rfind('.') {
                    Some(pos) => w[..pos].to_string(),
                    None => w.to_string(),
                }
            }
        };
        name
    }).collect();
    results.join(" ")
}

fn fn_addsuffix(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let suffix = &args[0];
    let words: Vec<&str> = args[1].split_whitespace().collect();
    let results: Vec<String> = words.iter().map(|w| format!("{}{}", w, suffix)).collect();
    results.join(" ")
}

fn fn_addprefix(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let prefix = &args[0];
    let words: Vec<&str> = args[1].split_whitespace().collect();
    let results: Vec<String> = words.iter().map(|w| format!("{}{}", prefix, w)).collect();
    results.join(" ")
}

fn fn_join(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let list1: Vec<&str> = args[0].split_whitespace().collect();
    let list2: Vec<&str> = args[1].split_whitespace().collect();
    let max = list1.len().max(list2.len());
    let mut results = Vec::new();
    for i in 0..max {
        let a = list1.get(i).unwrap_or(&"");
        let b = list2.get(i).unwrap_or(&"");
        results.push(format!("{}{}", a, b));
    }
    results.join(" ")
}

fn fn_wildcard(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let patterns: Vec<&str> = args[0].split_whitespace().collect();
    let mut results = Vec::new();
    for pattern in patterns {
        let mut matches = Vec::new();
        // Extract the directory prefix from the pattern to preserve it in results.
        // e.g., pattern "./foo*" → prefix "./", pattern "dir/foo*" → prefix "dir/"
        // GNU Make preserves the directory prefix from the pattern in its output.
        let prefix = {
            let p = std::path::Path::new(pattern);
            if let Some(parent) = p.parent() {
                let ps = parent.to_string_lossy();
                if ps.is_empty() || ps == "." {
                    // Pattern has no explicit dir prefix, or just "./" vs no prefix.
                    // Check if the pattern literally starts with "./"
                    if pattern.starts_with("./") {
                        "./"
                    } else {
                        ""
                    }
                } else {
                    // Pattern has a real directory component like "dir/" or "../"
                    // We need the prefix up to and including the last slash
                    if let Some(slash) = pattern.rfind('/') {
                        &pattern[..slash + 1]
                    } else {
                        ""
                    }
                }
            } else {
                ""
            }
        };
        if let Ok(paths) = glob::glob(pattern) {
            for entry in paths.flatten() {
                let s = entry.to_string_lossy().to_string();
                // If the glob crate stripped the directory prefix, re-add it.
                if !prefix.is_empty() && !s.starts_with(prefix) && !s.starts_with('/') {
                    matches.push(format!("{}{}", prefix, s));
                } else {
                    matches.push(s);
                }
            }
        }
        // GNU Make sorts wildcard results lexicographically
        matches.sort();
        results.extend(matches);
    }
    results.join(" ")
}

fn fn_realpath(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let words: Vec<&str> = args[0].split_whitespace().collect();
    let results: Vec<String> = words.iter().filter_map(|w| {
        std::fs::canonicalize(w).ok().map(|p| p.to_string_lossy().to_string())
    }).collect();
    results.join(" ")
}

fn fn_abspath(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let words: Vec<&str> = args[0].split_whitespace().collect();
    let cwd = std::env::current_dir().unwrap_or_default();
    let results: Vec<String> = words.iter().map(|w| {
        let p = Path::new(w);
        if p.is_absolute() {
            normalize_path(p)
        } else {
            normalize_path(&cwd.join(p))
        }
    }).collect();
    results.join(" ")
}

fn normalize_path(path: &Path) -> String {
    let mut components = Vec::new();
    let mut is_absolute = false;
    for component in path.components() {
        match component {
            std::path::Component::RootDir => { is_absolute = true; }
            std::path::Component::ParentDir => {
                if !components.is_empty() {
                    components.pop();
                }
            }
            std::path::Component::CurDir => {}
            c => components.push(c.as_os_str().to_string_lossy().to_string()),
        }
    }
    if is_absolute {
        format!("/{}", components.join("/"))
    } else if components.is_empty() {
        ".".to_string()
    } else {
        components.join("/")
    }
}

fn fn_if(args: &[String], expand: &dyn Fn(&str) -> String) -> String {
    let condition = args[0].trim();
    if !condition.is_empty() {
        if args.len() > 1 {
            expand(&args[1])
        } else {
            String::new()
        }
    } else {
        if args.len() > 2 {
            expand(&args[2])
        } else {
            String::new()
        }
    }
}

fn fn_or(args: &[String], expand: &dyn Fn(&str) -> String) -> String {
    for arg in args {
        let expanded = expand(arg);
        if !expanded.trim().is_empty() {
            return expanded;
        }
    }
    String::new()
}

fn fn_and(args: &[String], expand: &dyn Fn(&str) -> String) -> String {
    let mut last = String::new();
    for arg in args {
        last = expand(arg);
        if last.trim().is_empty() {
            return String::new();
        }
    }
    last
}

fn fn_foreach(args: &[String], expand: &dyn Fn(&str) -> String) -> String {
    let var = args[0].trim();
    let list: Vec<&str> = args[1].split_whitespace().collect();
    let body = &args[2];

    let results: Vec<String> = list.iter().map(|word| {
        // Replace $(var), ${var}, and $v (single-char) in body with word, then expand
        let mut substituted = body.replace(&format!("$({})", var), word)
                                  .replace(&format!("${{{}}}", var), word);
        if var.len() == 1 {
            substituted = substituted.replace(&format!("${}", var), word);
        }
        expand(&substituted)
    }).collect();
    results.join(" ")
}

fn fn_file(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    let op = args[0].trim();
    let text = if args.len() > 1 { &args[1] } else { "" };

    if let Some(filename) = op.strip_prefix('>') {
        let filename = filename.trim();
        if let Some(filename) = op.strip_prefix(">>") {
            let filename = filename.trim();
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(filename) {
                let _ = write!(f, "{}", text);
                if !text.is_empty() && !text.ends_with('\n') {
                    let _ = writeln!(f);
                }
            }
        } else {
            let content = if text.is_empty() { String::new() } else { format!("{}\n", text) };
            std::fs::write(filename, &content).ok();
        }
        String::new()
    } else if let Some(filename) = op.strip_prefix('<') {
        let filename = filename.trim();
        std::fs::read_to_string(filename).unwrap_or_default()
    } else {
        String::new()
    }
}

fn fn_call(args: &[String], expand: &dyn Fn(&str) -> String) -> String {
    // $(call var,param1,param2,...) - expand $(var) with $1, $2, etc. set
    // This is handled specially in the expand engine
    // Here we just return the expanded variable with substitutions
    if args.is_empty() {
        return String::new();
    }
    // The actual implementation is in the expander since it needs variable access
    // This stub just returns the first arg expanded
    expand(&args[0])
}

fn fn_value(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    // $(value var) returns the unexpanded value - handled in expander
    args[0].clone()
}

fn fn_eval(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    // $(eval text) - parsed as makefile content; handled in expander
    // Always returns empty string
    String::new()
}

fn fn_origin(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    // Handled in expander which has access to variable database
    args[0].clone()
}

fn fn_flavor(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    // Handled in expander
    args[0].clone()
}

/// Execute a shell command, returning (stdout_processed, exit_code).
/// stdout is processed per GNU Make rules: internal newlines replaced with spaces,
/// trailing whitespace stripped.
/// `extra_env` provides additional environment variables to set (or override).
/// `remove_env` lists variable names to remove from the environment.
/// `progname` is used for error messages (e.g. "make: cmd: No such file or directory").
pub fn fn_shell_exec_with_status_env(
    cmd: &str,
    extra_env: &HashMap<String, String>,
    remove_env: &[String],
    progname: &str,
) -> (String, i32) {
    let child_makelevel = std::env::var("MAKELEVEL")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .map(|l| (l + 1).to_string())
        .unwrap_or_else(|| "1".to_string());

    let mut c = Command::new("/bin/sh");
    c.arg("-c").arg(cmd);
    c.env("MAKELEVEL", &child_makelevel);
    for name in remove_env {
        c.env_remove(name);
    }
    for (k, v) in extra_env {
        c.env(k, v);
    }

    match c.output() {
        Ok(out) => {
            // Print any stderr output (e.g. "command not found" errors).
            // GNU Make strips the "sh: " or "/bin/sh: " prefix from the shell's error
            // messages and replaces it with the make program name.
            if !out.stderr.is_empty() {
                let stderr_str = String::from_utf8_lossy(&out.stderr);
                for line in stderr_str.lines() {
                    // Strip the shell prefix (e.g. "sh: " or "/bin/sh: ")
                    let msg = strip_shell_prefix(line);
                    // Normalize non-standard error message wording
                    let normalized = normalize_shell_error_msg(msg);
                    eprintln!("{}: {}", progname, normalized);
                }
            }
            let exit_code = out.status.code().unwrap_or_else(|| {
                // Process was terminated by a signal; return 128 + signal number
                // to match the conventional shell behavior.
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    out.status.signal().map(|s| 128 + s).unwrap_or(-1)
                }
                #[cfg(not(unix))]
                { -1 }
            });
            let raw = String::from_utf8_lossy(&out.stdout).to_string();
            let result = process_shell_output(&raw);
            (result, exit_code)
        }
        Err(_) => (String::new(), 127),
    }
}

/// Strip shell error prefix ("sh: " or "/bin/sh: " or similar) from a stderr line.
fn strip_shell_prefix(line: &str) -> &str {
    // Common patterns: "sh: cmd: msg", "/bin/sh: cmd: msg", "/bin/sh: line N: cmd: msg"
    // We strip up to and including the first colon-space that comes from the shell binary name.
    if let Some(colon_pos) = line.find(": ") {
        let prefix = &line[..colon_pos];
        // Check if prefix looks like a shell binary path.
        let basename = prefix.rsplit('/').next().unwrap_or(prefix);
        let is_shell = matches!(basename, "sh" | "bash" | "ksh" | "mksh" | "dash" | "zsh" | "ash");
        if is_shell {
            let rest = &line[colon_pos + 2..];
            // Also strip "line N: " prefix if present (e.g., "/bin/sh: line 1: cmd: msg")
            if let Some(line_prefix) = rest.strip_prefix("line ") {
                if let Some(end) = line_prefix.find(": ") {
                    let digits = &line_prefix[..end];
                    if digits.chars().all(|c| c.is_ascii_digit()) {
                        // Skip "line N: " (5 + len(digits) + 2)
                        return &rest[5 + end + 2..];
                    }
                }
            }
            return rest;
        }
    }
    line
}

/// Normalize shell error message text to use standard POSIX wording.
/// Some shells (e.g. mksh) use non-standard messages like "inaccessible or not found"
/// instead of the standard "No such file or directory".  GNU Make's test suite expects
/// the standard phrasing.
fn normalize_shell_error_msg(msg: &str) -> String {
    // Replace mksh's "inaccessible or not found" with "No such file or directory".
    // The pattern is "cmd: inaccessible or not found" → "cmd: No such file or directory"
    msg.replace(": inaccessible or not found", ": No such file or directory")
}

/// Execute a shell command, returning (stdout_processed, exit_code).
/// stdout is processed per GNU Make rules: internal newlines replaced with spaces,
/// trailing whitespace stripped.
pub fn fn_shell_exec_with_status(cmd: &str) -> (String, i32) {
    fn_shell_exec_with_status_env(cmd, &HashMap::new(), &[], "make")
}

/// Process shell output per GNU Make rules:
/// 1. Strip trailing newlines from the raw output.
/// 2. Replace remaining (internal) newlines with spaces.
/// Trailing non-newline whitespace (e.g. a trailing space) is preserved.
fn process_shell_output(raw: &str) -> String {
    // Strip trailing newlines only (not spaces or other whitespace)
    let stripped = raw.trim_end_matches('\n');
    // Also handle \r\n line endings: strip trailing \r after removing \n
    let stripped = stripped.trim_end_matches('\r');
    // Replace remaining internal newlines (and \r\n) with spaces
    stripped.replace("\r\n", " ").replace('\n', " ")
}

pub fn fn_shell_exec(cmd: &str) -> String {
    fn_shell_exec_with_status(cmd).0
}

fn fn_shell(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    fn_shell_exec(&args[0])
}

fn fn_error(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    eprintln!("*** {}.  Stop.", args[0]);
    std::process::exit(2);
}

fn fn_warning(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    eprintln!("{}", args[0]);
    String::new()
}

fn fn_info(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    println!("{}", args[0]);
    String::new()
}

fn fn_guile(args: &[String], _expand: &dyn Fn(&str) -> String) -> String {
    // Guile integration is not supported
    String::new()
}

fn fn_let(args: &[String], expand: &dyn Fn(&str) -> String) -> String {
    // $(let var [var ...],list,text) - like foreach but assigns multiple vars
    let vars: Vec<&str> = args[0].split_whitespace().collect();
    let words: Vec<&str> = args[1].split_whitespace().collect();
    let body = &args[2];

    let mut substituted = body.clone();
    for (i, var) in vars.iter().enumerate() {
        let val: String = if i == vars.len() - 1 {
            // Last variable gets all remaining words
            let remaining: Vec<&str> = if i < words.len() { words[i..].to_vec() } else { vec![] };
            remaining.join(" ")
        } else if i < words.len() {
            words[i].to_string()
        } else {
            String::new()
        };
        substituted = substituted.replace(&format!("$({})", var), &val)
                                 .replace(&format!("${{{}}}", var), &val);
        if var.len() == 1 {
            substituted = substituted.replace(&format!("${}", var), &val);
        }
    }
    expand(&substituted)
}

/// Compare two arbitrary-precision integer strings. Returns -1, 0, or 1.
fn bigint_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let a = a.trim();
    let b = b.trim();

    let (a_neg, a_digits) = if let Some(s) = a.strip_prefix('-') {
        (true, s.trim_start_matches('0'))
    } else {
        (false, a.strip_prefix('+').unwrap_or(a).trim_start_matches('0'))
    };
    let (b_neg, b_digits) = if let Some(s) = b.strip_prefix('-') {
        (true, s.trim_start_matches('0'))
    } else {
        (false, b.strip_prefix('+').unwrap_or(b).trim_start_matches('0'))
    };

    // Treat empty digits as 0
    let a_zero = a_digits.is_empty();
    let b_zero = b_digits.is_empty();

    // -0 == 0
    let a_neg = if a_zero { false } else { a_neg };
    let b_neg = if b_zero { false } else { b_neg };

    if a_neg != b_neg {
        return if a_neg { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater };
    }

    // Both same sign - compare magnitude
    let mag_cmp = if a_digits.len() != b_digits.len() {
        a_digits.len().cmp(&b_digits.len())
    } else {
        a_digits.cmp(b_digits)
    };

    if a_neg { mag_cmp.reverse() } else { mag_cmp }
}

fn fn_intcmp(args: &[String], expand: &dyn Fn(&str) -> String) -> String {
    let ord = bigint_cmp(&args[0], &args[1]);

    match args.len() {
        2 => {
            if ord == std::cmp::Ordering::Equal { args[1].trim().to_string() } else { String::new() }
        }
        3 => {
            if ord == std::cmp::Ordering::Less { expand(&args[2]) } else { String::new() }
        }
        4 => {
            // With 4 args, the 4th arg (args[3]) is the "greater-or-equal" catch-all:
            // it is returned for both == and > cases.
            match ord {
                std::cmp::Ordering::Less => expand(&args[2]),
                std::cmp::Ordering::Equal | std::cmp::Ordering::Greater => expand(&args[3]),
            }
        }
        5 | _ => {
            match ord {
                std::cmp::Ordering::Less => expand(&args[2]),
                std::cmp::Ordering::Equal => expand(&args[3]),
                std::cmp::Ordering::Greater => expand(&args.get(4).cloned().unwrap_or_default()),
            }
        }
    }
}

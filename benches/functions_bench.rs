//! Quick timing benchmark for the hot built-in functions.
//! Run with: cargo bench --bench functions_bench
//!
//! Uses std::time::Instant (no criterion dependency needed).

use std::time::Instant;

// We inline simple versions of the functions to benchmark the algorithm directly.

fn old_addsuffix(suffix: &str, words_str: &str) -> String {
    let words: Vec<&str> = words_str.split_whitespace().collect();
    let results: Vec<String> = words.iter().map(|w| format!("{}{}", w, suffix)).collect();
    results.join(" ")
}

fn new_addsuffix(suffix: &str, words_str: &str) -> String {
    let mut out = String::new();
    let mut first = true;
    for w in words_str.split_whitespace() {
        if !first { out.push(' '); }
        out.push_str(w);
        out.push_str(suffix);
        first = false;
    }
    out
}

fn old_addprefix(prefix: &str, words_str: &str) -> String {
    let words: Vec<&str> = words_str.split_whitespace().collect();
    let results: Vec<String> = words.iter().map(|w| format!("{}{}", prefix, w)).collect();
    results.join(" ")
}

fn new_addprefix(prefix: &str, words_str: &str) -> String {
    let mut out = String::new();
    let mut first = true;
    for w in words_str.split_whitespace() {
        if !first { out.push(' '); }
        out.push_str(prefix);
        out.push_str(w);
        first = false;
    }
    out
}

fn old_filter_out(patterns_str: &str, words_str: &str) -> String {
    let patterns: Vec<&str> = patterns_str.split_whitespace().collect();
    let words: Vec<&str> = words_str.split_whitespace().collect();
    let results: Vec<&str> = words.into_iter().filter(|w| {
        !patterns.iter().any(|p| w.starts_with(p.trim_end_matches('%')))
    }).collect();
    results.join(" ")
}

fn new_filter_out(patterns_str: &str, words_str: &str) -> String {
    let patterns: Vec<&str> = patterns_str.split_whitespace().collect();
    let mut out = String::new();
    let mut first = true;
    for w in words_str.split_whitespace() {
        if !patterns.iter().any(|p| w.starts_with(p.trim_end_matches('%'))) {
            if !first { out.push(' '); }
            out.push_str(w);
            first = false;
        }
    }
    out
}

// ── sort ──────────────────────────────────────────────────────────────────────

fn old_sort(words_str: &str) -> String {
    let mut words: Vec<&str> = words_str.split_whitespace().collect();
    words.sort_unstable();
    words.dedup();
    words.join(" ")
}

fn new_sort(words_str: &str) -> String {
    let mut words: Vec<&str> = words_str.split_whitespace().collect();
    words.sort_unstable();
    words.dedup();
    let mut out = String::new();
    let mut first = true;
    for w in &words {
        if !first { out.push(' '); }
        out.push_str(w);
        first = false;
    }
    out
}

// ── substitution ref ──────────────────────────────────────────────────────────

/// Simplified patsubst_word: percent pattern, replaces stem.
fn patsubst_word_simple(w: &str, pattern: &str, replacement: &str) -> String {
    // only handles the `prefix%suffix` form, sufficient for bench
    let pct = pattern.find('%').unwrap_or(pattern.len());
    let prefix = &pattern[..pct];
    let suffix = if pct < pattern.len() { &pattern[pct+1..] } else { "" };
    if w.starts_with(prefix) && w.ends_with(suffix) && w.len() >= prefix.len() + suffix.len() {
        let stem = &w[prefix.len()..w.len()-suffix.len()];
        let rpct = replacement.find('%').unwrap_or(replacement.len());
        let rpfx = &replacement[..rpct];
        let rsfx = if rpct < replacement.len() { &replacement[rpct+1..] } else { "" };
        format!("{}{}{}", rpfx, stem, rsfx)
    } else {
        w.to_string()
    }
}

fn old_subst_ref(words_str: &str, pattern: &str, replacement: &str) -> String {
    let words: Vec<&str> = words_str.split_whitespace().collect();
    let results: Vec<String> = words.iter()
        .map(|w| patsubst_word_simple(w, pattern, replacement))
        .collect();
    results.join(" ")
}

fn new_subst_ref(words_str: &str, pattern: &str, replacement: &str) -> String {
    let mut out = String::new();
    let mut first = true;
    for w in words_str.split_whitespace() {
        if !first { out.push(' '); }
        out.push_str(&patsubst_word_simple(w, pattern, replacement));
        first = false;
    }
    out
}

// ── realpath/abspath (IO-less path normalization stand-in) ────────────────────

fn old_abspath_sim(words_str: &str, cwd: &str) -> String {
    let words: Vec<&str> = words_str.split_whitespace().collect();
    let results: Vec<String> = words.iter().map(|w| format!("{}/{}", cwd, w)).collect();
    results.join(" ")
}

fn new_abspath_sim(words_str: &str, cwd: &str) -> String {
    let mut out = String::new();
    let mut first = true;
    for w in words_str.split_whitespace() {
        if !first { out.push(' '); }
        out.push_str(cwd);
        out.push('/');
        out.push_str(w);
        first = false;
    }
    out
}

fn bench<F: Fn() -> String>(name: &str, iters: u32, f: F) -> f64 {
    // Warm up
    for _ in 0..10 { let _ = f(); }
    let start = Instant::now();
    let mut result = String::new();
    for _ in 0..iters {
        result = f();
    }
    let elapsed = start.elapsed().as_secs_f64();
    let ns_per_iter = (elapsed / iters as f64) * 1e9;
    println!("{:<30} {:>8.1} ns/iter  result_len={}", name, ns_per_iter, result.len());
    ns_per_iter
}

fn main() {
    let n = 2000u32;

    // Build test data: 1000 words
    let words1000: String = (1..=1000).map(|i| format!("word{}", i)).collect::<Vec<_>>().join(" ");
    let suffix = ".o";
    let prefix = "build/";
    // 50 patterns for filter
    let pats50: String = (1..=50).map(|i| format!("word{}%", i)).collect::<Vec<_>>().join(" ");

    println!("\n=== addsuffix (1000 words, suffix=.o) ===");
    let old_a = bench("old: format!()+collect+join", n, || old_addsuffix(suffix, &words1000));
    let new_a = bench("new: push_str direct",         n, || new_addsuffix(suffix, &words1000));
    println!("  speedup: {:.2}x", old_a / new_a);

    println!("\n=== addprefix (1000 words, prefix=build/) ===");
    let old_p = bench("old: format!()+collect+join", n, || old_addprefix(prefix, &words1000));
    let new_p = bench("new: push_str direct",         n, || new_addprefix(prefix, &words1000));
    println!("  speedup: {:.2}x", old_p / new_p);

    println!("\n=== filter-out (50 patterns x 1000 words) ===");
    let old_f = bench("old: collect+join",  n, || old_filter_out(&pats50, &words1000));
    let new_f = bench("new: push_str direct", n, || new_filter_out(&pats50, &words1000));
    println!("  speedup: {:.2}x", old_f / new_f);

    println!("\n=== sort+dedup (1000 words) ===");
    let old_s = bench("old: dedup+join",       n, || old_sort(&words1000));
    let new_s = bench("new: dedup+push_str",   n, || new_sort(&words1000));
    println!("  speedup: {:.2}x", old_s / new_s);

    println!("\n=== substitution-ref (1000 words, %.c -> %.o) ===");
    let pat = "%.c";
    let repl = "%.o";
    let words_c: String = (1..=1000).map(|i| format!("src{}.c", i)).collect::<Vec<_>>().join(" ");
    let old_sr = bench("old: Vec+map+join",     n, || old_subst_ref(&words_c, pat, repl));
    let new_sr = bench("new: push_str direct",  n, || new_subst_ref(&words_c, pat, repl));
    println!("  speedup: {:.2}x", old_sr / new_sr);

    println!("\n=== abspath-sim (1000 words, cwd prefix) ===");
    let cwd = "/home/user/project";
    let old_ab = bench("old: Vec+format+join",  n, || old_abspath_sim(&words1000, cwd));
    let new_ab = bench("new: push_str direct",  n, || new_abspath_sim(&words1000, cwd));
    println!("  speedup: {:.2}x", old_ab / new_ab);
}

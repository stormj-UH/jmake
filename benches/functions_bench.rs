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
}

// Copyright (c) 2026 Jon-Erik G. Storm. All rights reserved.
// Regression test for the recipe-variable double-expansion bug that broke
// autoconf-style Makefiles.  See commit log for details.
//
// Bug symptom: a recursive variable defined with `;` in its value
// (for example, `V = a=$$X; echo $$X`) had its value stored with `$$`
// already collapsed to `$`.  When the variable was later expanded in a
// recipe, the stored `$X` was re-interpreted as a make-variable reference
// (undefined → empty), producing the wrong recipe.
//
// Root cause: `process_parsed_lines` in `src/eval/mod.rs` treated any
// non-recipe line containing `;` as an inline-recipe rule and expanded the
// whole line once before storing — but a variable assignment may legally
// contain `;` in its value and must not be expanded at parse time.

use std::io::Write;
use std::process::Command;

fn jmake_bin() -> &'static str {
    // cargo sets CARGO_BIN_EXE_<name> for [[bin]] targets in integration tests.
    env!("CARGO_BIN_EXE_jmake")
}

fn run_makefile(contents: &str, args: &[&str]) -> (String, String, i32) {
    let mut f = tempfile::NamedTempFile::new().expect("temp makefile");
    f.write_all(contents.as_bytes()).expect("write mk");
    let out = Command::new(jmake_bin())
        .arg("-f").arg(f.path())
        .args(args)
        .output()
        .expect("run jmake");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

/// Minimal repro from the bug report: `$$X` in a recipe must survive as a
/// single literal `$X` (passed through to the shell), not be consumed as a
/// make-variable reference.
#[test]
fn literal_double_dollar_in_recipe_is_single_dollar() {
    let mk = "X = hello\nall:\n\t@echo 'literal dollar: $$X'\n\t@echo 'var ref:       $(X)'\n";
    let (stdout, _stderr, code) = run_makefile(mk, &[]);
    assert_eq!(code, 0, "jmake should exit cleanly");
    assert!(
        stdout.contains("literal dollar: $X"),
        "expected literal dollar: $X in output, got: {stdout:?}"
    );
    assert!(
        stdout.contains("var ref:       hello"),
        "expected var expansion hello in output, got: {stdout:?}"
    );
}

/// Recursive variable whose value contains `;` and `$$X`: must store the
/// raw `$$X` and expand to `$X` on use (autoconf's `sane_makeflags=$$MAKEFLAGS`
/// pattern).
#[test]
fn recursive_var_with_semicolon_and_dollar_dollar() {
    let mk = "V = Y=foo; echo $$Y\n.PHONY: test\ntest:\n\t@$(V)\n";
    let (stdout, stderr, code) = run_makefile(mk, &["-s"]);
    assert_eq!(code, 0, "jmake should exit 0; stderr={stderr:?}");
    assert_eq!(
        stdout.trim(),
        "foo",
        "expected 'foo' (shell expands $Y after Y=foo), got stdout={stdout:?} stderr={stderr:?}"
    );
}

/// Full autoconf pattern: recursive variable mimicking automake's
/// `am__make_running_with_option`, used from a recipe via `$(VAR)`.
#[test]
fn automake_sane_makeflags_pattern() {
    let mk = concat!(
        "am__vars = sane_makeflags=$$MAKEFLAGS; echo \"sm=[$$sane_makeflags]\"\n",
        ".PHONY: test\n",
        "test:\n",
        "\t@$(am__vars)\n",
    );
    let (stdout, stderr, code) = run_makefile(mk, &["-s"]);
    assert_eq!(code, 0, "jmake should exit 0; stderr={stderr:?}");
    // With `make -s`, MAKEFLAGS includes the silent flag `s`.
    assert!(
        stdout.contains("sm=[s]"),
        "expected sm=[s] in output (MAKEFLAGS had 's'), got: {stdout:?}"
    );
    assert!(
        !stdout.contains("ane_makeflags"),
        "output contained 'ane_makeflags' — the classic double-expansion \
         bug where $$sane_makeflags lost its leading 's'; stdout={stdout:?}"
    );
}

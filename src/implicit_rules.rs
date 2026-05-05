// (c) 2026 Jon-Erik G. Storm, Inc., a California Corporation,
// doing business as LAVA GOAT SOFTWARE. All rights reserved.
// SPDX-License-Identifier: MIT

//! Built-in implicit rules and default variables, matching GNU Make 4.4 defaults.
//!
//! # Role in the pipeline
//!
//! This module is called once during Makefile database initialisation (before
//! parsing begins) to populate [`MakeDatabase`] with the standard pattern rules
//! and default variable values that GNU Make provides out of the box.  User-written
//! rules and variable assignments override these defaults through the normal
//! precedence rules.
//!
//! # Default variables
//!
//! [`register_default_variables`] inserts variables with origin
//! [`VarOrigin::Default`] and flavour [`VarFlavor::Recursive`] using
//! `entry(...).or_insert_with(...)`, so user assignments at `override` level or
//! plain assignments already in the database take precedence.
//!
//! The registered variables mirror GNU Make 4.4.1's built-in set:
//!
//! | Variable | Default value |
//! |---|---|
//! | `CC` | `cc` |
//! | `CXX` | `g++` |
//! | `AR` / `ARFLAGS` | `ar` / `rv` |
//! | `LEX` / `YACC` | `lex` / `yacc` |
//! | `COMPILE.c` | `$(CC) $(CFLAGS) $(CPPFLAGS) $(TARGET_ARCH) -c` |
//! | `LINK.o` | `$(CC) $(LDFLAGS) $(TARGET_ARCH)` |
//! | `OUTPUT_OPTION` | `-o $@` |
//! | `.LIBPATTERNS` | `lib%.a lib%.so` |
//!
//! `MAKE` is set to `$(MAKE_COMMAND)`, which the executor replaces with the actual
//! binary path at build time so recursive `$(MAKE)` invocations find the correct
//! executable.
//!
//! # Built-in pattern rules
//!
//! [`register_implicit_rules`] pushes pattern rules onto `db.pattern_rules` in
//! the order the executor's implicit rule search (`find_pattern_rule`) will try
//! them.  Rules registered first have higher priority.
//!
//! The full set of registered rules (in priority order):
//!
//! | Target | Prerequisites | Language |
//! |---|---|---|
//! | `%.o` | `%.c` | C compilation |
//! | `%.o` | `%.cc` | C++ compilation |
//! | `%.o` | `%.cpp` | C++ compilation |
//! | `%.o` | `%.C` | C++ compilation (uppercase extension) |
//! | `%.o` | `%.f` | Fortran compilation |
//! | `%.o` | `%.F` | Fortran with preprocessing |
//! | `%.o` | `%.s` | Assembly |
//! | `%.o` | `%.S` | Assembly with C preprocessor |
//! | `%` | `%.o` | Link from object file |
//! | `%` | `%.c` | Compile and link from C |
//! | `%` | `%.cc` | Compile and link from C++ |
//! | `%` | `%.cpp` | Compile and link from C++ |
//! | `%` | `%.f` | Compile and link from Fortran |
//! | `%` | `%.F` | Compile and link from Fortran (preprocessed) |
//! | `%` | `%.r` | Compile and link from Ratfor |
//! | `%.c` | `%.y` | Yacc → C source |
//! | `%.c` | `%.l` | Lex → C source |
//!
//! All rules are constructed via [`make_pattern_rule`], which produces a [`Rule`]
//! with `is_pattern = true`, `is_terminal = false`, and `lineno = 0` (built-in
//! rules have no source location).
//!
//! After `register_implicit_rules` returns, `db.builtin_pattern_rules_count` is
//! set to the number of rules just added.  This count is used by the parser to
//! implement `.SUFFIXES:` (empty): clearing `.SUFFIXES` removes all built-in
//! pattern rules by truncating `db.pattern_rules` to zero.
//!
//! # Suffix rules
//!
//! jmake converts old-style suffix rules (`.c.o:`) to the equivalent pattern rule
//! (`%.o: %.c`) during parsing (see [`crate::parser`]).  This module only provides
//! the modern pattern-rule equivalents; it does not register any suffix rules
//! directly.
//!
//! # Disabling built-in rules
//!
//! A Makefile can suppress all built-in pattern rules by writing:
//!
//! ```makefile
//! .SUFFIXES:
//! ```
//!
//! or by running `jmake -r` / `jmake --no-builtin-rules`.  In both cases the
//! executor's implicit rule search finds no built-in candidates and falls through
//! to an error for targets with no matching rule.

use crate::types::{Rule, VarFlavor, Variable, VarOrigin};
use crate::database::MakeDatabase;

pub fn register_default_variables(db: &mut MakeDatabase) {
    let defaults = vec![
        ("AR", "ar"),
        ("ARFLAGS", "rv"),
        ("AS", "as"),
        ("CC", "cc"),
        ("CXX", "g++"),
        ("CPP", "$(CC) -E"),
        ("FC", "f77"),
        ("GET", "get"),
        ("LEX", "lex"),
        ("LINT", "lint"),
        ("MAKE", "$(MAKE_COMMAND)"),
        ("PC", "pc"),
        ("YACC", "yacc"),
        ("YFLAGS", ""),
        // GNU Make 4.4.1: YACC.y is what the %.c: %.y rule actually invokes.
        ("YACC.y", "$(YACC) $(YFLAGS)"),
        ("MAKEINFO", "makeinfo"),
        ("TEX", "tex"),
        ("TEXI2DVI", "texi2dvi"),
        ("WEAVE", "weave"),
        ("CWEAVE", "cweave"),
        ("TANGLE", "tangle"),
        ("CTANGLE", "ctangle"),
        ("RM", "rm -f"),
        ("CO", "co"),
        ("COMPILE.c", "$(CC) $(CFLAGS) $(CPPFLAGS) $(TARGET_ARCH) -c"),
        ("COMPILE.C", "$(CXX) $(CXXFLAGS) $(CPPFLAGS) $(TARGET_ARCH) -c"),
        ("COMPILE.cc", "$(CXX) $(CXXFLAGS) $(CPPFLAGS) $(TARGET_ARCH) -c"),
        ("COMPILE.cpp", "$(CXX) $(CXXFLAGS) $(CPPFLAGS) $(TARGET_ARCH) -c"),
        ("COMPILE.f", "$(FC) $(FFLAGS) $(TARGET_ARCH) -c"),
        ("COMPILE.F", "$(FC) $(FFLAGS) $(CPPFLAGS) $(TARGET_ARCH) -c"),
        ("COMPILE.s", "$(AS) $(ASFLAGS) $(TARGET_MACH)"),
        ("COMPILE.S", "$(CC) $(ASFLAGS) $(CPPFLAGS) $(TARGET_MACH) -c"),
        ("LINK.c", "$(CC) $(CFLAGS) $(CPPFLAGS) $(LDFLAGS) $(TARGET_ARCH)"),
        ("LINK.C", "$(CXX) $(CXXFLAGS) $(CPPFLAGS) $(LDFLAGS) $(TARGET_ARCH)"),
        ("LINK.cc", "$(CXX) $(CXXFLAGS) $(CPPFLAGS) $(LDFLAGS) $(TARGET_ARCH)"),
        ("LINK.cpp", "$(CXX) $(CXXFLAGS) $(CPPFLAGS) $(LDFLAGS) $(TARGET_ARCH)"),
        ("LINK.o", "$(CC) $(LDFLAGS) $(TARGET_ARCH)"),
        ("LINK.f", "$(FC) $(FFLAGS) $(LDFLAGS) $(TARGET_ARCH)"),
        ("LINK.F", "$(FC) $(FFLAGS) $(CPPFLAGS) $(LDFLAGS) $(TARGET_ARCH)"),
        ("LINK.r", "$(FC) $(FFLAGS) $(RFLAGS) $(LDFLAGS) $(TARGET_ARCH)"),
        ("LINK.s", "$(CC) $(ASFLAGS) $(LDFLAGS) $(TARGET_MACH)"),
        ("LINK.S", "$(CC) $(ASFLAGS) $(CPPFLAGS) $(LDFLAGS) $(TARGET_MACH)"),
        ("PREPROCESS.F", "$(FC) $(FFLAGS) $(CPPFLAGS) $(TARGET_ARCH) -F"),
        ("PREPROCESS.r", "$(FC) $(FFLAGS) $(RFLAGS) $(TARGET_ARCH) -F"),
        ("PREPROCESS.S", "$(CC) -E $(CPPFLAGS)"),
        ("LINT.c", "$(LINT) $(LINTFLAGS) $(CPPFLAGS) $(TARGET_ARCH)"),
        ("OUTPUT_OPTION", "-o $@"),
        ("CFLAGS", ""),
        ("CXXFLAGS", ""),
        ("CPPFLAGS", ""),
        ("FFLAGS", ""),
        ("LDFLAGS", ""),
        ("LDLIBS", ""),
        ("LOADLIBES", ""),
        ("ASFLAGS", ""),
        ("LFLAGS", ""),
        // GNU Make 4.4.1: LEX.l is what the %.c: %.l rule actually invokes.
        ("LEX.l", "$(LEX) $(LFLAGS) -t"),
        ("RFLAGS", ""),
        ("TARGET_ARCH", ""),
        ("TARGET_MACH", ""),
        // Default library search patterns: used by -lname prerequisite resolution.
        // On Linux/Unix: search for shared library first, then static.
        (".LIBPATTERNS", "lib%.a lib%.so"),
    ];

    for (name, value) in defaults {
        db.variables.entry(name.to_string()).or_insert_with(|| {
            Variable::new(value.to_string(), VarFlavor::Recursive, VarOrigin::Default)
        });
    }
}

pub fn register_implicit_rules(db: &mut MakeDatabase) {
    // C compilation: %.o: %.c
    db.pattern_rules.push(make_pattern_rule(
        "%.o", &["%.c"],
        &["$(COMPILE.c) $(OUTPUT_OPTION) $<"],
    ));

    // C++ compilation: %.o: %.cc
    db.pattern_rules.push(make_pattern_rule(
        "%.o", &["%.cc"],
        &["$(COMPILE.cc) $(OUTPUT_OPTION) $<"],
    ));

    // C++ compilation: %.o: %.cpp
    db.pattern_rules.push(make_pattern_rule(
        "%.o", &["%.cpp"],
        &["$(COMPILE.cpp) $(OUTPUT_OPTION) $<"],
    ));

    // C++ compilation: %.o: %.C
    db.pattern_rules.push(make_pattern_rule(
        "%.o", &["%.C"],
        &["$(COMPILE.C) $(OUTPUT_OPTION) $<"],
    ));

    // Fortran: %.o: %.f
    db.pattern_rules.push(make_pattern_rule(
        "%.o", &["%.f"],
        &["$(COMPILE.f) $(OUTPUT_OPTION) $<"],
    ));

    // Fortran preprocessing: %.o: %.F
    db.pattern_rules.push(make_pattern_rule(
        "%.o", &["%.F"],
        &["$(COMPILE.F) $(OUTPUT_OPTION) $<"],
    ));

    // Assembly: %.o: %.s
    db.pattern_rules.push(make_pattern_rule(
        "%.o", &["%.s"],
        &["$(COMPILE.s) -o $@ $<"],
    ));

    // Assembly with cpp: %.o: %.S
    db.pattern_rules.push(make_pattern_rule(
        "%.o", &["%.S"],
        &["$(COMPILE.S) -o $@ $<"],
    ));

    // Linking: %: %.o
    db.pattern_rules.push(make_pattern_rule(
        "%", &["%.o"],
        &["$(LINK.o) $^ $(LOADLIBES) $(LDLIBS) -o $@"],
    ));

    // C linking: %: %.c
    db.pattern_rules.push(make_pattern_rule(
        "%", &["%.c"],
        &["$(LINK.c) $^ $(LOADLIBES) $(LDLIBS) -o $@"],
    ));

    // C++ linking: %: %.cc
    db.pattern_rules.push(make_pattern_rule(
        "%", &["%.cc"],
        &["$(LINK.cc) $^ $(LOADLIBES) $(LDLIBS) -o $@"],
    ));

    // C++ linking: %: %.cpp
    db.pattern_rules.push(make_pattern_rule(
        "%", &["%.cpp"],
        &["$(LINK.cpp) $^ $(LOADLIBES) $(LDLIBS) -o $@"],
    ));

    // Fortran direct linking: %: %.f
    db.pattern_rules.push(make_pattern_rule(
        "%", &["%.f"],
        &["$(LINK.f) $^ $(LOADLIBES) $(LDLIBS) -o $@"],
    ));

    // Fortran preprocessing and linking: %: %.F
    db.pattern_rules.push(make_pattern_rule(
        "%", &["%.F"],
        &["$(LINK.F) $^ $(LOADLIBES) $(LDLIBS) -o $@"],
    ));

    // Ratfor: %: %.r
    db.pattern_rules.push(make_pattern_rule(
        "%", &["%.r"],
        &["$(LINK.r) $^ $(LOADLIBES) $(LDLIBS) -o $@"],
    ));

    // Yacc: %.c: %.y
    db.pattern_rules.push(make_pattern_rule(
        "%.c", &["%.y"],
        &["$(YACC.y) $<", "mv -f y.tab.c $@"],
    ));

    // Lex: %.c: %.l
    db.pattern_rules.push(make_pattern_rule(
        "%.c", &["%.l"],
        &["@$(RM) $@", "$(LEX.l) $< > $@"],
    ));

    // Archive member rule: (%.o): %.o  -- handled specially

    // Record how many built-in rules we just added so that .SUFFIXES: (clear)
    // can remove them.
    db.builtin_pattern_rules_count = db.pattern_rules.len();
}

fn make_pattern_rule(target: &str, prereqs: &[&str], recipe: &[&str]) -> Rule {
    Rule {
        targets: vec![target.to_string()],
        prerequisites: prereqs.iter().map(|s| s.to_string()).collect(),
        order_only_prerequisites: Vec::new(),
        recipe: recipe.iter().map(|s| (0usize, s.to_string())).collect(),
        is_pattern: true,
        is_double_colon: false,
        is_terminal: false,
        is_compat: false,
        target_specific_vars: Vec::new(),
        source_file: String::new(),
        lineno: 0,
        static_stem: String::new(),
        second_expansion_prereqs: None,
        second_expansion_order_only: None,
        grouped_siblings: Vec::new(),
        has_inline_recipe_marker: false,
    }
}

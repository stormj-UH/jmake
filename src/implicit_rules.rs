// Copyright (c) 2026 Jon-Erik G. Storm. All rights reserved.
// Built-in implicit rules matching GNU Make defaults

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

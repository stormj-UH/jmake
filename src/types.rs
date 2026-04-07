// Copyright (c) 2026 Jon-Erik G. Storm. All rights reserved.

use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// How a variable was defined
#[derive(Debug, Clone, PartialEq)]
pub enum VarOrigin {
    Default,
    Environment,
    File,
    CommandLine,
    Override,
    Automatic,
}

/// Variable flavor: recursive (=) or simple (:= / ::=)
#[derive(Debug, Clone, PartialEq)]
pub enum VarFlavor {
    Recursive,
    Simple,
    Append,
    Conditional, // ?=
    Shell,       // !=
}

#[derive(Debug, Clone)]
pub struct Variable {
    pub value: String,
    pub flavor: VarFlavor,
    pub origin: VarOrigin,
    pub export: Option<bool>, // None = inherit, Some(true) = export, Some(false) = unexport
    /// When true, this target-specific variable is private and NOT inherited by prerequisites.
    pub is_private: bool,
    /// The makefile file path where this variable was defined (empty if unknown/built-in).
    pub source_file: String,
    /// The line number in the makefile where this variable was defined (0 if unknown).
    pub source_line: usize,
}

impl Variable {
    pub fn new(value: String, flavor: VarFlavor, origin: VarOrigin) -> Self {
        Variable {
            value,
            flavor,
            origin,
            export: None,
            is_private: false,
            source_file: String::new(),
            source_line: 0,
        }
    }
}

/// A single rule (explicit or pattern)
#[derive(Debug, Clone)]
pub struct Rule {
    pub targets: Vec<String>,
    pub prerequisites: Vec<String>,
    pub order_only_prerequisites: Vec<String>,
    /// Each recipe line stored as (line_number_in_makefile, text)
    pub recipe: Vec<(usize, String)>,
    pub is_pattern: bool,
    pub is_double_colon: bool,
    pub is_terminal: bool, // pattern rule with no recipe terminates chain
    /// Target-specific variable assignments, stored as a list to support
    /// multiple += entries for the same variable name.
    pub target_specific_vars: Vec<(String, Variable)>,
    /// The makefile file path where this rule was defined
    pub source_file: String,
    /// Line number in the makefile where this rule was defined
    pub lineno: usize,
    /// The stem computed when this rule was derived from a static pattern rule.
    /// Empty for rules that are not from static pattern rules, or for pattern
    /// rules (where the stem is computed at build time).
    pub static_stem: String,
    /// Raw (post-first-expansion) prerequisite text for second expansion.
    /// If this is Some, the prerequisites field holds already-expanded values
    /// and this field holds the text that should be re-expanded at build time.
    /// The string contains whitespace-separated prerequisite tokens but is NOT
    /// split, so that function calls like $(addsuffix .3,foo) are kept intact.
    pub second_expansion_prereqs: Option<String>,
    /// Raw (post-first-expansion) order-only prerequisite text for second expansion.
    pub second_expansion_order_only: Option<String>,
    /// For grouped target rules (`targets &: prereqs`): the other target names
    /// in the group (excluding this rule's own target).  When this rule is built,
    /// all grouped siblings are also built.  Empty for non-grouped rules.
    pub grouped_siblings: Vec<String>,
}

impl Rule {
    pub fn new() -> Self {
        Rule {
            targets: Vec::new(),
            prerequisites: Vec::new(),
            order_only_prerequisites: Vec::new(),
            recipe: Vec::new(),
            is_pattern: false,
            is_double_colon: false,
            is_terminal: false,
            target_specific_vars: Vec::new(),
            source_file: String::new(),
            lineno: 0,
            static_stem: String::new(),
            second_expansion_prereqs: None,
            second_expansion_order_only: None,
            grouped_siblings: Vec::new(),
        }
    }
}

/// Special targets that modify make's behavior
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SpecialTarget {
    Phony,
    Suffixes,
    Default,
    Precious,
    Intermediate,
    Secondary,
    SecondExpansion,
    DeleteOnError,
    Ignore,
    LowResolutionTime,
    Silent,
    ExportAllVariables,
    NotParallel,
    OneSHell,
    Posix,
    NotIntermediate,
    Wait,
}

impl SpecialTarget {
    pub fn from_str(s: &str) -> Option<SpecialTarget> {
        match s {
            ".PHONY" => Some(SpecialTarget::Phony),
            ".SUFFIXES" => Some(SpecialTarget::Suffixes),
            ".DEFAULT" => Some(SpecialTarget::Default),
            ".PRECIOUS" => Some(SpecialTarget::Precious),
            ".INTERMEDIATE" => Some(SpecialTarget::Intermediate),
            ".SECONDARY" => Some(SpecialTarget::Secondary),
            ".SECONDEXPANSION" => Some(SpecialTarget::SecondExpansion),
            ".DELETE_ON_ERROR" => Some(SpecialTarget::DeleteOnError),
            ".IGNORE" => Some(SpecialTarget::Ignore),
            ".LOW_RESOLUTION_TIME" => Some(SpecialTarget::LowResolutionTime),
            ".SILENT" => Some(SpecialTarget::Silent),
            ".EXPORT_ALL_VARIABLES" => Some(SpecialTarget::ExportAllVariables),
            ".NOTPARALLEL" => Some(SpecialTarget::NotParallel),
            ".ONESHELL" => Some(SpecialTarget::OneSHell),
            ".POSIX" => Some(SpecialTarget::Posix),
            ".NOTINTERMEDIATE" => Some(SpecialTarget::NotIntermediate),
            ".WAIT" => Some(SpecialTarget::Wait),
            _ => None,
        }
    }
}

/// Conditional directive type
#[derive(Debug, Clone)]
pub enum ConditionalKind {
    Ifdef(String),
    Ifndef(String),
    Ifeq(String, String),
    Ifneq(String, String),
}

/// Parsed line from a Makefile
#[derive(Debug, Clone)]
pub enum ParsedLine {
    Rule(Rule),
    VariableAssignment {
        name: String,
        value: String,
        flavor: VarFlavor,
        is_override: bool,
        is_export: bool,
        is_unexport: bool,
        is_private: bool,
        target: Option<String>,
    },
    Include {
        paths: Vec<String>,
        ignore_missing: bool, // -include / sinclude
    },
    Conditional(ConditionalKind),
    Else(Option<ConditionalKind>),
    Endif,
    VpathDirective {
        pattern: Option<String>,
        directories: Vec<String>,
    },
    ExportDirective {
        names: Vec<String>,
        export: bool, // true = export, false = unexport
    },
    Define {
        name: String,
        flavor: VarFlavor,
        is_override: bool,
        is_export: bool,
        has_extraneous: bool,  // true if there was extra text after the operator
    },
    Endef,
    UnExport {
        names: Vec<String>,
    },
    Undefine {
        name: String,
        is_override: bool,
    },
    /// Expanded static pattern rule: one Rule per target, already resolved.
    StaticPatternExpansion(Vec<Rule>),
    Recipe(String),
    Comment,
    Empty,
    /// Line that could not be parsed - missing separator error
    MissingSeparator(String), // the hint message (e.g., "did you mean TAB instead of 8 spaces?")
    /// Invalid syntax in a conditional directive (e.g. `ifeq` with no args or bad args)
    InvalidConditional,
    LoadDirective(String),
}

/// A pattern-specific variable assignment entry.
/// E.g. `%.o: CFLAGS += -Wall` produces PatternSpecificVar { pattern: "%.o", var_name: "CFLAGS", ... }
#[derive(Debug, Clone)]
pub struct PatternSpecificVar {
    pub pattern: String,
    pub var_name: String,
    pub var: Variable,
    pub is_override: bool,
}

/// Database of all rules, variables, etc.
#[derive(Debug)]
pub struct MakeDatabase {
    pub rules: IndexMap<String, Vec<Rule>>,
    pub pattern_rules: Vec<Rule>,
    pub suffix_rules: Vec<Rule>,
    pub variables: IndexMap<String, Variable>,
    pub special_targets: HashMap<SpecialTarget, HashSet<String>>,
    pub default_target: Option<String>,
    pub vpath: Vec<(String, Vec<PathBuf>)>, // (pattern, directories)
    pub vpath_general: Vec<PathBuf>,
    pub suffixes: Vec<String>,
    pub second_expansion: bool,
    pub one_shell: bool,
    pub export_all: bool,
    /// `unexport` (with no prerequisites) was seen: the global default is to NOT export
    /// variables to children, unless they are explicitly `export`ed.
    pub unexport_all: bool,
    pub posix_mode: bool,
    pub not_parallel: bool,
    pub default_rule: Option<Rule>,
    /// Set of names that are explicitly mentioned in the makefile as either:
    /// - Targets of explicit (non-pattern) rules
    /// - Prerequisites of explicit (non-pattern) rules
    /// - Literal (non-% containing) prerequisites of pattern rules
    /// Targets built by implicit rules are NOT intermediate if they appear here.
    pub explicitly_mentioned: HashSet<String>,
    /// Prerequisites of explicit (non-pattern) rules only.
    /// Used for the "compat rule" check in implicit rule search: a prereq is
    /// "compat-eligible" only if it is mentioned in an explicit rule's dep list
    /// (not just as a literal dep of a pattern rule).
    pub explicit_dep_names: HashSet<String>,
    /// Names of variables that were originally imported from the process environment.
    /// Even if overridden by the Makefile, these are still exported to child processes.
    pub env_var_names: HashSet<String>,
    /// Pattern-specific variable assignments (e.g. `%.o: CFLAGS += -Wall`).
    /// Applied to any target matching the pattern, with correct override semantics.
    pub pattern_specific_vars: Vec<PatternSpecificVar>,
    /// Number of built-in (default) pattern rules at the start of pattern_rules.
    /// When .SUFFIXES: clears all suffixes, these built-in rules are also removed.
    pub builtin_pattern_rules_count: usize,
}

impl MakeDatabase {
    pub fn new() -> Self {
        MakeDatabase {
            rules: IndexMap::new(),
            pattern_rules: Vec::new(),
            suffix_rules: Vec::new(),
            variables: IndexMap::new(),
            special_targets: HashMap::new(),
            default_target: None,
            vpath: Vec::new(),
            vpath_general: Vec::new(),
            suffixes: vec![
                ".out".into(), ".a".into(), ".ln".into(), ".o".into(),
                ".c".into(), ".cc".into(), ".C".into(), ".cpp".into(),
                ".p".into(), ".f".into(), ".F".into(), ".m".into(),
                ".r".into(), ".y".into(), ".l".into(), ".ym".into(),
                ".lm".into(), ".s".into(), ".S".into(), ".mod".into(),
                ".sym".into(), ".def".into(), ".h".into(), ".info".into(),
                ".dvi".into(), ".tex".into(), ".texinfo".into(),
                ".texi".into(), ".txinfo".into(), ".w".into(),
                ".ch".into(), ".web".into(), ".sh".into(), ".elc".into(),
                ".el".into(),
            ],
            second_expansion: false,
            one_shell: false,
            export_all: false,
            unexport_all: false,
            posix_mode: false,
            not_parallel: false,
            env_var_names: HashSet::new(),
            default_rule: None,
            pattern_specific_vars: Vec::new(),
            builtin_pattern_rules_count: 0,
            explicitly_mentioned: HashSet::new(),
            explicit_dep_names: HashSet::new(),
        }
    }

    /// Check if a name is explicitly mentioned in the makefile (either as a target
    /// or as a prerequisite of a non-pattern rule, or as a literal prereq in a pattern rule).
    pub fn is_explicitly_mentioned(&self, name: &str) -> bool {
        self.explicitly_mentioned.contains(name)
            || self.rules.contains_key(name)
    }

    pub fn is_phony(&self, target: &str) -> bool {
        self.special_targets
            .get(&SpecialTarget::Phony)
            .map_or(false, |set| set.contains(target))
    }

    pub fn is_precious(&self, target: &str) -> bool {
        self.special_targets
            .get(&SpecialTarget::Precious)
            .map_or(false, |set| set.contains(target))
    }

    pub fn is_intermediate(&self, target: &str) -> bool {
        // Explicitly listed as .INTERMEDIATE: <name>
        if self.special_targets
            .get(&SpecialTarget::Intermediate)
            .map_or(false, |set| set.contains(target))
        {
            return true;
        }
        false
    }

    pub fn is_secondary(&self, target: &str) -> bool {
        let set = match self.special_targets.get(&SpecialTarget::Secondary) {
            Some(s) => s,
            None => return false,
        };
        // .SECONDARY with no prerequisites means ALL targets are secondary
        if set.is_empty() {
            return true;
        }
        set.contains(target)
    }

    pub fn is_notintermediate(&self, target: &str) -> bool {
        // Check if explicitly marked as .NOTINTERMEDIATE
        // .NOTINTERMEDIATE with no prereqs means ALL targets are not intermediate
        let set = match self.special_targets.get(&SpecialTarget::NotIntermediate) {
            Some(s) => s,
            None => return false,
        };
        if set.is_empty() {
            return true;
        }
        // Pattern matching: check if any pattern in the set matches target
        for pat in set {
            if pat.contains('%') {
                // Simple % wildcard matching
                if let Some(pct) = pat.find('%') {
                    let prefix = &pat[..pct];
                    let suffix = &pat[pct+1..];
                    if target.starts_with(prefix) && target.ends_with(suffix)
                        && target.len() >= prefix.len() + suffix.len() {
                        return true;
                    }
                }
            } else if pat == target {
                return true;
            }
        }
        false
    }

    pub fn is_silent_target(&self, target: &str) -> bool {
        self.special_targets
            .get(&SpecialTarget::Silent)
            .map_or(false, |set| set.is_empty() || set.contains(target))
    }
}

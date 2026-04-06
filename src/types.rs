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
}

impl Variable {
    pub fn new(value: String, flavor: VarFlavor, origin: VarOrigin) -> Self {
        Variable {
            value,
            flavor,
            origin,
            export: None,
        }
    }
}

/// A single rule (explicit or pattern)
#[derive(Debug, Clone)]
pub struct Rule {
    pub targets: Vec<String>,
    pub prerequisites: Vec<String>,
    pub order_only_prerequisites: Vec<String>,
    pub recipe: Vec<String>,
    pub is_pattern: bool,
    pub is_double_colon: bool,
    pub is_terminal: bool, // pattern rule with no recipe terminates chain
    pub target_specific_vars: IndexMap<String, Variable>,
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
            target_specific_vars: IndexMap::new(),
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
    },
    Endef,
    UnExport {
        names: Vec<String>,
    },
    Recipe(String),
    Comment,
    Empty,
    LoadDirective(String),
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
    pub posix_mode: bool,
    pub not_parallel: bool,
    pub default_rule: Option<Rule>,
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
            posix_mode: false,
            not_parallel: false,
            default_rule: None,
        }
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
        self.special_targets
            .get(&SpecialTarget::Intermediate)
            .map_or(false, |set| set.contains(target))
    }

    pub fn is_silent_target(&self, target: &str) -> bool {
        self.special_targets
            .get(&SpecialTarget::Silent)
            .map_or(false, |set| set.is_empty() || set.contains(target))
    }
}

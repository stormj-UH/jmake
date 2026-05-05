// (c) 2026 Jon-Erik G. Storm, Inc., a California Corporation,
// doing business as LAVA GOAT SOFTWARE. All rights reserved.
// SPDX-License-Identifier: MIT

//! Shared type definitions for jmake.
//!
//! This module is the single source of truth for the core data model.  It
//! intentionally has no dependencies on other jmake modules so that the
//! parser, evaluator, and executor can all import from here without creating
//! cycles.
//!
//! # Key types
//!
//! | Type | Role |
//! |------|------|
//! | [`Variable`] | A single named variable with value, flavor, and origin. |
//! | [`Rule`] | One explicit, pattern, or suffix rule. |
//! | [`MakeDatabase`] | Runtime aggregate of all rules, variables, and make state. |
//! | [`ParsedLine`] | The discriminated-union output of the makefile parser. |
//! | [`SpecialTarget`] | Enum of the special `.PHONY`, `.SUFFIXES`, … targets. |
//!
//! # Thread safety
//!
//! None of the types in this module implement `Sync` or `Send` on their own.
//! [`MakeDatabase`] is used exclusively on the main thread by the evaluator;
//! worker threads in `exec::parallel` receive already-resolved data and do
//! not access the database directly.

use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// How a variable was defined.
///
/// Mirrors the origins reported by the GNU Make `$(origin)` function.  The
/// evaluator uses this to implement precedence: `CommandLine` and `Override`
/// variables cannot be superseded by `File` assignments.
#[derive(Debug, Clone, PartialEq)]
pub enum VarOrigin {
    /// A built-in default value (e.g. `CC = cc`).
    Default,
    /// Imported from the process environment at startup.
    Environment,
    /// Defined in a makefile.
    File,
    /// Supplied on the command line as `VAR=value`.
    CommandLine,
    /// Defined with the `override` keyword in a makefile.
    Override,
    /// Automatic variables (`$@`, `$<`, etc.) — the variant is matched in
    /// `$(origin)` expansion but automatic variables are never stored in the
    /// main variable table; they are passed as a temporary `HashMap` during
    /// recipe expansion.
    #[allow(dead_code)]
    Automatic,
}

/// Variable assignment flavor, corresponding to the operator used.
///
/// The flavor determines when and how the variable's value is expanded:
/// recursive flavors defer expansion until the variable is referenced, while
/// simple flavors expand immediately at assignment time.
#[derive(Debug, Clone, PartialEq)]
pub enum VarFlavor {
    /// `=`  — value is expanded lazily every time the variable is referenced.
    Recursive,
    /// `:=` or `::=`  — value is expanded once at assignment time.
    Simple,
    /// `:::=`  — POSIX immediate assignment.  Like `Simple`, the value is
    /// expanded at assignment time, but when `+=` is subsequently applied the
    /// appended text is stored raw (not immediately expanded).
    PosixSimple,
    /// `+=`  — appends to the existing value.  Expansion semantics depend on
    /// the current flavor of the variable being appended to.
    Append,
    /// `?=`  — conditional assignment; sets the variable only if it is not
    /// already defined.
    Conditional,
    /// `!=`  — shell assignment; the value is run through `/bin/sh` and the
    /// output (trailing newlines stripped) becomes the variable's value.
    Shell,       // !=
}

/// A single makefile variable binding.
///
/// Variables are stored in [`MakeDatabase::variables`] and in the
/// per-target variable tables inside each [`Rule`].  A `Variable` is
/// always associated with a name by its containing collection; the struct
/// itself holds only the value and metadata.
#[derive(Debug, Clone)]
pub struct Variable {
    /// The raw (possibly unexpanded) value string.
    pub value: String,
    /// How the value should be expanded when referenced.
    pub flavor: VarFlavor,
    /// Where the variable came from (affects override precedence).
    pub origin: VarOrigin,
    /// Export status for child processes.
    /// `None` means inherit the global default; `Some(true)` forces export;
    /// `Some(false)` forces suppression even if the global default is to export.
    pub export: Option<bool>,
    /// When `true`, this target-specific variable is *private*: it is set for
    /// the target but is **not** inherited by its prerequisites.
    pub is_private: bool,
    /// Path of the makefile that defined this variable (empty for built-ins).
    pub source_file: String,
    /// Line number in `source_file` where this variable was defined (`0` if unknown).
    pub source_line: usize,
}

impl Variable {
    /// Create a new [`Variable`] with the given value, flavor, and origin.
    ///
    /// Export status defaults to `None` (inherit global default), `is_private`
    /// to `false`, and source location to unknown (`""` / `0`).
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

/// A single makefile rule — explicit, pattern, or suffix.
///
/// One `Rule` object represents one `target: prerequisites` stanza in the
/// parsed makefile.  The evaluator's `register_rule` function inserts rules
/// into [`MakeDatabase::rules`] (for explicit targets) or
/// [`MakeDatabase::pattern_rules`] (for `%`-pattern rules).
///
/// # Second expansion
///
/// When `.SECONDEXPANSION` is active and the prerequisite list contains `$`,
/// the raw prerequisite text is preserved in `second_expansion_prereqs` and
/// re-expanded at build time with automatic variables in scope.  The
/// `prerequisites` field still holds the first-expansion result and is used
/// for initial dependency analysis.
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
    /// True if this rule was chosen as a "compatibility" rule (last resort):
    /// its prerequisite is only mentioned as someone else's dep, not a target.
    /// In this mode, missing prerequisites cause errors rather than being swallowed.
    pub is_compat: bool,
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
    /// True if the rule line had a semicolon (inline recipe marker), even if
    /// the recipe text after the semicolon is empty.  GNU Make treats any rule
    /// with a semicolon as "having a recipe" for the purpose of printing
    /// "'target' is up to date" vs "Nothing to be done for 'target'".
    pub has_inline_recipe_marker: bool,
}

impl Rule {
    /// Create a new, empty [`Rule`] with all fields zeroed / empty.
    ///
    /// Callers are expected to fill in at minimum `targets` before registering
    /// the rule with the database.
    pub fn new() -> Self {
        Rule {
            targets: Vec::new(),
            prerequisites: Vec::new(),
            order_only_prerequisites: Vec::new(),
            recipe: Vec::new(),
            is_pattern: false,
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
}

/// Special targets that modify jmake's behavior.
///
/// When the evaluator encounters a rule whose target name begins with `.`,
/// it checks this enum.  Special targets with no prerequisites act as
/// global mode switches (e.g., `.PHONY:` with explicit targets, `.POSIX:`).
///
/// The set of recognized names matches GNU Make 4.4.
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
    /// Parse a target name string into a [`SpecialTarget`] variant.
    ///
    /// Returns `None` for any name that is not a recognised special target,
    /// including names that start with `.` but are not in the list (e.g.
    /// `.hidden` is a plain file target, not a special target).
    ///
    /// # Examples
    ///
    /// ```
    /// # use jmake::types::SpecialTarget;
    /// assert_eq!(SpecialTarget::from_str(".PHONY"), Some(SpecialTarget::Phony));
    /// assert_eq!(SpecialTarget::from_str(".hidden"), None);
    /// ```
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

/// The kind of a conditional directive.
///
/// Used by [`ParsedLine::Conditional`] and [`ParsedLine::Else`] to carry the
/// condition that was parsed so the evaluator can evaluate it against the
/// current variable database.
#[derive(Debug, Clone)]
pub enum ConditionalKind {
    Ifdef(String),
    Ifndef(String),
    Ifeq(String, String),
    Ifneq(String, String),
}

/// The result of parsing a single logical line from a makefile.
///
/// The parser produces one `ParsedLine` per logical line (after backslash
/// continuation has been collapsed).  The evaluator ([`eval::MakeState`])
/// then consumes these values to build the [`MakeDatabase`].
///
/// Most variants carry all the data the evaluator needs inline.  The
/// [`ParsedLine::Rule`] variant carries a [`Rule`] that may still need its
/// recipe lines collected from subsequent [`ParsedLine::Recipe`] lines.
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
    /// Placeholder for GNU Make `.load` directive (dynamic plugin loading).
    /// Not yet implemented; variant retained for future .load support.
    #[allow(dead_code)]
    LoadDirective(String),
    /// Fatal parse error that must terminate make immediately (e.g. "target pattern has no %").
    FatalError(String),
}

/// A pattern-specific variable assignment.
///
/// Entries like `%.o: CFLAGS += -Wall` are stored here and applied to any
/// target whose name matches `pattern` before its recipe is executed.  The
/// evaluator checks [`MakeDatabase::pattern_specific_vars`] during target
/// setup in the executor.
///
/// # Example
///
/// `%.o: CFLAGS += -Wall` produces:
/// ```text
/// PatternSpecificVar { pattern: "%.o", var_name: "CFLAGS", … }
/// ```
#[derive(Debug, Clone)]
pub struct PatternSpecificVar {
    pub pattern: String,
    pub var_name: String,
    pub var: Variable,
    pub is_override: bool,
}

/// The runtime database of all rules, variables, and global make state.
///
/// [`MakeDatabase`] is populated by the evaluator as it reads each makefile
/// and then consulted by the executor when building targets.  It is the
/// central data structure that connects parsing, evaluation, and execution.
///
/// Rules are stored in two separate tables:
///
/// - `rules` — explicit rules keyed by target name (using an [`IndexMap`] to
///   preserve insertion order for correct `.DEFAULT_GOAL` detection).
/// - `pattern_rules` — `%`-pattern rules in the order they were defined;
///   the executor searches them last-to-first (most-recently-defined wins).
///
/// # Thread safety
///
/// [`MakeDatabase`] is accessed only from the main thread.  Worker threads in
/// `exec::parallel` receive resolved [`TargetPlan`]s and do not hold a
/// reference to the database.
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
    /// Targets listed as `.NOTPARALLEL: target1 target2` — their prerequisites run
    /// sequentially (as if jobs=1) even when the global parallel mode is active.
    /// This is distinct from `.NOTPARALLEL:` (no prereqs) which sets `not_parallel=true`.
    pub not_parallel_targets: HashSet<String>,
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
    /// True when .DEFAULT_GOAL was explicitly set to a non-empty value in the makefile.
    /// When true, automatic updates of .DEFAULT_GOAL from rule registration are suppressed.
    /// Reset to false when .DEFAULT_GOAL is explicitly cleared (set to empty).
    pub default_goal_explicit: bool,
}

impl MakeDatabase {
    /// Create a new, empty [`MakeDatabase`] pre-populated with the standard
    /// `.SUFFIXES` list from POSIX Make.
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
            not_parallel_targets: HashSet::new(),
            env_var_names: HashSet::new(),
            default_rule: None,
            pattern_specific_vars: Vec::new(),
            builtin_pattern_rules_count: 0,
            explicitly_mentioned: HashSet::new(),
            explicit_dep_names: HashSet::new(),
            default_goal_explicit: false,
        }
    }

    /// Returns `true` if `name` was explicitly mentioned in the makefile.
    ///
    /// "Explicitly mentioned" means the name appears as:
    /// - A target of an explicit (non-pattern) rule, OR
    /// - A prerequisite of an explicit rule, OR
    /// - A literal (non-`%`) prerequisite of a pattern rule.
    ///
    /// Targets that exist only because an implicit rule matched them are NOT
    /// considered explicitly mentioned, and remain eligible for intermediate
    /// file cleanup.
    pub fn is_explicitly_mentioned(&self, name: &str) -> bool {
        self.explicitly_mentioned.contains(name)
            || self.rules.contains_key(name)
    }

    /// Returns `true` if `target` is listed under `.PHONY`.
    pub fn is_phony(&self, target: &str) -> bool {
        self.special_targets
            .get(&SpecialTarget::Phony)
            .map_or(false, |set| set.contains(target))
    }

    /// Returns `true` if `target` is precious (should not be deleted on error).
    ///
    /// A target is precious when:
    /// - `.PRECIOUS:` with no prerequisites was seen (all targets are precious), OR
    /// - The target name is listed explicitly under `.PRECIOUS:`, OR
    /// - The target name matches a `%`-pattern listed under `.PRECIOUS:`.
    pub fn is_precious(&self, target: &str) -> bool {
        let set = match self.special_targets.get(&SpecialTarget::Precious) {
            Some(s) => s,
            None => return false,
        };
        // .PRECIOUS with no prerequisites means ALL targets are precious.
        if set.is_empty() {
            return true;
        }
        // Check for exact match first.
        if set.contains(target) {
            return true;
        }
        // Check for pattern match (e.g. .PRECIOUS: %.bar matches foo.bar).
        for pat in set {
            if let Some(pct) = pat.find('%') {
                let prefix = &pat[..pct];
                let suffix = &pat[pct+1..];
                if target.starts_with(prefix) && target.ends_with(suffix)
                    && target.len() >= prefix.len() + suffix.len() {
                    return true;
                }
            }
        }
        false
    }

    /// Returns `true` if `target` was explicitly listed under `.INTERMEDIATE:`.
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

    /// Returns `true` if `target` is secondary (keeps the file but does not
    /// consider it a real prerequisite for re-make purposes).
    ///
    /// `.SECONDARY:` with no prerequisites marks **all** targets as secondary.
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

    /// Returns `true` if `target` should be treated as not-intermediate.
    ///
    /// Evaluates the full priority matrix between `.NOTINTERMEDIATE`,
    /// `.INTERMEDIATE`, and `.SECONDARY` as specified by GNU Make 4.4:
    ///
    /// 1. An explicit name in `.INTERMEDIATE:` always wins (returns `false`).
    /// 2. An explicit name in `.SECONDARY:` (non-empty set) beats a global or
    ///    pattern `.NOTINTERMEDIATE:` but NOT an explicit-name `.NOTINTERMEDIATE:`.
    /// 3. `.NOTINTERMEDIATE:` (global or pattern) otherwise wins.
    pub fn is_notintermediate(&self, target: &str) -> bool {
        // Check if explicitly marked as .NOTINTERMEDIATE
        // .NOTINTERMEDIATE with no prereqs means ALL targets are not intermediate.
        //
        // Priority rules (matches GNU Make behavior):
        //   - An explicit file listing in .INTERMEDIATE beats any .NOTINTERMEDIATE
        //     (whether pattern, explicit, or global).
        //   - An explicit file listing in .SECONDARY (non-empty set, meaning the
        //     target was named explicitly) beats a global/pattern .NOTINTERMEDIATE.
        //   - A global .SECONDARY: (empty = all secondary) does NOT beat an explicit
        //     .NOTINTERMEDIATE: target_name or .NOTINTERMEDIATE: pattern.
        let set = match self.special_targets.get(&SpecialTarget::NotIntermediate) {
            Some(s) => s,
            None => return false,
        };

        // Determine if .NOTINTERMEDIATE would match this target at all.
        let notintermediate_matches = if set.is_empty() {
            true
        } else {
            let mut matched = false;
            for pat in set {
                if pat.contains('%') {
                    if let Some(pct) = pat.find('%') {
                        let prefix = &pat[..pct];
                        let suffix = &pat[pct+1..];
                        if target.starts_with(prefix) && target.ends_with(suffix)
                            && target.len() >= prefix.len() + suffix.len() {
                            matched = true;
                            break;
                        }
                    }
                } else if pat == target {
                    matched = true;
                    break;
                }
            }
            matched
        };

        if !notintermediate_matches {
            return false;
        }

        // Explicit .INTERMEDIATE on this target beats any .NOTINTERMEDIATE.
        if self.special_targets.get(&SpecialTarget::Intermediate)
            .map_or(false, |s| s.contains(target)) {
            return false;
        }

        // Explicit (by name, not global) .SECONDARY on this target beats
        // a global or pattern .NOTINTERMEDIATE but NOT an explicit .NOTINTERMEDIATE.
        // "Explicit .SECONDARY" means the .SECONDARY set is non-empty and contains
        // this target's name.
        // But if .NOTINTERMEDIATE also names this target explicitly (not just via
        // pattern or global), then .NOTINTERMEDIATE wins.
        let ni_explicit = set.contains(target); // target explicitly named in .NOTINTERMEDIATE
        if !ni_explicit {
            // .NOTINTERMEDIATE matched via pattern or global.
            // Check if target is explicitly in .SECONDARY (non-empty set containing target name).
            if self.special_targets.get(&SpecialTarget::Secondary)
                .map_or(false, |s| !s.is_empty() && s.contains(target)) {
                return false;
            }
        }

        true
    }

    /// Returns `true` if `target` has silent recipe execution.
    ///
    /// A target is silent when `.SILENT:` has no prerequisites (all targets
    /// are silent) or when `target` appears in the `.SILENT:` prerequisite list.
    pub fn is_silent_target(&self, target: &str) -> bool {
        self.special_targets
            .get(&SpecialTarget::Silent)
            .map_or(false, |set| set.is_empty() || set.contains(target))
    }
}

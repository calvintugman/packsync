//! The rules engine: decides, for every path under an instance, whether it is synced, how it is
//! chunked/compressed, and *why* (explanations are first-class: the UI and CLI show them).
//!
//! Layers, in precedence order:
//! 1. **Sealed excludes** (hard-coded in this file): credentials, tokens, keys, launcher account
//!    files, PackSync's own bookkeeping. No rule can include them, however broadly the user
//!    selects a directory.
//! 2. **Rule document** (TOML, `version = 1`): built-in defaults + launcher fragment + user
//!    overrides, concatenated; the **last matching rule wins**. The effective document is stored in
//!    every snapshot manifest, so all devices (and app versions) of an instance agree.
//! 3. `unmatched` policy for paths no rule mentions (default: exclude, and report them).

use std::collections::BTreeSet;

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::hash::Hash;

pub const RULES_FORMAT_VERSION: u32 = 1;
pub const MAX_RULES: usize = 500;
pub const MAX_PATTERNS_PER_RULE: usize = 200;
pub const MAX_RULES_TEXT_BYTES: usize = 256 * 1024;

pub const DEFAULT_RULES_TOML: &str = include_str!("defaults.toml");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Include,
    Exclude,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UnmatchedPolicy {
    #[default]
    Exclude,
    Include,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Compress {
    #[default]
    Auto,
    Always,
    Never,
}

/// How a file's bytes are split into content-addressed blobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Strategy {
    /// Chosen from the file's size and kind (see [`crate::chunk::resolve_strategy`]).
    #[default]
    Auto,
    Whole,
    Fixed(u32),
    Cdc {
        min: u32,
        avg: u32,
        max: u32,
    },
}

impl Strategy {
    pub fn parse(s: &str) -> Result<Strategy> {
        let bad = || {
            Error::invalid(
                "rule strategy",
                format!("{s:?}: expected auto | whole | fixed:<bytes> | cdc:<min>:<avg>:<max>"),
            )
        };
        let parts: Vec<&str> = s.split(':').collect();
        match parts.as_slice() {
            ["auto"] => Ok(Strategy::Auto),
            ["whole"] => Ok(Strategy::Whole),
            ["fixed", n] => {
                let n: u32 = n.parse().map_err(|_| bad())?;
                if !(4096..=8 * 1024 * 1024).contains(&n) {
                    return Err(Error::invalid("rule strategy", "fixed block size must be 4096..=8388608 bytes"));
                }
                Ok(Strategy::Fixed(n))
            }
            ["cdc", a, b, c] => {
                let (min, avg, max): (u32, u32, u32) =
                    (a.parse().map_err(|_| bad())?, b.parse().map_err(|_| bad())?, c.parse().map_err(|_| bad())?);
                let ok = (64..=1_048_576).contains(&min)
                    && (256..=4_194_304).contains(&avg)
                    && (1024..=8 * 1024 * 1024).contains(&max)
                    && min < avg
                    && avg < max
                    && min % 2 == 0
                    && avg % 2 == 0
                    && max % 2 == 0;
                if !ok {
                    return Err(Error::invalid(
                        "rule strategy",
                        "cdc sizes must be even, min < avg < max and within fastcdc's limits",
                    ));
                }
                Ok(Strategy::Cdc { min, avg, max })
            }
            _ => Err(bad()),
        }
    }
}

impl std::fmt::Display for Strategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Strategy::Auto => write!(f, "auto"),
            Strategy::Whole => write!(f, "whole"),
            Strategy::Fixed(n) => write!(f, "fixed:{n}"),
            Strategy::Cdc { min, avg, max } => write!(f, "cdc:{min}:{avg}:{max}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    #[serde(rename = "match")]
    pub patterns: Vec<String>,
    pub action: Action,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
    #[serde(default)]
    pub compress: Compress,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mixed: Option<String>,
    #[serde(default)]
    pub case_insensitive: bool,
    /// Excluded directories matching this rule may be skipped by the scanner without descending.
    #[serde(default)]
    pub prune: bool,
    /// Where the rule came from (`builtin`, `prism`, `user`); informational.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleDoc {
    pub version: u32,
    #[serde(default)]
    pub unmatched: Option<UnmatchedPolicy>,
    #[serde(default, rename = "rule")]
    pub rules: Vec<Rule>,
}

impl RuleDoc {
    pub fn parse(text: &str) -> Result<RuleDoc> {
        if text.len() > MAX_RULES_TEXT_BYTES {
            return Err(Error::invalid("rules", format!("rules document is larger than {MAX_RULES_TEXT_BYTES} bytes")));
        }
        let doc: RuleDoc = toml::from_str(text).map_err(|e| Error::invalid("rules", e.to_string()))?;
        if doc.version != RULES_FORMAT_VERSION {
            return Err(Error::invalid(
                "rules",
                format!(
                    "unsupported rules version {} (this build understands version {RULES_FORMAT_VERSION})",
                    doc.version
                ),
            ));
        }
        if doc.rules.len() > MAX_RULES {
            return Err(Error::invalid("rules", format!("more than {MAX_RULES} rules")));
        }
        Ok(doc)
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).map_err(|e| Error::invalid("rules", e.to_string()))
    }
}

/// Concatenate rule documents (later documents win on conflicts because rules are ordered).
/// `origins[i]` labels the rules of `texts[i]`.
pub fn compose(texts: &[(&str, &str)]) -> Result<String> {
    let mut out = RuleDoc { version: RULES_FORMAT_VERSION, unmatched: None, rules: vec![] };
    for (origin, text) in texts {
        if text.trim().is_empty() {
            continue;
        }
        let doc = RuleDoc::parse(text)?;
        if doc.unmatched.is_some() {
            out.unmatched = doc.unmatched;
        }
        for mut r in doc.rules {
            if r.origin.is_none() {
                r.origin = Some((*origin).to_string());
            }
            out.rules.push(r);
        }
    }
    out.to_toml()
}

/// Where a decision came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DecisionSource {
    /// Hard-coded protection that no rule can override.
    Sealed {
        id: String,
    },
    Rule {
        id: String,
        origin: Option<String>,
    },
    Unmatched,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub include: bool,
    pub source: DecisionSource,
    pub category: String,
    pub reason: String,
    pub strategy: Strategy,
    pub compress: Compress,
    pub mixed: Option<String>,
}

impl Decision {
    pub fn explain(&self, path: &str) -> String {
        let verdict = if self.include { "SYNCED" } else { "NOT synced" };
        let by = match &self.source {
            DecisionSource::Sealed { id } => format!("sealed protection '{id}' (cannot be overridden)"),
            DecisionSource::Rule { id, origin } => {
                format!("rule '{id}'{}", origin.as_ref().map(|o| format!(" [{o}]")).unwrap_or_default())
            }
            DecisionSource::Unmatched => "no rule matches this path (unrecognised)".to_string(),
        };
        format!("{path}: {verdict} — {by}: {}", self.reason)
    }
}

struct Compiled {
    rule: Rule,
    strategy: Strategy,
}

/// Credential/secret patterns that are excluded no matter what (matched at any depth, case-insensitively).
const SEALED: &[(&str, &str, &[&str])] = &[
    (
        "launcher-accounts",
        "Launcher account and sign-in files contain tokens.",
        &[
            "**/accounts.json",
            "**/launcher_accounts*.json",
            "**/launcher_msa_credentials*.json",
            "**/launcher_profiles.json",
            "**/launcher_ms_*.json",
            "**/prismlauncher.cfg",
            "**/multimc.cfg",
            "**/polymc.cfg",
        ],
    ),
    (
        "credential-files",
        "Files named like credentials, tokens or secrets.",
        &[
            "**/.env",
            "**/.env.*",
            "**/credentials.json",
            "**/credentials",
            "**/client_secret*.json",
            "**/token.json",
            "**/tokens.json",
            "**/*.token",
            "**/.netrc",
            "**/.git-credentials",
            "**/.npmrc",
            "**/.pgpass",
        ],
    ),
    (
        "private-keys",
        "Private keys and key stores.",
        &[
            "**/*.pem",
            "**/*.key",
            "**/*.p12",
            "**/*.pfx",
            "**/*.jks",
            "**/*.keystore",
            "**/*.ppk",
            "**/id_rsa",
            "**/id_rsa.*",
            "**/id_dsa",
            "**/id_ecdsa",
            "**/id_ecdsa.*",
            "**/id_ed25519",
            "**/id_ed25519.*",
            "**/.ssh/**",
            "**/.gnupg/**",
            "**/.aws/**",
        ],
    ),
    ("packsync-state", "PackSync's own bookkeeping directory is never synced.", &["**/.packsync/**", ".packsync/**"]),
];

pub struct RuleSet {
    compiled: Vec<Compiled>,
    sealed_set: GlobSet,
    sealed_ids: Vec<(String, String)>,
    sealed_pattern_to_idx: Vec<usize>,
    /// (pattern index → rule index) for case-sensitive and case-insensitive sets.
    set_cs: GlobSet,
    map_cs: Vec<usize>,
    set_ci: GlobSet,
    map_ci: Vec<usize>,
    unmatched: UnmatchedPolicy,
    text_hash: Hash,
    game_root: String,
    prunes: Vec<usize>,
}

fn expand_game(pattern: &str, game_root: &str) -> String {
    if let Some(rest) = pattern.strip_prefix("$GAME/") {
        if game_root.is_empty() { rest.to_string() } else { format!("{game_root}/{rest}") }
    } else if pattern == "$GAME" {
        game_root.to_string()
    } else {
        pattern.to_string()
    }
}

fn build_glob(pattern: &str, ci: bool) -> Result<Glob> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .case_insensitive(ci)
        .backslash_escape(false)
        .build()
        .map_err(|e| Error::invalid("rule pattern", format!("{pattern:?}: {e}")))
}

/// Literal leading components of a pattern (before the first component containing a glob metachar).
fn literal_prefix(pattern: &str) -> Vec<&str> {
    let mut out = vec![];
    for c in pattern.split('/') {
        if c.contains(['*', '?', '[', '{']) {
            break;
        }
        out.push(c);
    }
    out
}

impl RuleSet {
    /// Compile a rules document. `game_root` replaces `$GAME`.
    pub fn compile(text: &str, game_root: &str) -> Result<RuleSet> {
        let doc = RuleDoc::parse(text)?;
        let mut ids = BTreeSet::new();
        let mut compiled = Vec::with_capacity(doc.rules.len());
        let (mut b_cs, mut b_ci) = (GlobSetBuilder::new(), GlobSetBuilder::new());
        let (mut map_cs, mut map_ci) = (vec![], vec![]);
        let mut prunes = vec![];

        for (i, r) in doc.rules.into_iter().enumerate() {
            if r.id.is_empty()
                || r.id.len() > 80
                || !r.id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            {
                return Err(Error::invalid("rule id", format!("{:?}: use letters, digits, '-', '_' or '.'", r.id)));
            }
            if !ids.insert(r.id.clone()) {
                return Err(Error::invalid("rule id", format!("duplicate rule id {:?}", r.id)));
            }
            if r.patterns.is_empty() || r.patterns.len() > MAX_PATTERNS_PER_RULE {
                return Err(Error::invalid("rule", format!("{}: needs 1..={MAX_PATTERNS_PER_RULE} patterns", r.id)));
            }
            let strategy = match &r.strategy {
                Some(s) => Strategy::parse(s)?,
                None => Strategy::Auto,
            };
            for p in &r.patterns {
                if p.is_empty() || p.starts_with('/') || p.contains("..") || p.contains('\\') {
                    return Err(Error::invalid(
                        "rule pattern",
                        format!("{}: {p:?} must be a relative, '/'-separated glob without '..'", r.id),
                    ));
                }
                let g = build_glob(&expand_game(p, game_root), r.case_insensitive)?;
                if r.case_insensitive {
                    b_ci.add(g);
                    map_ci.push(i);
                } else {
                    b_cs.add(g);
                    map_cs.push(i);
                }
            }
            if r.prune && r.action == Action::Exclude {
                prunes.push(i);
            }
            compiled.push(Compiled { rule: r, strategy });
        }

        let mut sb = GlobSetBuilder::new();
        let mut sealed_ids = vec![];
        let mut sealed_pattern_to_idx = vec![];
        for (idx, (id, reason, pats)) in SEALED.iter().enumerate() {
            sealed_ids.push((id.to_string(), reason.to_string()));
            for p in *pats {
                sb.add(build_glob(p, true)?);
                sealed_pattern_to_idx.push(idx);
            }
        }
        let set = |b: GlobSetBuilder| b.build().map_err(|e| Error::invalid("rules", e.to_string()));
        Ok(RuleSet {
            compiled,
            sealed_set: set(sb)?,
            sealed_ids,
            sealed_pattern_to_idx,
            set_cs: set(b_cs)?,
            map_cs,
            set_ci: set(b_ci)?,
            map_ci,
            unmatched: doc.unmatched.unwrap_or_default(),
            text_hash: Hash::of(text.as_bytes()),
            game_root: game_root.to_string(),
            prunes,
        })
    }

    pub fn text_hash(&self) -> Hash {
        self.text_hash
    }
    pub fn game_root(&self) -> &str {
        &self.game_root
    }

    fn sealed(&self, path: &str) -> Option<Decision> {
        let m = self.sealed_set.matches(path);
        let first = m.first()?;
        let (id, reason) = &self.sealed_ids[self.sealed_pattern_to_idx[*first]];
        Some(Decision {
            include: false,
            source: DecisionSource::Sealed { id: id.clone() },
            category: "secret".into(),
            reason: reason.clone(),
            strategy: Strategy::Auto,
            compress: Compress::Auto,
            mixed: None,
        })
    }

    fn last_matching_rule(&self, path: &str) -> Option<usize> {
        let mut best: Option<usize> = None;
        for i in self.set_cs.matches(path) {
            best = Some(best.map_or(self.map_cs[i], |b| b.max(self.map_cs[i])));
        }
        for i in self.set_ci.matches(path) {
            best = Some(best.map_or(self.map_ci[i], |b| b.max(self.map_ci[i])));
        }
        best
    }

    /// Decide about a **file** path (snapshot-relative, `/`-separated).
    pub fn decide(&self, path: &str) -> Decision {
        if let Some(d) = self.sealed(path) {
            return d;
        }
        match self.last_matching_rule(path) {
            Some(i) => {
                let c = &self.compiled[i];
                let include = c.rule.action == Action::Include;
                Decision {
                    include,
                    source: DecisionSource::Rule { id: c.rule.id.clone(), origin: c.rule.origin.clone() },
                    category: c.rule.category.clone(),
                    reason: c.rule.reason.clone(),
                    strategy: c.strategy,
                    compress: c.rule.compress,
                    mixed: if include { c.rule.mixed.clone() } else { None },
                }
            }
            None => Decision {
                include: self.unmatched == UnmatchedPolicy::Include,
                source: DecisionSource::Unmatched,
                category: "unrecognised".into(),
                reason: match self.unmatched {
                    UnmatchedPolicy::Exclude => {
                        "No rule mentions this path, so it is kept local. Add an include rule to sync it.".into()
                    }
                    UnmatchedPolicy::Include => "No rule mentions this path; the rules sync unrecognised files.".into(),
                },
                strategy: Strategy::Auto,
                compress: Compress::Auto,
                mixed: None,
            },
        }
    }

    /// A path that stands for "some file directly inside `dir`" (a name no real file has), to ask what the
    /// rules decide for a directory's content as a whole.
    fn inside(dir: &str) -> String {
        format!("{dir}/\u{1}x")
    }

    /// Whether a file inside `dir` would be synced (used for empty directories and for what lies below a
    /// link that is never followed).
    pub fn includes_inside(&self, dir: &str) -> bool {
        self.decide(&Self::inside(dir)).include
    }

    /// If the whole directory can be skipped by the scanner, why. A directory is skipped when it is
    /// sealed-excluded, or matches a `prune` exclude rule and no later include rule could match
    /// anything beneath it.
    pub fn prune_dir(&self, dir: &str) -> Option<Decision> {
        let sentinel = Self::inside(dir);
        if let Some(d) = self.sealed(&sentinel).or_else(|| self.sealed(dir)) {
            return Some(d);
        }
        let i = self.last_matching_rule(&sentinel)?;
        if !self.prunes.contains(&i) {
            return None;
        }
        // Could a later include rule match below `dir`?
        let dir_parts: Vec<&str> = dir.split('/').collect();
        for later in &self.compiled[i + 1..] {
            if later.rule.action != Action::Include {
                continue;
            }
            for p in &later.rule.patterns {
                let expanded = expand_game(p, &self.game_root);
                let lit = literal_prefix(&expanded);
                let n = lit.len().min(dir_parts.len());
                let overlaps = lit[..n]
                    .iter()
                    .zip(&dir_parts[..n])
                    .all(|(a, b)| if later.rule.case_insensitive { a.eq_ignore_ascii_case(b) } else { a == b });
                if overlaps {
                    return None;
                }
            }
        }
        let c = &self.compiled[i];
        Some(Decision {
            include: false,
            source: DecisionSource::Rule { id: c.rule.id.clone(), origin: c.rule.origin.clone() },
            category: c.rule.category.clone(),
            reason: c.rule.reason.clone(),
            strategy: Strategy::Auto,
            compress: Compress::Auto,
            mixed: None,
        })
    }

    /// Mixed-policy ids referenced by include rules (the engine checks that each exists).
    pub fn mixed_ids(&self) -> BTreeSet<String> {
        self.compiled.iter().filter_map(|c| c.rule.mixed.clone()).collect()
    }
}

/// The built-in defaults composed with the optional launcher fragment and user overrides.
pub fn effective_rules_text(launcher_fragment: &str, user_overrides: &str) -> Result<String> {
    compose(&[("builtin", DEFAULT_RULES_TOML), ("launcher", launcher_fragment), ("user", user_overrides)])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rs(game: &str) -> RuleSet {
        RuleSet::compile(DEFAULT_RULES_TOML, game).unwrap()
    }

    #[test]
    fn defaults_compile_for_all_layouts() {
        for g in [".minecraft", "minecraft", ""] {
            rs(g);
        }
    }

    #[test]
    fn default_decisions_for_a_realistic_tree() {
        let r = rs(".minecraft");
        let yes = [
            ".minecraft/saves/My World/level.dat",
            ".minecraft/saves/My World/region/r.0.0.mca",
            ".minecraft/saves/My World/datapacks/vanilla_tweaks.zip",
            ".minecraft/saves/My World/ftbquests/data.snbt",
            ".minecraft/mods/jei-1.20.1.jar",
            ".minecraft/config/ae2/common.json",
            ".minecraft/kubejs/server_scripts/recipes.js",
            ".minecraft/resourcepacks/Faithful.zip",
            ".minecraft/options.txt",
            ".minecraft/servers.dat",
            ".minecraft/journeymap/data/sp/My World~/waypoints/wp.json",
            ".minecraft/XaeroWaypoints/My World/dim%0/mw$default_1.txt",
        ];
        for p in yes {
            assert!(r.decide(p).include, "{p} should sync: {}", r.decide(p).explain(p));
        }
        let no = [
            ".minecraft/logs/latest.log",
            ".minecraft/crash-reports/crash-2026.txt",
            ".minecraft/saves/My World/session.lock",
            ".minecraft/screenshots/2026-01-01.png",
            ".minecraft/shaderpacks/BSL.zip",
            ".minecraft/.fabric/remappedJars/x.jar",
            ".minecraft/config/sodium-options.json",
            ".minecraft/mods/some-native.dll",
            ".minecraft/mods/libfoo.so",
            ".minecraft/backups/world-2026.zip",
            ".minecraft/natives/lwjgl.so",
            "natives/lwjgl.dll",
            ".minecraft/optionsof.txt",
            ".minecraft/journeymap/data/sp/My World~/DIM0/day/1,1.png",
            ".minecraft/hs_err_pid123.log",
        ];
        for p in no {
            assert!(!r.decide(p).include, "{p} must NOT sync: {}", r.decide(p).explain(p));
        }
        // unrecognised paths are excluded and labelled as such
        let d = r.decide(".minecraft/some_new_mod_dir/data.bin");
        assert!(!d.include);
        assert_eq!(d.source, DecisionSource::Unmatched);
    }

    #[test]
    fn datapack_zips_in_worlds_are_not_treated_as_backups() {
        let r = rs(".minecraft");
        assert!(r.decide(".minecraft/saves/W/datapacks/x.zip").include);
        assert!(!r.decide(".minecraft/saves/backup-2026.zip").include, "top-level zips beside worlds are backups");
    }

    #[test]
    fn options_txt_carries_its_mixed_policy() {
        let d = rs(".minecraft").decide(".minecraft/options.txt");
        assert_eq!(d.mixed.as_deref(), Some("minecraft.options_txt"));
    }

    #[test]
    fn sealed_credentials_cannot_be_included_by_any_rule() {
        let user = r#"
version = 1
unmatched = "include"
[[rule]]
id = "everything"
match = ["**"]
action = "include"
reason = "I selected the whole directory"
"#;
        let text = effective_rules_text("", user).unwrap();
        let r = RuleSet::compile(&text, ".minecraft").unwrap();
        for p in [
            "accounts.json",
            "instances/foo/.minecraft/launcher_accounts.json",
            ".minecraft/config/discord/.env",
            "x/y/PRISMLAUNCHER.CFG",
            "keys/server.PEM",
            ".ssh/id_ed25519",
            ".packsync/instance.json",
            "a/.packsync/tx/journal",
            "app/.aws/credentials",
            "prismlauncher.cfg",
        ] {
            let d = r.decide(p);
            assert!(!d.include, "{p} must stay excluded: {}", d.explain(p));
            assert!(matches!(d.source, DecisionSource::Sealed { .. }), "{p}");
        }
        assert!(r.decide(".minecraft/mods/x.jar").include, "the broad include still works for ordinary files");
    }

    #[test]
    fn last_matching_rule_wins_and_user_overrides_apply() {
        let user = r#"
version = 1
[[rule]]
id = "keep-screenshots"
match = ["$GAME/screenshots/**"]
action = "include"
reason = "I want my screenshots"
[[rule]]
id = "drop-quests"
match = ["$GAME/saves/**/ftbquests/**"]
action = "exclude"
reason = "test"
"#;
        let r = RuleSet::compile(&effective_rules_text("", user).unwrap(), ".minecraft").unwrap();
        assert!(r.decide(".minecraft/screenshots/a.png").include);
        assert!(!r.decide(".minecraft/saves/W/ftbquests/x.snbt").include);
        assert!(r.decide(".minecraft/saves/W/level.dat").include);
        let d = r.decide(".minecraft/screenshots/a.png");
        assert_eq!(d.source, DecisionSource::Rule { id: "keep-screenshots".into(), origin: Some("user".into()) });
    }

    #[test]
    fn pruning_respects_later_includes() {
        let defaults = rs(".minecraft");
        assert!(defaults.prune_dir(".minecraft/logs").is_some());
        assert!(defaults.prune_dir(".minecraft/crash-reports").is_some());
        assert!(defaults.prune_dir(".minecraft/saves").is_none(), "included areas are never pruned");
        assert!(defaults.prune_dir("natives").is_some());

        let user = r#"
version = 1
[[rule]]
id = "keep-one-cache"
match = ["$GAME/cache/important/**"]
action = "include"
reason = "x"
"#;
        let r = RuleSet::compile(&effective_rules_text("", user).unwrap(), ".minecraft").unwrap();
        assert!(r.prune_dir(".minecraft/cache").is_none(), "an include below the cache dir prevents pruning");
        assert!(r.decide(".minecraft/cache/important/a.dat").include);
        assert!(!r.decide(".minecraft/cache/other/a.dat").include);
        assert!(r.prune_dir(".minecraft/logs").is_some());
    }

    #[test]
    fn sealed_directories_are_pruned() {
        let r = rs("");
        assert!(r.prune_dir(".packsync").is_some());
        assert!(r.prune_dir("deep/.ssh").is_some());
    }

    #[test]
    fn plain_game_folder_layout_has_no_prefix() {
        let r = rs("");
        assert!(r.decide("saves/W/level.dat").include);
        assert!(r.decide("mods/a.jar").include);
        assert!(!r.decide("logs/latest.log").include);
    }

    #[test]
    fn invalid_documents_are_rejected_with_useful_errors() {
        for (doc, needle) in [
            ("version = 2", "unsupported rules version"),
            ("version = 1\n[[rule]]\nid = \"a\"\nmatch = []\naction = \"include\"", "needs 1..="),
            ("version = 1\n[[rule]]\nid = \"a\"\nmatch = [\"../x\"]\naction = \"include\"", "without '..'"),
            ("version = 1\n[[rule]]\nid = \"a\"\nmatch = [\"/abs\"]\naction = \"include\"", "relative"),
            (
                "version = 1\n[[rule]]\nid = \"a\"\nmatch = [\"x\"]\naction = \"include\"\n[[rule]]\nid = \"a\"\nmatch = [\"y\"]\naction = \"include\"",
                "duplicate rule id",
            ),
            (
                "version = 1\n[[rule]]\nid = \"a\"\nmatch = [\"x\"]\naction = \"include\"\nstrategy = \"fixed:7\"",
                "fixed block size",
            ),
            ("version = 1\n[[rule]]\nid = \"a b\"\nmatch = [\"x\"]\naction = \"include\"", "rule id"),
            ("not toml at all [", "invalid"),
        ] {
            let err =
                RuleSet::compile(doc, "").err().unwrap_or_else(|| panic!("{doc:?} should be rejected")).to_string();
            assert!(err.contains(needle), "{doc:?}: {err}");
        }
    }

    #[test]
    fn strategies_parse_and_roundtrip() {
        for s in ["auto", "whole", "fixed:65536", "cdc:16384:65536:262144"] {
            assert_eq!(Strategy::parse(s).unwrap().to_string(), s);
        }
        assert!(Strategy::parse("cdc:3:4:5").is_err());
        assert!(Strategy::parse("cdc:65537:65536:262144").is_err());
        assert!(Strategy::parse("nonsense").is_err());
    }

    #[test]
    fn compose_is_stable_and_hash_tracks_content() {
        let a = effective_rules_text("", "").unwrap();
        let b = effective_rules_text("", "").unwrap();
        assert_eq!(a, b);
        assert_eq!(RuleSet::compile(&a, "").unwrap().text_hash(), RuleSet::compile(&b, "").unwrap().text_hash());
        let c = effective_rules_text("", "version = 1\n[[rule]]\nid=\"z\"\nmatch=[\"q\"]\naction=\"include\"").unwrap();
        assert_ne!(RuleSet::compile(&a, "").unwrap().text_hash(), RuleSet::compile(&c, "").unwrap().text_hash());
    }
}

//! Suggestions for content no rule covers, and the rules a person's decisions turn into.
//!
//! The built-in rules know the usual places (worlds, mods, configs, …), but every modpack has mods that
//! keep progress somewhere else: VisualProspecting's ore maps, ServerUtilities' claims, JourneyMap's
//! waypoints. Unknown content is grouped by its top-level folder or file ([`crate::scan::ExcludedSummary::unmatched_groups`])
//! and each group gets a suggestion: include, leave out, or ask — with the reason shown to the person.
//! Their decisions become ordinary rules of the instance, so every computer applies the same choice.

use serde::{Deserialize, Serialize};

/// What PackSync proposes for an unknown group, and how sure it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Suggestion {
    /// From the instance root; folders end in `/` (`.minecraft/visualprospecting/`).
    pub path: String,
    pub files: u64,
    pub bytes: u64,
    /// The proposal.
    pub include: bool,
    /// `false`: PackSync is guessing and the person should decide.
    pub sure: bool,
    pub reason: String,
}

/// A person's answer for one group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleDecision {
    pub path: String,
    pub include: bool,
}

/// Mods and tools whose data is progress (or pack content) and belongs to the instance.
const KNOWN_PROGRESS: &[(&str, &str)] = &[
    ("visualprospecting", "VisualProspecting: the ores and fluids you have prospected"),
    ("serverutilities", "ServerUtilities: claims, homes, ranks and other player data"),
    ("journeymap", "JourneyMap: waypoints and the map you have explored"),
    ("xaerowaypoints", "Xaero's Minimap: your waypoints"),
    ("xaeroworldmap", "Xaero's World Map: the map you have explored"),
    ("xaero", "Xaero's maps: waypoints and explored map"),
    ("voxelmap", "VoxelMap: waypoints"),
    ("rei_minimap", "Rei's Minimap: waypoints"),
    ("botaniavars.dat", "Botania: saved progress"),
    ("tcnodetracker", "Thaumcraft Node Tracker: the aura nodes you have found"),
    ("local", "FTB mods: local player data (quests, teams)"),
    ("betterquesting", "BetterQuesting: quest data"),
    ("kubejs", "KubeJS: the pack's scripts and recipes"),
    ("scripts", "CraftTweaker / ZenScript: the pack's recipe scripts"),
    ("defaultconfigs", "Default configs the pack ships for new worlds"),
    ("global_packs", "Global data and resource packs of the pack"),
    ("openloader", "OpenLoader: data and resource packs of the pack"),
    ("schematics", "Your schematics"),
    ("structures", "Your saved structures"),
    ("hats", "Hats: your hat collection"),
    ("waypoints", "Waypoints"),
];

/// Generated, regenerable or machine-specific content.
const KNOWN_CACHE: &[(&str, &str)] = &[
    ("coretweaks", "CoreTweaks: a cache of transformed classes, rebuilt at start"),
    ("modernfix", "ModernFix: caches, rebuilt at start"),
    (".fabric", "Fabric: remapping cache"),
    (".mixin.out", "Mixin debug output"),
    ("bettercrashes-stackdeobfuscator-methods.csv", "BetterCrashes: a generated lookup table"),
    ("ezminer", "EZMiner: local cache"),
    (".analysis", "Not part of the game: a tool's working folder"),
    (".git", "Not part of the game: version control"),
    (".idea", "Not part of the game: editor settings"),
    (".vscode", "Not part of the game: editor settings"),
    ("mod-backups", "Backups of replaced mods"),
];

const CACHE_WORDS: &[&str] =
    &["cache", "tmp", "temp", "log", "crash", "backup", "dump", "debug", "telemetry", "download"];
const PROGRESS_WORDS: &[&str] =
    &["waypoint", "quest", "save", "player", "progress", "research", "stats", "claim", "map", "data"];
const BIG: u64 = 500 * 1024 * 1024;

/// Classify the unknown groups of a scan. `game_root` is the game folder relative to the instance root.
pub fn suggest(groups: &std::collections::BTreeMap<String, (u64, u64)>, game_root: &str) -> Vec<Suggestion> {
    let mut out: Vec<Suggestion> = groups
        .iter()
        .map(|(path, &(files, bytes))| {
            let (include, sure, reason) = classify(path, game_root, bytes);
            Suggestion { path: path.clone(), files, bytes, include, sure, reason }
        })
        .collect();
    // what the person has to decide first, biggest first
    out.sort_by(|a, b| a.sure.cmp(&b.sure).then(b.bytes.cmp(&a.bytes)));
    out
}

fn classify(path: &str, game_root: &str, bytes: u64) -> (bool, bool, String) {
    let in_game = game_root.is_empty() || path.starts_with(&format!("{game_root}/"));
    let rel = if game_root.is_empty() { path } else { path.strip_prefix(&format!("{game_root}/")).unwrap_or(path) };
    let name = rel.trim_end_matches('/').to_ascii_lowercase();
    let known = |list: &[(&str, &str)]| list.iter().find(|(n, _)| *n == name).map(|(_, r)| r.to_string());
    if let Some(r) = known(KNOWN_CACHE) {
        return (false, true, r);
    }
    if let Some(r) = known(KNOWN_PROGRESS)
        && in_game
    {
        return (true, true, r);
    }
    if !in_game {
        // beside the game folder: launcher and tool files, never the game's own
        return (false, false, "Beside the game folder: usually a tool's or the launcher's file".into());
    }
    if bytes > BIG {
        return (false, false, format!("Large ({} MB): sync only if it holds progress", bytes / (1024 * 1024)));
    }
    let has = |words: &[&str]| words.iter().any(|w| name.contains(w));
    if has(CACHE_WORDS) {
        return (false, false, "Looks like a cache, log or backup".into());
    }
    if has(PROGRESS_WORDS) {
        return (true, false, "Looks like mod data or progress".into());
    }
    let small_text = [".dat", ".json", ".nbt", ".snbt", ".cfg", ".toml", ".txt", ".properties", ".md"]
        .iter()
        .any(|e| name.ends_with(e));
    if small_text || rel.ends_with('/') {
        return (true, false, "Unknown mod data; small enough to sync".into());
    }
    (false, false, "Unknown file".into())
}

/// Escape glob metacharacters so a path matches only itself.
fn glob_literal(p: &str) -> String {
    let mut s = String::with_capacity(p.len());
    for c in p.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '{' | '}' | '\\' | '!') {
            s.push('[');
            s.push(c);
            s.push(']');
        } else {
            s.push(c);
        }
    }
    s
}

fn toml_string(s: &str) -> String {
    serde_json::to_string(s).expect("a string serialises") // a JSON string is a valid TOML basic string
}

/// The readable part of a decision rule's id: `.minecraft/serverutilities/` → `minecraft-serverutilities`.
fn slug(path: &str) -> String {
    let s: String = path
        .trim_end_matches('/')
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    s.trim_matches('-').to_string()
}

/// The rules document (TOML) that carries out `decisions`.
pub fn decisions_toml(decisions: &[RuleDecision]) -> String {
    let mut out = String::from("version = 1\n");
    for d in decisions {
        let pattern = match d.path.strip_suffix('/') {
            Some(dir) => format!("{}/**", glob_literal(dir)),
            None => glob_literal(&d.path),
        };
        let id = slug(&d.path);
        let (action, reason) = if d.include {
            ("include", format!("You chose to sync {}.", d.path))
        } else {
            ("exclude", format!("You chose not to sync {}.", d.path))
        };
        out.push_str(&format!(
            "\n[[rule]]\nid = {}\nmatch = [{}]\naction = \"{action}\"\ncategory = \"{}\"\nreason = {}\n",
            toml_string(&format!("user-{}-{id}", if d.include { "include" } else { "exclude" })),
            toml_string(&pattern),
            if d.include { "mod-data" } else { "excluded" },
            toml_string(&reason),
        ));
    }
    out
}

/// `rules` with `decisions` applied: decision rules go FIRST, so — the last matching rule winning — they
/// decide only paths no other rule matches (what they were asked about) and can never override a
/// protective exclusion (a cache inside an included folder, native files, locks). An earlier decision
/// about the same path is replaced, so a person can change their mind.
pub fn apply_decisions(rules: &str, decisions: &[RuleDecision]) -> crate::error::Result<String> {
    if decisions.is_empty() {
        return Ok(rules.to_string());
    }
    let mut doc = super::RuleDoc::parse(rules)?;
    let decided: std::collections::HashSet<String> = decisions.iter().map(|d| slug(&d.path)).collect();
    doc.rules.retain(|r| {
        let slug = r.id.strip_prefix("user-include-").or_else(|| r.id.strip_prefix("user-exclude-"));
        !slug.is_some_and(|s| decided.contains(s))
    });
    let mut new = super::RuleDoc::parse(&decisions_toml(decisions))?.rules;
    for r in &mut new {
        r.origin = Some("decision".into());
    }
    doc.rules.splice(0..0, new);
    doc.to_toml()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{DEFAULT_RULES_TOML, RuleSet, compose};
    use std::collections::BTreeMap;

    fn groups(items: &[(&str, u64)]) -> BTreeMap<String, (u64, u64)> {
        items.iter().map(|(p, b)| (p.to_string(), (3, *b))).collect()
    }

    #[test]
    fn known_progress_is_included_caches_are_not_and_the_rest_is_asked() {
        let s = suggest(
            &groups(&[
                (".minecraft/visualprospecting/", 40_000_000),
                (".minecraft/serverutilities/", 2_000),
                (".minecraft/BotaniaVars.dat", 300),
                (".minecraft/coretweaks/", 9_000_000),
                (".minecraft/shinymod_cache/", 1_000),
                (".minecraft/questbook/", 1_000),
                (".minecraft/mystery/", 1_000),
                (".minecraft/giant/", 900 * 1024 * 1024),
                (".analysis/", 1_000),
                ("notes.txt", 10),
            ]),
            ".minecraft",
        );
        let get = |p: &str| s.iter().find(|x| x.path == p).unwrap();
        assert!(get(".minecraft/visualprospecting/").include && get(".minecraft/visualprospecting/").sure);
        assert!(get(".minecraft/serverutilities/").include && get(".minecraft/serverutilities/").sure);
        assert!(get(".minecraft/BotaniaVars.dat").include, "names match case-insensitively");
        assert!(!get(".minecraft/coretweaks/").include && get(".minecraft/coretweaks/").sure);
        assert!(!get(".minecraft/shinymod_cache/").include && !get(".minecraft/shinymod_cache/").sure);
        assert!(get(".minecraft/questbook/").include && !get(".minecraft/questbook/").sure);
        assert!(get(".minecraft/mystery/").include && !get(".minecraft/mystery/").sure);
        assert!(!get(".minecraft/giant/").include && !get(".minecraft/giant/").sure, "big: ask, default no");
        assert!(!get(".analysis/").include && get(".analysis/").sure);
        assert!(!get("notes.txt").include && !get("notes.txt").sure, "beside the game folder");
        assert!(!s[0].sure, "undecided ones come first");
    }

    #[test]
    fn decisions_become_rules_that_win_over_the_defaults() {
        let decisions = vec![
            RuleDecision { path: ".minecraft/visualprospecting/".into(), include: true },
            RuleDecision { path: ".minecraft/BotaniaVars.dat".into(), include: true },
            RuleDecision { path: ".minecraft/odd [name]/".into(), include: true },
            RuleDecision { path: ".minecraft/coretweaks/".into(), include: false },
        ];
        let text = apply_decisions(&compose(&[("builtin", DEFAULT_RULES_TOML)]).unwrap(), &decisions).unwrap();
        let rs = RuleSet::compile(&text, ".minecraft").unwrap();
        assert!(rs.decide(".minecraft/visualprospecting/data/ores.bin").include);
        assert!(rs.decide(".minecraft/BotaniaVars.dat").include);
        assert!(rs.decide(".minecraft/odd [name]/x.dat").include, "glob characters are taken literally");
        assert!(!rs.decide(".minecraft/oddn/x.dat").include);
        assert!(!rs.decide(".minecraft/coretweaks/cache.bin").include);
        assert!(!rs.decide(".minecraft/visualprospectingX/a").include, "only that folder");
    }

    #[test]
    fn decisions_never_override_protective_rules_and_can_be_changed() {
        let base = compose(&[("builtin", DEFAULT_RULES_TOML)]).unwrap();
        // "sync all of .minecraft/logs" must not bring back what the defaults exclude on purpose
        let d = |p: &str, include| RuleDecision { path: p.into(), include };
        let text = apply_decisions(&base, &[d(".minecraft/logs/", true), d(".minecraft/newmod/", true)]).unwrap();
        let rs = RuleSet::compile(&text, ".minecraft").unwrap();
        assert!(!rs.decide(".minecraft/logs/latest.log").include, "a default exclusion still wins");
        assert!(rs.decide(".minecraft/newmod/a.dat").include);
        // changing one's mind replaces the earlier answer instead of failing on a duplicate rule id
        let again = apply_decisions(&text, &[d(".minecraft/newmod/", false)]).unwrap();
        let rs = RuleSet::compile(&again, ".minecraft").unwrap();
        assert!(!rs.decide(".minecraft/newmod/a.dat").include);
        let back = apply_decisions(&again, &[d(".minecraft/newmod/", true)]).unwrap();
        assert!(RuleSet::compile(&back, ".minecraft").unwrap().decide(".minecraft/newmod/a.dat").include);
    }
}

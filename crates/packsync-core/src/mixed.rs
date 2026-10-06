//! Mixed settings files: files that hold both portable data (keybinds, language, resource packs)
//! and machine-local data (graphics, window size, Java path).
//!
//! Policy: only **explicitly known, simple, line-based formats** are ever split. Unknown mod
//! configuration syntax (JSON/TOML/CFG of arbitrary mods) is synced as a whole file or excluded by
//! rules, never rewritten. The projection/merge here preserves unknown lines verbatim.
//!
//! * [`project`] produces the *portable projection* that is hashed and stored in a snapshot, so a
//!   change to a local-only key never creates a new snapshot.
//! * [`merge`] combines a restored portable projection with the local file's local-only values.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MixedFormat {
    /// `key:value` lines (Minecraft `options.txt`).
    KeyValueColon,
    /// `[Section]` headers with `key=value` lines (Prism `instance.cfg`).
    Ini,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MixedPolicy {
    /// Stable identifier recorded in manifests (`minecraft.options_txt`, `prism.instance_cfg`).
    pub id: String,
    /// Exact instance-root-relative path, with the game root already expanded.
    pub path: String,
    pub format: MixedFormat,
    /// When `Some`, ONLY these keys are portable and everything else is local (allow-list;
    /// preferred for formats we do not fully control, because unknown future keys stay local).
    /// INI keys may be qualified as `Section/Key`; unqualified INI keys match any section.
    pub portable_keys: Option<Vec<String>>,
    /// When `portable_keys` is `None`: these keys are local and everything else is portable.
    pub local_keys: Vec<String>,
    /// INI only: sections that are entirely local.
    pub local_sections: Vec<String>,
}

impl MixedPolicy {
    fn key_is_portable(&self, section: &str, key: &str) -> bool {
        if self.local_sections.iter().any(|s| s == section) {
            return false;
        }
        let qualified = format!("{section}/{key}");
        let matches = |list: &[String]| list.iter().any(|k| k == key || *k == qualified);
        match &self.portable_keys {
            Some(allow) => matches(allow),
            None => !matches(&self.local_keys),
        }
    }
}

/// One parsed line of a mixed file.
enum Line<'a> {
    /// Section header (INI) — carries the section name.
    Section(&'a str),
    /// A recognised `key<sep>value` pair.
    Pair { key: &'a str },
    /// Anything else (comments, blank lines, unknown syntax): preserved verbatim.
    Other,
}

fn classify<'a>(fmt: MixedFormat, line: &'a str) -> Line<'a> {
    let t = line.trim_end_matches(['\r', '\n']);
    match fmt {
        MixedFormat::KeyValueColon => match t.split_once(':') {
            Some((k, _)) if !k.is_empty() && !k.contains(char::is_whitespace) => Line::Pair { key: k },
            _ => Line::Other,
        },
        MixedFormat::Ini => {
            let trimmed = t.trim();
            if trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.len() >= 2 {
                Line::Section(&trimmed[1..trimmed.len() - 1])
            } else if trimmed.starts_with(';') || trimmed.starts_with('#') {
                Line::Other
            } else {
                match t.split_once('=') {
                    Some((k, _)) if !k.trim().is_empty() => Line::Pair { key: k.trim() },
                    _ => Line::Other,
                }
            }
        }
    }
}

fn text(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes).map_err(|_| Error::invalid("mixed settings file", "not valid UTF-8"))
}

/// Portable projection of `bytes`. Local-only keys are removed. For line-based `key:value` files
/// every other line is preserved byte-for-byte. For INI files the projection is **canonical**
/// (sections and keys sorted, comments dropped, values verbatim) so that
/// `project(merge(P, local)) == P` regardless of the key order in the local file; otherwise a
/// restore would immediately look like a modification. Deterministic.
pub fn project(policy: &MixedPolicy, bytes: &[u8]) -> Result<Vec<u8>> {
    let s = text(bytes)?;
    if policy.format == MixedFormat::Ini {
        use std::collections::BTreeMap;
        let mut sections: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        let mut section = "General".to_string();
        for line in s.split_inclusive('\n') {
            match classify(MixedFormat::Ini, line) {
                Line::Section(n) => section = n.to_string(),
                Line::Pair { key } => {
                    if policy.key_is_portable(&section, key) {
                        let mut l = line.trim_end_matches(['\r', '\n']).to_string();
                        l.push('\n');
                        sections.entry(section.clone()).or_default().insert(key.to_string(), l);
                    }
                }
                Line::Other => {}
            }
        }
        let mut out = String::new();
        for (name, keys) in &sections {
            out.push_str(&format!("[{name}]\n"));
            for l in keys.values() {
                out.push_str(l);
            }
        }
        return Ok(out.into_bytes());
    }
    let mut out = String::with_capacity(s.len());
    for line in s.split_inclusive('\n') {
        match classify(policy.format, line) {
            Line::Pair { key } => {
                if policy.key_is_portable("General", key) {
                    out.push_str(line);
                }
            }
            _ => out.push_str(line),
        }
    }
    Ok(out.into_bytes())
}

/// Combine a portable projection (from a snapshot) with the existing local file (if any).
/// Result: portable keys take the snapshot's values; local-only keys keep the local values.
pub fn merge(policy: &MixedPolicy, portable: &[u8], local: Option<&[u8]>) -> Result<Vec<u8>> {
    let portable_s = text(portable)?;
    let Some(local) = local else {
        return Ok(portable_s.as_bytes().to_vec());
    };
    let local_s = text(local)?;
    match policy.format {
        MixedFormat::KeyValueColon => merge_kv(policy, portable_s, local_s),
        MixedFormat::Ini => merge_ini(policy, portable_s, local_s),
    }
}

fn merge_kv(policy: &MixedPolicy, portable: &str, local: &str) -> Result<Vec<u8>> {
    // Portable file order wins; local-only keys from the local file are appended afterwards.
    let mut out = String::new();
    for line in portable.split_inclusive('\n') {
        out.push_str(line);
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    for line in local.split_inclusive('\n') {
        if let Line::Pair { key } = classify(MixedFormat::KeyValueColon, line)
            && !policy.key_is_portable("General", key)
        {
            out.push_str(line);
            if !line.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    Ok(out.into_bytes())
}

/// Parse `[Section]\nkey=value` into ordered (section, key, raw-line) triples.
fn ini_pairs(s: &str) -> Vec<(String, String, &str)> {
    let mut section = "General".to_string();
    let mut out = Vec::new();
    for line in s.split_inclusive('\n') {
        match classify(MixedFormat::Ini, line) {
            Line::Section(n) => section = n.to_string(),
            Line::Pair { key } => out.push((section.clone(), key.to_string(), line)),
            Line::Other => {}
        }
    }
    out
}

fn merge_ini(policy: &MixedPolicy, portable: &str, local: &str) -> Result<Vec<u8>> {
    use std::collections::BTreeMap;
    // portable values to apply: (section,key) → raw line
    let want: BTreeMap<(String, String), &str> = ini_pairs(portable).into_iter().map(|(s, k, l)| ((s, k), l)).collect();

    let mut out = String::new();
    let mut section = "General".to_string();
    let mut seen: std::collections::BTreeSet<(String, String)> = Default::default();
    let mut sections_present: Vec<String> = Vec::new();
    let mut last_section_end: BTreeMap<String, usize> = BTreeMap::new();

    for line in local.split_inclusive('\n') {
        match classify(MixedFormat::Ini, line) {
            Line::Section(n) => {
                section = n.to_string();
                sections_present.push(section.clone());
                out.push_str(line);
                if !line.ends_with('\n') {
                    out.push('\n');
                }
                last_section_end.insert(section.clone(), out.len());
            }
            Line::Pair { key } => {
                if policy.key_is_portable(&section, key) {
                    if let Some(new_line) = want.get(&(section.clone(), key.to_string())) {
                        out.push_str(new_line);
                        if !new_line.ends_with('\n') {
                            out.push('\n');
                        }
                        seen.insert((section.clone(), key.to_string()));
                    }
                    // portable key absent from the snapshot: the snapshot removed it → drop it
                } else {
                    out.push_str(line);
                    if !line.ends_with('\n') {
                        out.push('\n');
                    }
                }
                last_section_end.insert(section.clone(), out.len());
            }
            Line::Other => {
                out.push_str(line);
                if !line.ends_with('\n') {
                    out.push('\n');
                }
                last_section_end.insert(section.clone(), out.len());
            }
        }
    }
    // Portable keys the local file did not have yet: insert at the end of their section.
    let mut by_section: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for ((s, k), l) in &want {
        if !seen.contains(&(s.clone(), k.clone())) {
            by_section.entry(s.as_str()).or_default().push(l);
        }
    }
    // Insert in reverse byte order so earlier offsets stay valid.
    let mut inserts: Vec<(usize, String)> = Vec::new();
    let mut appended = String::new();
    for (s, lines) in by_section {
        let block: String =
            lines.iter().map(|l| if l.ends_with('\n') { l.to_string() } else { format!("{l}\n") }).collect();
        if let Some(&pos) = last_section_end.get(s) {
            inserts.push((pos, block));
        } else {
            appended.push_str(&format!("\n[{s}]\n{block}"));
        }
    }
    inserts.sort_by_key(|(p, _)| std::cmp::Reverse(*p));
    for (pos, block) in inserts {
        out.insert_str(pos, &block);
    }
    out.push_str(&appended);
    Ok(out.into_bytes())
}

/// Built-in policy for Minecraft's `options.txt` (graphics/window/device settings stay local).
pub fn options_txt_policy(game_root: &str) -> MixedPolicy {
    let path = if game_root.is_empty() { "options.txt".to_string() } else { format!("{game_root}/options.txt") };
    MixedPolicy {
        id: "minecraft.options_txt".into(),
        path,
        format: MixedFormat::KeyValueColon,
        portable_keys: None,
        local_keys: [
            "fullscreen",
            "fullscreenResolution",
            "overrideWidth",
            "overrideHeight",
            "graphicsMode",
            "fancyGraphics",
            "renderDistance",
            "simulationDistance",
            "maxFps",
            "enableVsync",
            "fboEnable",
            "useVbo",
            "biomeBlendRadius",
            "entityDistanceScaling",
            "particles",
            "mipmapLevels",
            "renderClouds",
            "ao",
            "soundDevice",
            "glDebugVerbosity",
            "lastServer",
            "onboardAccessibility",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
        local_sections: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv() -> MixedPolicy {
        MixedPolicy {
            id: "t.kv".into(),
            path: "options.txt".into(),
            format: MixedFormat::KeyValueColon,
            portable_keys: None,
            local_keys: vec!["renderDistance".into(), "fullscreen".into()],
            local_sections: vec![],
        }
    }

    fn ini() -> MixedPolicy {
        MixedPolicy {
            id: "t.ini".into(),
            path: "instance.cfg".into(),
            format: MixedFormat::Ini,
            portable_keys: Some(vec!["InstanceType".into(), "notes".into(), "General/ManagedPackID".into()]),
            local_keys: vec![],
            local_sections: vec!["UI".into()],
        }
    }

    #[test]
    fn kv_projection_drops_only_local_keys_and_keeps_unknown_lines() {
        let src = b"version:3465\nrenderDistance:32\nlang:en_us\n# weird line without colon\nfullscreen:true\nkey_key.jump:key.keyboard.space\r\n";
        let p = project(&kv(), src).unwrap();
        let p = String::from_utf8(p).unwrap();
        assert_eq!(p, "version:3465\nlang:en_us\n# weird line without colon\nkey_key.jump:key.keyboard.space\r\n");
    }

    #[test]
    fn projection_is_independent_of_local_values() {
        let a = project(&kv(), b"renderDistance:8\nlang:en_us\nfullscreen:false\n").unwrap();
        let b = project(&kv(), b"renderDistance:32\nlang:en_us\nfullscreen:true\n").unwrap();
        assert_eq!(a, b, "changing only local keys must not change the snapshot content");
    }

    #[test]
    fn kv_merge_keeps_local_values_and_takes_portable_ones() {
        let portable = b"lang:de_de\nguiScale:3\n";
        let local = b"lang:en_us\nrenderDistance:12\nfullscreen:true\nguiScale:2\n";
        let out = String::from_utf8(merge(&kv(), portable, Some(local)).unwrap()).unwrap();
        assert!(out.contains("lang:de_de"));
        assert!(!out.contains("lang:en_us"));
        assert!(out.contains("guiScale:3") && !out.contains("guiScale:2"));
        assert!(out.contains("renderDistance:12"));
        assert!(out.contains("fullscreen:true"));
    }

    #[test]
    fn kv_merge_without_local_file_is_just_the_projection() {
        let out = merge(&kv(), b"lang:de_de\n", None).unwrap();
        assert_eq!(out, b"lang:de_de\n");
    }

    #[test]
    fn ini_projection_keeps_only_allowlisted_keys() {
        let src = b"[General]\nConfigVersion=1.3\nInstanceType=OneSix\nJavaPath=/usr/lib/jvm/x/bin/java\nnotes=hello\nname=My Pack\n\n[UI]\nmods_Page\\Columns=\"AAAA\"\n";
        let p = String::from_utf8(project(&ini(), src).unwrap()).unwrap();
        assert_eq!(p, "[General]\nInstanceType=OneSix\nnotes=hello\n");
        // canonical: key order in the source does not matter
        let reordered = b"[General]\nnotes=hello\nInstanceType=OneSix\n";
        assert_eq!(project(&ini(), reordered).unwrap(), p.as_bytes());
        assert!(!p.contains("JavaPath") && !p.contains("UI"));
    }

    #[test]
    fn ini_merge_updates_portable_keys_and_preserves_everything_else() {
        let portable = b"[General]\nInstanceType=OneSix\nnotes=from cloud\nManagedPackID=abc\n";
        let local = b"[General]\nInstanceType=OneSix\nJavaPath=/opt/java/bin/java\nMaxMemAlloc=8192\nnotes=old\nname=Local Name\n\n[UI]\nfoo=bar\n";
        let out = String::from_utf8(merge(&ini(), portable, Some(local)).unwrap()).unwrap();
        assert!(out.contains("notes=from cloud") && !out.contains("notes=old"));
        assert!(out.contains("JavaPath=/opt/java/bin/java"), "{out}");
        assert!(out.contains("MaxMemAlloc=8192"));
        assert!(out.contains("name=Local Name"));
        assert!(out.contains("[UI]\nfoo=bar"));
        assert!(out.contains("ManagedPackID=abc"), "new portable key is added: {out}");
        // idempotent
        let again = merge(&ini(), &project(&ini(), out.as_bytes()).unwrap(), Some(out.as_bytes())).unwrap();
        assert_eq!(String::from_utf8(again).unwrap(), out);
    }

    #[test]
    fn projection_of_a_merge_is_the_projection_that_went_in() {
        // The invariant restores rely on: merging a snapshot's projection into any local file and
        // projecting again yields the same bytes (otherwise every restore would look like an edit).
        let portable = project(&ini(), b"[General]\nnotes=n\nInstanceType=OneSix\nManagedPackID=7\n").unwrap();
        for local in [
            &b"[General]\nJavaPath=/x\nnotes=old\n[UI]\na=b\n"[..],
            b"[UI]\na=b\n[General]\nManagedPackID=1\nz=1\n",
            b"",
        ] {
            let merged = merge(&ini(), &portable, if local.is_empty() { None } else { Some(local) }).unwrap();
            assert_eq!(project(&ini(), &merged).unwrap(), portable);
        }
        let kvp = project(&kv(), b"lang:de\nrenderDistance:8\nguiScale:2\n").unwrap();
        let merged = merge(&kv(), &kvp, Some(b"renderDistance:12\nlang:en\n")).unwrap();
        assert_eq!(project(&kv(), &merged).unwrap(), kvp);
    }

    #[test]
    fn rejects_non_utf8() {
        assert!(project(&kv(), &[0xff, 0xfe, 0x00]).is_err());
    }
}

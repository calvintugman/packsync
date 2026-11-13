//! Snapshot manifests (remote format v1, `docs/architecture.md`).
//!
//! A snapshot is an immutable, self-describing record of an instance at one moment: every synced
//! path with its content hashes, plus identity (instance, device), ancestry (`parents`), the
//! effective rules, launcher/pack identity and the session it came from. Its id is the BLAKE3 hash
//! of the canonical (uncompressed) manifest bytes, so a manifest is verifiable from its name alone.
//!
//! Manifests are **untrusted input** when read from the cloud: [`Manifest::decode`] enforces size
//! limits, strictly sorted unique valid paths, tree consistency, recipe/size rules and stats.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::chunk::MAX_BLOB;
use crate::codec::{compress_doc, decompress_doc};
use crate::error::{Error, Result};
use crate::hash::Hash;
use crate::ids::{DeviceId, InstanceId, SessionId, SnapshotId};
use crate::launcher::{PackIdentity, WorldInfo};
use crate::paths::validate_relpath;

pub const SNAPSHOT_FORMAT: &str = "packsync.snapshot";
pub const SNAPSHOT_VERSION: u32 = 1;
pub const MAX_MANIFEST_RAW: u64 = 256 * 1024 * 1024;
pub const MAX_MANIFEST_STORED: u64 = 128 * 1024 * 1024;
pub const MAX_ENTRIES: usize = 3_000_000;
pub const MAX_TOTAL_BYTES: u64 = 1 << 50;
pub const MAX_RECIPE_CHUNKS: usize = 1 << 20;
pub const MAX_PARENTS: usize = 8;
const MAX_NAME: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryType {
    #[serde(rename = "f")]
    File,
    #[serde(rename = "d")]
    Dir,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Instance-root-relative path, `/`-separated.
    pub p: String,
    pub t: EntryType,
    /// Size in bytes (files). For mixed files this is the size of the portable projection.
    #[serde(default)]
    pub s: u64,
    /// Modification time, milliseconds since the epoch (restore fidelity only; never identity).
    #[serde(default)]
    pub m: i64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub x: bool,
    /// Whole-content BLAKE3 (files).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub h: Option<Hash>,
    /// Recipe blob (multi-chunk files). `None` means the file is the single blob `h`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r: Option<Hash>,
    /// Mixed-policy id: the stored content is the portable projection of the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mx: Option<String>,
}

impl Entry {
    pub fn dir(path: impl Into<String>) -> Entry {
        Entry { p: path.into(), t: EntryType::Dir, s: 0, m: 0, x: false, h: None, r: None, mx: None }
    }
    pub fn is_file(&self) -> bool {
        self.t == EntryType::File
    }
    /// Content identity used for change detection (everything except timestamps).
    pub fn same_content(&self, o: &Entry) -> bool {
        self.t == o.t && self.s == o.s && self.h == o.h && self.x == o.x && self.mx == o.mx
    }
}

/// Chunk list of a multi-chunk file, stored as a blob in a pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipe {
    pub chunks: Vec<(Hash, u32)>,
}

const RECIPE_MAGIC: &[u8; 4] = b"PSR1";

impl Recipe {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.chunks.len() * 36);
        out.extend(RECIPE_MAGIC);
        out.extend((self.chunks.len() as u32).to_le_bytes());
        for (h, len) in &self.chunks {
            out.extend(h.0);
            out.extend(len.to_le_bytes());
        }
        out
    }
    pub fn hash(&self) -> Hash {
        Hash::of(&self.encode())
    }
    /// Decode and validate: bounded count, every chunk non-empty and ≤ `MAX_BLOB`, lengths sum to `expected_total`.
    pub fn decode(b: &[u8], expected_total: u64) -> Result<Recipe> {
        let bad = |m: &str| Error::invalid("recipe", m.to_string());
        if b.len() < 8 || &b[..4] != RECIPE_MAGIC {
            return Err(bad("bad magic"));
        }
        let n = u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize;
        if n == 0 || n > MAX_RECIPE_CHUNKS || b.len() != 8 + n * 36 {
            return Err(bad("chunk count does not match the recipe length"));
        }
        let mut chunks = Vec::with_capacity(n);
        let mut total = 0u64;
        for i in 0..n {
            let o = 8 + i * 36;
            let h = Hash(b[o..o + 32].try_into().unwrap());
            let len = u32::from_le_bytes(b[o + 32..o + 36].try_into().unwrap());
            if len == 0 || len as usize > MAX_BLOB {
                return Err(bad("chunk length out of range"));
            }
            total = total.checked_add(len as u64).ok_or_else(|| bad("size overflow"))?;
            chunks.push((h, len));
        }
        if total != expected_total {
            return Err(bad(&format!("chunk lengths sum to {total}, file size is {expected_total}")));
        }
        Ok(Recipe { chunks })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotKind {
    /// A normal snapshot; participates in head computation.
    #[default]
    Normal,
    /// Taken after a crash/unclean exit: quarantined, never an automatic head or fast-forward target.
    Crash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionOutcome {
    Clean,
    Crashed,
    /// The session record was open but no live game process existed (power loss, kill -9, ...).
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub started: String,
    pub ended: Option<String>,
    pub outcome: Option<SessionOutcome>,
    /// The snapshot this play session started from.
    pub base: Option<SnapshotId>,
    /// Played without a cloud connection.
    #[serde(default)]
    pub offline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceRef {
    pub id: InstanceId,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRef {
    pub id: DeviceId,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkOrigin {
    pub instance: InstanceId,
    pub snapshot: SnapshotId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutInfo {
    pub launcher: String,
    /// Game directory relative to the instance root.
    pub game_root: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulesInfo {
    pub version: u32,
    pub hash: Hash,
    /// The complete effective rules document. Devices use the *base snapshot's* rules, not their own defaults.
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: String,
    pub version: u32,
    pub instance: InstanceRef,
    pub device: DeviceRef,
    pub created: String,
    pub parents: Vec<SnapshotId>,
    #[serde(default)]
    pub kind: SnapshotKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_of: Option<ForkOrigin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionSummary>,
    pub layout: LayoutInfo,
    #[serde(default)]
    pub pack: PackIdentity,
    #[serde(default)]
    pub worlds: Vec<WorldInfo>,
    pub rules: RulesInfo,
    pub stats: Stats,
    /// Free-form note ("promoted crash save", "local state before restoring …").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub entries: Vec<Entry>,
}

pub fn now_rfc3339() -> String {
    OffsetDateTime::now_utc().format(&Rfc3339).expect("RFC 3339 formatting cannot fail")
}

pub fn parse_rfc3339(s: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(s, &Rfc3339).ok()
}

fn clean_name(what: &'static str, s: &str) -> Result<()> {
    if s.len() > MAX_NAME || s.chars().any(|c| c.is_control()) {
        return Err(Error::invalid(what, "too long or contains control characters"));
    }
    Ok(())
}

impl Manifest {
    pub fn new(instance: InstanceRef, device: DeviceRef, layout: LayoutInfo, rules: RulesInfo) -> Manifest {
        Manifest {
            format: SNAPSHOT_FORMAT.into(),
            version: SNAPSHOT_VERSION,
            instance,
            device,
            created: now_rfc3339(),
            parents: vec![],
            kind: SnapshotKind::Normal,
            fork_of: None,
            session: None,
            layout,
            pack: PackIdentity::default(),
            worlds: vec![],
            rules,
            stats: Stats::default(),
            note: None,
            entries: vec![],
        }
    }

    /// Sort entries into canonical order, compute stats and validate. Call before [`Manifest::encode`].
    pub fn finalize(&mut self) -> Result<()> {
        self.entries.sort_by(|a, b| a.p.as_bytes().cmp(b.p.as_bytes()));
        self.stats = self.compute_stats();
        self.validate()
    }

    fn compute_stats(&self) -> Stats {
        let mut st = Stats::default();
        for e in &self.entries {
            match e.t {
                EntryType::File => {
                    st.files += 1;
                    st.bytes += e.s;
                }
                EntryType::Dir => st.dirs += 1,
            }
        }
        st
    }

    pub fn validate(&self) -> Result<()> {
        let inv = |d: String| Error::invalid("snapshot manifest", d);
        if self.format != SNAPSHOT_FORMAT {
            return Err(inv(format!("unexpected format {:?}", self.format)));
        }
        if self.version != SNAPSHOT_VERSION {
            return Err(Error::Unsupported(format!(
                "snapshot format version {} (this build reads version {SNAPSHOT_VERSION}); update PackSync",
                self.version
            )));
        }
        clean_name("instance name", &self.instance.name)?;
        clean_name("device name", &self.device.name)?;
        clean_name("game root", &self.layout.game_root)?;
        if !self.layout.game_root.is_empty() {
            validate_relpath(&self.layout.game_root).map_err(|e| inv(format!("game root: {e}")))?;
        }
        if parse_rfc3339(&self.created).is_none() {
            return Err(inv(format!("bad timestamp {:?}", self.created)));
        }
        if self.parents.len() > MAX_PARENTS || self.parents.iter().collect::<HashSet<_>>().len() != self.parents.len() {
            return Err(inv("too many or duplicate parents".into()));
        }
        if self.rules.text.len() > crate::rules::MAX_RULES_TEXT_BYTES
            || Hash::of(self.rules.text.as_bytes()) != self.rules.hash
        {
            return Err(inv("embedded rules do not match their hash".into()));
        }
        if self.entries.len() > MAX_ENTRIES {
            return Err(inv(format!("{} entries exceed the limit of {MAX_ENTRIES}", self.entries.len())));
        }
        let empty = Hash::of(b"");
        let mut total: u64 = 0;
        let mut files: HashSet<&str> = HashSet::new();
        let mut prev: Option<&str> = None;
        for e in &self.entries {
            validate_relpath(&e.p).map_err(|er| inv(format!("path {:?}: {er}", e.p)))?;
            if let Some(p) = prev
                && p.as_bytes() >= e.p.as_bytes()
            {
                return Err(inv(format!("entries not strictly sorted at {:?}", e.p)));
            }
            prev = Some(&e.p);
            match e.t {
                EntryType::Dir => {
                    if e.s != 0 || e.h.is_some() || e.r.is_some() || e.mx.is_some() || e.x {
                        return Err(inv(format!("directory {:?} carries file fields", e.p)));
                    }
                }
                EntryType::File => {
                    let h = e.h.ok_or_else(|| inv(format!("file {:?} has no content hash", e.p)))?;
                    total = total
                        .checked_add(e.s)
                        .filter(|t| *t <= MAX_TOTAL_BYTES)
                        .ok_or_else(|| inv("total size too large".into()))?;
                    if e.s == 0 {
                        if e.r.is_some() || h != empty {
                            return Err(inv(format!(
                                "empty file {:?} must have the empty-content hash and no recipe",
                                e.p
                            )));
                        }
                    } else if e.r.is_none() && e.s as usize > MAX_BLOB {
                        return Err(inv(format!("file {:?} is larger than one blob but has no recipe", e.p)));
                    }
                    if let Some(id) = &e.mx
                        && (id.is_empty()
                            || id.len() > 64
                            || !id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
                    {
                        return Err(inv(format!("bad mixed-policy id on {:?}", e.p)));
                    }
                    files.insert(&e.p);
                }
            }
        }
        // A file may not also be a directory ancestor of another entry.
        for e in &self.entries {
            let mut idx = 0;
            while let Some(pos) = e.p[idx..].find('/') {
                idx += pos;
                if files.contains(&e.p[..idx]) {
                    return Err(inv(format!("{:?} is both a file and a parent directory of {:?}", &e.p[..idx], e.p)));
                }
                idx += 1;
            }
        }
        if self.stats != self.compute_stats() {
            return Err(inv("stats do not match the entries".into()));
        }
        Ok(())
    }

    /// Canonical encoding → `(snapshot id, stored bytes)`. The id is BLAKE3 of the uncompressed JSON.
    pub fn encode(&self) -> Result<(SnapshotId, Vec<u8>)> {
        let json = serde_json::to_vec(self)?;
        if json.len() as u64 > MAX_MANIFEST_RAW {
            return Err(Error::invalid("snapshot manifest", "manifest too large"));
        }
        Ok((SnapshotId(Hash::of(&json)), compress_doc(&json)))
    }

    /// Decode untrusted stored bytes. When `expected` is given the content hash must match it.
    pub fn decode(stored: &[u8], expected: Option<SnapshotId>) -> Result<(Manifest, SnapshotId)> {
        if stored.len() as u64 > MAX_MANIFEST_STORED {
            return Err(Error::invalid("snapshot manifest", "stored object is implausibly large"));
        }
        let json = decompress_doc(stored, MAX_MANIFEST_RAW)?;
        let id = SnapshotId(Hash::of(&json));
        if let Some(exp) = expected
            && exp != id
        {
            return Err(Error::Corrupt(format!(
                "snapshot {} does not match its content hash {}",
                exp.short(),
                id.short()
            )));
        }
        let m: Manifest =
            serde_json::from_slice(&json).map_err(|e| Error::Corrupt(format!("snapshot manifest: {e}")))?;
        m.validate()?;
        Ok((m, id))
    }

    pub fn entry(&self, path: &str) -> Option<&Entry> {
        self.entries.binary_search_by(|e| e.p.as_bytes().cmp(path.as_bytes())).ok().map(|i| &self.entries[i])
    }

    pub fn created_at(&self) -> Option<OffsetDateTime> {
        parse_rfc3339(&self.created)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffKind {
    Added,
    Removed,
    Modified,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffItem {
    pub path: String,
    pub kind: DiffKind,
}

/// Compare two sorted entry lists by content identity (timestamps ignored).
pub fn diff_entries(a: &[Entry], b: &[Entry]) -> Vec<DiffItem> {
    let mut out = vec![];
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        match (a.get(i), b.get(j)) {
            (Some(x), Some(y)) => match x.p.as_bytes().cmp(y.p.as_bytes()) {
                std::cmp::Ordering::Less => {
                    out.push(DiffItem { path: x.p.clone(), kind: DiffKind::Removed });
                    i += 1;
                }
                std::cmp::Ordering::Greater => {
                    out.push(DiffItem { path: y.p.clone(), kind: DiffKind::Added });
                    j += 1;
                }
                std::cmp::Ordering::Equal => {
                    if !x.same_content(y) {
                        out.push(DiffItem { path: x.p.clone(), kind: DiffKind::Modified });
                    }
                    i += 1;
                    j += 1;
                }
            },
            (Some(x), None) => {
                out.push(DiffItem { path: x.p.clone(), kind: DiffKind::Removed });
                i += 1;
            }
            (None, Some(y)) => {
                out.push(DiffItem { path: y.p.clone(), kind: DiffKind::Added });
                j += 1;
            }
            (None, None) => unreachable!(),
        }
    }
    out
}

/// Summary used by history views.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffSummary {
    pub added: u64,
    pub removed: u64,
    pub modified: u64,
}

pub fn summarize(items: &[DiffItem]) -> DiffSummary {
    let mut s = DiffSummary::default();
    for i in items {
        match i.kind {
            DiffKind::Added => s.added += 1,
            DiffKind::Removed => s.removed += 1,
            DiffKind::Modified => s.modified += 1,
        }
    }
    s
}

/// What a change between two versions of an instance means to the person: mods added, removed or
/// updated (a jar replaced by a newer one of the same mod), the pack's versions, configs and worlds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSummary {
    pub mods_added: Vec<String>,
    pub mods_removed: Vec<String>,
    pub mods_updated: Vec<String>,
    pub config_files: u64,
    pub world_files: u64,
    pub other_files: u64,
    /// `Minecraft 1.20.1 · Forge 47.4.0` → `… 47.4.16`, when the pack's versions changed.
    pub pack_versions: Option<(String, String)>,
}

/// A mod's name without its version and file type: `jei-1.20.1-15.2.0.jar` → `jei`.
fn mod_stem(file: &str) -> String {
    let lower = file.to_ascii_lowercase();
    let base = lower.trim_end_matches(".disabled");
    let base = base.rsplit_once('.').map(|(b, _)| b).unwrap_or(base);
    let mut out = String::new();
    let chars: Vec<char> = base.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        let next_digit = chars.get(i + 1).is_some_and(|n| n.is_ascii_digit())
            || (matches!(chars.get(i + 1), Some('v' | 'm'))
                && chars.get(i + 2).is_some_and(|n| n.is_ascii_digit() || *n == 'c'));
        if matches!(c, '-' | '_' | '+' | ' ') && next_digit {
            break;
        }
        out.push(*c);
    }
    out.trim_end_matches(['-', '_', ' ']).to_string()
}

fn pack_line(p: &crate::launcher::PackIdentity) -> String {
    let mut s = p.minecraft_version.as_deref().map(|v| format!("Minecraft {v}")).unwrap_or_default();
    if let Some(l) = &p.loader {
        let mut kind = l.kind.clone();
        if let Some(f) = kind.get_mut(0..1) {
            f.make_ascii_uppercase();
        }
        s = format!("{s} · {kind} {}", l.version).trim_start_matches(" · ").to_string();
    }
    s
}

/// Describe the change from `before` to `after` of an instance whose game folder is `game_root`.
pub fn change_summary(before: &Manifest, after: &Manifest, game_root: &str) -> ChangeSummary {
    let g = if game_root.is_empty() { String::new() } else { format!("{game_root}/") };
    let mods = format!("{g}mods/");
    let mut s = ChangeSummary::default();
    let mut added: BTreeMap<String, String> = BTreeMap::new();
    let mut removed: BTreeMap<String, String> = BTreeMap::new();
    let (old, new) = (entry_map(before), entry_map(after));
    for d in diff_entries(&before.entries, &after.entries) {
        let p = d.path.as_str();
        if !(old.get(p).is_some_and(|e| e.is_file()) || new.get(p).is_some_and(|e| e.is_file())) {
            continue;
        }
        match d.path.strip_prefix(&mods) {
            Some(file) if !file.contains('/') => {
                let name = file.to_string();
                match d.kind {
                    DiffKind::Added => {
                        added.insert(mod_stem(&name), name);
                    }
                    DiffKind::Removed => {
                        removed.insert(mod_stem(&name), name);
                    }
                    DiffKind::Modified => s.mods_updated.push(name),
                }
            }
            _ if d.path.starts_with(&format!("{g}config/")) || d.path.starts_with(&format!("{g}defaultconfigs/")) => {
                s.config_files += 1
            }
            _ if d.path.starts_with(&format!("{g}saves/")) => s.world_files += 1,
            _ => s.other_files += 1,
        }
    }
    for (stem, file) in added {
        match removed.remove(&stem) {
            Some(_) => s.mods_updated.push(file),
            None => s.mods_added.push(file),
        }
    }
    s.mods_removed = removed.into_values().collect();
    s.mods_updated.sort();
    if before.pack != after.pack {
        s.pack_versions = Some((pack_line(&before.pack), pack_line(&after.pack)));
    }
    s
}

/// Map of path → entry (for quick lookups where order does not matter).
pub fn entry_map(m: &Manifest) -> BTreeMap<&str, &Entry> {
    m.entries.iter().map(|e| (e.p.as_str(), e)).collect()
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;

    pub fn sample_rules() -> RulesInfo {
        let text = crate::rules::DEFAULT_RULES_TOML.to_string();
        RulesInfo { version: 1, hash: Hash::of(text.as_bytes()), text }
    }

    pub fn sample() -> Manifest {
        let mut m = Manifest::new(
            InstanceRef { id: InstanceId::new(), name: "Test Pack".into() },
            DeviceRef { id: DeviceId::new(), name: "desk".into() },
            LayoutInfo { launcher: "prism".into(), game_root: ".minecraft".into() },
            sample_rules(),
        );
        let file = |p: &str, data: &[u8]| Entry {
            p: p.into(),
            t: EntryType::File,
            s: data.len() as u64,
            m: 1_700_000_000_000,
            x: false,
            h: Some(Hash::of(data)),
            r: None,
            mx: None,
        };
        m.entries = vec![
            file(".minecraft/mods/a.jar", b"jar-a"),
            file(".minecraft/mods/b.jar", b"jar-b"),
            file(".minecraft/saves/W/level.dat", b"level"),
            file(".minecraft/empty.txt", b""),
            Entry::dir(".minecraft/saves/W/empty-dir"),
            file(".minecraft/a-b", b"x"),
            file(".minecraft/a.txt", b"y"),
        ];
        m.finalize().unwrap();
        m
    }
}

#[cfg(test)]
mod change_tests {
    use super::*;

    #[test]
    fn mod_names_lose_their_versions() {
        assert_eq!(mod_stem("jei-1.20.1-forge-15.2.0.27.jar"), "jei");
        assert_eq!(mod_stem("appliedenergistics2-forge-15.0.11.jar"), "appliedenergistics2-forge");
        assert_eq!(mod_stem("GregTech-5.09.45.jar"), "gregtech");
        assert_eq!(mod_stem("Botania r1.10-363.jar"), "botania r1.10");
        assert_eq!(mod_stem("journeymap-1.20.1-5.9.18-forge.jar.disabled"), "journeymap");
        assert_eq!(mod_stem("sodium-fabric-mc1.20.1-0.5.3.jar"), "sodium-fabric");
        assert_eq!(mod_stem("create_v0.5.1.jar"), "create");
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    #[test]
    fn encode_decode_roundtrip_and_id_is_content_hash() {
        let m = sample();
        let (id, stored) = m.encode().unwrap();
        let (back, id2) = Manifest::decode(&stored, Some(id)).unwrap();
        assert_eq!(id, id2);
        assert_eq!(back, m);
        // deterministic
        assert_eq!(m.encode().unwrap().0, id);
        // entries are sorted bytewise ('-' < '.' < '/')
        let paths: Vec<_> = back.entries.iter().map(|e| e.p.as_str()).collect();
        let mut sorted = paths.clone();
        sorted.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        assert_eq!(paths, sorted);
        assert_eq!(back.stats, Stats { files: 6, dirs: 1, bytes: ((5 + 5 + 5) + 1 + 1) });
    }

    #[test]
    fn wrong_name_and_tampering_are_detected() {
        let m = sample();
        let (id, stored) = m.encode().unwrap();
        assert!(Manifest::decode(&stored, Some(SnapshotId(Hash::of(b"nope")))).is_err());
        let mut t = stored.clone();
        let n = t.len();
        t[n / 2] ^= 0x01;
        assert!(Manifest::decode(&t, Some(id)).is_err());
        assert!(Manifest::decode(b"garbage", None).is_err());
    }

    fn reencode(m: &Manifest) -> Vec<u8> {
        compress_doc(&serde_json::to_vec(m).unwrap())
    }

    #[test]
    fn malicious_manifests_are_rejected() {
        let ok = sample();
        let mutate = |f: &dyn Fn(&mut Manifest)| {
            let mut m = ok.clone();
            f(&mut m);
            Manifest::decode(&reencode(&m), None)
        };
        assert!(mutate(&|_| {}).is_ok());
        for (name, r) in [
            ("traversal", mutate(&|m| m.entries[0].p = "../../etc/passwd".into())),
            ("absolute", mutate(&|m| m.entries[0].p = "/etc/passwd".into())),
            ("backslash", mutate(&|m| m.entries[0].p = "a\\b".into())),
            ("unsorted", mutate(&|m| m.entries.swap(0, 1))),
            (
                "duplicate",
                mutate(&|m| {
                    let e = m.entries[0].clone();
                    m.entries.insert(1, e);
                    m.stats = m.compute_stats();
                }),
            ),
            (
                "file-as-dir",
                mutate(&|m| {
                    m.entries.push(Entry::dir(".minecraft/mods/a.jar/child"));
                    m.entries.sort_by(|a, b| a.p.as_bytes().cmp(b.p.as_bytes()));
                    m.stats = m.compute_stats();
                }),
            ),
            ("missing hash", mutate(&|m| m.entries[0].h = None)),
            (
                "dir with hash",
                mutate(&|m| {
                    let i = m.entries.iter().position(|e| e.t == EntryType::Dir).unwrap();
                    m.entries[i].h = Some(Hash::of(b"x"));
                }),
            ),
            ("stats lie", mutate(&|m| m.stats.bytes += 1)),
            (
                "huge file no recipe",
                mutate(&|m| {
                    m.entries[0].s = (MAX_BLOB as u64) + 1;
                    m.stats = m.compute_stats();
                }),
            ),
            (
                "empty file with recipe",
                mutate(&|m| {
                    let i = m.entries.iter().position(|e| e.p.ends_with("empty.txt")).unwrap();
                    m.entries[i].r = Some(Hash::of(b"r"));
                }),
            ),
            ("rules hash mismatch", mutate(&|m| m.rules.text.push('#'))),
            ("future version", mutate(&|m| m.version = 99)),
            ("bad time", mutate(&|m| m.created = "yesterday".into())),
            ("too many parents", mutate(&|m| m.parents = (0..9u8).map(|i| SnapshotId(Hash([i; 32]))).collect())),
            ("control chars in name", mutate(&|m| m.instance.name = "a\u{7}b".into())),
            ("bad mixed id", mutate(&|m| m.entries[0].mx = Some("../x".into()))),
        ] {
            assert!(r.is_err(), "{name} must be rejected");
        }
    }

    #[test]
    fn manifest_bombs_are_rejected_before_parsing() {
        let bomb = compress_doc(&vec![b' '; (MAX_MANIFEST_RAW as usize) + 1024]);
        let err = Manifest::decode(&bomb, None).unwrap_err().to_string();
        assert!(err.contains("limit"), "{err}");
    }

    #[test]
    fn recipes_validate_sizes_and_roundtrip() {
        let r = Recipe { chunks: vec![(Hash::of(b"a"), 100), (Hash::of(b"b"), 50)] };
        let b = r.encode();
        assert_eq!(Recipe::decode(&b, 150).unwrap(), r);
        assert!(Recipe::decode(&b, 151).is_err(), "sum must equal the file size");
        assert!(Recipe::decode(&b[..b.len() - 1], 150).is_err());
        let mut bad = b.clone();
        bad[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Recipe::decode(&bad, 150).is_err());
        let zero = Recipe { chunks: vec![(Hash::of(b"a"), 0)] };
        assert!(Recipe::decode(&zero.encode(), 0).is_err());
        let huge = Recipe { chunks: vec![(Hash::of(b"a"), (MAX_BLOB + 1) as u32)] };
        assert!(Recipe::decode(&huge.encode(), (MAX_BLOB + 1) as u64).is_err());
        assert_eq!(r.hash(), Hash::of(&r.encode()));
    }

    #[test]
    fn diff_ignores_timestamps_and_finds_changes() {
        let a = sample();
        let mut b = a.clone();
        b.entries[0].m += 999; // mtime only
        assert!(diff_entries(&a.entries, &b.entries).is_empty());
        b.entries[1].h = Some(Hash::of(b"changed"));
        b.entries[1].s = 7;
        b.entries.remove(2);
        b.entries.push(Entry {
            p: "zzz.txt".into(),
            t: EntryType::File,
            s: 1,
            m: 0,
            x: false,
            h: Some(Hash::of(b"z")),
            r: None,
            mx: None,
        });
        let d = diff_entries(&a.entries, &b.entries);
        let s = summarize(&d);
        assert_eq!((s.added, s.removed, s.modified), (1, 1, 1));
        assert_eq!(a.entry(".minecraft/mods/a.jar").unwrap().s, 5);
        assert!(a.entry("nope").is_none());
    }
}

//! Cloud cleanup: retention and mark-and-sweep. **Manual only** — nothing in PackSync calls this on
//! its own, and the CLI/UI always show a plan first and ask for confirmation.
//!
//! What it does, in order of caution:
//!
//! 1. *Orphans*: packs (and indexes) that no retained snapshot needs, e.g. leftovers of an upload that
//!    was abandoned halfway, are deleted — but only when they are older than the grace period (an upload
//!    in flight on another computer looks exactly like an orphan until its manifest is published) and
//!    carry PackSync's own tag. Objects PackSync did not create are never touched.
//! 2. *Retention* (optional): the **content** of old snapshots is released. Their manifests stay, so the
//!    history graph and every ancestry decision are unchanged; an `expired/` marker tells all computers
//!    that such a snapshot can no longer be restored. Heads, crash saves and their parents are never
//!    expired.
//! 3. *Index consolidation*: many small index objects are merged into one (`supersedes`).
//!
//! Space is reclaimed per pack: a pack is deleted only when *none* of its blobs is still needed. Packs
//! that are partly needed are reported ("reclaimable after repacking"), never rewritten in this version.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::hash::Hash;
use crate::ids::{InstanceId, SnapshotId};
use crate::manifest::SnapshotKind;

pub const EXPIRED_FORMAT: &str = "packsync.expired";
/// Never delete anything younger than this, whatever the parameters say.
pub const MIN_GRACE_HOURS: u32 = 24;
pub const DEFAULT_GRACE_HOURS: u32 = 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionPolicy {
    /// Keep the content of the newest N normal snapshots.
    pub keep_snapshots: usize,
    /// …and of every normal snapshot younger than this many days.
    pub keep_days: u32,
    /// Keep crash saves (and their parents) for this many days.
    pub keep_crash_days: u32,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self { keep_snapshots: 20, keep_days: 30, keep_crash_days: 90 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcParams {
    /// Objects younger than this are never deleted (in-flight uploads). Values below
    /// [`MIN_GRACE_HOURS`] are raised to it.
    pub grace_hours: u32,
    /// Release the content of old snapshots (`None`: only remove objects nothing needs).
    pub retention: Option<RetentionPolicy>,
    /// Restrict retention to one instance (orphan cleanup is always repository-wide).
    pub instance: Option<InstanceId>,
    /// Merge small index objects.
    pub consolidate_indexes: bool,
}

impl Default for GcParams {
    fn default() -> Self {
        Self { grace_hours: DEFAULT_GRACE_HOURS, retention: None, instance: None, consolidate_indexes: true }
    }
}

impl GcParams {
    pub fn effective_grace_ms(&self) -> i64 {
        self.grace_hours.max(MIN_GRACE_HOURS) as i64 * 3_600_000
    }
}

/// A commit as retention sees it.
#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub id: SnapshotId,
    pub parents: Vec<SnapshotId>,
    pub kind: SnapshotKind,
    /// `None` when the timestamp cannot be parsed: such a commit is never expired.
    pub created_ms: Option<i64>,
}

/// The snapshots of one instance whose content may be released under `policy`.
///
/// Kept: every head (a normal commit nothing builds on), the newest `keep_snapshots` normal commits,
/// every normal commit younger than `keep_days`, crash saves younger than `keep_crash_days` and the
/// parents of the crash saves that are kept (so that "discard the crash" can still go back), and
/// anything already unparseable. Everything else may expire.
pub fn expirable(commits: &[CommitInfo], policy: &RetentionPolicy, now_ms: i64) -> BTreeSet<SnapshotId> {
    let by_id: HashMap<SnapshotId, &CommitInfo> = commits.iter().map(|c| (c.id, c)).collect();
    let mut has_child: HashSet<SnapshotId> = HashSet::new();
    for c in commits.iter().filter(|c| c.kind == SnapshotKind::Normal) {
        has_child.extend(c.parents.iter().copied());
    }
    let mut keep: HashSet<SnapshotId> = HashSet::new();
    let mut normal: Vec<&CommitInfo> = commits.iter().filter(|c| c.kind == SnapshotKind::Normal).collect();
    normal.sort_by(|a, b| b.created_ms.cmp(&a.created_ms).then(a.id.cmp(&b.id)));
    for (i, c) in normal.iter().enumerate() {
        let young = c.created_ms.is_none_or(|t| t >= now_ms - policy.keep_days as i64 * 86_400_000);
        if i < policy.keep_snapshots || young || !has_child.contains(&c.id) {
            keep.insert(c.id);
        }
    }
    for c in commits.iter().filter(|c| c.kind == SnapshotKind::Crash) {
        let recent = c.created_ms.is_none_or(|t| t >= now_ms - policy.keep_crash_days as i64 * 86_400_000);
        if recent {
            keep.insert(c.id);
            for p in &c.parents {
                if by_id.contains_key(p) {
                    keep.insert(*p);
                }
            }
        }
    }
    commits.iter().filter(|c| !keep.contains(&c.id)).map(|c| c.id).collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpireItem {
    pub instance: InstanceId,
    pub snapshot: SnapshotId,
    pub created: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcObject {
    pub key: String,
    /// `pack` or `index`.
    pub kind: String,
    pub size: u64,
    pub age_hours: i64,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Consolidation {
    /// Index objects to merge (all are deleted after the merged one is written and verified).
    pub merge: Vec<String>,
    pub packs: u64,
    pub blobs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PartialSummary {
    pub packs: u64,
    /// Bytes inside those packs that nothing needs any more (reclaimable only by repacking).
    pub reclaimable_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcPlan {
    /// Identifies exactly what this plan does; `gc run` refuses a plan whose id no longer matches.
    pub id: String,
    pub params: GcParams,
    pub generated_ms: i64,
    pub provider: String,
    pub expire: Vec<ExpireItem>,
    /// Indexes first, then packs: the order they are deleted in.
    pub delete: Vec<GcObject>,
    pub consolidate: Option<Consolidation>,
    pub partially_live: PartialSummary,
    /// Objects in PackSync's folders that PackSync did not create or cannot interpret: never touched.
    pub kept_unknown: Vec<String>,
    pub total_delete_bytes: u64,
    pub notes: Vec<String>,
}

impl GcPlan {
    pub fn is_empty(&self) -> bool {
        self.expire.is_empty() && self.delete.is_empty() && self.consolidate.is_none()
    }
}

/// The id of a plan: BLAKE3 over exactly the actions it would perform (not over timestamps).
pub fn plan_id(
    provider: &str,
    expire: &[ExpireItem],
    delete: &[GcObject],
    consolidate: &Option<Consolidation>,
) -> String {
    let doc = serde_json::json!({
        "provider": provider,
        "expire": expire.iter().map(|e| (e.instance.to_string(), e.snapshot.to_hex())).collect::<Vec<_>>(),
        "delete": delete.iter().map(|d| (&d.key, d.size)).collect::<Vec<_>>(),
        "consolidate": consolidate.as_ref().map(|c| &c.merge),
    });
    Hash::of(doc.to_string().as_bytes()).to_hex()[..16].to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GcReport {
    pub expired: u32,
    pub deleted_packs: u32,
    pub deleted_indexes: u32,
    pub consolidated_into: Option<String>,
    pub bytes_freed: u64,
    pub notes: Vec<String>,
}

/// The marker object that tells every computer a snapshot's content was released.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpiredRecord {
    pub format: String,
    pub version: u32,
    pub instance: InstanceId,
    pub snapshot: SnapshotId,
    pub expired_at: String,
    pub device: String,
}

/// Group `items` (key → blob count) into chunks of at most `max_blobs` entries, keeping order.
pub fn chunk_by_blobs(items: &BTreeMap<String, u64>, max_blobs: u64) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = vec![];
    let mut cur: Vec<String> = vec![];
    let mut n = 0;
    for (k, c) in items {
        if !cur.is_empty() && n + c > max_blobs {
            out.push(std::mem::take(&mut cur));
            n = 0;
        }
        cur.push(k.clone());
        n += c;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> SnapshotId {
        SnapshotId(Hash([n; 32]))
    }
    fn c(n: u8, parents: &[u8], kind: SnapshotKind, days_ago: i64) -> CommitInfo {
        CommitInfo {
            id: id(n),
            parents: parents.iter().map(|p| id(*p)).collect(),
            kind,
            created_ms: Some(1_000 * 86_400_000 - days_ago * 86_400_000),
        }
    }
    const NOW: i64 = 1_000 * 86_400_000;

    #[test]
    fn heads_recent_and_newest_n_are_kept_everything_else_may_expire() {
        use SnapshotKind::*;
        // linear history 1..=8, oldest first; 8 is the head
        let commits: Vec<_> = (1..=8u8)
            .map(|n| {
                let parents: Vec<u8> = if n == 1 { vec![] } else { vec![n - 1] };
                c(n, &parents, Normal, (9 - n as i64) * 20)
            })
            .collect();
        let p = RetentionPolicy { keep_snapshots: 2, keep_days: 30, keep_crash_days: 90 };
        let exp = expirable(&commits, &p, NOW);
        // 8 (head, newest), 7 (second newest, 40 days); 6 is 60 days old etc.
        assert_eq!(exp, [1, 2, 3, 4, 5, 6].iter().map(|n| id(*n)).collect::<BTreeSet<_>>());
        // a generous policy keeps everything
        let all = RetentionPolicy { keep_snapshots: 100, keep_days: 3650, keep_crash_days: 3650 };
        assert!(expirable(&commits, &all, NOW).is_empty());
    }

    #[test]
    fn every_branch_head_survives_even_when_old() {
        use SnapshotKind::*;
        // 1 ← 2 ← 3 (head, very old) and 1 ← 4 (second head, very old): both heads stay
        let commits =
            vec![c(1, &[], Normal, 500), c(2, &[1], Normal, 480), c(3, &[2], Normal, 470), c(4, &[1], Normal, 460)];
        let p = RetentionPolicy { keep_snapshots: 0, keep_days: 1, keep_crash_days: 1 };
        let exp = expirable(&commits, &p, NOW);
        assert_eq!(exp, [1, 2].iter().map(|n| id(*n)).collect::<BTreeSet<_>>());
    }

    #[test]
    fn a_kept_crash_save_keeps_its_parent_so_discarding_it_still_works() {
        use SnapshotKind::*;
        let commits =
            vec![c(1, &[], Normal, 300), c(2, &[1], Normal, 200), c(3, &[2], Crash, 5), c(4, &[2], Normal, 4)];
        let p = RetentionPolicy { keep_snapshots: 0, keep_days: 1, keep_crash_days: 90 };
        let exp = expirable(&commits, &p, NOW);
        assert_eq!(
            exp,
            [id(1)].into_iter().collect::<BTreeSet<_>>(),
            "2 is the crash save's parent, 4 is the head, 3 is recent"
        );
        // an old crash save stops protecting its parent
        let commits =
            vec![c(1, &[], Normal, 300), c(2, &[1], Normal, 200), c(3, &[2], Crash, 150), c(4, &[2], Normal, 4)];
        let exp = expirable(&commits, &p, NOW);
        assert!(exp.contains(&id(3)) && exp.contains(&id(1)) && exp.contains(&id(2)) && !exp.contains(&id(4)));
    }

    #[test]
    fn unparseable_times_are_never_expired() {
        use SnapshotKind::*;
        let mut a = c(1, &[], Normal, 900);
        a.created_ms = None;
        let commits = vec![a, c(2, &[1], Normal, 1)];
        let p = RetentionPolicy { keep_snapshots: 0, keep_days: 1, keep_crash_days: 1 };
        assert!(expirable(&commits, &p, NOW).is_empty());
    }

    #[test]
    fn plan_ids_follow_the_actions_not_the_clock() {
        let d = vec![GcObject {
            key: "packs/a.pack".into(),
            kind: "pack".into(),
            size: 10,
            age_hours: 100,
            reason: "x".into(),
        }];
        let a = plan_id("p", &[], &d, &None);
        let mut d2 = d.clone();
        d2[0].age_hours = 999;
        d2[0].reason = "other".into();
        assert_eq!(a, plan_id("p", &[], &d2, &None), "age and reason are informational");
        d2[0].size = 11;
        assert_ne!(a, plan_id("p", &[], &d2, &None));
        assert_ne!(a, plan_id("q", &[], &d, &None));
    }

    #[test]
    fn grace_has_a_floor_and_chunking_respects_the_cap() {
        let p = GcParams { grace_hours: 1, ..Default::default() };
        assert_eq!(p.effective_grace_ms(), 24 * 3_600_000);
        let items: BTreeMap<String, u64> =
            [("a", 5), ("b", 5), ("c", 5), ("d", 100)].into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        let g = chunk_by_blobs(&items, 10);
        assert_eq!(g, vec![vec!["a".to_string(), "b".into()], vec!["c".into()], vec!["d".into()]]);
    }
}

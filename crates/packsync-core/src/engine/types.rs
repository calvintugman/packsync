//! Request/response types of the engine API. All are `serde` types: the CLI prints them as JSON
//! and the desktop app sends them to the UI.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::db::{InstanceState, SnapshotState};
use crate::graph::ConflictKind;
use crate::ids::{DeviceId, InstanceId, SessionId, SnapshotId};
use crate::launcher::{LauncherRef, LocalSettings, PackIdentity, WorldInfo};
use crate::manifest::{SessionOutcome, SnapshotKind};
use crate::restore::PlanIssue;
use crate::scan::{ExcludedSummary, ScanIssue};
use crate::snapshot::SnapshotStats;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnableRequest {
    pub root: PathBuf,
    /// Launcher kind (`prism`, `generic`).
    pub launcher: String,
    pub name: Option<String>,
    /// User rules appended to the built-in defaults (TOML, `version = 1`). May be empty.
    #[serde(default)]
    pub rules_overrides: String,
    /// The person's answers for content no rule covers (from the preview's suggestions).
    #[serde(default)]
    pub decisions: Vec<crate::rules::suggest::RuleDecision>,
    pub launcher_ref: Option<LauncherRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnableOutcome {
    pub instance: InstanceId,
    pub snapshot: SnapshotId,
    pub stats: SnapshotStats,
    pub issues: Vec<ScanIssue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreviewRequest {
    pub root: PathBuf,
    pub launcher: String,
    #[serde(default)]
    pub rules_overrides: String,
    /// Also hash everything to estimate how much would be uploaded (reads the whole instance).
    #[serde(default)]
    pub estimate_upload: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncludedGroup {
    pub category: String,
    pub files: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreviewReport {
    pub name: String,
    pub game_root: String,
    pub pack: PackIdentity,
    pub worlds: Vec<WorldInfo>,
    pub included_files: u64,
    pub included_bytes: u64,
    pub included_by_category: Vec<IncludedGroup>,
    pub empty_dirs: u64,
    pub excluded: ExcludedSummary,
    pub issues: Vec<ScanIssue>,
    /// Bytes that would actually be uploaded (new to the cloud), when estimated.
    pub estimated_upload_bytes: Option<u64>,
    pub rules_text: String,
    /// A sample of what is synced, per category (path, size).
    pub samples: Vec<(String, String, u64)>,
    /// Content no rule covers, with what PackSync proposes for each part (see [`crate::rules::suggest`]).
    #[serde(default)]
    pub suggestions: Vec<crate::rules::suggest::Suggestion>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PendingChanges {
    pub dirty: bool,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub modified: Vec<String>,
    pub issues: Vec<ScanIssue>,
    /// `false` when the answer is a metadata-only estimate (not every changed-looking file was hashed).
    pub exact: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SnapshotOpts {
    pub kind: SnapshotKind,
    pub session: Option<crate::manifest::SessionSummary>,
    pub note: Option<String>,
    /// Create a snapshot even if nothing changed.
    pub force: bool,
    /// Skip the "no game is running" check (tests; crash recovery of a dead session).
    pub allow_running: bool,
    pub confirm_mass_delete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum SnapshotResult {
    Unchanged { base: SnapshotId },
    Created { id: SnapshotId },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotReport {
    pub result: SnapshotResult,
    pub stats: SnapshotStats,
    pub issues: Vec<ScanIssue>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UploadSummary {
    pub snapshots: Vec<crate::upload::UploadReport>,
    pub bytes_uploaded: u64,
    /// Set when a persisted backoff postponed the work.
    pub deferred_until_ms: Option<i64>,
    pub remaining: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ReconcileOpts {
    /// Adopt a newer cloud snapshot when the local copy is clean.
    pub apply_fast_forward: bool,
    /// Upload local snapshots that are ahead of the cloud.
    pub upload_local: bool,
}

impl Default for ReconcileOpts {
    fn default() -> Self {
        Self { apply_fast_forward: true, upload_local: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ReconcileReport {
    UpToDate {
        head: SnapshotId,
    },
    CloudEmpty,
    /// Local snapshots exist that the cloud does not have. `uploaded` tells whether they were published.
    LocalAhead {
        snapshots: Vec<SnapshotId>,
        uploaded: bool,
        upload_error: Option<String>,
    },
    /// The cloud is ahead and the local copy is clean.
    CloudAhead {
        head: SnapshotId,
    },
    FastForwarded {
        from: Option<SnapshotId>,
        to: SnapshotId,
        files_written: u64,
        files_deleted: u64,
        /// What changed, in the person's terms (mods updated, pack versions, …).
        #[serde(default)]
        changes: crate::manifest::ChangeSummary,
    },
    Conflict(ConflictReport),
    /// The cloud could not be reached. The local copy can be played; its base is recorded.
    Offline {
        base: Option<SnapshotId>,
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictReport {
    pub kind: ConflictKind,
    pub local: Option<SnapshotSummary>,
    pub local_dirty: bool,
    pub cloud: Vec<SnapshotSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotSummary {
    pub id: SnapshotId,
    pub kind: SnapshotKind,
    pub created: Option<String>,
    pub device: Option<String>,
    pub files: u64,
    pub bytes: u64,
    pub worlds: Vec<WorldInfo>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: SnapshotId,
    pub kind: SnapshotKind,
    pub created: Option<String>,
    pub device_id: Option<DeviceId>,
    pub device: Option<String>,
    pub files: u64,
    pub bytes: u64,
    pub note: Option<String>,
    pub outcome: Option<SessionOutcome>,
    pub parents: Vec<SnapshotId>,
    pub state: SnapshotState,
    pub is_base: bool,
    pub is_cloud_head: bool,
    /// The content was released by a cloud cleanup (retention): the entry stays in the history but
    /// cannot be restored.
    #[serde(default)]
    pub expired: bool,
}

/// Where the instance stands, in words the UI can show without interpretation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncPhase {
    NotEnabled,
    /// A local snapshot exists; nothing has been uploaded yet.
    LocalSnapshotComplete,
    /// Upload in progress, retrying or postponed. **Not** synced.
    UploadPending,
    /// Stored and verified in the cloud, not yet seen in a fresh listing.
    CloudPublished,
    /// Verified and discoverable: another computer can restore it.
    ReadyOnOtherDevices,
    /// The cloud has a newer snapshot from another device.
    CloudAhead,
    Conflict,
    CrashPending,
    Restoring,
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceStatus {
    pub id: InstanceId,
    pub name: String,
    pub root: PathBuf,
    pub launcher: String,
    pub sync_enabled: bool,
    pub state: InstanceState,
    pub state_detail: Option<String>,
    pub phase: SyncPhase,
    pub base: Option<SnapshotId>,
    pub base_state: Option<SnapshotState>,
    pub last_synced_ms: Option<i64>,
    pub last_synced_by: Option<String>,
    pub pending_snapshots: u32,
    pub pending_pack_bytes: u64,
    pub next_retry_ms: Option<i64>,
    pub last_error: Option<String>,
    pub worlds: Vec<WorldInfo>,
    pub pack: PackIdentity,
    pub total_files: u64,
    pub total_bytes: u64,
    pub playing_elsewhere: Vec<String>,
    pub provider_id: Option<String>,
    /// Content no rule covers that the person has not decided about yet (found by the last save; new
    /// mods' data folders appear here). Only the ones worth asking about.
    #[serde(default)]
    pub unknown: Vec<crate::rules::suggest::Suggestion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudInstance {
    pub id: InstanceId,
    pub name: String,
    pub pack: PackIdentity,
    pub worlds: Vec<WorldInfo>,
    pub heads: Vec<SnapshotSummary>,
    pub files: u64,
    pub bytes: u64,
    pub last_updated: Option<String>,
    pub last_device: Option<String>,
    /// Where this computer already has the instance, if anywhere.
    pub local_root: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestorePreview {
    pub target: SnapshotSummary,
    pub files_to_write: u64,
    pub files_unchanged: u64,
    pub files_to_delete: u64,
    pub bytes_to_write: u64,
    pub local_only_preserved: u64,
    pub sample_writes: Vec<String>,
    pub sample_deletes: Vec<String>,
    pub skipped: Vec<PlanIssue>,
    /// Local changes that would be preserved as a recovery snapshot first.
    pub local_changes: bool,
    pub problems: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RestoreNewRequest {
    pub instance: InstanceId,
    /// Snapshot to restore; `None` = the single cloud head.
    pub snapshot: Option<SnapshotId>,
    /// Directory in which the instance folder is created (Prism's instances directory).
    pub parent_dir: PathBuf,
    pub folder_name: String,
    pub launcher: String,
    pub launcher_ref: Option<LauncherRef>,
    pub local: LocalSettings,
    pub skip_incompatible: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoredInstance {
    pub instance: InstanceId,
    pub root: PathBuf,
    pub snapshot: SnapshotId,
    pub notes: Vec<String>,
    pub report: crate::restore::RestoreReport,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RestoreOpts {
    pub skip_incompatible: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "choice", rename_all = "snake_case")]
pub enum Resolution {
    /// Continue with this computer's version; the cloud's version stays in history.
    KeepLocal,
    /// Continue with the cloud's version (`head` required when the cloud has several); this
    /// computer's version is preserved first.
    KeepCloud { head: Option<SnapshotId> },
    /// Keep this computer's version as a new, separate instance; the original stays as it is in the cloud.
    SplitLocal { new_name: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveReport {
    pub resolution: String,
    /// The snapshot that is now the single head (or the new instance's first snapshot).
    pub head: SnapshotId,
    pub new_instance: Option<InstanceId>,
    pub preserved_local: Option<SnapshotId>,
    pub uploaded: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeginSession {
    pub owner: crate::db::SessionOwner,
    /// Idempotency token: the same token never starts two sessions.
    pub token: String,
    pub offline: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionBegun {
    pub session: SessionId,
    pub already_owned: bool,
    pub base: Option<SnapshotId>,
    /// Other devices that appear to be playing this instance right now (advisory).
    pub playing_elsewhere: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExitInfo {
    pub exit_code: Option<i32>,
    /// The launcher signalled a normal exit (Prism's post-exit hook).
    pub clean_signal: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndReport {
    pub outcome: SessionOutcome,
    pub snapshot: Option<SnapshotId>,
    pub kind: Option<SnapshotKind>,
    pub crash_artifacts: Vec<String>,
    pub uploaded: bool,
    pub upload_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyReport {
    pub files_checked: u64,
    pub changed: Vec<String>,
    pub missing: Vec<String>,
    pub extra: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RemoteVerifyReport {
    pub snapshot: Option<SnapshotId>,
    pub packs_checked: u64,
    pub missing_packs: Vec<String>,
    pub blobs_checked: u64,
    pub damaged_blobs: Vec<String>,
    pub unlisted_blobs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryPointInfo {
    pub tx: String,
    pub created: String,
    pub from: Option<SnapshotId>,
    pub to: SnapshotId,
    pub files: u64,
    pub bytes: u64,
    pub reason: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecoveryReport {
    pub rolled_back: Vec<InstanceId>,
    pub rolled_forward: Vec<InstanceId>,
    pub discarded_transactions: u32,
    pub removed_partial_restores: Vec<PathBuf>,
    pub interrupted_sessions: Vec<SessionId>,
    pub orphan_staging_removed: u32,
    /// Interrupted restores that could not be recovered (a damaged or forged journal): the instance and
    /// the transaction folder were left untouched for a person to look at. One bad journal never stops
    /// the recovery of other instances.
    #[serde(default)]
    pub could_not_recover: Vec<(InstanceId, String)>,
}

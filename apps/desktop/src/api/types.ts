// Types of the JSON the application layer (`packsync-app::rpc`) returns. They mirror the Rust `serde`
// types field for field; the end-to-end suite runs the real engine behind these types, so a drift
// shows up as a failing test rather than a blank screen.

export type InstanceId = string;
export type SnapshotId = string;
export type JobId = string;

export type InstanceState = "ok" | "crash_pending" | "conflict" | "restoring" | "unavailable";
export type SnapshotState = "local_complete" | "uploading" | "published" | "visible" | "remote";
export type SnapshotKind = "normal" | "crash";
export type SessionOutcome = "clean" | "crashed" | "interrupted";

/** Where an instance stands, in the words of the UI (`SyncPhase` in the engine). */
export type SyncPhase =
  | "not_enabled"
  | "local_snapshot_complete"
  | "upload_pending"
  | "cloud_published"
  | "ready_on_other_devices"
  | "cloud_ahead"
  | "conflict"
  | "crash_pending"
  | "restoring"
  | "unavailable";

export interface LoaderInfo {
  kind: string;
  version: string;
}
export interface ComponentInfo {
  uid: string;
  version: string | null;
}
export interface PackIdentity {
  minecraft_version: string | null;
  loader: LoaderInfo | null;
  components: ComponentInfo[];
}
export interface WorldInfo {
  folder: string;
  name: string | null;
  last_played_ms: number | null;
}

export interface InstanceStatus {
  id: InstanceId;
  name: string;
  root: string;
  launcher: string;
  sync_enabled: boolean;
  state: InstanceState;
  state_detail: string | null;
  phase: SyncPhase;
  base: SnapshotId | null;
  base_state: SnapshotState | null;
  last_synced_ms: number | null;
  last_synced_by: string | null;
  pending_snapshots: number;
  pending_pack_bytes: number;
  next_retry_ms: number | null;
  last_error: string | null;
  worlds: WorldInfo[];
  pack: PackIdentity;
  total_files: number;
  total_bytes: number;
  playing_elsewhere: string[];
  provider_id: string | null;
}

export interface SnapshotSummary {
  id: SnapshotId;
  kind: SnapshotKind;
  created: string | null;
  device: string | null;
  files: number;
  bytes: number;
  worlds: WorldInfo[];
  note: string | null;
}

export interface HistoryEntry {
  id: SnapshotId;
  kind: SnapshotKind;
  created: string | null;
  device_id: string | null;
  device: string | null;
  files: number;
  bytes: number;
  note: string | null;
  outcome: SessionOutcome | null;
  parents: SnapshotId[];
  state: SnapshotState;
  is_base: boolean;
  is_cloud_head: boolean;
  expired: boolean;
}

export interface CloudInstance {
  id: InstanceId;
  name: string;
  pack: PackIdentity;
  worlds: WorldInfo[];
  heads: SnapshotSummary[];
  files: number;
  bytes: number;
  last_updated: string | null;
  last_device: string | null;
  local_root: string | null;
}

export interface LocalSettings {
  java_path?: string | null;
  min_memory_mb?: number | null;
  max_memory_mb?: number | null;
  jvm_args?: string | null;
}

// ------------------------------------------------------------------------------------ storage

export interface StorageStatus {
  kind: "none" | "local" | "google" | "r2";
  connected: boolean;
  detail: string;
  account: string | null;
  space: string | null;
  needs_reconnect: boolean;
  oauth_client_configured: boolean;
  oauth_client_source: string | null;
  credential_store: string | null;
  problem: string | null;
  setup_hint: string | null;
}

export type OfflinePolicy = "play_local" | "block";

export type StorageSetting =
  | { kind: "none" }
  | { kind: "local"; path: string }
  | { kind: "google"; space: string; account?: string | null }
  | { kind: "r2"; account_id: string; bucket: string; prefix: string; access_key_id: string };

export interface Settings {
  version: number;
  device_name: string | null;
  storage: StorageSetting;
  google_client_file: string | null;
  prism_executable: string | null;
  prism_data_dirs: string[];
  cli_path: string | null;
  hook_offline_policy: OfflinePolicy;
  hook_block_on_problem: boolean;
  upload_concurrency: number;
  recovery_point_days: number;
  lease_ttl_secs: number;
}

export interface SettingsUpdate {
  settings: Settings;
  storage_problem: string | null;
}

export interface AppInfo {
  version: string;
  os: string;
  arch: string;
  paths: { config: string; state: string };
  device: { id: string; name: string };
  storage: StorageStatus;
  prism: { executable: string | null; data_dirs: string[] };
  running_jobs: number;
}

export interface AccountInfo {
  email: string | null;
  display_name: string | null;
  permission_id: string | null;
}

export interface StorageUsage {
  provider: string;
  quota: { used: number; total: number | null } | null;
}

export interface StorageTest {
  provider: string;
  quota: { used: number; total: number | null } | null;
  instances_in_cloud: number;
}

// ------------------------------------------------------------------------------------ discovery & preview

export interface DiscoveredInstance {
  launcher_ref: { launcher: string; data_dir: string | null; instance_id: string | null; executable?: string | null };
  root: string;
  name: string;
  game_root: string;
  identity: PackIdentity;
  worlds: WorldInfo[];
}
export interface DiscoveredView extends DiscoveredInstance {
  registered: InstanceId | null;
  sync_enabled: boolean;
}

export type IssueKind =
  | "symlink"
  | "special_file"
  | "unsupported_name"
  | "unreadable"
  | "non_portable_name"
  | "case_collision"
  | "unstable";

export interface ScanIssue {
  path: string;
  kind: IssueKind;
  detail: string;
}

export interface IncludedGroup {
  category: string;
  files: number;
  bytes: number;
}
export interface ExcludedGroup {
  category: string;
  reason: string;
  files: number;
  bytes: number;
  samples: string[];
}
export interface ExcludedSummary {
  groups: Record<string, ExcludedGroup>;
  pruned_dirs: [string, string, string][];
  unmatched_by_dir: Record<string, [number, number]>;
}

export interface PreviewReport {
  name: string;
  game_root: string;
  pack: PackIdentity;
  worlds: WorldInfo[];
  included_files: number;
  included_bytes: number;
  included_by_category: IncludedGroup[];
  empty_dirs: number;
  excluded: ExcludedSummary;
  issues: ScanIssue[];
  estimated_upload_bytes: number | null;
  rules_text: string;
  samples: [string, string, number][];
}

export interface SnapshotStats {
  files: number;
  dirs: number;
  bytes: number;
  hashed_files: number;
  reused_files: number;
  hashed_bytes: number;
  new_blobs: number;
  reused_blobs: number;
  new_raw_bytes: number;
  new_stored_bytes: number;
  packs: number;
  added: number;
  removed: number;
  modified: number;
}

export interface UploadReport {
  snapshot: SnapshotId;
  packs_uploaded: number;
  packs_already_present: number;
  bytes_uploaded: number;
  state: SnapshotState;
  deferred_until_ms: number | null;
}
export interface UploadSummary {
  snapshots: UploadReport[];
  bytes_uploaded: number;
  deferred_until_ms: number | null;
  remaining: number;
}

export interface ErrorView {
  code: string;
  message: string;
  advice: string | null;
  exit_code: number;
}

export interface EnableResult {
  outcome: { instance: InstanceId; snapshot: SnapshotId; stats: SnapshotStats; issues: ScanIssue[] };
  upload: UploadSummary | null;
  upload_error: ErrorView | null;
}

export interface PendingChanges {
  dirty: boolean;
  added: string[];
  removed: string[];
  modified: string[];
  issues: ScanIssue[];
  exact: boolean;
}

export interface SnapshotReport {
  result: { result: "unchanged"; base: SnapshotId } | { result: "created"; id: SnapshotId };
  stats: SnapshotStats;
  issues: ScanIssue[];
}

// ------------------------------------------------------------------------------------ reconcile & conflicts

export type ConflictKind = "diverged" | "multiple_cloud_heads";

export interface ConflictDetails {
  kind: ConflictKind;
  local: SnapshotSummary | null;
  local_dirty: boolean;
  cloud: SnapshotSummary[];
}

export type ReconcileReport =
  | { outcome: "up_to_date"; head: SnapshotId }
  | { outcome: "cloud_empty" }
  | { outcome: "local_ahead"; snapshots: SnapshotId[]; uploaded: boolean; upload_error: string | null }
  | { outcome: "cloud_ahead"; head: SnapshotId }
  | { outcome: "fast_forwarded"; from: SnapshotId | null; to: SnapshotId; files_written: number; files_deleted: number }
  | ({ outcome: "conflict" } & ConflictDetails)
  | { outcome: "offline"; base: SnapshotId | null; reason: string };

export interface SyncResult {
  snapshot: SnapshotReport;
  reconcile: ReconcileReport;
}

export interface ConflictShow {
  conflict: boolean;
  report: ReconcileReport;
}

export type Resolution =
  | { choice: "keep_local" }
  | { choice: "keep_cloud"; head?: SnapshotId | null }
  | { choice: "split_local"; new_name: string };

export interface ResolveReport {
  resolution: string;
  head: SnapshotId;
  new_instance: InstanceId | null;
  preserved_local: SnapshotId | null;
  uploaded: boolean;
}

export interface CrashPromote {
  snapshot: SnapshotId;
  upload: UploadSummary | { error: ErrorView };
}

// ------------------------------------------------------------------------------------ restore

export interface PlanIssue {
  path: string;
  problem: string;
}

export interface RestorePreview {
  target: SnapshotSummary;
  files_to_write: number;
  files_unchanged: number;
  files_to_delete: number;
  bytes_to_write: number;
  local_only_preserved: number;
  sample_writes: string[];
  sample_deletes: string[];
  skipped: PlanIssue[];
  local_changes: boolean;
  problems: string[];
}

export interface RestoreReport {
  target: SnapshotId | null;
  written: number;
  unchanged: number;
  deleted: number;
  bytes_written: number;
  bytes_downloaded: number;
  bytes_reused_locally: number;
  recovery_point: string | null;
  skipped: PlanIssue[];
}

export interface RestoredInstance {
  instance: InstanceId;
  root: string;
  snapshot: SnapshotId;
  notes: string[];
  report: RestoreReport;
}

export interface RestoreCloudRequest {
  instance?: InstanceId | null;
  snapshot?: SnapshotId | null;
  data_dir?: string | null;
  folder_name?: string | null;
  local?: LocalSettings;
  skip_incompatible?: boolean;
}

export interface RecoveryPoint {
  tx: string;
  created: string;
  from: SnapshotId | null;
  to: SnapshotId;
  files: number;
  bytes: number;
  reason: string;
  path: string;
}

// ------------------------------------------------------------------------------------ launch & hooks

export interface LaunchOptions {
  offline_player?: string | null;
  profile?: string | null;
  skip_sync?: boolean;
  no_upload?: boolean;
}

export interface EndReport {
  outcome: SessionOutcome;
  snapshot: SnapshotId | null;
  kind: SnapshotKind | null;
  crash_artifacts: string[];
  uploaded: boolean;
  upload_error: string | null;
}

export interface LaunchOutcome {
  session: string | null;
  pre_sync: ReconcileReport | null;
  offline: boolean;
  playing_elsewhere: string[];
  game_started: boolean;
  launcher_exit_code: number | null;
  clean_signal: boolean;
  end: EndReport | null;
  warnings: string[];
}

export interface HookState {
  installed: boolean;
  intact: boolean;
  /** Only one of the two commands is PackSync's: install again to repair, or remove what is left. */
  partial: boolean;
  pre_launch: string | null;
  post_exit: string | null;
}
export interface HookChange {
  file: string;
  key: string;
  before: string | null;
  after: string | null;
}
export interface HookPlan {
  root: string;
  changes: HookChange[];
  preserved_commands: string[];
  blockers: string[];
}

export interface PreviewRequest {
  root: string;
  launcher: string;
  rules_overrides?: string;
  estimate_upload?: boolean;
}

export interface VerifyReport {
  files_checked: number;
  changed: string[];
  missing: string[];
  extra: string[];
}

export interface RemoteVerifyReport {
  snapshot: SnapshotId | null;
  packs_checked: number;
  missing_packs: string[];
  blobs_checked: number;
  damaged_blobs: string[];
  unlisted_blobs: string[];
}

export interface RulesShow {
  rules: string;
}
export interface RulesExplain {
  path: string;
  synced: boolean;
  explanation: string;
}

// ------------------------------------------------------------------------------------ cleanup

export interface GcParams {
  grace_hours: number;
  retention: { keep_snapshots: number; keep_days: number; keep_crash_days: number } | null;
  instance: InstanceId | null;
  consolidate_indexes: boolean;
}
export interface GcPlan {
  id: string;
  params: GcParams;
  generated_ms: number;
  provider: string;
  expire: { instance: InstanceId; snapshot: SnapshotId; created: string | null }[];
  delete: { key: string; kind: string; size: number; age_hours: number; reason: string }[];
  consolidate: { merge: string[]; packs: number; blobs: number } | null;
  partially_live: { packs: number; reclaimable_bytes: number };
  kept_unknown: string[];
  total_delete_bytes: number;
  notes: string[];
}
export interface GcReport {
  expired: number;
  deleted_packs: number;
  deleted_indexes: number;
  consolidated_into: string | null;
  bytes_freed: number;
  notes: string[];
}

// ------------------------------------------------------------------------------------ diagnostics & events

export interface Diagnostics {
  checks: { name: string; ok: boolean; detail: string }[];
  recent_events: string[];
}

export interface LogEntry {
  ts_ms: number;
  instance: InstanceId | null;
  level: string;
  code: string;
  message: string;
}

export type Phase =
  | "discovery"
  | "scan"
  | "snapshot"
  | "upload"
  | "publish"
  | "reconcile"
  | "stage_download"
  | "validate"
  | "apply"
  | "recover"
  | "verify"
  | "cleanup"
  | "conflict"
  | "launch"
  | "idle";

export type EventKind = { type: "phase" } | { type: "progress" } | { type: "message"; level: "info" | "warn" | "error" };

export interface ProgressEvent {
  job: JobId | null;
  instance: InstanceId | null;
  phase: Phase;
  kind: EventKind;
  message: string;
  bytes_done: number;
  bytes_total: number;
  items_done: number;
  items_total: number;
}

export type JobState = "running" | "succeeded" | "failed" | "cancelled";

export interface JobInfo {
  id: JobId;
  method: string;
  instance: InstanceId | null;
  state: JobState;
  started_ms: number;
  finished_ms: number | null;
  error: ErrorView | null;
  result: unknown;
  progress: ProgressEvent | null;
}

export type AppEvent =
  | ({ type: "progress" } & ProgressEvent)
  | ({ type: "job" } & JobInfo)
  | { type: "instance_changed"; instance: InstanceId }
  | { type: "storage_changed" }
  | { type: "notice"; level: string; message: string };

export interface MethodInfo {
  name: string;
  long: boolean;
  doc: string;
}

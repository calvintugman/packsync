import type {
  AppInfo,
  InstanceStatus,
  JobInfo,
  Settings,
  SnapshotSummary,
  StorageStatus,
  SyncPhase,
} from "../api/types";
import { FakeTransport } from "./fakeTransport";

export function instance(over: Partial<InstanceStatus> = {}): InstanceStatus {
  return {
    id: "11111111-1111-4111-8111-111111111111",
    name: "Skyblock Pack",
    root: "/home/me/prism/instances/Skyblock Pack",
    launcher: "prism",
    sync_enabled: true,
    state: "ok",
    state_detail: null,
    phase: "ready_on_other_devices",
    base: "aa".repeat(32),
    base_state: "visible",
    last_synced_ms: Date.now() - 5 * 60_000,
    last_synced_by: "Laptop",
    pending_snapshots: 0,
    pending_pack_bytes: 0,
    next_retry_ms: null,
    last_error: null,
    worlds: [{ folder: "World 0", name: "My World", last_played_ms: 1_700_000_000_000 }],
    pack: { minecraft_version: "1.20.1", loader: { kind: "forge", version: "47.4.0" }, components: [] },
    total_files: 25,
    total_bytes: 1_048_000,
    playing_elsewhere: [],
    provider_id: "local:/cloud",
    ...over,
  };
}

export function withPhase(phase: SyncPhase, over: Partial<InstanceStatus> = {}): InstanceStatus {
  const state = phase === "conflict" ? "conflict" : phase === "crash_pending" ? "crash_pending" : phase === "unavailable" ? "unavailable" : "ok";
  return instance({ phase, state, ...over });
}

export function storage(over: Partial<StorageStatus> = {}): StorageStatus {
  return {
    kind: "local",
    connected: true,
    detail: "Folder /cloud",
    account: null,
    space: null,
    needs_reconnect: false,
    oauth_client_configured: false,
    oauth_client_source: null,
    credential_store: null,
    problem: null,
    setup_hint: null,
    ...over,
  };
}

export function settings(over: Partial<Settings> = {}): Settings {
  return {
    version: 1,
    device_name: "Desktop",
    storage: { kind: "local", path: "/cloud" },
    google_client_file: null,
    prism_executable: null,
    prism_data_dirs: [],
    cli_path: null,
    hook_offline_policy: "play_local",
    hook_block_on_problem: true,
    upload_concurrency: 4,
    recovery_point_days: 14,
    lease_ttl_secs: 120,
    ...over,
  };
}

export function appInfo(over: Partial<AppInfo> = {}): AppInfo {
  return {
    version: "0.1.0",
    os: "linux",
    arch: "x86_64",
    paths: { config: "/home/me/.config/packsync", state: "/home/me/.local/state/packsync" },
    device: { id: "dev-1", name: "Desktop" },
    storage: storage(),
    prism: { executable: "/usr/bin/prismlauncher", data_dirs: ["/home/me/prism"] },
    running_jobs: 0,
    ...over,
  };
}

export function summary(over: Partial<SnapshotSummary> = {}): SnapshotSummary {
  return {
    id: "bb".repeat(32),
    kind: "normal",
    created: new Date(Date.now() - 3600_000).toISOString(),
    device: "Laptop",
    files: 26,
    bytes: 1_060_000,
    worlds: [{ folder: "World 0", name: "My World", last_played_ms: Date.now() - 3600_000 }],
    note: null,
    ...over,
  };
}

export function finishedJob(over: Partial<JobInfo> = {}): JobInfo {
  return {
    id: "job-x",
    method: "instance.sync",
    instance: null,
    state: "succeeded",
    started_ms: Date.now() - 2000,
    finished_ms: Date.now(),
    error: null,
    result: null,
    progress: null,
    ...over,
  };
}

/** A transport with the handlers every screen needs on start-up. */
export function baseTransport(over: { instances?: InstanceStatus[]; storage?: StorageStatus } = {}): FakeTransport {
  const t = new FakeTransport();
  const instances = over.instances ?? [instance()];
  t.on("app.info", () => appInfo())
    .on("settings.get", () => settings())
    .on("storage.status", () => over.storage ?? storage())
    .on("instances.list", () => instances)
    .on("jobs.list", () => [...t.jobs.values()])
    .on("launcher.discover", () => [])
    .on("events.recent", () => []);
  return t;
}

import type {
  RuleDecision,
  AccountInfo,
  AppInfo,
  CloudInstance,
  ConflictShow,
  CrashPromote,
  DiscoveredInstance,
  DiscoveredView,
  Diagnostics,
  EnableResult,
  GcParams,
  GcPlan,
  GcReport,
  HistoryEntry,
  HookPlan,
  HookState,
  InstanceId,
  InstanceStatus,
  JobInfo,
  LaunchOptions,
  LaunchOutcome,
  LogEntry,
  PendingChanges,
  PreviewReport,
  PreviewRequest,
  ReconcileReport,
  RecoveryPoint,
  RemoteVerifyReport,
  ResolveReport,
  Resolution,
  RestoreCloudRequest,
  RestorePreview,
  RestoreReport,
  RestoredInstance,
  RulesExplain,
  RulesShow,
  Settings,
  SettingsUpdate,
  SnapshotId,
  SnapshotReport,
  StorageStatus,
  StorageTest,
  StorageUsage,
  SyncResult,
  UploadSummary,
  VerifyReport,
} from "./types";

interface M<P, R, L extends boolean = false> {
  params: P;
  result: R;
  long: L;
}

type Inst = { instance: InstanceId };

/** Every method of `packsync-app::rpc`, with its parameters and result. `long` methods run as jobs. */
export interface Methods {
  "app.info": M<void, AppInfo>;
  "settings.get": M<void, Settings>;
  "settings.update": M<{ patch: Partial<Settings> }, SettingsUpdate>;
  "device.rename": M<{ name: string }, { id: string; name: string }>;
  "storage.status": M<void, StorageStatus>;
  "storage.use_local": M<{ path: string }, StorageStatus>;
  "storage.use_google": M<{ space?: "visible_folder" | "app_data" }, StorageStatus>;
  "storage.google_login": M<void, { account: AccountInfo; status: StorageStatus }, true>;
  "storage.disconnect": M<{ revoke?: boolean }, StorageStatus>;
  "storage.test": M<void, StorageTest, true>;
  "storage.usage": M<void, StorageUsage>;
  "instance.decide_unknown": M<{ instance: InstanceId; decisions: RuleDecision[] }, { decided: number }>;
  "storage.hosted_start": M<{ service?: string } | void, StorageStatus, true>;
  "storage.hosted_join": M<{ code: string; service?: string }, StorageStatus, true>;
  "storage.hosted_recover": M<{ key: string }, StorageStatus, true>;
  "storage.hosted_pair_start": M<void, { code: string }>;
  "storage.hosted_pair_wait": M<void, { joined: boolean }, true>;
  "storage.hosted_recovery_key": M<void, { key: string }>;
  "storage.passkey_add": M<void, { added: boolean }, true>;
  "storage.passkey_recover": M<{ service?: string } | void, StorageStatus, true>;
  "launcher.discover": M<void, DiscoveredView[]>;
  "launcher.describe": M<{ launcher?: string; path: string }, DiscoveredInstance>;
  "instances.list": M<{ online?: boolean } | void, InstanceStatus[]>;
  "instance.status": M<Inst & { online?: boolean }, InstanceStatus>;
  "instances.preview": M<PreviewRequest, PreviewReport, true>;
  "instances.enable": M<
    { path: string; launcher?: string; rules_overrides?: string; decisions?: RuleDecision[]; upload?: boolean },
    EnableResult,
    true
  >;
  "instances.disable": M<Inst, { instance: InstanceId; sync_enabled: false }>;
  "instance.changes": M<Inst, PendingChanges, true>;
  "instance.history": M<Inst & { online?: boolean }, HistoryEntry[]>;
  "instance.snapshot": M<Inst & { force?: boolean; note?: string }, SnapshotReport, true>;
  "instance.upload": M<Inst & { force?: boolean }, UploadSummary, true>;
  "instance.sync": M<Inst, SyncResult, true>;
  "instance.reconcile": M<Inst & { apply_fast_forward?: boolean; upload_local?: boolean }, ReconcileReport, true>;
  "instance.verify_local": M<Inst, VerifyReport, true>;
  "instance.verify_remote": M<Inst & { snapshot?: SnapshotId; deep?: boolean }, RemoteVerifyReport, true>;
  "cloud.instances": M<void, CloudInstance[], true>;
  "restore.preview": M<Inst & { snapshot: SnapshotId }, RestorePreview, true>;
  "restore.apply": M<Inst & { snapshot: SnapshotId; skip_incompatible?: boolean }, RestoreReport, true>;
  "restore.new": M<RestoreCloudRequest, RestoredInstance, true>;
  "restore.points": M<Inst, RecoveryPoint[]>;
  "conflict.show": M<Inst, ConflictShow, true>;
  "conflict.resolve": M<Inst & { resolution: Resolution }, ResolveReport, true>;
  "crash.promote": M<Inst, CrashPromote, true>;
  "crash.discard": M<Inst, RestoreReport, true>;
  "launch.start": M<Inst & { options?: LaunchOptions }, LaunchOutcome, true>;
  "hooks.state": M<Inst, HookState>;
  "hooks.plan_install": M<Inst & { cli?: string }, HookPlan>;
  "hooks.plan_remove": M<Inst, HookPlan>;
  "hooks.apply": M<Inst & { plan: HookPlan }, HookState>;
  "rules.show": M<Inst, RulesShow, true>;
  "rules.explain": M<Inst & { path: string }, RulesExplain, true>;
  "gc.plan": M<GcParams, GcPlan, true>;
  "gc.run": M<{ plan_id: string; params: GcParams }, GcReport, true>;
  "jobs.list": M<void, JobInfo[]>;
  "jobs.cancel": M<{ job: string }, { cancelled: boolean }>;
  "events.recent": M<{ instance?: InstanceId; limit?: number } | void, LogEntry[]>;
  "diagnostics.report": M<{ online?: boolean } | void, Diagnostics, true>;
}

export type MethodName = keyof Methods;
export type LongMethodName = { [K in MethodName]: Methods[K]["long"] extends true ? K : never }[MethodName];
export type ShortMethodName = Exclude<MethodName, LongMethodName>;
export type Params<K extends MethodName> = Methods[K]["params"];
export type Result<K extends MethodName> = Methods[K]["result"];

/** The `long` flag of every method, as the server reports it at `/api/methods`. Kept in step by a test. */
export const LONG_METHODS: readonly LongMethodName[] = [
  "storage.google_login",
  "storage.test",
  "storage.hosted_start",
  "storage.hosted_join",
  "storage.hosted_recover",
  "storage.hosted_pair_wait",
  "storage.passkey_add",
  "storage.passkey_recover",
  "instances.preview",
  "instances.enable",
  "instance.changes",
  "instance.snapshot",
  "instance.upload",
  "instance.sync",
  "instance.reconcile",
  "instance.verify_local",
  "instance.verify_remote",
  "cloud.instances",
  "restore.preview",
  "restore.apply",
  "restore.new",
  "conflict.show",
  "conflict.resolve",
  "crash.promote",
  "crash.discard",
  "launch.start",
  "rules.show",
  "rules.explain",
  "gc.plan",
  "gc.run",
  "diagnostics.report",
];

export const ALL_METHODS: readonly MethodName[] = [
  "app.info",
  "settings.get",
  "settings.update",
  "device.rename",
  "storage.status",
  "storage.use_local",
  "storage.use_google",
  "storage.google_login",
  "storage.disconnect",
  "storage.test",
  "storage.usage",
  "instance.decide_unknown",
  "storage.hosted_start",
  "storage.hosted_join",
  "storage.hosted_recover",
  "storage.hosted_pair_start",
  "storage.hosted_pair_wait",
  "storage.hosted_recovery_key",
  "storage.passkey_add",
  "storage.passkey_recover",
  "launcher.discover",
  "launcher.describe",
  "instances.list",
  "instance.status",
  "instances.preview",
  "instances.enable",
  "instances.disable",
  "instance.changes",
  "instance.history",
  "instance.snapshot",
  "instance.upload",
  "instance.sync",
  "instance.reconcile",
  "instance.verify_local",
  "instance.verify_remote",
  "cloud.instances",
  "restore.preview",
  "restore.apply",
  "restore.new",
  "restore.points",
  "conflict.show",
  "conflict.resolve",
  "crash.promote",
  "crash.discard",
  "launch.start",
  "hooks.state",
  "hooks.plan_install",
  "hooks.plan_remove",
  "hooks.apply",
  "rules.show",
  "rules.explain",
  "gc.plan",
  "gc.run",
  "jobs.list",
  "jobs.cancel",
  "events.recent",
  "diagnostics.report",
];

export function isLongMethod(m: string): boolean {
  return (LONG_METHODS as readonly string[]).includes(m);
}

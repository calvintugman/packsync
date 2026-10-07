import { ApiError } from "../api/errors";
import type { LongMethodName, MethodName, Params, Result } from "../api/methods";
import type { ConnectionStatus, Transport } from "../api/transport";
import type {
  AppEvent,
  AppInfo,
  DiscoveredView,
  InstanceId,
  InstanceStatus,
  JobId,
  JobInfo,
  ProgressEvent,
  Settings,
  StorageStatus,
} from "../api/types";

export interface Toast {
  id: number;
  level: "info" | "success" | "warn" | "error";
  title?: string;
  message: string;
}

export interface AppState {
  status: "loading" | "ready" | "failed";
  loadError: ApiError | null;
  connection: ConnectionStatus;
  info: AppInfo | null;
  settings: Settings | null;
  storage: StorageStatus | null;
  instances: InstanceStatus[];
  discovered: DiscoveredView[] | null;
  jobs: Record<JobId, JobInfo>;
  /** The latest progress event of every running job. */
  progress: Record<JobId, ProgressEvent>;
  toasts: Toast[];
  /** Bumped when something about an instance changed, so open screens know to refetch their own data. */
  revisions: Record<InstanceId, number>;
}

export const INITIAL_STATE: AppState = {
  status: "loading",
  loadError: null,
  connection: "connecting",
  info: null,
  settings: null,
  storage: null,
  instances: [],
  discovered: null,
  jobs: {},
  progress: {},
  toasts: [],
  revisions: {},
};

export interface JobMeta {
  /** Shown in the activity list; defaults to the method name. */
  title?: string;
}

export interface StoreOptions {
  /** Poll interval for state other processes (launcher hooks, the CLI) can change. 0 disables it. */
  pollMs?: number;
  /** Cap on kept finished jobs. */
  keepJobs?: number;
}

const FINISHED = (j: JobInfo) => j.state !== "running";

/**
 * Everything the screens read. It holds the last known state of the application layer, keeps it fresh
 * from the event stream (and a slow poll for changes made by other processes) and turns long
 * operations into promises. It never invents state: whatever the engine says is what is shown.
 */
export class AppStore {
  private state: AppState = INITIAL_STATE;
  private readonly listeners = new Set<() => void>();
  private waiters = new Map<JobId, ((j: JobInfo) => void)[]>();
  /** How each job of this session was started, so a failed one can be run again from the activity list. */
  private readonly jobArgs = new Map<JobId, { method: LongMethodName; params: unknown }>();
  private unsubscribe: (() => void) | null = null;
  private poll: ReturnType<typeof setInterval> | null = null;
  private jobPoll: ReturnType<typeof setInterval> | null = null;
  private refreshTimer: ReturnType<typeof setTimeout> | null = null;
  private toastSeq = 0;
  private readonly pollMs: number;
  private readonly keepJobs: number;
  private started = false;

  constructor(
    readonly transport: Transport,
    opts: StoreOptions = {},
  ) {
    this.pollMs = opts.pollMs ?? 10_000;
    this.keepJobs = opts.keepJobs ?? 40;
  }

  // ---------------------------------------------------------------------------------- subscription

  getState = (): AppState => this.state;

  subscribe = (l: () => void): (() => void) => {
    this.listeners.add(l);
    return () => this.listeners.delete(l);
  };

  private set(patch: Partial<AppState> | ((s: AppState) => Partial<AppState>)): void {
    const p = typeof patch === "function" ? patch(this.state) : patch;
    this.state = { ...this.state, ...p };
    for (const l of this.listeners) l();
  }

  // ---------------------------------------------------------------------------------- lifecycle

  /** Load everything once and start listening. Safe to call again (a no-op while running). */
  async start(): Promise<void> {
    if (this.started) return;
    this.started = true;
    this.unsubscribe = this.transport.subscribe(
      (ev) => this.onEvent(ev),
      (c) => {
        const was = this.state.connection;
        this.set({ connection: c });
        if (c === "live" && was === "lost") void this.refreshAll(); // catch up on what happened meanwhile
      },
    );
    try {
      await this.refreshAll();
      this.set({ status: "ready", loadError: null });
    } catch (e) {
      this.set({ status: "failed", loadError: ApiError.from(e) });
    }
    if (this.pollMs > 0) {
      this.poll = setInterval(() => void this.refreshQuiet(), this.pollMs);
    }
    if (typeof document !== "undefined") {
      document.addEventListener("visibilitychange", this.onVisible);
    }
  }

  stop(): void {
    this.started = false;
    this.unsubscribe?.();
    this.unsubscribe = null;
    if (this.poll) clearInterval(this.poll);
    if (this.jobPoll) clearInterval(this.jobPoll);
    if (this.refreshTimer) clearTimeout(this.refreshTimer);
    this.poll = this.jobPoll = this.refreshTimer = null;
    if (typeof document !== "undefined") document.removeEventListener("visibilitychange", this.onVisible);
  }

  private onVisible = (): void => {
    if (document.visibilityState === "visible") void this.refreshQuiet();
  };

  // ---------------------------------------------------------------------------------- reading

  call<K extends MethodName>(method: K, ...params: Params<K> extends void ? [] : [Params<K>]): Promise<Result<K>> {
    return this.transport.call<Result<K>>(method, params[0]);
  }

  async refreshInfo(): Promise<void> {
    this.set({ info: await this.call("app.info") });
  }
  async refreshSettings(): Promise<void> {
    this.set({ settings: await this.call("settings.get") });
  }
  async refreshStorage(): Promise<void> {
    this.set({ storage: await this.call("storage.status") });
  }
  private instancesRequest = 0;
  async refreshInstances(): Promise<void> {
    // Responses can arrive out of order (a slow one started before a fast one): only the latest request
    // may change the state, or an old "ready" could replace a newer "uploading" for a polling interval.
    const request = ++this.instancesRequest;
    const list = await this.call("instances.list", { online: false });
    if (request !== this.instancesRequest) return;
    this.set((s) => {
      const revisions = { ...s.revisions };
      for (const i of list) {
        const before = s.instances.find((x) => x.id === i.id);
        if (before && JSON.stringify(before) !== JSON.stringify(i)) revisions[i.id] = (revisions[i.id] ?? 0) + 1;
      }
      return { instances: list, revisions };
    });
  }
  async refreshDiscovered(): Promise<DiscoveredView[]> {
    const d = await this.call("launcher.discover");
    this.set({ discovered: d });
    return d;
  }
  async refreshJobs(): Promise<void> {
    const list = await this.call("jobs.list");
    this.set((s) => {
      const jobs = { ...s.jobs };
      for (const j of list) {
        // a listing that was requested before the job finished must not bring a finished job back to life
        const known = jobs[j.id];
        if (known && FINISHED(known) && !FINISHED(j)) continue;
        jobs[j.id] = j;
      }
      return { jobs: this.trimJobs(jobs) };
    });
    for (const j of list) if (FINISHED(j)) this.settle(j);
  }

  async refreshAll(): Promise<void> {
    await Promise.all([
      this.refreshInfo(),
      this.refreshSettings(),
      this.refreshStorage(),
      this.refreshInstances(),
      this.refreshJobs(),
    ]);
  }

  /** The periodic refresh: failures only flip the connection indicator. */
  private async refreshQuiet(): Promise<void> {
    try {
      await Promise.all([this.refreshInstances(), this.refreshStorage()]);
      if (this.state.connection === "lost" && this.transport.kind === "tauri") this.set({ connection: "live" });
    } catch {
      /* the next tick tries again */
    }
  }

  private scheduleRefresh(): void {
    if (this.refreshTimer) return;
    this.refreshTimer = setTimeout(() => {
      this.refreshTimer = null;
      void this.refreshQuiet();
    }, 150);
  }

  // ---------------------------------------------------------------------------------- events

  onEvent(ev: AppEvent): void {
    switch (ev.type) {
      case "progress": {
        const { type: _t, ...p } = ev;
        if (p.job) this.set((s) => ({ progress: { ...s.progress, [p.job as string]: p } }));
        break;
      }
      case "job": {
        const { type: _t, ...job } = ev;
        this.set((s) => {
          const progress = { ...s.progress };
          if (FINISHED(job)) delete progress[job.id];
          return { jobs: this.trimJobs({ ...s.jobs, [job.id]: job }), progress };
        });
        if (FINISHED(job)) {
          this.settle(job);
          if (job.instance) this.bump(job.instance);
          this.scheduleRefresh();
        }
        break;
      }
      case "instance_changed":
        this.bump(ev.instance);
        this.scheduleRefresh();
        break;
      case "storage_changed":
        void Promise.all([this.refreshStorage(), this.refreshSettings(), this.refreshInstances()]).catch(() => {});
        break;
      case "notice":
        this.toast(ev.level === "error" ? "error" : ev.level === "warn" ? "warn" : "info", ev.message);
        break;
    }
  }

  private bump(id: InstanceId): void {
    this.set((s) => ({ revisions: { ...s.revisions, [id]: (s.revisions[id] ?? 0) + 1 } }));
  }

  private trimJobs(jobs: Record<JobId, JobInfo>): Record<JobId, JobInfo> {
    const all = Object.values(jobs);
    const finished = all
      .filter(FINISHED)
      .sort((a, b) => (b.finished_ms ?? 0) - (a.finished_ms ?? 0) || b.started_ms - a.started_ms || b.id.localeCompare(a.id));
    const drop = new Set(finished.slice(this.keepJobs).map((j) => j.id));
    if (drop.size === 0) return jobs;
    return Object.fromEntries(Object.entries(jobs).filter(([id]) => !drop.has(id)));
  }

  // ---------------------------------------------------------------------------------- jobs

  private settle(job: JobInfo): void {
    const ws = this.waiters.get(job.id);
    if (!ws) return;
    this.waiters.delete(job.id);
    if (this.waiters.size === 0 && this.jobPoll) {
      clearInterval(this.jobPoll);
      this.jobPoll = null;
    }
    for (const w of ws) w(job);
  }

  private waitFor(id: JobId): Promise<JobInfo> {
    const known = this.state.jobs[id];
    if (known && FINISHED(known)) return Promise.resolve(known);
    return new Promise((resolve) => {
      this.waiters.set(id, [...(this.waiters.get(id) ?? []), resolve]);
      // The finished event can be lost (the stream reconnecting): ask now and then while anyone waits.
      this.jobPoll ??= setInterval(() => void this.refreshJobs().catch(() => {}), 2000);
      // …and it may already have been delivered between the start call and now.
      void this.refreshJobs().catch(() => {});
    });
  }

  /**
   * Run a long operation and wait for its result. Progress is on `state.progress` while it runs;
   * `onStart` hands out the job id so a dialog can follow (or cancel) exactly its own job.
   */
  async runJob<K extends LongMethodName>(
    method: K,
    params: Params<K>,
    opts: { onStart?: (id: JobId) => void } = {},
  ): Promise<Result<K>> {
    const id = await this.transport.startJob(method, params);
    this.jobArgs.set(id, { method, params });
    opts.onStart?.(id);
    const job = await this.waitFor(id);
    if (job.state === "succeeded") return job.result as Result<K>;
    if (job.state === "cancelled") throw new ApiError({ code: "cancelled", message: "Cancelled." });
    throw ApiError.from(job.error ?? { code: "internal", message: "The operation failed." });
  }

  /** Can this job be started again (it was started from this window)? */
  canRetry(id: JobId): boolean {
    return this.jobArgs.has(id);
  }

  /** Run a finished job again with the same arguments, reporting the outcome as a notification. */
  async retryJob(id: JobId): Promise<void> {
    const a = this.jobArgs.get(id);
    if (!a) return;
    try {
      await this.runJob(a.method, a.params as never);
      this.toast("success", "Done.");
    } catch (e) {
      const err = ApiError.from(e);
      if (err.code !== "cancelled") this.toast("error", err.message, "It failed again");
    }
  }

  async cancelJob(id: JobId): Promise<void> {
    await this.call("jobs.cancel", { job: id });
  }

  runningJobs(instance?: InstanceId): JobInfo[] {
    return Object.values(this.state.jobs).filter(
      (j) => j.state === "running" && (instance === undefined || j.instance === instance),
    );
  }

  // ---------------------------------------------------------------------------------- toasts

  toast(level: Toast["level"], message: string, title?: string): number {
    const id = ++this.toastSeq;
    this.set((s) => ({ toasts: [...s.toasts.slice(-4), { id, level, message, ...(title ? { title } : {}) }] }));
    return id;
  }

  dismissToast(id: number): void {
    this.set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) }));
  }
}

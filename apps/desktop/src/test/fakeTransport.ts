import { ApiError } from "../api/errors";
import type { ConnectionStatus, Transport } from "../api/transport";
import type { AppEvent, JobInfo } from "../api/types";

type Handler = (params: unknown) => unknown | Promise<unknown>;

/** An in-memory stand-in for the application layer: handlers per method, jobs that finish by themselves. */
export class FakeTransport implements Transport {
  readonly kind: "tauri" | "http" = "http";
  readonly handlers = new Map<string, Handler>();
  readonly calls: { method: string; params: unknown; job: boolean }[] = [];
  private listeners = new Set<(ev: AppEvent) => void>();
  private statusListeners = new Set<(s: ConnectionStatus) => void>();
  private seq = 0;
  /** When set, jobs stay running until `finish(id)` is called. */
  holdJobs = false;
  private held = new Map<string, () => void>();
  readonly jobs = new Map<string, JobInfo>();

  on(method: string, h: Handler): this {
    this.handlers.set(method, h);
    return this;
  }

  private async run(method: string, params: unknown): Promise<unknown> {
    const h = this.handlers.get(method);
    if (!h) throw new ApiError({ code: "not_found", message: `no handler for ${method}` });
    return await h(params);
  }

  async call<T>(method: string, params?: unknown): Promise<T> {
    this.calls.push({ method, params, job: false });
    return (await this.run(method, params)) as T;
  }

  async startJob(method: string, params?: unknown): Promise<string> {
    this.calls.push({ method, params, job: true });
    const id = `job-${++this.seq}`;
    const instance =
      params && typeof params === "object" && "instance" in params && typeof (params as { instance: unknown }).instance === "string"
        ? (params as { instance: string }).instance
        : null;
    const base: JobInfo = { id, method, instance, state: "running", started_ms: Date.now(), finished_ms: null, error: null, result: null, progress: null };
    this.jobs.set(id, base);
    this.emit({ type: "job", ...base });
    const settle = async () => {
      try {
        const result = await this.run(method, params);
        this.finishWith(id, { state: "succeeded", result });
      } catch (e) {
        const a = ApiError.from(e);
        this.finishWith(id, { state: "failed", error: a.view() });
      }
    };
    if (this.holdJobs) this.held.set(id, () => void settle());
    else queueMicrotask(() => void settle());
    return id;
  }

  /** Let a held job run to its end. */
  finish(id: string): void {
    const f = this.held.get(id);
    this.held.delete(id);
    f?.();
  }

  finishWith(id: string, patch: Partial<JobInfo>): void {
    const cur = this.jobs.get(id);
    if (!cur) return;
    const next = { ...cur, finished_ms: Date.now(), ...patch };
    this.jobs.set(id, next);
    this.emit({ type: "job", ...next });
  }

  subscribe(onEvent: (ev: AppEvent) => void, onStatus?: (s: ConnectionStatus) => void): () => void {
    this.listeners.add(onEvent);
    if (onStatus) {
      this.statusListeners.add(onStatus);
      onStatus("live");
    }
    return () => {
      this.listeners.delete(onEvent);
      if (onStatus) this.statusListeners.delete(onStatus);
    };
  }

  emit(ev: AppEvent): void {
    for (const l of this.listeners) l(ev);
  }

  setConnection(s: ConnectionStatus): void {
    for (const l of this.statusListeners) l(s);
  }

  count(method: string): number {
    return this.calls.filter((c) => c.method === method).length;
  }
}

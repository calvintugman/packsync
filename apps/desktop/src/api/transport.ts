import { ApiError } from "./errors";
import type { AppEvent, JobId } from "./types";

export type ConnectionStatus = "connecting" | "live" | "lost";

/** How the UI reaches the application layer: Tauri commands in the desktop app, HTTP in the dev bridge. */
export interface Transport {
  readonly kind: "tauri" | "http";
  call<T>(method: string, params?: unknown): Promise<T>;
  startJob(method: string, params?: unknown): Promise<JobId>;
  /** Subscribe to events. Returns the unsubscribe function. */
  subscribe(onEvent: (ev: AppEvent) => void, onStatus?: (s: ConnectionStatus) => void): () => void;
  /** Native folder picker, when the shell has one. */
  pickFolder?(title: string): Promise<string | null>;
}

// ------------------------------------------------------------------------------------ HTTP (dev bridge)

const TOKEN_KEY = "packsync-bridge-token";

/** The bridge token is handed over in the URL fragment (never sent to a server) and kept for the tab only. */
export function readBridgeToken(hash: string, storage: Pick<Storage, "getItem" | "setItem"> | null): string | null {
  const m = /(?:^#|&)token=([0-9a-fA-F]+)/.exec(hash);
  if (m?.[1]) {
    try {
      storage?.setItem(TOKEN_KEY, m[1]);
    } catch {
      /* storage unavailable: the token still works for this page load */
    }
    return m[1];
  }
  try {
    return storage?.getItem(TOKEN_KEY) ?? null;
  } catch {
    return null;
  }
}

interface Envelope<T> {
  ok: boolean;
  result?: T;
  error?: unknown;
}

export class HttpTransport implements Transport {
  readonly kind = "http" as const;

  constructor(
    private readonly token: string,
    private readonly base = "",
    private readonly fetchImpl: typeof fetch = (...a) => fetch(...a),
    private readonly eventSource: (url: string) => EventSource = (u) => new EventSource(u),
  ) {}

  private async post<T>(path: string, method: string, params: unknown): Promise<T> {
    let res: Response;
    try {
      res = await this.fetchImpl(`${this.base}${path}`, {
        method: "POST",
        headers: { "content-type": "application/json", "x-packsync-token": this.token },
        body: JSON.stringify({ method, params: params ?? null }),
      });
    } catch {
      throw new ApiError({
        code: "bridge_unreachable",
        message: "The PackSync development bridge is not reachable.",
        advice: "Is `packsync dev-bridge` still running?",
      });
    }
    if (res.status === 401 || res.status === 403) {
      throw new ApiError({
        code: "bridge_unauthorized",
        message: "The development bridge refused the request (missing or wrong token).",
        advice: "Open the address `packsync dev-bridge` printed, including the #token=… part.",
      });
    }
    let doc: Envelope<T>;
    try {
      doc = (await res.json()) as Envelope<T>;
    } catch {
      throw new ApiError({ code: "internal", message: `The bridge sent an unreadable answer (HTTP ${res.status}).` });
    }
    if (!doc.ok) throw ApiError.from(doc.error);
    return doc.result as T;
  }

  call<T>(method: string, params?: unknown): Promise<T> {
    return this.post<T>("/api/call", method, params);
  }

  async startJob(method: string, params?: unknown): Promise<JobId> {
    const r = await this.post<{ job: JobId }>("/api/job", method, params);
    return r.job;
  }

  subscribe(onEvent: (ev: AppEvent) => void, onStatus?: (s: ConnectionStatus) => void): () => void {
    onStatus?.("connecting");
    const es = this.eventSource(`${this.base}/api/events?token=${encodeURIComponent(this.token)}`);
    es.onopen = () => onStatus?.("live");
    es.onerror = () => onStatus?.("lost"); // EventSource reconnects by itself
    es.onmessage = (m: MessageEvent<string>) => {
      try {
        onEvent(JSON.parse(m.data) as AppEvent);
      } catch {
        /* a malformed event is dropped; the next refresh catches up */
      }
    };
    return () => es.close();
  }

  async pickFolder(): Promise<string | null> {
    return null; // a browser cannot show a native folder picker with a real path
  }
}

// ------------------------------------------------------------------------------------ Tauri

export const TAURI_EVENT = "packsync://event";

export class TauriTransport implements Transport {
  readonly kind = "tauri" as const;

  async call<T>(method: string, params?: unknown): Promise<T> {
    const { invoke } = await import("@tauri-apps/api/core");
    try {
      return await invoke<T>("call", { method, params: params ?? null });
    } catch (e) {
      throw ApiError.from(e);
    }
  }

  async startJob(method: string, params?: unknown): Promise<JobId> {
    const { invoke } = await import("@tauri-apps/api/core");
    try {
      return await invoke<JobId>("start_job", { method, params: params ?? null });
    } catch (e) {
      throw ApiError.from(e);
    }
  }

  subscribe(onEvent: (ev: AppEvent) => void, onStatus?: (s: ConnectionStatus) => void): () => void {
    let stop: (() => void) | null = null;
    let cancelled = false;
    onStatus?.("connecting");
    void import("@tauri-apps/api/event").then(async ({ listen }) => {
      const un = await listen<AppEvent>(TAURI_EVENT, (e) => onEvent(e.payload));
      if (cancelled) un();
      else stop = un;
      onStatus?.("live");
    });
    return () => {
      cancelled = true;
      stop?.();
    };
  }

  async pickFolder(title: string): Promise<string | null> {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const r = await open({ directory: true, multiple: false, title });
    return typeof r === "string" ? r : null;
  }
}

export function inTauri(w: object = window): boolean {
  return "__TAURI_INTERNALS__" in w;
}

/** Pick the transport for this environment. `null` means "no way to reach the application layer". */
export function createTransport(w: Window = window): Transport | null {
  if (inTauri(w)) return new TauriTransport();
  const token = readBridgeToken(w.location.hash, sessionStorageOf(w));
  return token ? new HttpTransport(token) : null;
}

function sessionStorageOf(w: Window): Storage | null {
  try {
    return w.sessionStorage;
  } catch {
    return null; // blocked by the browser's privacy settings
  }
}

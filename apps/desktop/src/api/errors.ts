import type { ErrorView } from "./types";

/** An error from the application layer, in the shape the CLI uses too (`code` is stable and matchable). */
export class ApiError extends Error {
  readonly code: string;
  readonly advice: string | null;
  readonly exitCode: number;

  constructor(view: { code: string; message: string; advice?: string | null; exit_code?: number }) {
    super(view.message);
    this.name = "ApiError";
    this.code = view.code;
    this.advice = view.advice ?? null;
    this.exitCode = view.exit_code ?? 1;
  }

  /** Normalise whatever was thrown (an `ErrorView` object, a string, an `Error`) into an `ApiError`. */
  static from(e: unknown): ApiError {
    if (e instanceof ApiError) return e;
    if (typeof e === "string") return new ApiError({ code: "internal", message: e });
    if (e && typeof e === "object") {
      const o = e as Record<string, unknown>;
      if (typeof o.code === "string" && typeof o.message === "string") {
        return new ApiError({
          code: o.code,
          message: o.message,
          advice: typeof o.advice === "string" ? o.advice : null,
          exit_code: typeof o.exit_code === "number" ? o.exit_code : 1,
        });
      }
      if (e instanceof Error) return new ApiError({ code: "internal", message: e.message });
    }
    return new ApiError({ code: "internal", message: "Something went wrong." });
  }

  view(): ErrorView {
    return { code: this.code, message: this.message, advice: this.advice, exit_code: this.exitCode };
  }
}

export interface ErrorDescription {
  title: string;
  message: string;
  advice: string | null;
  /** A follow-up the UI can offer. */
  action?: "sign_in" | "open_conflict" | "open_crash" | "open_settings";
}

const TITLES: Record<string, string> = {
  offline: "Can't reach the cloud",
  storage_unavailable: "The cloud isn't responding",
  auth_required: "Sign-in needed",
  auth_revoked: "Access was revoked",
  auth_scope: "Google access is incomplete",
  quota_exceeded: "Google Drive is full",
  rate_limited: "Google asked PackSync to slow down",
  conflict: "Two versions need your decision",
  crash_pending: "A crash save is waiting for you",
  busy: "Another operation is running",
  game_running: "The game is running",
  no_space: "Not enough disk space",
  instance_unavailable: "The instance folder isn't available",
  safety_guard: "PackSync stopped to protect your files",
  corrupt_remote: "Cloud data is damaged or incomplete",
  cloud_data_missing: "Some cloud data is missing",
  cancelled: "Cancelled",
  config: "A setting needs attention",
  not_found: "Not found",
  invalid: "That didn't work",
  unsupported: "Not supported",
};

const ACTIONS: Record<string, ErrorDescription["action"]> = {
  auth_required: "sign_in",
  auth_revoked: "sign_in",
  auth_scope: "sign_in",
  conflict: "open_conflict",
  crash_pending: "open_crash",
  config: "open_settings",
};

export function describeError(e: unknown): ErrorDescription {
  const a = ApiError.from(e);
  return {
    title: TITLES[a.code] ?? "Something went wrong",
    message: a.message,
    advice: a.advice,
    action: ACTIONS[a.code],
  };
}

/** True for errors that mean "try again later" rather than "something is wrong". */
export function isTransient(e: unknown): boolean {
  const c = ApiError.from(e).code;
  return c === "offline" || c === "storage_unavailable" || c === "rate_limited";
}

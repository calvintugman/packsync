import type { InstanceStatus, SyncPhase } from "../api/types";

export type Tone = "ok" | "info" | "warn" | "danger" | "neutral";

export interface PhaseInfo {
  label: string;
  tone: Tone;
  /** One sentence that says what this means for the user's progress. */
  summary: string;
  /** Index in the four-step upload journey, or `null` when the journey does not apply. */
  step: 0 | 1 | 2 | 3 | null;
  attention: boolean;
}

export const STEPS = [
  { label: "Saved on this computer", hint: "A complete local snapshot exists." },
  { label: "Uploading", hint: "Sending new data to your cloud." },
  { label: "Published", hint: "Stored and verified in the cloud." },
  { label: "Ready on your other computers", hint: "Another computer can find and restore it." },
] as const;

export const PHASES: Record<SyncPhase, PhaseInfo> = {
  not_enabled: {
    label: "Not syncing",
    tone: "neutral",
    summary: "This instance is not synced yet.",
    step: null,
    attention: false,
  },
  local_snapshot_complete: {
    label: "Saved here, not uploaded yet",
    tone: "warn",
    summary: "Your progress is saved on this computer but is not in the cloud yet. Other computers cannot see it.",
    step: 0,
    attention: false,
  },
  upload_pending: {
    label: "Upload pending",
    tone: "warn",
    summary: "The upload is running or waiting to retry. This is not synced yet.",
    step: 1,
    attention: false,
  },
  cloud_published: {
    label: "Published to the cloud",
    tone: "info",
    summary: "Stored and verified in the cloud. Waiting for it to appear in a fresh listing.",
    step: 2,
    attention: false,
  },
  ready_on_other_devices: {
    label: "Ready on your other computers",
    tone: "ok",
    summary: "Your latest progress is in the cloud and other computers can continue from it.",
    step: 3,
    attention: false,
  },
  cloud_ahead: {
    label: "Newer version in the cloud",
    tone: "info",
    summary: "Another computer saved newer progress. It is applied the next time you sync or start the game.",
    step: null,
    attention: true,
  },
  conflict: {
    label: "Needs your decision",
    tone: "danger",
    summary: "This computer and the cloud both changed. PackSync never merges worlds automatically.",
    step: null,
    attention: true,
  },
  crash_pending: {
    label: "Crash save waiting",
    tone: "danger",
    summary: "The last session did not end normally. Decide whether to keep or discard what it saved.",
    step: null,
    attention: true,
  },
  restoring: {
    label: "Restore in progress",
    tone: "info",
    summary: "A restore is running or needs recovery.",
    step: null,
    attention: true,
  },
  unavailable: {
    label: "Folder not available",
    tone: "danger",
    summary: "The instance folder is missing (an unplugged drive?). PackSync will not touch anything until it is back.",
    step: null,
    attention: true,
  },
};

export function phaseInfo(phase: SyncPhase): PhaseInfo {
  return PHASES[phase];
}

/** Does this instance need the user to do something? */
export function needsAttention(i: InstanceStatus): boolean {
  return PHASES[i.phase].attention && i.phase !== "cloud_ahead" && i.phase !== "restoring";
}

export function canPlay(i: InstanceStatus): boolean {
  return i.sync_enabled && i.state !== "unavailable" && i.state !== "restoring";
}

import type { InstanceId, JobInfo, ProgressEvent } from "../api/types";
import type { AppState } from "./store";

export function runningJobsFor(s: AppState, instance: InstanceId): JobInfo[] {
  return Object.values(s.jobs).filter((j) => j.state === "running" && j.instance === instance);
}

export function progressOf(s: AppState, job: JobInfo): ProgressEvent | null {
  return s.progress[job.id] ?? job.progress ?? null;
}

const LABELS: Record<string, string> = {
  "instance.sync": "Syncing",
  "instance.snapshot": "Saving a snapshot of",
  "instance.upload": "Uploading",
  "instance.reconcile": "Checking the cloud for",
  "instance.changes": "Looking for changes in",
  "instance.verify_local": "Verifying",
  "instance.verify_remote": "Verifying the cloud copy of",
  "instances.enable": "Starting to sync",
  "instances.preview": "Previewing",
  "restore.apply": "Restoring",
  "restore.new": "Bringing to this computer:",
  "restore.preview": "Previewing a restore of",
  "conflict.resolve": "Resolving the conflict in",
  "conflict.show": "Comparing versions of",
  "crash.promote": "Keeping the crash save of",
  "crash.discard": "Discarding the crash save of",
  "launch.start": "Playing",
  "rules.show": "Reading the rules of",
  "rules.explain": "Checking the rules of",
};

const PLAIN: Record<string, string> = {
  "storage.google_login": "Signing in to Google",
  "storage.test": "Testing the cloud connection",
  "cloud.instances": "Looking up instances in the cloud",
  "gc.plan": "Planning a cloud cleanup",
  "gc.run": "Cleaning up the cloud",
  "diagnostics.report": "Running checks",
};

/** "Syncing Skyblock Pack" for the activity list. */
export function jobTitle(j: JobInfo, nameOf: (id: InstanceId) => string | undefined): string {
  const plain = PLAIN[j.method];
  if (plain) return plain;
  const verb = LABELS[j.method] ?? j.method;
  const name = j.instance ? nameOf(j.instance) : undefined;
  return name ? `${verb} ${name}` : verb;
}

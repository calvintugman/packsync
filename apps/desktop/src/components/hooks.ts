import type { InstanceId, InstanceStatus, JobInfo } from "../api/types";
import { useAppState } from "../state/context";

export function useInstance(id: InstanceId): InstanceStatus | undefined {
  return useAppState((s) => s.instances.find((i) => i.id === id));
}

export function useRevision(id: InstanceId): number {
  return useAppState((s) => s.revisions[id] ?? 0);
}

/** Running jobs of one instance (newest first). */
export function useRunningJobs(id: InstanceId): JobInfo[] {
  const jobs = useAppState((s) => s.jobs);
  return Object.values(jobs)
    .filter((j) => j.state === "running" && j.instance === id)
    .sort((a, b) => b.started_ms - a.started_ms);
}

import { useState } from "react";
import type { InstanceStatus, PendingChanges } from "../api/types";
import { useInstance, useRevision, useRunningJobs } from "../components/hooks";
import { JobBar } from "../components/JobBar";
import { usePlay, useSyncNow } from "../components/InstanceCard";
import { ConflictDialog } from "../dialogs/ConflictDialog";
import { CrashDialog } from "../dialogs/CrashDialog";
import { describeUpload } from "../lib/describe";
import { formatBytes, formatDateTime, formatRelative, packLine, plural, worldName } from "../lib/format";
import { canPlay, phaseInfo } from "../lib/phases";
import { href, navigate } from "../lib/router";
import { useAction, useStore } from "../state/context";
import { Banner, Button, EmptyState, Kv, Pill } from "../ui/controls";
import { ConfirmDialog } from "../ui/ConfirmDialog";
import { ErrorNote } from "../ui/ErrorNote";
import { Stepper } from "../ui/Stepper";
import { Tabs } from "../ui/Tabs";
import { HistoryTab } from "./instance/HistoryTab";
import { HooksTab } from "./instance/HooksTab";
import { RecoveryTab } from "./instance/RecoveryTab";
import { RulesTab } from "./instance/RulesTab";

const TABS = [
  { id: "history", label: "History" },
  { id: "rules", label: "What is synced" },
  { id: "hooks", label: "Launcher hooks" },
  { id: "recovery", label: "Recovery points" },
];

function Changes({ inst }: { inst: InstanceStatus }) {
  const store = useStore();
  const [changes, setChanges] = useState<PendingChanges | null>(null);
  const look = useAction(async () => setChanges(await store.runJob("instance.changes", { instance: inst.id })));
  const total = changes ? changes.added.length + changes.modified.length + changes.removed.length : 0;
  return (
    <div className="card col">
      <div className="row">
        <h3 className="grow">Changes since the last snapshot</h3>
        <Button size="sm" busy={look.busy} onClick={() => void look.run()}>
          {changes ? "Check again" : "Look for changes"}
        </Button>
      </div>
      {look.error ? <ErrorNote error={look.error} instanceId={inst.id} /> : null}
      {!changes && !look.busy ? <p className="muted">Nothing has been checked yet. This reads the files, so it can take a moment.</p> : null}
      {changes && total === 0 ? <p>No changes: what is on disk matches the last snapshot.</p> : null}
      {changes && total > 0 ? (
        <div className="col" style={{ gap: 6 }}>
          <p>
            {plural(changes.added.length, "file")} added, {plural(changes.modified.length, "file")} changed, {plural(changes.removed.length, "file")} removed
            {changes.exact ? "" : " (estimate)"}.
          </p>
          <ul style={{ margin: 0, paddingLeft: 18 }}>
            {[
              ...changes.modified.slice(0, 6).map((p) => ["changed", p] as const),
              ...changes.added.slice(0, 6).map((p) => ["added", p] as const),
              ...changes.removed.slice(0, 6).map((p) => ["removed", p] as const),
            ]
              .slice(0, 12)
              .map(([k, p]) => (
                <li key={k + p}>
                  <span className="muted">{k}</span> <code>{p}</code>
                </li>
              ))}
          </ul>
        </div>
      ) : null}
    </div>
  );
}

export function InstanceDetailScreen({ id, tab }: { id: string; tab: string | null }) {
  const store = useStore();
  const inst = useInstance(id);
  const revision = useRevision(id);
  const jobs = useRunningJobs(id);
  const [dialog, setDialog] = useState<"conflict" | "crash" | "disable" | null>(null);
  const sync = useSyncNow({ id, name: inst?.name ?? "" }, () => setDialog("conflict"));
  const play = usePlay({ id, name: inst?.name ?? "" });
  const snapshot = useAction(async () => {
    const r = await store.runJob("instance.snapshot", { instance: id });
    store.toast("success", r.result.result === "created" ? "Snapshot saved." : "Nothing changed since the last snapshot.");
    await store.refreshInstances();
  });
  const upload = useAction(async () => {
    const r = await store.runJob("instance.upload", { instance: id });
    const o = describeUpload(r);
    store.toast(o.tone === "warn" ? "warn" : "success", o.text);
    await store.refreshInstances();
  });

  if (!inst) {
    return (
      <div className="page">
        <a href={href({ name: "instances" })}>← Instances</a>
        <div className="card">
          <EmptyState title="This instance isn't in PackSync" action={<Button onClick={() => navigate({ name: "instances" })}>Back to instances</Button>}>
            It may have been removed or never started syncing.
          </EmptyState>
        </div>
      </div>
    );
  }

  const info = phaseInfo(inst.phase);
  const busy = jobs.length > 0 || sync.busy || play.busy || snapshot.busy || upload.busy;
  const playing = jobs.find((j) => j.method === "launch.start");
  const activeTab = TABS.some((t) => t.id === tab) ? (tab as string) : "history";

  return (
    <div className="page">
      <a href={href({ name: "instances" })}>← Instances</a>
      <div className="page-head" data-phase={inst.phase}>
        <h1>{inst.name}</h1>
        <Pill tone={info.tone}>{info.label}</Pill>
      </div>
      <div className="row wrap">
        <Button
          variant="primary"
          icon="play"
          busy={Boolean(playing) || play.busy}
          disabled={!canPlay(inst) || (busy && !playing) || inst.phase === "conflict" || inst.phase === "crash_pending"}
          onClick={() => void play.run()}
        >
          {playing ? "Playing…" : "Play"}
        </Button>
        <Button icon="refresh" busy={sync.busy} disabled={busy || inst.state === "unavailable"} onClick={() => void sync.run()}>
          Sync now
        </Button>
        <Button busy={snapshot.busy} disabled={busy || inst.state !== "ok"} onClick={() => void snapshot.run()}>
          Save a snapshot
        </Button>
        {inst.pending_snapshots > 0 ? (
          <Button icon="upload" busy={upload.busy} disabled={busy} onClick={() => void upload.run()}>
            Upload now
          </Button>
        ) : null}
        <span className="spacer" />
        <Button variant="ghost" onClick={() => setDialog("disable")}>
          Stop syncing…
        </Button>
      </div>

      {jobs.map((j) => (
        <JobBar key={j.id} jobId={j.id} cancellable={j.method !== "launch.start"} />
      ))}
      {sync.error ? <ErrorNote error={sync.error} instanceId={id} /> : null}
      {play.error ? <ErrorNote error={play.error} instanceId={id} /> : null}
      {snapshot.error ? <ErrorNote error={snapshot.error} instanceId={id} /> : null}
      {upload.error ? <ErrorNote error={upload.error} instanceId={id} /> : null}

      {inst.phase === "conflict" ? (
        <Banner tone="danger" title="Two versions need your decision" actions={<Button size="sm" variant="primary" onClick={() => setDialog("conflict")}>Choose a version…</Button>}>
          {inst.state_detail ?? "This computer and the cloud both changed. PackSync never merges worlds automatically."}
        </Banner>
      ) : null}
      {inst.phase === "crash_pending" ? (
        <Banner tone="danger" title="The last session did not end normally" actions={<Button size="sm" variant="primary" onClick={() => setDialog("crash")}>Review the crash save…</Button>}>
          {inst.state_detail ?? "PackSync saved what it found and is waiting for your decision."}
        </Banner>
      ) : null}
      {inst.phase === "unavailable" ? <Banner tone="danger" title="Folder not available">{inst.state_detail ?? inst.root}</Banner> : null}
      {inst.playing_elsewhere.length > 0 ? (
        <Banner tone="warn" title={`Possibly being played on ${inst.playing_elsewhere.join(", ")}`}>
          Playing on two computers at once leads to conflicts.
        </Banner>
      ) : null}
      {inst.last_error ? (
        <Banner tone="warn" title="The last upload did not finish">
          {inst.last_error}
          {inst.next_retry_ms ? ` It will retry ${formatRelative(inst.next_retry_ms)}.` : ""}
        </Banner>
      ) : null}

      <div className="card col">
        <Stepper phase={inst.phase} />
        <p className="meta">{info.summary}</p>
        <Kv
          rows={[
            ["Version", packLine(inst.pack)],
            [
              "Last synced",
              inst.last_synced_ms ? (
                <span title={formatDateTime(inst.last_synced_ms)}>
                  {formatRelative(inst.last_synced_ms)}
                  {inst.last_synced_by ? ` from ${inst.last_synced_by}` : ""}
                </span>
              ) : (
                "Not yet"
              ),
            ],
            ["Size", `${plural(inst.total_files, "file")} · ${formatBytes(inst.total_bytes)}`],
            ["Worlds", inst.worlds.length ? inst.worlds.map((w) => worldName(w)).join(", ") : "None yet"],
            ["Folder", <code key="f">{inst.root}</code>],
            ["Cloud", inst.provider_id ?? "—"],
          ]}
        />
      </div>

      <Changes inst={inst} />

      <Tabs tabs={TABS} active={activeTab} onChange={(t) => navigate({ name: "instance", id, tab: t })}>
        {activeTab === "history" ? <HistoryTab instance={id} revision={revision} /> : null}
        {activeTab === "rules" ? <RulesTab inst={inst} /> : null}
        {activeTab === "hooks" ? <HooksTab inst={inst} /> : null}
        {activeTab === "recovery" ? <RecoveryTab instance={id} revision={revision} /> : null}
      </Tabs>

      {dialog === "conflict" ? <ConflictDialog instance={inst} onClose={() => setDialog(null)} /> : null}
      {dialog === "crash" ? <CrashDialog instance={inst} onClose={() => setDialog(null)} /> : null}
      {dialog === "disable" ? (
        <ConfirmDialog
          title={`Stop syncing ${inst.name}?`}
          confirmLabel="Stop syncing"
          danger
          onClose={() => setDialog(null)}
          onConfirm={async () => {
            await store.call("instances.disable", { instance: id });
            await store.refreshInstances();
            store.toast("info", `${inst.name} is no longer synced. Its files and cloud copy are untouched.`);
            navigate({ name: "instances" });
          }}
        >
          <p>
            PackSync stops saving and uploading this instance. <strong>Nothing is deleted</strong> — not the files on this
            computer and not the copy in your cloud. You can start syncing it again later.
          </p>
        </ConfirmDialog>
      ) : null}
    </div>
  );
}

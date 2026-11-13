import { useState } from "react";
import type { ApiError } from "../api/errors";
import type { InstanceStatus } from "../api/types";
import { describeLaunch, describeSync, describeUpload } from "../lib/describe";
import { formatBytes, formatRelative, packLine, plural, sentence, worldsLine } from "../lib/format";
import { canPlay, phaseInfo } from "../lib/phases";
import { href, navigate } from "../lib/router";
import { ConflictDialog } from "../dialogs/ConflictDialog";
import { CrashDialog } from "../dialogs/CrashDialog";
import { UnknownDialog } from "../dialogs/UnknownDialog";
import { useAction, useStore } from "../state/context";
import { Banner, Button, Pill } from "../ui/controls";
import { ErrorNote } from "../ui/ErrorNote";
import { Stepper } from "../ui/Stepper";
import { useRunningJobs } from "./hooks";
import { JobBar } from "./JobBar";

/** Run "Sync now" and report the outcome; shared by the card and the detail page. */
export function useSyncNow(inst: { id: string; name: string }, openConflict: () => void) {
  const store = useStore();
  return useAction(async () => {
    const r = await store.runJob("instance.sync", { instance: inst.id });
    const o = describeSync(r);
    if (r.reconcile.outcome === "conflict") {
      openConflict(); // the dialog says it all; a toast on top of it would only be in the way
    } else {
      store.toast(o.tone === "danger" ? "error" : o.tone === "warn" ? "warn" : "success", o.text, inst.name);
    }
    await store.refreshInstances();
    return r;
  });
}

/** Start the game. `skipSync` is the person's explicit choice to play the local copy without asking the cloud. */
export function usePlay(inst: { id: string; name: string }) {
  const store = useStore();
  return useAction(async (opts?: { skipSync?: boolean }) => {
    const r = await store.runJob("launch.start", { instance: inst.id, options: opts?.skipSync ? { skip_sync: true } : {} });
    const o = describeLaunch(r);
    store.toast(o.tone === "danger" ? "error" : o.tone === "warn" ? "warn" : "success", o.text, inst.name);
    for (const w of r.warnings) store.toast("warn", w, inst.name);
    await store.refreshInstances();
    return r;
  });
}

/** What to offer when Play failed: for "the cloud can't be reached", the explicit choice to play offline. */
export function PlayError({ error, play, inst }: { error: ApiError | null; play: ReturnType<typeof usePlay>; inst: { id: string } }) {
  if (!error) return null;
  const offline = error.code === "offline" || error.code === "storage_unavailable";
  return (
    <ErrorNote
      error={error}
      instanceId={inst.id}
      onRetry={() => void play.run()}
      actions={
        offline ? (
          <Button size="sm" variant="primary" onClick={() => void play.run({ skipSync: true })}>
            Play from this computer anyway
          </Button>
        ) : null
      }
    />
  );
}

/** Upload what is waiting now, even if an earlier failure put a back-off in place: the person asked. */
export function useUploadNow(inst: { id: string; name: string }) {
  const store = useStore();
  return useAction(async () => {
    const r = await store.runJob("instance.upload", { instance: inst.id, force: true });
    const o = describeUpload(r);
    store.toast(o.tone === "warn" ? "warn" : "success", o.text, inst.name);
    await store.refreshInstances();
  });
}

/** A coloured tile with the pack's initials; the colour is stable per name. */
export function PackTile({ name }: { name: string }) {
  const words = name.replace(/[^\p{L}\p{N} ]/gu, " ").trim().split(/\s+/).filter(Boolean);
  const initials = (words.length > 1 ? words[0]![0]! + words[1]![0]! : (words[0] ?? "?").slice(0, 2)).toUpperCase();
  let hue = 0;
  for (const ch of name) hue = (hue * 31 + ch.codePointAt(0)!) % 360;
  return (
    <span className="pack-tile" aria-hidden="true" style={{ "--hue": hue } as React.CSSProperties}>
      {initials}
    </span>
  );
}

export function InstanceCard({ inst }: { inst: InstanceStatus }) {
  const [dialog, setDialog] = useState<"conflict" | "crash" | "unknown" | null>(null);
  const jobs = useRunningJobs(inst.id);
  const sync = useSyncNow(inst, () => setDialog("conflict"));
  const play = usePlay(inst);
  const upload = useUploadNow(inst);
  const info = phaseInfo(inst.phase);
  const busy = jobs.length > 0 || sync.busy || play.busy || upload.busy;
  const playing = jobs.find((j) => j.method === "launch.start");
  const attention = inst.phase === "conflict" || inst.phase === "crash_pending" || inst.phase === "unavailable";

  return (
    <article className={`card instance-card ${attention ? "attention" : ""}`} aria-label={inst.name} data-phase={inst.phase}>
      <div className="row" style={{ alignItems: "flex-start", gap: 14 }}>
        <PackTile name={inst.name} />
        <div className="grow">
          <a className="instance-title" href={href({ name: "instance", id: inst.id, tab: null })}>
            {inst.name}
          </a>
          <div className="meta">
            {packLine(inst.pack)}
            <span className="sep">·</span>
            {worldsLine(inst.worlds)}
            <span className="sep">·</span>
            {formatBytes(inst.total_bytes)}
          </div>
        </div>
        <Pill tone={info.tone}>{info.label}</Pill>
      </div>

      <p className="meta">
        {info.summary}
        {inst.last_synced_ms ? (
          <>
            {" "}
            Last synced {formatRelative(inst.last_synced_ms)}
            {inst.last_synced_by ? ` from ${inst.last_synced_by}` : ""}.
          </>
        ) : null}
      </p>

      <Stepper phase={inst.phase} />
      {inst.unknown.length > 0 ? (
        <Banner
          tone="info"
          title="New mod data found"
          actions={
            <Button size="sm" variant="primary" onClick={() => setDialog("unknown")}>
              Review
            </Button>
          }
        >
          {plural(inst.unknown.length, "folder")} PackSync doesn't recognise yet, like <code>{inst.unknown[0]!.path}</code>.
          Choose what to sync to your other computers.
        </Banner>
      ) : null}
      {inst.pending_snapshots > 0 ? (
        <p className="meta" data-testid="pending">
          {plural(inst.pending_snapshots, "snapshot")} ({formatBytes(inst.pending_pack_bytes)}) waiting to upload.
        </p>
      ) : null}

      {(inst.phase === "upload_pending" || inst.phase === "local_snapshot_complete") && inst.last_error && !sync.error && !upload.error ? (
        <Banner tone="warn" title="The last upload did not finish">
          {sentence(inst.last_error)}
          {inst.next_retry_ms ? ` It will retry ${formatRelative(inst.next_retry_ms)}.` : ""}
        </Banner>
      ) : null}
      {inst.playing_elsewhere.length > 0 ? (
        <Banner tone="warn" title={`Possibly being played on ${inst.playing_elsewhere.join(", ")}`}>
          Playing on two computers at once leads to conflicts. Close it there first, or continue if that is old news.
        </Banner>
      ) : null}
      {inst.phase === "conflict" ? (
        <Banner tone="danger" title="Two versions need your decision" actions={<Button size="sm" variant="primary" onClick={() => setDialog("conflict")}>Choose a version…</Button>}>
          {inst.state_detail ?? "This computer and the cloud both changed."}
        </Banner>
      ) : null}
      {inst.phase === "crash_pending" ? (
        <Banner tone="danger" title="The last session did not end normally" actions={<Button size="sm" variant="primary" onClick={() => setDialog("crash")}>Review the crash save…</Button>}>
          {inst.state_detail ?? "PackSync saved what it found and is waiting for your decision."}
        </Banner>
      ) : null}
      {inst.phase === "unavailable" ? <Banner tone="danger" title="Folder not available">{inst.state_detail ?? inst.root}</Banner> : null}

      {jobs.map((j) => (
        <JobBar key={j.id} jobId={j.id} cancellable={j.method !== "launch.start"} />
      ))}
      {sync.error ? (
        <ErrorNote error={sync.error} instanceId={inst.id} onRetry={() => void sync.run()} onSignIn={() => navigate({ name: "settings" })} />
      ) : null}
      {upload.error ? (
        <ErrorNote error={upload.error} instanceId={inst.id} onRetry={() => void upload.run()} onSignIn={() => navigate({ name: "settings" })} />
      ) : null}
      <PlayError error={play.error} play={play} inst={inst} />

      <div className="row wrap">
        <Button
          variant="primary"
          icon="play"
          busy={play.busy || Boolean(playing)}
          disabled={!canPlay(inst) || (busy && !playing) || inst.phase === "conflict" || inst.phase === "crash_pending"}
          title={inst.phase === "conflict" ? "Resolve the conflict first" : inst.phase === "crash_pending" ? "Decide about the crash save first" : undefined}
          onClick={() => void play.run()}
        >
          {playing ? "Playing…" : "Play"}
        </Button>
        <Button icon="refresh" busy={sync.busy} disabled={busy || inst.state === "unavailable"} onClick={() => void sync.run()}>
          Sync now
        </Button>
        {inst.pending_snapshots > 0 ? (
          <Button icon="upload" busy={upload.busy} disabled={busy} onClick={() => void upload.run()}>
            Upload now
          </Button>
        ) : null}
        <a className="btn ghost" href={href({ name: "instance", id: inst.id, tab: null })}>
          Details
        </a>
      </div>

      {dialog === "conflict" ? <ConflictDialog instance={inst} onClose={() => setDialog(null)} /> : null}
      {dialog === "crash" ? <CrashDialog instance={inst} onClose={() => setDialog(null)} /> : null}
      {dialog === "unknown" ? <UnknownDialog inst={inst} onClose={() => setDialog(null)} /> : null}
    </article>
  );
}

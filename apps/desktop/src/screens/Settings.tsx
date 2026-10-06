import { useState } from "react";
import type { Diagnostics, OfflinePolicy, Settings, StorageTest } from "../api/types";
import { PathField } from "../components/PathField";
import { StorageSetup } from "../components/StorageSetup";
import { GcDialog } from "../dialogs/GcDialog";
import { formatBytes } from "../lib/format";
import { useAction, useAppState, useStore } from "../state/context";
import { Banner, Button, Checkbox, Choice, Kv, Pill, TextInput } from "../ui/controls";
import { ConfirmDialog } from "../ui/ConfirmDialog";
import { ErrorNote } from "../ui/ErrorNote";

function Section({ title, children, id }: { title: string; children: React.ReactNode; id: string }) {
  return (
    <section className="card col" aria-labelledby={id}>
      <h2 id={id}>{title}</h2>
      {children}
    </section>
  );
}

function ThisComputer() {
  const store = useStore();
  const info = useAppState((s) => s.info);
  const [name, setName] = useState(info?.device.name ?? "");
  const rename = useAction(async () => {
    await store.call("device.rename", { name: name.trim() });
    await store.refreshInfo();
    store.toast("success", "Renamed. Other computers see the new name from your next save.");
  });
  return (
    <Section id="s-computer" title="This computer">
      <form
        className="row"
        style={{ alignItems: "flex-end" }}
        onSubmit={(e) => {
          e.preventDefault();
          void rename.run();
        }}
      >
        <div className="grow">
          <TextInput label="Name" value={name} onChange={(e) => setName(e.target.value)} hint="Shown in History and when another computer might be playing the same instance." />
        </div>
        <Button type="submit" busy={rename.busy} disabled={!name.trim() || name.trim() === info?.device.name}>
          Rename
        </Button>
      </form>
      {rename.error ? <ErrorNote error={rename.error} /> : null}
    </Section>
  );
}

function CloudSection() {
  const store = useStore();
  const storage = useAppState((s) => s.storage);
  const [changing, setChanging] = useState(false);
  const [disconnect, setDisconnect] = useState(false);
  const [revoke, setRevoke] = useState(true);
  const [test, setTest] = useState<StorageTest | null>(null);
  const run = useAction(async () => {
    setTest(await store.runJob("storage.test", undefined));
  });
  return (
    <Section id="s-cloud" title="Cloud">
      <div className="row wrap">
        <Pill tone={storage?.connected ? "ok" : storage?.needs_reconnect ? "danger" : "neutral"}>
          {storage?.connected ? "Connected" : storage?.needs_reconnect ? "Sign in again" : "Not connected"}
        </Pill>
        <span>{storage?.detail ?? "—"}</span>
      </div>
      {storage?.problem ? <Banner tone="warn">{storage.problem}</Banner> : null}
      {storage?.kind === "google" && storage.credential_store ? (
        <p className="muted">Your sign-in is kept in {storage.credential_store}. PackSync never writes it to a file.</p>
      ) : null}
      {test ? (
        <Banner tone="ok" title="The connection works">
          {test.provider} · {test.instances_in_cloud} instance{test.instances_in_cloud === 1 ? "" : "s"} in the cloud
          {test.quota ? ` · ${formatBytes(test.quota.used)} used${test.quota.total ? ` of ${formatBytes(test.quota.total)}` : ""}` : ""}
        </Banner>
      ) : null}
      {run.error ? <ErrorNote error={run.error} /> : null}
      <div className="row wrap">
        <Button busy={run.busy} disabled={!storage?.connected} onClick={() => void run.run()}>
          Test the connection
        </Button>
        <Button onClick={() => setChanging((v) => !v)}>{changing ? "Hide" : storage?.kind === "none" ? "Connect…" : "Change or sign in again…"}</Button>
        {storage && storage.kind !== "none" ? (
          <Button variant="ghost" onClick={() => setDisconnect(true)}>
            Disconnect…
          </Button>
        ) : null}
      </div>
      {changing ? <StorageSetup onConnected={() => setChanging(false)} /> : null}
      {disconnect ? (
        <ConfirmDialog
          title="Disconnect from the cloud?"
          confirmLabel="Disconnect"
          danger
          onClose={() => setDisconnect(false)}
          onConfirm={async () => {
            await store.call("storage.disconnect", { revoke });
            await store.refreshStorage();
            await store.refreshSettings();
            store.toast("info", "Disconnected. Your files and cloud data are untouched.");
          }}
        >
          <p>
            PackSync forgets the connection on this computer. <strong>Nothing is deleted</strong> — not your local files and not what is
            stored in the cloud. Snapshots keep being saved here and upload once you reconnect.
          </p>
          {storage?.kind === "google" ? (
            <Checkbox checked={revoke} onChange={setRevoke} hint="Asks Google to cancel the permission you gave PackSync.">
              Also revoke PackSync's access at Google
            </Checkbox>
          ) : null}
        </ConfirmDialog>
      ) : null}
    </Section>
  );
}

function LauncherSection({ settings }: { settings: Settings }) {
  const store = useStore();
  const info = useAppState((s) => s.info);
  const [exe, setExe] = useState(settings.prism_executable ?? "");
  const [dirs, setDirs] = useState(settings.prism_data_dirs.join("\n"));
  const save = useAction(async () => {
    await store.call("settings.update", {
      patch: {
        prism_executable: exe.trim() || null,
        prism_data_dirs: dirs.split("\n").map((d) => d.trim()).filter(Boolean),
      },
    });
    await Promise.all([store.refreshSettings(), store.refreshInfo()]);
    store.toast("success", "Launcher settings saved.");
  });
  return (
    <Section id="s-launcher" title="Prism Launcher">
      <Kv
        rows={[
          ["Found", info?.prism.executable ? <code key="e">{info.prism.executable}</code> : "Not found"],
          ["Data folders", info?.prism.data_dirs.length ? info.prism.data_dirs.map((d) => <div key={d}><code>{d}</code></div>) : "None found"],
        ]}
      />
      <PathField label="Prism executable (only if it was not found)" value={exe} onChange={setExe} placeholder="Leave empty to detect it" browseTitle="Choose the Prism executable folder" />
      <label className="field">
        <span className="field-label">Prism data folders (one per line)</span>
        <textarea className="textarea" value={dirs} onChange={(e) => setDirs(e.target.value)} spellCheck={false} placeholder="Leave empty to detect them" />
      </label>
      {save.error ? <ErrorNote error={save.error} /> : null}
      <div className="row">
        <Button variant="primary" busy={save.busy} onClick={() => void save.run()}>
          Save
        </Button>
      </div>
    </Section>
  );
}

function PlaySection({ settings }: { settings: Settings }) {
  const store = useStore();
  // shown at once, saved in the background; the engine's answer wins if it disagrees
  const [policy, setPolicy] = useState<OfflinePolicy>(settings.hook_offline_policy);
  const [block, setBlock] = useState(settings.hook_block_on_problem);
  const update = useAction(async (patch: Partial<Settings>) => {
    try {
      await store.call("settings.update", { patch });
    } finally {
      await store.refreshSettings();
      const s = store.getState().settings;
      if (s) {
        setPolicy(s.hook_offline_policy);
        setBlock(s.hook_block_on_problem);
      }
    }
  });
  return (
    <Section id="s-play" title="Launcher hooks">
      <fieldset className="col" style={{ border: 0, padding: 0, margin: 0 }}>
        <legend className="field-label" style={{ marginBottom: 6 }}>
          When the cloud can't be reached
        </legend>
        {(
          [
            ["play_local", "Play from this computer", "The game starts with the local copy. Your progress is saved and uploaded later."],
            ["block", "Don't start the game", "Safer if you often play on several computers: you never play an out-of-date copy."],
          ] as [OfflinePolicy, string, string][]
        ).map(([v, t, d]) => (
          <Choice
            key={v}
            name="offline"
            value={v}
            selected={policy === v}
            onSelect={() => {
              setPolicy(v);
              void update.run({ hook_offline_policy: v });
            }}
            title={t}
          >
            {d}
          </Choice>
        ))}
      </fieldset>
      <Checkbox
        checked={block}
        onChange={(v) => {
          setBlock(v);
          void update.run({ hook_block_on_problem: v });
        }}
        hint="Recommended. Otherwise the game starts without syncing that instance."
      >
        Don't start the game while a conflict or crash save is waiting for a decision
      </Checkbox>
      {update.error ? <ErrorNote error={update.error} /> : null}
      <CliPath settings={settings} />
    </Section>
  );
}

/** The command-line tool that launcher hooks call. Found automatically next to the app or on PATH. */
function CliPath({ settings }: { settings: Settings }) {
  const store = useStore();
  const [path, setPath] = useState(settings.cli_path ?? "");
  const save = useAction(async () => {
    await store.call("settings.update", { patch: { cli_path: path.trim() || null } });
    await store.refreshSettings();
    store.toast("success", path.trim() ? "Saved." : "PackSync will look for its command-line tool by itself.");
  });
  return (
    <div className="col">
      <PathField
        label="PackSync command-line tool (used by the hooks)"
        value={path}
        onChange={setPath}
        placeholder="Leave empty to find it next to the app or on PATH"
        hint="The path stored in Prism's settings when you install the hooks."
        browseTitle="Choose the packsync executable"
      />
      {save.error ? <ErrorNote error={save.error} /> : null}
      <div className="row">
        <Button busy={save.busy} disabled={path.trim() === (settings.cli_path ?? "")} onClick={() => void save.run()}>
          Save
        </Button>
      </div>
    </div>
  );
}

function AdvancedSection({ settings }: { settings: Settings }) {
  const store = useStore();
  const [conc, setConc] = useState(String(settings.upload_concurrency));
  const [days, setDays] = useState(String(settings.recovery_point_days));
  const save = useAction(async () => {
    await store.call("settings.update", {
      patch: { upload_concurrency: Number(conc), recovery_point_days: Number(days) },
    });
    await store.refreshSettings();
    store.toast("success", "Saved.");
  });
  return (
    <Section id="s-advanced" title="Advanced">
      <div className="row">
        <TextInput label="Parallel uploads" inputMode="numeric" value={conc} onChange={(e) => setConc(e.target.value)} hint="1–32. More is faster on a fast connection." />
        <TextInput label="Keep recovery points (days)" inputMode="numeric" value={days} onChange={(e) => setDays(e.target.value)} hint="How long files replaced by a restore are kept." />
      </div>
      {save.error ? <ErrorNote error={save.error} /> : null}
      <div className="row">
        <Button variant="primary" busy={save.busy} onClick={() => void save.run()}>
          Save
        </Button>
      </div>
    </Section>
  );
}

function CleanupSection() {
  const [open, setOpen] = useState(false);
  const storage = useAppState((s) => s.storage);
  return (
    <Section id="s-cleanup" title="Cloud cleanup">
      <p className="muted">
        Free space by removing leftovers of interrupted uploads, and optionally releasing old snapshots. Always previewed, never automatic.
      </p>
      <div className="row">
        <Button disabled={!storage?.connected} onClick={() => setOpen(true)}>
          Clean up the cloud…
        </Button>
      </div>
      {open ? <GcDialog onClose={() => setOpen(false)} /> : null}
    </Section>
  );
}

function DiagnosticsSection() {
  const store = useStore();
  const [report, setReport] = useState<Diagnostics | null>(null);
  const run = useAction(async (online: boolean) => {
    setReport(await store.runJob("diagnostics.report", { online }));
  });
  const text = report
    ? [...report.checks.map((c) => `${c.ok ? "OK  " : "FAIL"} ${c.name}: ${c.detail}`), "", ...report.recent_events].join("\n")
    : "";
  return (
    <Section id="s-diag" title="Diagnostics">
      <p className="muted">Checks the installation. The report never contains tokens, secrets or file contents.</p>
      <div className="row">
        <Button busy={run.busy} onClick={() => void run.run(false)}>
          Run checks
        </Button>
        <Button busy={run.busy} onClick={() => void run.run(true)}>
          Run checks, including the cloud
        </Button>
        {report ? (
          <Button
            onClick={() => {
              void navigator.clipboard?.writeText(text).then(() => store.toast("success", "Report copied."));
            }}
          >
            Copy the report
          </Button>
        ) : null}
      </div>
      {run.error ? <ErrorNote error={run.error} /> : null}
      {report ? (
        <table className="table" aria-label="Checks">
          <tbody>
            {report.checks.map((c) => (
              <tr key={c.name}>
                <td style={{ width: 28 }}>{c.ok ? "✓" : "✗"}</td>
                <td>
                  <strong>{c.name}</strong>
                </td>
                <td className="muted">{c.detail}</td>
              </tr>
            ))}
          </tbody>
        </table>
      ) : null}
    </Section>
  );
}

export function SettingsScreen() {
  const settings = useAppState((s) => s.settings);
  const info = useAppState((s) => s.info);
  return (
    <div className="page">
      <div className="page-head">
        <h1>Settings</h1>
      </div>
      <ThisComputer />
      <CloudSection />
      {settings ? <LauncherSection key={`${settings.prism_executable}|${settings.prism_data_dirs.join()}`} settings={settings} /> : null}
      {settings ? <PlaySection settings={settings} /> : null}
      {settings ? <AdvancedSection key={`${settings.upload_concurrency}|${settings.recovery_point_days}`} settings={settings} /> : null}
      <CleanupSection />
      <DiagnosticsSection />
      <Section id="s-about" title="About">
        <Kv
          rows={[
            ["Version", info?.version ?? "—"],
            ["System", info ? `${info.os} · ${info.arch}` : "—"],
            ["Settings folder", info ? <code key="c">{info.paths.config}</code> : "—"],
            ["Data folder", info ? <code key="s">{info.paths.state}</code> : "—"],
            ["Licence", "MIT"],
          ]}
        />
        <p className="muted">
          PackSync is not affiliated with Mojang, Microsoft, Prism Launcher or Google. Your files are stored unencrypted in
          the cloud you choose.
        </p>
      </Section>
    </div>
  );
}

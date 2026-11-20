import { useState } from "react";
import type { StorageStatus } from "../api/types";
import { useAction, useAppState, useStore } from "../state/context";
import { Banner, Button, Choice, TextInput } from "../ui/controls";
import { ErrorNote } from "../ui/ErrorNote";
import { HostedSetup } from "./HostedSetup";
import { JobBar } from "./JobBar";
import { PathField } from "./PathField";

type Mode = "cloud" | "google" | "local" | "r2";
type Space = "visible_folder" | "app_data";

function initialMode(kind: string | undefined): Mode {
  switch (kind) {
    case "google":
      return "google";
    case "local":
      return "local";
    case "r2":
      return "r2";
    default:
      return "cloud";
  }
}

/**
 * Choose where the worlds live: PackSync Cloud (Minecraft sign-in) first, plus clean switcher options for
 * Google Drive, a synced folder on this computer, or Cloudflare R2.
 */
export function StorageSetup({ onConnected }: { onConnected?: (s: StorageStatus) => void }) {
  const store = useStore();
  const storage = useAppState((s) => s.storage);
  const settings = useAppState((s) => s.settings);
  const info = useAppState((s) => s.info);

  const [mode, setMode] = useState<Mode>(() => initialMode(storage?.kind));
  const [space, setSpace] = useState<Space>(storage?.space === "app_data" ? "app_data" : "visible_folder");
  const [clientFile, setClientFile] = useState(settings?.google_client_file ?? "");
  const [needsClient, setNeedsClient] = useState(false);
  const [google, setGoogle] = useState<StorageStatus | null>(null);
  const [jobId, setJobId] = useState<string | null>(null);

  const [localPath, setLocalPath] = useState(
    settings?.storage.kind === "local" ? settings.storage.path : "",
  );

  const existingR2 = settings?.storage.kind === "r2" ? settings.storage : null;
  const [accountId, setAccountId] = useState(existingR2?.account_id ?? "");
  const [bucket, setBucket] = useState(existingR2?.bucket ?? "");
  const [prefix, setPrefix] = useState(existingR2?.prefix ?? "packsync");
  const [accessKeyId, setAccessKeyId] = useState(existingR2?.access_key_id ?? "");
  const [secretAccessKey, setSecretAccessKey] = useState("");

  const signInGoogle = useAction(async () => {
    const status = await store.call("storage.use_google", { space });
    setGoogle(status);
    void store.refreshStorage().catch(() => {});
    if (!status.oauth_client_configured) {
      setNeedsClient(true);
      return;
    }
    setNeedsClient(false);
    const r = await store.runJob("storage.google_login", undefined, { onStart: setJobId });
    await Promise.all([store.refreshStorage(), store.refreshSettings()]);
    store.toast("success", `Connected as ${r.account.email ?? "your Google account"}.`);
    onConnected?.(r.status);
  });

  const saveClient = useAction(async () => {
    const r = await store.call("settings.update", { patch: { google_client_file: clientFile.trim() || null } });
    await store.refreshSettings();
    if (r.storage_problem) throw new Error(r.storage_problem);
    await signInGoogle.run();
  });

  const connectLocal = useAction(async () => {
    const status = await store.call("storage.use_local", { path: localPath.trim() });
    await Promise.all([store.refreshSettings(), store.refreshStorage(), store.refreshInfo()]);
    store.toast("success", "Connected to synced folder.");
    onConnected?.(status);
  });

  const connectR2 = useAction(async () => {
    const status = await store.runJob("storage.use_r2", {
      account_id: accountId.trim(),
      bucket: bucket.trim(),
      prefix: prefix.trim() || "packsync",
      access_key_id: accessKeyId.trim(),
      secret_access_key: secretAccessKey,
    });
    setSecretAccessKey("");
    await Promise.all([store.refreshSettings(), store.refreshStorage(), store.refreshInfo()]);
    store.toast("success", "Connected to Cloudflare R2.");
    onConnected?.(status);
  });

  const connectedCurrent =
    storage?.connected &&
    ((mode === "google" && storage.kind === "google") ||
      (mode === "local" && storage.kind === "local") ||
      (mode === "r2" && storage.kind === "r2"));

  return (
    <div className="col gap-lg">
      <div className="provider-tabs" role="group" aria-label="Storage provider">
        {(
          [
            ["cloud", "PackSync Cloud"],
            ["google", "Google Drive"],
            ["local", "Synced Folder"],
            ["r2", "Cloudflare R2"],
          ] as [Mode, string][]
        ).map(([key, label]) => (
          <button
            key={key}
            type="button"
            className={`provider-tab ${mode === key ? "active" : ""}`}
            aria-pressed={mode === key}
            onClick={() => setMode(key)}
          >
            {label}
          </button>
        ))}
      </div>

      {mode === "cloud" ? <HostedSetup onConnected={onConnected} /> : null}

      {mode === "google" ? (
        <div className="col">
          <fieldset className="col" style={{ border: 0, padding: 0, margin: 0 }}>
            <legend className="field-label" style={{ marginBottom: 6 }}>
              Where in Drive?
            </legend>
            <Choice
              name="space"
              value="visible_folder"
              selected={space === "visible_folder"}
              onSelect={() => setSpace("visible_folder")}
              title="A “PackSync” folder in My Drive"
            >
              You can see it in Drive. PackSync can only see the files it created itself (permission “drive.file”).
            </Choice>
            <Choice
              name="space"
              value="app_data"
              selected={space === "app_data"}
              onSelect={() => setSpace("app_data")}
              title="Hidden app data"
            >
              Not visible in Drive and not browsable. Other apps cannot see it either (permission “drive.appdata”).
            </Choice>
          </fieldset>
          <Banner tone="info" title="Privacy">
            Files are stored <strong>unencrypted</strong> in your Drive: Google, and anyone with access to your account or
            that folder, can read them. Worlds can contain player names and coordinates.
          </Banner>
          {needsClient || (storage?.kind === "google" && storage.oauth_client_configured === false) ? (
            <div className="card col">
              <h3>PackSync needs a Google OAuth client</h3>
              <p className="muted">
                Google requires every app to identify itself, so you create a free one for your own account (about ten
                minutes). In the Google Cloud console: create a project, enable the Google Drive API, set up the consent
                screen with the <code>drive.file</code> scope and publish it (in testing mode Google signs you out every
                7 days), then create an OAuth client of type “Desktop app”. Choose the downloaded file here.
              </p>
              {(google ?? storage)?.problem ? <p className="faint">{(google ?? storage)?.problem}</p> : null}
              <PathField
                kind="file"
                label="Downloaded client file (JSON)"
                value={clientFile}
                onChange={setClientFile}
                placeholder={info ? `${info.paths.config}/google-oauth.json` : "google-oauth.json"}
                hint="Or save it as google-oauth.json in PackSync's settings folder and press “Check again”."
                browseTitle="Choose the OAuth client file"
              />
              <div className="row">
                <Button variant="primary" busy={saveClient.busy} onClick={() => void saveClient.run()}>
                  Save and continue
                </Button>
                <Button busy={signInGoogle.busy} onClick={() => void signInGoogle.run()}>
                  Check again
                </Button>
              </div>
              {saveClient.error ? <ErrorNote error={saveClient.error} /> : null}
            </div>
          ) : null}
          {signInGoogle.busy ? (
            <div className="col">
              <p>Finish signing in in your browser…</p>
              <JobBar jobId={jobId} />
            </div>
          ) : null}
          {signInGoogle.error ? <ErrorNote error={signInGoogle.error} /> : null}
          <div className="row">
            <Button variant="primary" icon="cloud" busy={signInGoogle.busy} onClick={() => void signInGoogle.run()}>
              {storage?.kind === "google" && storage.connected ? "Sign in again" : "Sign in with Google"}
            </Button>
          </div>
        </div>
      ) : null}

      {mode === "local" ? (
        <div className="col">
          <Banner tone="info" title="Unencrypted folder">
            Snapshots are written unencrypted into a folder on this computer — useful with Syncthing, Dropbox, Nextcloud,
            or a network share.
          </Banner>
          <PathField
            kind="folder"
            label="Synced folder"
            value={localPath}
            onChange={setLocalPath}
            placeholder="/path/to/packsync-cloud"
            browseTitle="Choose the synced folder"
          />
          {connectLocal.error ? <ErrorNote error={connectLocal.error} /> : null}
          <div className="row">
            <Button
              variant="primary"
              icon="folder"
              busy={connectLocal.busy}
              disabled={!localPath.trim()}
              onClick={() => void connectLocal.run()}
            >
              Use this folder
            </Button>
          </div>
        </div>
      ) : null}

      {mode === "r2" ? (
        <form
          className="col"
          onSubmit={(e) => {
            e.preventDefault();
            void connectR2.run();
          }}
        >
          <Banner tone="info" title="Cloudflare R2 (S3-compatible)">
            Files are stored unencrypted in your R2 bucket. Your Secret Access Key is saved in the OS credential store
            and never written to settings.
          </Banner>
          <div className="r2-grid">
            <TextInput
              label="Account ID"
              value={accountId}
              onChange={(e) => setAccountId(e.currentTarget.value)}
              placeholder="32-char Cloudflare account ID"
              spellCheck={false}
              autoComplete="off"
            />
            <TextInput
              label="Bucket"
              value={bucket}
              onChange={(e) => setBucket(e.currentTarget.value)}
              placeholder="packsync-worlds"
              spellCheck={false}
              autoComplete="off"
            />
            <TextInput
              label="Prefix"
              value={prefix}
              onChange={(e) => setPrefix(e.currentTarget.value)}
              placeholder="packsync"
              spellCheck={false}
              autoComplete="off"
            />
            <TextInput
              label="Access Key ID"
              value={accessKeyId}
              onChange={(e) => setAccessKeyId(e.currentTarget.value)}
              spellCheck={false}
              autoComplete="off"
            />
          </div>
          <TextInput
            label="Secret Access Key"
            type="password"
            value={secretAccessKey}
            onChange={(e) => setSecretAccessKey(e.currentTarget.value)}
            autoComplete="off"
          />
          {connectR2.error ? <ErrorNote error={connectR2.error} /> : null}
          <div className="row">
            <Button
              type="submit"
              variant="primary"
              icon="cloud"
              busy={connectR2.busy}
              disabled={!accountId.trim() || !bucket.trim() || !accessKeyId.trim() || !secretAccessKey}
            >
              Connect Cloudflare R2
            </Button>
          </div>
        </form>
      ) : null}

      {connectedCurrent && storage ? (
        <Banner tone="ok" title="Connected">
          {storage.detail}
        </Banner>
      ) : null}
    </div>
  );
}

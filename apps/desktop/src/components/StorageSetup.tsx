import { useState } from "react";
import type { StorageStatus } from "../api/types";
import { useAction, useAppState, useStore } from "../state/context";
import { Banner, Button, Choice } from "../ui/controls";
import { ErrorNote } from "../ui/ErrorNote";
import { HostedSetup } from "./HostedSetup";
import { JobBar } from "./JobBar";
import { PathField } from "./PathField";

type Mode = "cloud" | "google";
type Space = "visible_folder" | "app_data";

/**
 * Choose where the worlds live: the PackSync cloud (nothing to set up), or — for people who prefer their
 * own account — Google Drive. A folder, R2 and the rest remain in the command-line tool.
 */
export function StorageSetup({ onConnected }: { onConnected?: (s: StorageStatus) => void }) {
  const store = useStore();
  const storage = useAppState((s) => s.storage);
  const settings = useAppState((s) => s.settings);
  const info = useAppState((s) => s.info);
  const [mode, setMode] = useState<Mode>(storage?.kind === "google" ? "google" : "cloud");
  const [space, setSpace] = useState<Space>(storage?.space === "app_data" ? "app_data" : "visible_folder");
  const [clientFile, setClientFile] = useState(settings?.google_client_file ?? "");
  const [needsClient, setNeedsClient] = useState(false);
  const [google, setGoogle] = useState<StorageStatus | null>(null);
  const [jobId, setJobId] = useState<string | null>(null);

  const signIn = useAction(async () => {
    const status = await store.call("storage.use_google", { space });
    setGoogle(status);
    void store.refreshStorage().catch(() => {});
    if (!status.oauth_client_configured) {
      setNeedsClient(true);
      return;
    }
    setNeedsClient(false);
    const r = await store.runJob("storage.google_login", undefined, { onStart: setJobId });
    await store.refreshStorage();
    store.toast("success", `Connected as ${r.account.email ?? "your Google account"}.`);
    onConnected?.(r.status);
  });
  const saveClient = useAction(async () => {
    const r = await store.call("settings.update", { patch: { google_client_file: clientFile.trim() || null } });
    await store.refreshSettings();
    if (r.storage_problem) throw new Error(r.storage_problem);
    await signIn.run();
  });

  const connected = storage?.connected && mode === "google" && storage.kind === "google";

  return (
    <div className="col gap-lg">

      {mode === "cloud" ? (
        <div className="col gap-lg">
          <HostedSetup onConnected={onConnected} />
          {storage?.kind !== "hosted" ? (
            <p className="faint">
              Prefer your own account?{" "}
              <button type="button" className="link-btn" onClick={() => setMode("google")}>
                Use my own Google Drive instead
              </button>
            </p>
          ) : null}
        </div>
      ) : (
        <div className="col">
          <fieldset className="col" style={{ border: 0, padding: 0, margin: 0 }}>
            <legend className="field-label" style={{ marginBottom: 6 }}>
              Where in Drive?
            </legend>
            <Choice name="space" value="visible_folder" selected={space === "visible_folder"} onSelect={() => setSpace("visible_folder")} title="A “PackSync” folder in My Drive">
              You can see it in Drive. PackSync can only see the files it created itself (permission “drive.file”).
            </Choice>
            <Choice name="space" value="app_data" selected={space === "app_data"} onSelect={() => setSpace("app_data")} title="Hidden app data">
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
                Google requires every app to identify itself. Create a free OAuth client for your own Google account
                (about ten minutes); <code>docs/oauth-setup.md</code> in the PackSync repository walks through it. Then
                choose the downloaded file here.
              </p>
              {(google ?? storage)?.problem ? <p className="faint">{(google ?? storage)?.problem}</p> : null}
              <PathField
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
                <Button busy={signIn.busy} onClick={() => void signIn.run()}>
                  Check again
                </Button>
              </div>
              {saveClient.error ? <ErrorNote error={saveClient.error} /> : null}
            </div>
          ) : null}
          {signIn.busy ? (
            <div className="col">
              <p>Finish signing in in your browser…</p>
              <JobBar jobId={jobId} />
            </div>
          ) : null}
          {signIn.error ? <ErrorNote error={signIn.error} /> : null}
          <div className="row">
            <Button variant="primary" icon="cloud" busy={signIn.busy} onClick={() => void signIn.run()}>
              {storage?.kind === "google" && storage.connected ? "Sign in again" : "Sign in with Google"}
            </Button>
            {storage?.kind !== "google" ? (
              <button type="button" className="link-btn" onClick={() => setMode("cloud")}>
                Use the PackSync cloud instead
              </button>
            ) : null}
          </div>
        </div>
      )}

      {connected && storage ? (
        <Banner tone="ok" title="Connected">
          {storage.detail}
        </Banner>
      ) : null}
    </div>
  );
}

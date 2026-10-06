import { useState } from "react";
import type { StorageStatus } from "../api/types";
import { useAction, useAppState, useStore } from "../state/context";
import { Banner, Button, Choice } from "../ui/controls";
import { ErrorNote } from "../ui/ErrorNote";
import { JobBar } from "./JobBar";
import { PathField } from "./PathField";

type Mode = "google" | "folder";
type Space = "visible_folder" | "app_data";

/** Choose where the worlds live: the user's own Google Drive (recommended) or a plain folder. */
export function StorageSetup({ onConnected }: { onConnected?: (s: StorageStatus) => void }) {
  const store = useStore();
  const storage = useAppState((s) => s.storage);
  const settings = useAppState((s) => s.settings);
  const info = useAppState((s) => s.info);
  const [mode, setMode] = useState<Mode>(storage?.kind === "local" ? "folder" : "google");
  const [space, setSpace] = useState<Space>(storage?.space === "app_data" ? "app_data" : "visible_folder");
  const [folder, setFolder] = useState(settings?.storage.kind === "local" ? settings.storage.path : "");
  const [clientFile, setClientFile] = useState(settings?.google_client_file ?? "");
  const [needsClient, setNeedsClient] = useState(false);
  const [jobId, setJobId] = useState<string | null>(null);

  const signIn = useAction(async () => {
    const status = await store.call("storage.use_google", { space });
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
  const useFolder = useAction(async () => {
    const status = await store.call("storage.use_local", { path: folder.trim() });
    await store.refreshSettings();
    await store.refreshStorage();
    store.toast("success", "Using that folder as the cloud.");
    onConnected?.(status);
  });

  const connected = storage?.connected && (mode === "google" ? storage.kind === "google" : storage.kind === "local");

  return (
    <div className="col gap-lg">
      <fieldset className="col" style={{ border: 0, padding: 0, margin: 0 }}>
        <legend className="sr-only">Where should your worlds live?</legend>
        <Choice name="storage-mode" value="google" selected={mode === "google"} onSelect={() => setMode("google")} title="Google Drive (recommended)">
          Your worlds are stored in <strong>your own</strong> Drive. PackSync has no server and never sees your account.
        </Choice>
        <Choice name="storage-mode" value="folder" selected={mode === "folder"} onSelect={() => setMode("folder")} title="A folder on this computer or network">
          Useful for a NAS, a folder another sync tool mirrors, or for trying PackSync out.
        </Choice>
      </fieldset>

      {mode === "google" ? (
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
          {needsClient || storage?.oauth_client_configured === false ? (
            <div className="card col">
              <h3>PackSync needs a Google OAuth client</h3>
              <p className="muted">
                {storage?.setup_hint ??
                  "Google requires every app to identify itself. Create a free OAuth client for your own Google account (about five minutes) — docs/oauth-setup.md in the PackSync repository walks through it."}
              </p>
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
          </div>
        </div>
      ) : (
        <div className="col">
          <PathField label="Folder" value={folder} onChange={setFolder} placeholder="/mnt/nas/packsync" hint="It is created if it does not exist. Use a folder only you can write to." browseTitle="Choose the storage folder" />
          {useFolder.error ? <ErrorNote error={useFolder.error} /> : null}
          <div className="row">
            <Button variant="primary" busy={useFolder.busy} disabled={!folder.trim()} onClick={() => void useFolder.run()}>
              Use this folder
            </Button>
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

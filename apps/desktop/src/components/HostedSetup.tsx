import { useState } from "react";
import type { HostedLoginPrompt, StorageStatus } from "../api/types";
import { openExternalUrl } from "../lib/external";
import { useAction, useAppState, useStore } from "../state/context";
import { ConfirmDialog } from "../ui/ConfirmDialog";
import { Banner, Button, Spinner } from "../ui/controls";
import { ErrorNote } from "../ui/ErrorNote";
import { Icon } from "../ui/Icon";

/**
 * PackSync Cloud: Prism-style Microsoft/Minecraft sign-in. Signing in with the same Minecraft account
 * automatically restores cloud access and encryption keys across all your computers.
 */
export function HostedSetup({ onConnected }: { onConnected?: (s: StorageStatus) => void }) {
  const store = useStore();
  const storage = useAppState((s) => s.storage);
  const settings = useAppState((s) => s.settings);
  const info = useAppState((s) => s.info);

  const [prompt, setPrompt] = useState<HostedLoginPrompt | null>(null);
  const [waitJobId, setWaitJobId] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);

  const finish = async (status: StorageStatus, message: string) => {
    await Promise.all([store.refreshInfo(), store.refreshSettings(), store.refreshStorage()]);
    store.toast("success", message);
    onConnected?.(status);
  };

  const signIn = useAction(async () => {
    setCopied(false);
    const loginPrompt = await store.call("storage.hosted_login_start", { open_browser: false });
    setPrompt(loginPrompt);
    try {
      const status = await store.runJob("storage.hosted_login_wait", undefined, { onStart: setWaitJobId });
      setPrompt(null);
      setWaitJobId(null);
      await finish(status, "Connected to PackSync Cloud.");
    } catch (e) {
      setPrompt(null);
      setWaitJobId(null);
      throw e;
    }
  });

  const cancelSignIn = async () => {
    const id = waitJobId;
    setPrompt(null);
    setWaitJobId(null);
    if (id) await store.cancelJob(id).catch(() => {});
  };

  const copyCode = async () => {
    if (!prompt) return;
    try {
      await navigator.clipboard?.writeText(prompt.user_code);
      setCopied(true);
      store.toast("info", "Sign-in code copied.");
    } catch {
      store.toast("warn", "Could not access the clipboard; copy the code above manually.");
    }
  };

  const deleteAccount = async () => {
    await store.runJob("storage.hosted_delete_account", undefined);
    await Promise.all([
      store.refreshInfo(),
      store.refreshSettings(),
      store.refreshStorage(),
      store.refreshInstances(),
      store.refreshDiscovered(),
    ]);
    store.toast("info", "Your PackSync Cloud account and cloud data were deleted.");
  };

  if (storage?.kind === "hosted" && storage.connected) {
    const hostedDisplay = settings?.storage.kind === "hosted" ? settings.storage.display_name : null;
    const accountName = info?.minecraft_profile?.name ?? hostedDisplay ?? storage.account;

    return (
      <div className="col gap-lg">
        <Banner tone="ok" title="Using PackSync Cloud">
          <div className="col" style={{ gap: 4 }}>
            <span>
              {accountName ? (
                <>
                  Signed in as <strong>{accountName}</strong>.{" "}
                </>
              ) : null}
              {storage.detail}.
            </span>
            <span className="muted">
              Everything is encrypted on this computer before upload; the PackSync service cannot read your worlds.
            </span>
          </div>
        </Banner>

        <div className="card col">
          <div className="row" style={{ gap: 8 }}>
            <Icon name="computer" size={18} />
            <h3>Automatic multi-computer sync</h3>
          </div>
          <p className="muted">
            Install PackSync on any other computer and choose <strong>Sign in with Minecraft</strong> using the same
            Microsoft account. Your cloud vault and encryption key sync automatically — no pairing codes or recovery keys
            to type.
          </p>
        </div>

        <div className="card col">
          <h3>Account &amp; cloud data</h3>
          <p className="muted">
            Permanently remove your PackSync Cloud account and delete all encrypted snapshots stored on the server.
            Instances and worlds saved on this computer stay untouched.
          </p>
          <div className="row">
            <Button variant="danger" icon="trash" onClick={() => setConfirmDelete(true)}>
              Delete cloud account &amp; data…
            </Button>
          </div>
        </div>

        {confirmDelete ? (
          <ConfirmDialog
            title="Delete PackSync Cloud account and data?"
            confirmLabel="Delete cloud account & data"
            danger
            onClose={() => setConfirmDelete(false)}
            onConfirm={deleteAccount}
          >
            <p>
              This permanently deletes your PackSync Cloud account and every snapshot, pack, and session stored in the
              cloud. <strong>This cannot be undone.</strong>
            </p>
            <p className="muted">Local Prism Launcher instances and worlds on this computer are not deleted.</p>
          </ConfirmDialog>
        ) : null}
      </div>
    );
  }

  return (
    <div className="col gap-lg">
      <div className="minecraft-signin-card">
        <div className="row" style={{ gap: 10 }}>
          <Icon name="shield" size={18} />
          <strong>End-to-end encrypted PackSync Cloud</strong>
        </div>
        <p className="muted">
          Sign in with your Microsoft / Minecraft account, just like Prism Launcher. Signing in with the same account on
          any computer unlocks the same private cloud automatically.
        </p>

        {!prompt ? (
          <div className="row wrap">
            <Button variant="primary" icon="user" busy={signIn.busy} onClick={() => void signIn.run()}>
              Sign in with Minecraft
            </Button>
          </div>
        ) : (
          <div className="col" aria-live="polite" style={{ gap: 12 }}>
            <p className="muted">Enter this code in your browser at Microsoft to finish signing in:</p>
            <p className="pairing-code" data-testid="device-code">
              {prompt.user_code}
            </p>
            <div className="row wrap">
              <Button icon="copy" onClick={() => void copyCode()}>
                {copied ? "Copied" : "Copy code"}
              </Button>
              <Button variant="primary" icon="external" onClick={() => void openExternalUrl(store, prompt.verification_uri)}>
                Open microsoft.com/link
              </Button>
              <Button onClick={() => void cancelSignIn()}>Cancel</Button>
              <button
                type="button"
                className="link-btn"
                style={{ marginLeft: "auto" }}
                onClick={() => void openExternalUrl(store, prompt.privacy_url)}
              >
                Privacy Policy &amp; Terms
              </button>
            </div>
            <div className="row faint" style={{ gap: 8, fontSize: 12.5 }}>
              <Spinner />
              <span>
                Waiting for Microsoft sign-in to complete… (expires in {Math.max(1, Math.round(prompt.expires_in / 60))}{" "}
                min)
              </span>
            </div>
          </div>
        )}
      </div>

      {signIn.error ? <ErrorNote error={signIn.error} /> : null}
    </div>
  );
}

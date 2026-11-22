import { sentence } from "../lib/format";
import { navigate } from "../lib/router";
import { useStore } from "../state/context";
import { Banner, Button, Pill } from "../ui/controls";
import { Dialog } from "../ui/Dialog";

export function UploadTooLargeDialog({
  instanceName,
  detail,
  onClose,
  onChooseProvider,
  onCancelUpload,
}: {
  instanceName?: string;
  detail?: string | null;
  onClose: () => void;
  onChooseProvider?: (provider: "google" | "r2") => void;
  onCancelUpload?: () => void;
}) {
  const store = useStore();

  const choose = (provider: "google" | "r2") => {
    if (onChooseProvider) {
      onChooseProvider(provider);
      return;
    }
    onClose();
    navigate({ name: "settings", provider });
  };

  const cancel = () => {
    if (onCancelUpload) {
      onCancelUpload();
      return;
    }
    store.toast("info", "Upload cancelled. Your local world is safe on this computer.", instanceName);
    onClose();
  };

  return (
    <Dialog
      title="World couldn't be uploaded to our servers"
      onClose={cancel}
      wide
      footer={
        <Button onClick={cancel} data-autofocus>
          Cancel upload
        </Button>
      }
    >
      <Banner tone="warn" title="Upload cancelled">
        <span>
          {instanceName ? `${instanceName} is` : "This world is"} too large for PackSync Cloud's 10 GB server limit, so
          the upload was cancelled instead of stalling. Your local world and files are untouched.
        </span>
        {detail ? <span className="muted"> {sentence(detail)}</span> : null}
      </Banner>

      <div className="col" style={{ gap: 10 }}>
        <h3>Other ways to upload this world</h3>
        <div className="card col" style={{ gap: 8 }}>
          <div className="row wrap" style={{ justifyContent: "space-between", alignItems: "center" }}>
            <strong>Set up your own Google Drive</strong>
            <Pill tone="warn">Medium difficulty</Pill>
          </div>
          <p className="muted">
            Upload directly to your own Google Drive (15 GB free, or larger with Google One). Requires creating a free
            Google OAuth desktop app (~10 minutes).
          </p>
          <div className="row">
            <Button variant="primary" icon="cloud" onClick={() => choose("google")}>
              Set up Google Drive
            </Button>
          </div>
        </div>

        <div className="card col" style={{ gap: 8 }}>
          <div className="row wrap" style={{ justifyContent: "space-between", alignItems: "center" }}>
            <strong>Set up your own Cloudflare R2</strong>
            <Pill tone="warn">Medium difficulty</Pill>
          </div>
          <p className="muted">
            Upload directly to your own Cloudflare R2 bucket (10 GB/month free, very low pay-as-you-go rates, and no
            PackSync server limit). Requires creating a bucket and API token in Cloudflare.
          </p>
          <div className="row">
            <Button variant="primary" icon="cloud" onClick={() => choose("r2")}>
              Set up Cloudflare R2
            </Button>
          </div>
        </div>

        <div className="card col" style={{ gap: 8 }}>
          <strong>Cancel the upload</strong>
          <p className="muted">
            Keep your world safely saved on this computer for now without uploading it to the cloud.
          </p>
          <div className="row">
            <Button onClick={cancel}>Cancel upload</Button>
          </div>
        </div>
      </div>
    </Dialog>
  );
}

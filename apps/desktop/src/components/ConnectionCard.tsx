import { useAppState } from "../state/context";
import { StatusDot } from "../ui/controls";
import { Icon } from "../ui/Icon";

function providerLabel(kind: string | undefined): string {
  switch (kind) {
    case "hosted":
      return "PackSync cloud";
    case "google":
      return "Google Drive";
    case "r2":
      return "Cloudflare R2";
    case "local":
      return "Folder";
    default:
      return "No cloud";
  }
}

export function ConnectionCard({ compact = false }: { compact?: boolean }) {
  const storage = useAppState((s) => s.storage);
  const settings = useAppState((s) => s.settings);
  const info = useAppState((s) => s.info);

  const tone = storage?.connected ? "ok" : storage?.needs_reconnect || storage?.problem ? "danger" : "neutral";
  const provider = providerLabel(storage?.kind);
  const hostedDisplay = settings?.storage.kind === "hosted" ? settings.storage.display_name : null;
  const mcName = info?.minecraft_profile?.name ?? null;
  const accountLabel = storage?.connected
    ? storage.kind === "hosted"
      ? (mcName ?? hostedDisplay ?? storage.account ?? "End-to-end encrypted")
      : (storage.account ?? mcName ?? storage.detail)
    : storage?.needs_reconnect
      ? "Sign in again"
      : "Not connected";
  const deviceName = info?.device.name ?? "This computer";
  const initialsSource = (mcName || hostedDisplay || storage?.account || deviceName || "PS").trim();
  const initials = initialsSource.slice(0, 2).toUpperCase();

  if (compact) {
    return (
      <div className="conn-card compact" aria-label="Connection">
        <span className="conn-avatar" aria-hidden="true">
          {initials}
        </span>
        <div className="conn-meta">
          <StatusDot tone={tone} />
          <strong className="truncate">{provider}</strong>
          <span className="muted truncate" title={storage?.detail}>
            {accountLabel}
          </span>
          <span className="faint truncate">
            <Icon name="computer" size={13} /> {deviceName}
          </span>
        </div>
      </div>
    );
  }

  return (
    <section className="card conn-card" aria-label="Connection">
      <div className="row" style={{ gap: 12 }}>
        <span className="conn-avatar" aria-hidden="true">
          {initials}
        </span>
        <div className="grow col" style={{ gap: 2 }}>
          <div className="row" style={{ gap: 8 }}>
            <StatusDot tone={tone} />
            <strong className="truncate">{provider}</strong>
          </div>
          <div className="muted truncate" title={storage?.detail}>
            {accountLabel}
          </div>
        </div>
      </div>
      <div className="row faint" style={{ gap: 6, fontSize: 12.5 }}>
        <Icon name="computer" size={13} />
        <span className="truncate">{deviceName}</span>
        {mcName && storage?.kind !== "hosted" ? (
          <span className="tag" style={{ marginLeft: "auto" }}>
            {mcName}
          </span>
        ) : null}
      </div>
    </section>
  );
}

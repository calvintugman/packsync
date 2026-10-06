import { useEffect, useRef } from "react";
import type { ReactNode } from "react";
import { BRAND } from "./brand";
import { needsAttention } from "./lib/phases";
import { navigate, parseRoute, useRouteHash } from "./lib/router";
import type { Route } from "./lib/router";
import { ActivityScreen } from "./screens/Activity";
import { CloudScreen } from "./screens/Cloud";
import { InstanceDetailScreen } from "./screens/InstanceDetail";
import { InstancesScreen } from "./screens/Instances";
import { SettingsScreen } from "./screens/Settings";
import { SetupScreen } from "./screens/Setup";
import { useAppState, useStore } from "./state/context";
import { Banner, Button, Spinner, StatusDot } from "./ui/controls";
import { ErrorNote } from "./ui/ErrorNote";
import { Icon } from "./ui/Icon";
import type { IconName } from "./ui/Icon";
import { Toasts } from "./ui/Toasts";

const NAV: { name: Exclude<Route["name"], "instance" | "setup">; href: string; label: string; icon: IconName }[] = [
  { name: "instances", href: "#/", label: "Instances", icon: "layers" },
  { name: "cloud", href: "#/cloud", label: "Cloud", icon: "cloud" },
  { name: "activity", href: "#/activity", label: "Activity", icon: "activity" },
  { name: "settings", href: "#/settings", label: "Settings", icon: "sliders" },
];

function Sidebar({ route }: { route: Route }) {
  const instances = useAppState((s) => s.instances);
  const jobs = useAppState((s) => s.jobs);
  const storage = useAppState((s) => s.storage);
  const info = useAppState((s) => s.info);
  const attention = instances.filter(needsAttention).length;
  const running = Object.values(jobs).filter((j) => j.state === "running").length;
  const active = route.name === "instance" ? "instances" : route.name;
  const tone = storage?.connected ? "ok" : storage?.needs_reconnect || storage?.problem ? "danger" : "neutral";
  return (
    <nav className="sidebar" aria-label="Main">
      <div className="brand">
        <span className="brand-mark" aria-hidden="true">
          <Icon name="box" size={17} />
        </span>
        {BRAND.name}
      </div>
      {NAV.map((n) => (
        <a key={n.name} className="nav-link" href={n.href} aria-current={active === n.name ? "page" : undefined}>
          <Icon name={n.icon} />
          {n.label}
          {n.name === "instances" && attention > 0 ? (
            <span className="nav-badge" aria-label={`${attention} need attention`}>
              {attention}
            </span>
          ) : null}
          {n.name === "activity" && running > 0 ? (
            <span className="nav-badge info" aria-label={`${running} running`}>
              {running}
            </span>
          ) : null}
        </a>
      ))}
      <div className="sidebar-foot" aria-label="Connection">
        <div className="row" style={{ gap: 8 }}>
          <StatusDot tone={tone} />
          <strong className="truncate">{storage?.kind === "google" ? "Google Drive" : storage?.kind === "local" ? "Folder" : "No cloud"}</strong>
        </div>
        <div className="muted truncate" title={storage?.detail}>
          {storage?.connected ? (storage.account ?? storage.detail) : storage?.needs_reconnect ? "Sign in again" : "Not connected"}
        </div>
        <div className="faint truncate" style={{ marginTop: 6 }}>
          <Icon name="computer" size={13} /> {info?.device.name ?? "This computer"}
        </div>
      </div>
    </nav>
  );
}

function Screen({ route }: { route: Route }): ReactNode {
  switch (route.name) {
    case "instances":
      return <InstancesScreen />;
    case "instance":
      return <InstanceDetailScreen id={route.id} tab={route.tab} />;
    case "cloud":
      return <CloudScreen />;
    case "activity":
      return <ActivityScreen />;
    case "settings":
      return <SettingsScreen />;
    case "setup":
      return <SetupScreen />;
  }
}

export function App() {
  const store = useStore();
  const status = useAppState((s) => s.status);
  const loadError = useAppState((s) => s.loadError);
  const connection = useAppState((s) => s.connection);
  const storageKind = useAppState((s) => s.storage?.kind);
  const instanceCount = useAppState((s) => s.instances.length);
  const hash = useRouteHash();
  const route = parseRoute(hash);
  const redirected = useRef(false);

  // First run: nothing is connected and nothing synced yet, so start with the setup.
  useEffect(() => {
    if (status !== "ready" || redirected.current) return;
    redirected.current = true;
    if (storageKind === "none" && instanceCount === 0 && route.name === "instances") navigate({ name: "setup" });
  }, [status, storageKind, instanceCount, route.name]);

  useEffect(() => {
    document.title = BRAND.name;
  }, []);

  if (status === "loading") {
    return (
      <div className="empty" style={{ height: "100%", justifyContent: "center" }} role="status">
        <Spinner />
        <p>Starting {BRAND.name}…</p>
      </div>
    );
  }
  if (status === "failed") {
    return (
      <div className="wizard">
        <h1>{BRAND.name} can't start</h1>
        <ErrorNote error={loadError} />
        <div className="row">
          <Button variant="primary" onClick={() => window.location.reload()}>
            Try again
          </Button>
        </div>
      </div>
    );
  }
  if (route.name === "setup") {
    return (
      <>
        <div className="main" style={{ height: "100%" }}>
          <SetupScreen />
        </div>
        <Toasts />
      </>
    );
  }
  return (
    <div className="app">
      <Sidebar route={route} />
      <main className="main" id="main">
        {connection === "lost" ? (
          <div style={{ padding: "12px 28px 0" }}>
            <Banner tone="warn" title="Lost contact with PackSync">
              Retrying… What you see may be out of date.
            </Banner>
          </div>
        ) : null}
        <Screen route={route} />
      </main>
      <Toasts />
      {/* keep the store reference alive for devtools and tests */}
      <span hidden data-store={store.transport.kind} />
    </div>
  );
}

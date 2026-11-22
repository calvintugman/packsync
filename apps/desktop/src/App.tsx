import { useEffect, useRef } from "react";
import type { ReactNode } from "react";
import { BRAND } from "./brand";
import { needsAttention } from "./lib/phases";
import { navigate, parseRoute, useRouteHash } from "./lib/router";
import type { Route } from "./lib/router";
import { AboutScreen } from "./screens/About";
import { ActivityScreen } from "./screens/Activity";
import { CloudScreen } from "./screens/Cloud";
import { InstanceDetailScreen } from "./screens/InstanceDetail";
import { InstancesScreen } from "./screens/Instances";
import { SettingsScreen } from "./screens/Settings";
import { SetupScreen } from "./screens/Setup";
import { ConnectionCard } from "./components/ConnectionCard";
import { useAppState, useStore } from "./state/context";
import { Banner, Button, Spinner } from "./ui/controls";
import { ErrorNote } from "./ui/ErrorNote";
import { Icon } from "./ui/Icon";
import type { IconName } from "./ui/Icon";
import { Toasts } from "./ui/Toasts";

type NavRouteName = Exclude<Route["name"], "instance" | "setup">;

const PRIMARY_NAV: { name: NavRouteName; href: string; label: string; icon: IconName }[] = [
  { name: "instances", href: "#/", label: "Instances", icon: "grid" },
  { name: "activity", href: "#/activity", label: "Activity", icon: "activity" },
  { name: "cloud", href: "#/cloud", label: "Cloud", icon: "cloud" },
  { name: "about", href: "#/about", label: "About", icon: "sparkles" },
];

const SETTINGS_NAV: { name: NavRouteName; href: string; label: string; icon: IconName } = {
  name: "settings",
  href: "#/settings",
  label: "Settings",
  icon: "sliders",
};

function Sidebar({ route }: { route: Route }) {
  const instances = useAppState((s) => s.instances);
  const jobs = useAppState((s) => s.jobs);
  const attention = instances.filter(needsAttention).length;
  const running = Object.values(jobs).filter((j) => j.state === "running").length;
  const active = route.name === "instance" ? "instances" : route.name;

  const renderLink = (n: { name: NavRouteName; href: string; label: string; icon: IconName }) => (
    <a
      key={n.name}
      className="nav-link"
      href={n.href}
      title={n.label}
      aria-current={active === n.name ? "page" : undefined}
    >
      <Icon name={n.icon} size={20} />
      <span className="sr-only">{n.label}</span>
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
  );

  return (
    <nav className="sidebar" aria-label="Main">
      <div className="brand">
        <a className="brand-link" href="#/" title={BRAND.name}>
          <span className="brand-mark" aria-hidden="true">
            <Icon name="box" size={18} />
          </span>
          <span className="sr-only">{BRAND.name}</span>
        </a>
      </div>
      <div className="nav-group">{PRIMARY_NAV.map(renderLink)}</div>
      <div className="nav-footer">{renderLink(SETTINGS_NAV)}</div>
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
    case "about":
      return <AboutScreen />;
    case "settings":
      return <SettingsScreen provider={route.provider} />;
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
        {route.name !== "instances" ? (
          <div className={`topbar ${route.name === "about" ? "floating" : ""}`}>
            <ConnectionCard compact />
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

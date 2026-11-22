import { useSyncExternalStore } from "react";

export type StorageProviderTab = "cloud" | "google" | "local" | "r2";

export type Route =
  | { name: "instances" }
  | { name: "instance"; id: string; tab: string | null }
  | { name: "cloud" }
  | { name: "activity" }
  | { name: "about" }
  | { name: "settings"; provider?: StorageProviderTab }
  | { name: "setup" };

function parseProvider(raw: string | undefined): StorageProviderTab | undefined {
  return raw === "cloud" || raw === "google" || raw === "local" || raw === "r2" ? raw : undefined;
}

/** Hash routes work with any static file server and with the Tauri asset protocol. */
export function parseRoute(hash: string): Route {
  const clean = hash.replace(/^#\/?/, "").replace(/(^|&)token=[0-9a-fA-F]+/, "");
  const [first = "", second, third] = clean.split("/").filter(Boolean);
  switch (first) {
    case "instance":
      return second ? { name: "instance", id: decodeURIComponent(second), tab: third ?? null } : { name: "instances" };
    case "cloud":
      return { name: "cloud" };
    case "activity":
      return { name: "activity" };
    case "about":
      return { name: "about" };
    case "settings": {
      const provider = parseProvider(second);
      return provider ? { name: "settings", provider } : { name: "settings" };
    }
    case "setup":
      return { name: "setup" };
    default:
      return { name: "instances" };
  }
}

export function href(r: Route): string {
  switch (r.name) {
    case "instances":
      return "#/";
    case "instance":
      return `#/instance/${encodeURIComponent(r.id)}${r.tab ? `/${r.tab}` : ""}`;
    case "settings":
      return r.provider ? `#/settings/${r.provider}` : "#/settings";
    default:
      return `#/${r.name}`;
  }
}

export function navigate(r: Route): void {
  window.location.hash = href(r);
}

function subscribe(cb: () => void): () => void {
  window.addEventListener("hashchange", cb);
  return () => window.removeEventListener("hashchange", cb);
}

export function useRouteHash(): string {
  return useSyncExternalStore(subscribe, () => window.location.hash);
}

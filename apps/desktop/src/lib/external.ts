import type { AppStore } from "../state/store";

/** Open an external URL through the native opener, falling back to `window.open`. */
export async function openExternalUrl(store: AppStore, url: string): Promise<boolean> {
  try {
    const res = await store.call("app.open_url", { url });
    if (res.opened) return true;
  } catch {
    /* fall back to the browser window opener when the RPC handler is absent */
  }
  if (typeof window !== "undefined" && typeof window.open === "function") {
    window.open(url, "_blank", "noopener,noreferrer");
    return true;
  }
  return false;
}

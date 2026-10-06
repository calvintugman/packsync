import { BRAND } from "./brand";

/** Shown when the UI is opened in a plain browser without a way to reach the application layer. */
export function NoBridge() {
  return (
    <div className="wizard">
      <h1>{BRAND.name}</h1>
      <p>
        This page is the {BRAND.name} desktop interface. It needs the desktop app, or the development bridge: run{" "}
        <code>packsync dev-bridge --ui-dir apps/desktop/dist</code> and open the address it prints (it ends in{" "}
        <code>#token=…</code>).
      </p>
    </div>
  );
}

import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { baseTransport, settings, storage } from "../test/fixtures";
import { renderWithStore } from "../test/render";
import { StorageSetup } from "./StorageSetup";

/** Nothing configured yet: a fresh installation. */
function fresh() {
  const t = baseTransport({ storage: storage({ kind: "none", connected: false, detail: "Not connected" }) });
  t.on("settings.get", () => settings({ storage: { kind: "none" } }));
  return t;
}

describe("StorageSetup", () => {
  it("offers the PackSync cloud first; Google Drive is upfront that files are not encrypted", async () => {
    const u = userEvent.setup();
    const t = fresh();
    await renderWithStore(<StorageSetup />, t);
    expect(screen.getByRole("radio", { name: /PackSync cloud/ })).toBeChecked();
    expect(screen.getByRole("button", { name: "Start on this computer" })).toBeInTheDocument();
    await u.click(screen.getByRole("radio", { name: /Google Drive/ }));
    expect(screen.getByText(/unencrypted/)).toBeInTheDocument();
    expect(screen.getByRole("radio", { name: /A “PackSync” folder in My Drive/ })).toBeChecked();
  });

  it("explains the missing OAuth client instead of failing", async () => {
    const u = userEvent.setup();
    const t = fresh();
    t.on("storage.use_google", () => storage({ kind: "google", connected: false, oauth_client_configured: false, setup_hint: "Create a client: docs/oauth-setup.md" }));
    await renderWithStore(<StorageSetup />, t);
    await u.click(screen.getByRole("radio", { name: /Google Drive/ }));
    await u.click(screen.getByRole("button", { name: "Sign in with Google" }));
    expect(await screen.findByRole("heading", { name: "PackSync needs a Google OAuth client" })).toBeInTheDocument();
    expect(t.count("storage.google_login")).toBe(0); // no browser, no network
  });

  it("selects the hidden app-data space when asked", async () => {
    const u = userEvent.setup();
    const t = fresh();
    t.on("storage.use_google", () => storage({ kind: "google", connected: false, oauth_client_configured: false }));
    await renderWithStore(<StorageSetup />, t);
    await u.click(screen.getByRole("radio", { name: /Google Drive/ }));
    await u.click(screen.getByRole("radio", { name: /Hidden app data/ }));
    await u.click(screen.getByRole("button", { name: "Sign in with Google" }));
    await waitFor(() => expect(t.count("storage.use_google")).toBe(1));
    expect(t.calls.find((c) => c.method === "storage.use_google")?.params).toEqual({ space: "app_data" });
  });

  it("signs in through the browser and reports who connected", async () => {
    const u = userEvent.setup();
    const t = fresh();
    const connected = storage({ kind: "google", connected: true, oauth_client_configured: true, account: "me@example.com", detail: "me@example.com" });
    t.on("storage.use_google", () => ({ ...connected, connected: false }));
    t.on("storage.google_login", () => ({ account: { email: "me@example.com", display_name: null, permission_id: null }, status: connected }));
    const onConnected = vi.fn();
    const { store } = await renderWithStore(<StorageSetup onConnected={onConnected} />, t);
    await u.click(screen.getByRole("radio", { name: /Google Drive/ }));
    await u.click(screen.getByRole("button", { name: "Sign in with Google" }));
    await waitFor(() => expect(onConnected).toHaveBeenCalledWith(connected));
    expect(store.getState().toasts.some((x) => x.message === "Connected as me@example.com.")).toBe(true);
  });

  it("uses a folder as the cloud", async () => {
    const u = userEvent.setup();
    const t = fresh();
    t.on("storage.use_local", () => storage());
    const onConnected = vi.fn();
    await renderWithStore(<StorageSetup onConnected={onConnected} />, t);
    await u.click(screen.getByRole("radio", { name: /A folder on this computer/ }));
    const button = screen.getByRole("button", { name: "Use this folder" });
    expect(button).toBeDisabled();
    await u.type(screen.getByLabelText("Folder", { exact: true }), "/mnt/nas/packsync");
    await u.click(button);
    await waitFor(() => expect(onConnected).toHaveBeenCalled());
    expect(t.calls.find((c) => c.method === "storage.use_local")?.params).toEqual({ path: "/mnt/nas/packsync" });
  });

  it("shows the engine's error when the folder cannot be used", async () => {
    const u = userEvent.setup();
    const t = fresh();
    t.on("storage.use_local", () => {
      throw Object.assign(new Error("x"), { code: "io", message: "cannot create /nope: permission denied", advice: null, exit_code: 1 });
    });
    await renderWithStore(<StorageSetup />, t);
    await u.click(screen.getByRole("radio", { name: /A folder on this computer/ }));
    await u.type(screen.getByLabelText("Folder", { exact: true }), "/nope");
    await u.click(screen.getByRole("button", { name: "Use this folder" }));
    expect(await screen.findByText(/permission denied/)).toBeInTheDocument();
  });
});

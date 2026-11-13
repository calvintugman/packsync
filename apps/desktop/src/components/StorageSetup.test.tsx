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
  it("goes back from Google Drive to the PackSync cloud", async () => {
    const u = userEvent.setup();
    await renderWithStore(<StorageSetup />, fresh());
    await u.click(screen.getByRole("button", { name: "Use my own Google Drive instead" }));
    await u.click(screen.getByRole("button", { name: "Use the PackSync cloud instead" }));
    expect(screen.getByRole("button", { name: "Start on this computer" })).toBeInTheDocument();
  });

  it("offers the PackSync cloud first and Google Drive only on request, upfront that it is not encrypted", async () => {
    const u = userEvent.setup();
    const t = fresh();
    await renderWithStore(<StorageSetup />, t);
    expect(screen.getByRole("button", { name: "Start on this computer" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Sign in with Google" })).not.toBeInTheDocument();
    expect(screen.queryByText(/folder on this computer/i)).not.toBeInTheDocument();
    await u.click(screen.getByRole("button", { name: "Use my own Google Drive instead" }));
    expect(screen.getByText(/unencrypted/)).toBeInTheDocument();
    expect(screen.getByRole("radio", { name: /A “PackSync” folder in My Drive/ })).toBeChecked();
  });

  it("explains the missing OAuth client instead of failing", async () => {
    const u = userEvent.setup();
    const t = fresh();
    t.on("storage.use_google", () => storage({ kind: "google", connected: false, oauth_client_configured: false, setup_hint: "Create a client" }));
    await renderWithStore(<StorageSetup />, t);
    await u.click(screen.getByRole("button", { name: "Use my own Google Drive instead" }));
    await u.click(screen.getByRole("button", { name: "Sign in with Google" }));
    expect(await screen.findByRole("heading", { name: "PackSync needs a Google OAuth client" })).toBeInTheDocument();
    expect(t.count("storage.google_login")).toBe(0); // no browser, no network
  });

  it("selects the hidden app-data space when asked", async () => {
    const u = userEvent.setup();
    const t = fresh();
    t.on("storage.use_google", () => storage({ kind: "google", connected: false, oauth_client_configured: false }));
    await renderWithStore(<StorageSetup />, t);
    await u.click(screen.getByRole("button", { name: "Use my own Google Drive instead" }));
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
    await u.click(screen.getByRole("button", { name: "Use my own Google Drive instead" }));
    await u.click(screen.getByRole("button", { name: "Sign in with Google" }));
    await waitFor(() => expect(onConnected).toHaveBeenCalledWith(connected));
    expect(store.getState().toasts.some((x) => x.message === "Connected as me@example.com.")).toBe(true);
  });
});

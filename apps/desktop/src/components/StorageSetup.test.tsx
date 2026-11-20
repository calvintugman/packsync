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
  it("offers PackSync Cloud (Minecraft sign-in) first and switches cleanly between all providers", async () => {
    const u = userEvent.setup();
    await renderWithStore(<StorageSetup />, fresh());

    expect(screen.getByRole("button", { name: "Sign in with Minecraft" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Sign in with Google" })).not.toBeInTheDocument();
    expect(screen.queryByLabelText("Synced folder")).not.toBeInTheDocument();
    expect(screen.queryByLabelText("Account ID")).not.toBeInTheDocument();

    await u.click(screen.getByRole("button", { name: "Google Drive" }));
    expect(screen.getByText(/unencrypted/)).toBeInTheDocument();
    expect(screen.getByRole("radio", { name: /A “PackSync” folder in My Drive/ })).toBeChecked();

    await u.click(screen.getByRole("button", { name: "Synced Folder" }));
    expect(screen.getByLabelText("Synced folder")).toBeInTheDocument();

    await u.click(screen.getByRole("button", { name: "Cloudflare R2" }));
    expect(screen.getByLabelText("Account ID")).toBeInTheDocument();

    await u.click(screen.getByRole("button", { name: "PackSync Cloud" }));
    expect(screen.getByRole("button", { name: "Sign in with Minecraft" })).toBeInTheDocument();
  });

  it("explains the missing Google OAuth client instead of failing", async () => {
    const u = userEvent.setup();
    const t = fresh();
    t.on("storage.use_google", () => storage({ kind: "google", connected: false, oauth_client_configured: false, setup_hint: "Create a client" }));
    await renderWithStore(<StorageSetup />, t);
    await u.click(screen.getByRole("button", { name: "Google Drive" }));
    await u.click(screen.getByRole("button", { name: "Sign in with Google" }));
    expect(await screen.findByRole("heading", { name: "PackSync needs a Google OAuth client" })).toBeInTheDocument();
    expect(t.count("storage.google_login")).toBe(0);
  });

  it("selects the hidden Google Drive app-data space when asked", async () => {
    const u = userEvent.setup();
    const t = fresh();
    t.on("storage.use_google", () => storage({ kind: "google", connected: false, oauth_client_configured: false }));
    await renderWithStore(<StorageSetup />, t);
    await u.click(screen.getByRole("button", { name: "Google Drive" }));
    await u.click(screen.getByRole("radio", { name: /Hidden app data/ }));
    await u.click(screen.getByRole("button", { name: "Sign in with Google" }));
    await waitFor(() => expect(t.count("storage.use_google")).toBe(1));
    expect(t.calls.find((c) => c.method === "storage.use_google")?.params).toEqual({ space: "app_data" });
  });

  it("signs in through Google and reports who connected", async () => {
    const u = userEvent.setup();
    const t = fresh();
    const connected = storage({ kind: "google", connected: true, oauth_client_configured: true, account: "me@example.com", detail: "me@example.com" });
    t.on("storage.use_google", () => ({ ...connected, connected: false }));
    t.on("storage.google_login", () => ({ account: { email: "me@example.com", display_name: null, permission_id: null }, status: connected }));
    const onConnected = vi.fn();
    const { store } = await renderWithStore(<StorageSetup onConnected={onConnected} />, t);
    await u.click(screen.getByRole("button", { name: "Google Drive" }));
    await u.click(screen.getByRole("button", { name: "Sign in with Google" }));
    await waitFor(() => expect(onConnected).toHaveBeenCalledWith(connected));
    expect(store.getState().toasts.some((x) => x.message === "Connected as me@example.com.")).toBe(true);
  });

  it("connects a synced local folder via storage.use_local", async () => {
    const u = userEvent.setup();
    const t = fresh();
    const connected = storage({ kind: "local", connected: true, detail: "Local folder: /mnt/cloud" });
    t.on("storage.use_local", () => connected);
    const onConnected = vi.fn();

    await renderWithStore(<StorageSetup onConnected={onConnected} />, t);
    await u.click(screen.getByRole("button", { name: "Synced Folder" }));
    await u.type(screen.getByLabelText("Synced folder"), "/mnt/cloud");
    await u.click(screen.getByRole("button", { name: "Use this folder" }));

    await waitFor(() => expect(onConnected).toHaveBeenCalledWith(connected));
    expect(t.calls.find((c) => c.method === "storage.use_local")?.params).toEqual({ path: "/mnt/cloud" });
  });

  it("connects Cloudflare R2 via storage.use_r2", async () => {
    const u = userEvent.setup();
    const t = fresh();
    const connected = storage({ kind: "r2", connected: true, detail: "Cloudflare R2 — bucket saves, folder packsync/" });
    t.on("storage.use_r2", () => connected);
    const onConnected = vi.fn();

    await renderWithStore(<StorageSetup onConnected={onConnected} />, t);
    await u.click(screen.getByRole("button", { name: "Cloudflare R2" }));
    await u.type(screen.getByLabelText("Account ID"), "acct1234");
    await u.type(screen.getByLabelText("Bucket"), "saves");
    await u.type(screen.getByLabelText("Access Key ID"), "keyid");
    await u.type(screen.getByLabelText("Secret Access Key"), "supersecret");
    await u.click(screen.getByRole("button", { name: "Connect Cloudflare R2" }));

    await waitFor(() => expect(onConnected).toHaveBeenCalledWith(connected));
    expect(t.calls.find((c) => c.method === "storage.use_r2")?.params).toEqual({
      account_id: "acct1234",
      bucket: "saves",
      prefix: "packsync",
      access_key_id: "keyid",
      secret_access_key: "supersecret",
    });
    expect(screen.getByLabelText("Secret Access Key")).toHaveValue("");
  });
});

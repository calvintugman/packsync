import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { ApiError } from "../api/errors";
import { appInfo, baseTransport, settings, storage } from "../test/fixtures";
import { renderWithStore } from "../test/render";
import { HostedSetup } from "./HostedSetup";

const hosted = storage({
  kind: "hosted",
  connected: true,
  account: "acct_1234567890",
  detail: "PackSync cloud, end-to-end encrypted (packsync.example, account acct_1…)",
});

function fresh() {
  const t = baseTransport({ storage: storage({ kind: "none", connected: false, detail: "Not connected" }) });
  t.on("settings.get", () => settings({ storage: { kind: "none" } }));
  return t;
}

describe("HostedSetup", () => {
  it("runs Minecraft device-code login, shows code actions, and finishes when wait resolves", async () => {
    const u = userEvent.setup();
    const t = fresh();
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
    t.on("app.open_url", () => ({ opened: true }));
    t.on("storage.hosted_login_start", () => ({
      user_code: "WXYZ-1234",
      verification_uri: "https://www.microsoft.com/link",
      expires_in: 900,
      privacy_url: "https://packsync.example/privacy",
    }));
    let resolveWait!: (v: typeof hosted) => void;
    t.on(
      "storage.hosted_login_wait",
      () =>
        new Promise((resolve) => {
          resolveWait = resolve;
        }),
    );

    const onConnected = vi.fn();
    await renderWithStore(<HostedSetup onConnected={onConnected} />, t);

    await u.click(screen.getByRole("button", { name: "Sign in with Minecraft" }));
    expect(await screen.findByTestId("device-code")).toHaveTextContent("WXYZ-1234");
    expect(screen.getByRole("button", { name: "Cancel" })).toBeInTheDocument();

    await u.click(screen.getByRole("button", { name: "Copy code" }));
    expect(writeText).toHaveBeenCalledWith("WXYZ-1234");

    await u.click(screen.getByRole("button", { name: "Open microsoft.com/link" }));
    await u.click(screen.getByRole("button", { name: "Privacy Policy & Terms" }));
    expect(t.calls.filter((c) => c.method === "app.open_url").map((c) => c.params)).toEqual([
      { url: "https://www.microsoft.com/link" },
      { url: "https://packsync.example/privacy" },
    ]);

    resolveWait(hosted);
    await waitFor(() => expect(onConnected).toHaveBeenCalledWith(hosted));
    expect(screen.queryByTestId("device-code")).toBeNull();
  });

  it("cancels device-code polling when Cancel is clicked", async () => {
    const u = userEvent.setup();
    const t = fresh();
    t.on("storage.hosted_login_start", () => ({
      user_code: "ABCD-5678",
      verification_uri: "https://www.microsoft.com/link",
      expires_in: 900,
      privacy_url: "https://packsync.example/privacy",
    }));
    t.on("storage.hosted_login_wait", () => new Promise(() => {}));
    t.on("jobs.cancel", (params) => {
      const { job } = params as { job: string };
      t.finishWith(job, { state: "cancelled" });
      return { cancelled: true };
    });

    await renderWithStore(<HostedSetup />, t);
    await u.click(screen.getByRole("button", { name: "Sign in with Minecraft" }));
    expect(await screen.findByTestId("device-code")).toHaveTextContent("ABCD-5678");

    await u.click(screen.getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(t.count("jobs.cancel")).toBe(1));
    expect(screen.queryByTestId("device-code")).toBeNull();
    expect(screen.getByRole("button", { name: "Sign in with Minecraft" })).toBeInTheDocument();
  });

  it("reports errors when sign-in cannot start", async () => {
    const u = userEvent.setup();
    const t = fresh();
    t.on("storage.hosted_login_start", () => {
      throw new ApiError({ code: "offline", message: "The PackSync service could not be reached." });
    });

    await renderWithStore(<HostedSetup />, t);
    await u.click(screen.getByRole("button", { name: "Sign in with Minecraft" }));
    expect(await screen.findByText(/could not be reached/)).toBeInTheDocument();
  });

  it("when connected, shows Minecraft account status, multi-PC sync note, and deletes cloud account after confirmation", async () => {
    const u = userEvent.setup();
    const t = baseTransport({ storage: hosted });
    t.on("app.info", () => appInfo({ minecraft_profile: { name: "Steve", uuid: "069a79f4-44e9-4726-a5be-fca90e38aaf5" } }));
    t.on("settings.get", () =>
      settings({ storage: { kind: "hosted", endpoint: "https://packsync.example", account: "acct_1234567890", display_name: "Steve" } }),
    );
    const noneStatus = storage({ kind: "none", connected: false, detail: "Nothing is connected yet." });
    t.on("storage.hosted_delete_account", () => noneStatus);

    await renderWithStore(<HostedSetup />, t);
    expect(screen.getByText(/Steve/)).toBeInTheDocument();
    expect(screen.getByText(/Automatic multi-computer sync/i)).toBeInTheDocument();

    await u.click(screen.getByRole("button", { name: "Delete cloud account & data…" }));
    const dialog = await screen.findByRole("dialog", { name: "Delete PackSync Cloud account and data?" });
    expect(dialog).toBeInTheDocument();

    t.on("storage.status", () => noneStatus);
    t.on("settings.get", () => settings({ storage: { kind: "none" } }));
    await u.click(screen.getByRole("button", { name: "Delete cloud account & data" }));
    await waitFor(() => expect(t.count("storage.hosted_delete_account")).toBe(1));
  });
});

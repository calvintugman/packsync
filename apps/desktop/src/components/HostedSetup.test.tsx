import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { baseTransport, settings, storage } from "../test/fixtures";
import { renderWithStore } from "../test/render";
import { HostedSetup } from "./HostedSetup";

const hosted = storage({ kind: "hosted", connected: true, detail: "PackSync cloud, end-to-end encrypted (packsync.example, account abcdef…)" });

function fresh() {
  const t = baseTransport({ storage: storage({ kind: "none", connected: false, detail: "Not connected" }) });
  t.on("settings.get", () => settings({ storage: { kind: "none" } }));
  return t;
}

describe("HostedSetup", () => {
  it("starts a PackSync cloud with one click", async () => {
    const u = userEvent.setup();
    const t = fresh();
    t.on("storage.hosted_start", () => hosted);
    const onConnected = vi.fn();
    await renderWithStore(<HostedSetup onConnected={onConnected} />, t);
    expect(screen.getByText(/cannot read them/)).toBeInTheDocument();
    await u.click(screen.getByRole("button", { name: "Start on this computer" }));
    await waitFor(() => expect(onConnected).toHaveBeenCalledWith(hosted));
  });

  it("joins with a code typed any way the person likes", async () => {
    const u = userEvent.setup();
    const t = fresh();
    t.on("storage.hosted_join", () => hosted);
    const onConnected = vi.fn();
    await renderWithStore(<HostedSetup onConnected={onConnected} />, t);
    await u.click(screen.getByRole("button", { name: "Join from another computer" }));
    const join = screen.getByRole("button", { name: "Join" });
    await u.type(screen.getByLabelText("Pairing code"), "4821 k7qm");
    expect(join).toBeDisabled();
    await u.type(screen.getByLabelText("Pairing code"), " x2pd");
    await u.click(join);
    await waitFor(() => expect(onConnected).toHaveBeenCalled());
    expect(t.calls.find((c) => c.method === "storage.hosted_join")?.params).toEqual({ code: "4821 k7qm x2pd" });
  });

  it("explains a wrong code", async () => {
    const u = userEvent.setup();
    const t = fresh();
    t.on("storage.hosted_join", () => {
      throw Object.assign(new Error("x"), { code: "storage", message: "the code does not match the one shown on the other computer", advice: null, exit_code: 1 });
    });
    await renderWithStore(<HostedSetup />, t);
    await u.click(screen.getByRole("button", { name: "Join from another computer" }));
    await u.type(screen.getByLabelText("Pairing code"), "4821-K7QM-X2PD");
    await u.click(screen.getByRole("button", { name: "Join" }));
    expect(await screen.findByText(/does not match/)).toBeInTheDocument();
  });

  it("on a connected computer shows a pairing code while waiting, and the recovery key only when asked", async () => {
    const u = userEvent.setup();
    const t = baseTransport({ storage: hosted });
    t.on("settings.get", () => settings({ storage: { kind: "hosted", endpoint: "https://packsync.example", account: "abcdefghijklmnopqrstuvwxyz" } }));
    t.on("storage.hosted_pair_start", () => ({ code: "4821-K7QM-X2PD" }));
    let release: (v: { joined: boolean }) => void = () => {};
    t.on("storage.hosted_pair_wait", () => new Promise((r) => (release = r)));
    t.on("storage.hosted_recovery_key", () => ({ key: "packsync1-SECRET" }));
    await renderWithStore(<HostedSetup />, t);
    expect(screen.queryByText(/packsync1-SECRET/)).not.toBeInTheDocument();
    await u.click(screen.getByRole("button", { name: "Show a pairing code" }));
    expect(await screen.findByText("4821-K7QM-X2PD")).toBeInTheDocument();
    release({ joined: true });
    expect(await screen.findByText(/The other computer joined/)).toBeInTheDocument();
    expect(screen.queryByText("4821-K7QM-X2PD")).not.toBeInTheDocument();
    await u.click(screen.getByRole("button", { name: "Show the recovery key" }));
    expect(await screen.findByDisplayValue("packsync1-SECRET")).toBeInTheDocument();
  });
});

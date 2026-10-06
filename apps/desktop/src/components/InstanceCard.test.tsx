import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { baseTransport, instance, summary, withPhase } from "../test/fixtures";
import { renderWithStore } from "../test/render";
import { InstanceCard } from "./InstanceCard";

describe("InstanceCard", () => {
  it("shows what the instance is and where its sync stands", async () => {
    const inst = instance();
    await renderWithStore(<InstanceCard inst={inst} />, baseTransport({ instances: [inst] }));
    const card = screen.getByRole("article", { name: "Skyblock Pack" });
    expect(within(card).getByText("Minecraft 1.20.1 · Forge 47.4.0", { exact: false })).toBeInTheDocument();
    expect(within(card).getByText("Ready on your other computers", { selector: ".pill" })).toBeInTheDocument();
    expect(card).toHaveAttribute("data-phase", "ready_on_other_devices");
    expect(within(card).getByText(/Last synced 5 minutes ago from Laptop/)).toBeInTheDocument();
  });

  it("shows the four-stage journey while an upload is unfinished, with the right stage", async () => {
    const inst = withPhase("upload_pending", { pending_snapshots: 1, last_error: "Google Drive is full", next_retry_ms: Date.now() + 120_000 });
    await renderWithStore(<InstanceCard inst={inst} />, baseTransport({ instances: [inst] }));
    const steps = screen.getByRole("list", { name: "Upload progress" });
    expect(within(steps).getAllByRole("listitem")).toHaveLength(4);
    expect(within(steps).getByText("Uploading").closest("li")).toHaveAttribute("aria-current", "step");
    expect(screen.getByText("The last upload did not finish")).toBeInTheDocument();
    expect(screen.getByText(/Google Drive is full/)).toBeInTheDocument();
    // it must not look finished
    expect(screen.getByText("Upload pending", { selector: ".pill" })).toBeInTheDocument();
  });

  it("has no journey for a conflict, and Play waits for the decision", async () => {
    const inst = withPhase("conflict", { state_detail: "diverged" });
    await renderWithStore(<InstanceCard inst={inst} />, baseTransport({ instances: [inst] }));
    expect(screen.queryByRole("list", { name: "Upload progress" })).toBeNull();
    expect(screen.getByRole("button", { name: "Play" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Choose a version…" })).toBeEnabled();
  });

  it("warns when another computer may be playing the same instance", async () => {
    const inst = instance({ playing_elsewhere: ["Laptop"] });
    await renderWithStore(<InstanceCard inst={inst} />, baseTransport({ instances: [inst] }));
    expect(screen.getByText("Possibly being played on Laptop")).toBeInTheDocument();
  });

  it("Sync now runs the sync job and reports the outcome in words", async () => {
    const u = userEvent.setup();
    const inst = instance();
    const t = baseTransport({ instances: [inst] });
    t.on("instance.sync", () => ({
      snapshot: { result: { result: "unchanged", base: "aa" }, stats: {}, issues: [] },
      reconcile: { outcome: "fast_forwarded", from: "a", to: "b", files_written: 2, files_deleted: 0 },
    }));
    const { store } = await renderWithStore(<InstanceCard inst={inst} />, t);
    await u.click(screen.getByRole("button", { name: "Sync now" }));
    await screen.findByText(/Applied newer progress from the cloud \(2 files updated\)/, {}, { timeout: 3000 }).catch(() => undefined);
    expect(t.calls.find((c) => c.method === "instance.sync")).toMatchObject({ job: true, params: { instance: inst.id } });
    await new Promise((r) => setTimeout(r, 20));
    expect(store.getState().toasts.some((x) => x.message.includes("Applied newer progress"))).toBe(true);
  });

  it("opens the conflict dialog by itself when a sync finds one", async () => {
    const u = userEvent.setup();
    const inst = instance();
    const t = baseTransport({ instances: [inst] });
    const report = { outcome: "conflict", kind: "diverged", local: summary({ device: "Desktop" }), local_dirty: false, cloud: [summary()] };
    t.on("instance.sync", () => ({ snapshot: { result: { result: "unchanged", base: "a" }, stats: {}, issues: [] }, reconcile: report }));
    t.on("conflict.show", () => ({ conflict: true, report }));
    await renderWithStore(<InstanceCard inst={inst} />, t);
    await u.click(screen.getByRole("button", { name: "Sync now" }));
    expect(await screen.findByRole("dialog", { name: "Two versions of Skyblock Pack" })).toBeInTheDocument();
  });

  it("shows an engine error next to the button that caused it", async () => {
    const u = userEvent.setup();
    const inst = instance();
    const t = baseTransport({ instances: [inst] });
    t.on("instance.sync", () => {
      throw Object.assign(new Error("x"), { code: "offline", message: "The network is down", advice: "Check your connection.", exit_code: 10 });
    });
    await renderWithStore(<InstanceCard inst={inst} />, t);
    await u.click(screen.getByRole("button", { name: "Sync now" }));
    expect(await screen.findByText("Can't reach the cloud")).toBeInTheDocument();
    expect(screen.getByText(/The network is down/)).toBeInTheDocument();
  });
});

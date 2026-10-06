import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { HistoryEntry, HookPlan, RestorePreview } from "../api/types";
import { baseTransport, instance, summary } from "../test/fixtures";
import { renderWithStore } from "../test/render";
import { CrashDialog } from "./CrashDialog";
import { HooksDialog } from "./HooksDialog";
import { RestoreDialog } from "./RestoreDialog";

const crashEntry: HistoryEntry = {
  id: "cc".repeat(32), kind: "crash", created: new Date().toISOString(), device_id: "d", device: "Desktop",
  files: 30, bytes: 1_000_000, note: null, outcome: "crashed", parents: [], state: "local_complete",
  is_base: true, is_cloud_head: false, expired: false,
};

describe("CrashDialog", () => {
  it("promotes or discards exactly as chosen", async () => {
    const u = userEvent.setup();
    const t = baseTransport();
    t.on("instance.history", () => [crashEntry]);
    t.on("crash.promote", () => ({ snapshot: "s", upload: {} }));
    t.on("crash.discard", () => ({ written: 1 }));
    const inst = instance({ phase: "crash_pending", state: "crash_pending" });
    const onClose = vi.fn();
    await renderWithStore(<CrashDialog instance={inst} onClose={onClose} />, t);
    expect(await screen.findByText("Crash save")).toBeInTheDocument();
    // keep is the default
    await u.click(screen.getByRole("button", { name: "Apply this choice" }));
    await waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(t.count("crash.promote")).toBe(1);
    expect(t.count("crash.discard")).toBe(0);
  });

  it("goes back to the last good save when asked", async () => {
    const u = userEvent.setup();
    const t = baseTransport();
    t.on("instance.history", () => [crashEntry]);
    t.on("crash.discard", () => ({ written: 1 }));
    const onClose = vi.fn();
    await renderWithStore(<CrashDialog instance={instance({ phase: "crash_pending", state: "crash_pending" })} onClose={onClose} />, t);
    await screen.findByText("Crash save");
    await u.click(screen.getByRole("radio", { name: /Go back to the last good save/ }));
    await u.click(screen.getByRole("button", { name: "Apply this choice" }));
    await waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(t.calls.find((c) => c.method === "crash.discard")?.params).toEqual({ instance: instance().id });
    expect(t.count("crash.promote")).toBe(0);
  });
});

const plan = (over: Partial<HookPlan> = {}): HookPlan => ({
  root: "/r",
  changes: [{ file: "/r/instance.cfg", key: "PreLaunchCommand", before: null, after: '"packsync" hook pre-launch' }],
  preserved_commands: [],
  blockers: [],
  ...over,
});

describe("HooksDialog", () => {
  it("shows the exact change and applies the plan it showed", async () => {
    const u = userEvent.setup();
    const t = baseTransport();
    const p = plan({ preserved_commands: ["pre-launch: echo hi"] });
    t.on("hooks.plan_install", () => p);
    t.on("hooks.apply", () => ({ installed: true, intact: true, pre_launch: "x", post_exit: "y" }));
    const onApplied = vi.fn();
    const onClose = vi.fn();
    await renderWithStore(<HooksDialog instance={instance().id} mode="install" onClose={onClose} onApplied={onApplied} />, t);
    expect(await screen.findByLabelText("PreLaunchCommand after")).toHaveTextContent("hook pre-launch");
    expect(screen.getByLabelText("PreLaunchCommand before")).toHaveTextContent("(not set)");
    expect(screen.getByText("Your own commands keep running")).toBeInTheDocument();
    await u.click(screen.getByRole("button", { name: "Install hooks" }));
    await waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(onApplied).toHaveBeenCalled();
    expect(t.calls.find((c) => c.method === "hooks.apply")?.params).toEqual({ instance: instance().id, plan: p });
  });

  it("cannot be applied while something blocks it", async () => {
    const t = baseTransport();
    t.on("hooks.plan_install", () => plan({ blockers: ["Prism Launcher is running"] }));
    await renderWithStore(<HooksDialog instance={instance().id} mode="install" onClose={() => {}} onApplied={() => {}} />, t);
    expect(await screen.findByText("Prism Launcher is running")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Install hooks" })).toBeDisabled();
  });

  it("says so when there is nothing to change", async () => {
    const t = baseTransport();
    t.on("hooks.plan_remove", () => plan({ changes: [] }));
    await renderWithStore(<HooksDialog instance={instance().id} mode="remove" onClose={() => {}} onApplied={() => {}} />, t);
    expect(await screen.findByText("There is nothing to change.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Remove hooks" })).toBeDisabled();
  });
});

const entry: HistoryEntry = { ...crashEntry, id: "dd".repeat(32), kind: "normal", outcome: null, is_base: false, state: "visible" };
const preview = (over: Partial<RestorePreview> = {}): RestorePreview => ({
  target: summary(), files_to_write: 3, files_unchanged: 20, files_to_delete: 1, bytes_to_write: 5000,
  local_only_preserved: 2, sample_writes: ["config/a.toml"], sample_deletes: ["config/b.toml"], skipped: [], local_changes: false, problems: [], ...over,
});

describe("RestoreDialog", () => {
  it("previews before it restores and sends the chosen snapshot", async () => {
    const u = userEvent.setup();
    const t = baseTransport();
    t.on("restore.preview", () => preview());
    t.on("restore.apply", () => ({ written: 3, unchanged: 20, deleted: 1, bytes_written: 5000, bytes_downloaded: 0, bytes_reused_locally: 0, recovery_point: "rp1", skipped: [], target: "t" }));
    const inst = instance();
    await renderWithStore(<RestoreDialog instance={inst.id} entry={entry} onClose={() => {}} />, t);
    expect(await screen.findByText("files will be removed")).toBeInTheDocument();
    expect(t.count("restore.apply")).toBe(0); // nothing happens before the person confirms
    await u.click(screen.getByRole("button", { name: "Restore" }));
    expect(await screen.findByText(/kept as a recovery point \(rp1\)/)).toBeInTheDocument();
    expect(t.calls.find((c) => c.method === "restore.apply")?.params).toEqual({ instance: inst.id, snapshot: entry.id, skip_incompatible: false });
  });

  it("will not restore what the engine says cannot be restored", async () => {
    const t = baseTransport();
    t.on("restore.preview", () => preview({ problems: ["the snapshot's content was released by a cleanup"] }));
    await renderWithStore(<RestoreDialog instance={instance().id} entry={entry} onClose={() => {}} />, t);
    expect(await screen.findByText("This can't be restored right now")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Restore" })).toBeDisabled();
  });

  it("makes the person agree to skip files this computer cannot hold", async () => {
    const u = userEvent.setup();
    const t = baseTransport();
    t.on("restore.preview", () => preview({ skipped: [{ path: "mods/aux.jar", problem: "reserved name on Windows" }] }));
    t.on("restore.apply", () => ({ written: 1, unchanged: 0, deleted: 0, bytes_written: 1, bytes_downloaded: 0, bytes_reused_locally: 0, recovery_point: null, skipped: [], target: "t" }));
    await renderWithStore(<RestoreDialog instance={instance().id} entry={entry} onClose={() => {}} />, t);
    const restore = await screen.findByRole("button", { name: "Restore" });
    expect(restore).toBeDisabled();
    await u.click(screen.getByRole("checkbox", { name: /Skip these files/ }));
    expect(restore).toBeEnabled();
    await u.click(restore);
    await waitFor(() => expect(t.count("restore.apply")).toBe(1));
    expect(t.calls.find((c) => c.method === "restore.apply")?.params).toMatchObject({ skip_incompatible: true });
  });

  it("mentions that unsaved changes are saved first", async () => {
    const t = baseTransport();
    t.on("restore.preview", () => preview({ local_changes: true }));
    await renderWithStore(<RestoreDialog instance={instance().id} entry={entry} onClose={() => {}} />, t);
    expect(await screen.findByText("You have changes that are not saved yet")).toBeInTheDocument();
  });
});

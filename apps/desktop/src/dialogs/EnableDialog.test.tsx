import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { HookPlan, PreviewReport } from "../api/types";
import { baseTransport } from "../test/fixtures";
import { renderWithStore } from "../test/render";
import { EnableDialog } from "./EnableDialog";

const preview: PreviewReport = {
  name: "GTNH",
  game_root: ".minecraft",
  pack: { minecraft_version: "1.7.10", loader: null, components: [] },
  worlds: [],
  included_files: 10,
  included_bytes: 1000,
  included_by_category: [],
  empty_dirs: 0,
  excluded: { groups: {}, pruned_dirs: [], unmatched_by_dir: {} },
  issues: [],
  estimated_upload_bytes: null,
  rules_text: "",
  samples: [],
};
const plan = (blockers: string[] = []): HookPlan => ({ root: "/i", changes: [], preserved_commands: [], blockers });

function transport(blockers: string[] = []) {
  const t = baseTransport();
  t.on("instances.preview", () => preview);
  t.on("instances.enable", () => ({ outcome: { instance: "i-1" }, upload_error: null }));
  t.on("hooks.plan_install", () => plan(blockers));
  t.on("hooks.apply", () => ({ installed: true, intact: true, partial: false, pre_launch: "x", post_exit: "y" }));
  return t;
}

describe("EnableDialog", () => {
  it("installs the Prism hooks along with syncing, by default", async () => {
    const u = userEvent.setup();
    const t = transport();
    const onEnabled = vi.fn();
    await renderWithStore(<EnableDialog root="/i" onClose={() => {}} onEnabled={onEnabled} />, t);
    expect(await screen.findByLabelText("Sync automatically when I press Play in Prism")).toBeChecked();
    await u.click(screen.getByRole("button", { name: "Start syncing" }));
    await waitFor(() => expect(onEnabled).toHaveBeenCalledWith("i-1"));
    expect(t.count("hooks.plan_install")).toBe(1);
    expect(t.count("hooks.apply")).toBe(1);
  });

  it("leaves Prism alone when unticked, and never applies a blocked plan", async () => {
    const u = userEvent.setup();
    const t = transport();
    const onEnabled = vi.fn();
    await renderWithStore(<EnableDialog root="/i" onClose={() => {}} onEnabled={onEnabled} />, t);
    await u.click(await screen.findByLabelText("Sync automatically when I press Play in Prism"));
    await u.click(screen.getByRole("button", { name: "Start syncing" }));
    await waitFor(() => expect(onEnabled).toHaveBeenCalled());
    expect(t.count("hooks.plan_install")).toBe(0);

    const blocked = transport(["Prism Launcher is running."]);
    const onEnabled2 = vi.fn();
    await renderWithStore(<EnableDialog root="/i" onClose={() => {}} onEnabled={onEnabled2} />, blocked);
    await u.click((await screen.findAllByRole("button", { name: "Start syncing" })).at(-1)!);
    await waitFor(() => expect(onEnabled2).toHaveBeenCalled());
    expect(blocked.count("hooks.plan_install")).toBe(1);
    expect(blocked.count("hooks.apply")).toBe(0);
  });
});

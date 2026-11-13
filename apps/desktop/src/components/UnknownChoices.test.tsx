import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { PreviewReport, Suggestion } from "../api/types";
import { EnableDialog } from "../dialogs/EnableDialog";
import { baseTransport, instance } from "../test/fixtures";
import { renderWithStore } from "../test/render";
import { InstanceCard } from "./InstanceCard";

const vp: Suggestion = { path: ".minecraft/visualprospecting/", files: 40, bytes: 4_000_000, include: true, sure: true, reason: "VisualProspecting" };
const odd: Suggestion = { path: ".minecraft/mystery/", files: 3, bytes: 900, include: true, sure: false, reason: "Unknown mod data" };

const preview: PreviewReport = {
  name: "GTNH",
  game_root: ".minecraft",
  pack: { minecraft_version: "1.7.10", loader: null, components: [] },
  worlds: [],
  included_files: 10,
  included_bytes: 1000,
  included_by_category: [],
  empty_dirs: 0,
  excluded: { groups: {}, pruned_dirs: [], unmatched_by_dir: {}, unmatched_groups: {} },
  issues: [],
  estimated_upload_bytes: null,
  rules_text: "",
  samples: [],
  suggestions: [odd, vp],
};

describe("content PackSync does not recognise", () => {
  it("is offered before syncing starts, unsure first, and the answers go with 'Start syncing'", async () => {
    const u = userEvent.setup();
    const t = baseTransport();
    t.on("instances.preview", () => preview);
    t.on("instances.enable", () => ({ outcome: { instance: "i-1" }, upload_error: null }));
    const onEnabled = vi.fn();
    await renderWithStore(<EnableDialog root="/i" onClose={() => {}} onEnabled={onEnabled} />, t);
    const mystery = await screen.findByRole("checkbox", { name: ".minecraft/mystery/" });
    expect(mystery).toBeChecked();
    // the recognised one is folded away
    expect(screen.queryByRole("checkbox", { name: ".minecraft/visualprospecting/" })).not.toBeInTheDocument();
    await u.click(mystery); // "no, not that one"
    await u.click(screen.getByLabelText("Sync automatically when I press Play in Prism"));
    await u.click(screen.getByRole("button", { name: "Start syncing" }));
    await waitFor(() => expect(onEnabled).toHaveBeenCalled());
    const params = t.calls.find((c) => c.method === "instances.enable")?.params as { decisions: unknown[] };
    expect(params.decisions).toEqual(
      expect.arrayContaining([
        { path: ".minecraft/mystery/", include: false },
        { path: ".minecraft/visualprospecting/", include: true },
      ]),
    );
  });

  it("found later (a new mod), is reviewed from the card and applied by a sync", async () => {
    const u = userEvent.setup();
    const t = baseTransport();
    t.on("instance.decide_unknown", () => ({ decided: 1 }));
    t.on("instance.sync", () => ({ snapshot: null, reconcile: { outcome: "up_to_date", head: "aa".repeat(32) } }));
    const inst = instance({ unknown: [odd] });
    await renderWithStore(<InstanceCard inst={inst} />, t);
    await u.click(screen.getByRole("button", { name: "Review" }));
    expect(screen.getByRole("dialog", { name: `New mod data in ${inst.name}` })).toBeInTheDocument();
    await u.click(screen.getByRole("button", { name: "Save and sync" }));
    await waitFor(() => expect(t.count("instance.sync")).toBe(1));
    expect(t.calls.find((c) => c.method === "instance.decide_unknown")?.params).toEqual({
      instance: inst.id,
      decisions: [{ path: ".minecraft/mystery/", include: true }],
    });
  });
});

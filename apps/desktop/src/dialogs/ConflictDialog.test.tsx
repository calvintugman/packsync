import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { baseTransport, instance, summary } from "../test/fixtures";
import { renderWithStore } from "../test/render";
import { ConflictDialog } from "./ConflictDialog";

const local = summary({ id: "11".repeat(32), device: "Desktop", files: 30 });
const cloudA = summary({ id: "22".repeat(32), device: "Laptop", files: 31 });
const cloudB = summary({ id: "33".repeat(32), device: "Steam Deck", files: 29 });

function setup(cloud = [cloudA]) {
  const t = baseTransport();
  t.on("conflict.show", () => ({
    conflict: true,
    report: { outcome: "conflict", kind: cloud.length > 1 ? "multiple_cloud_heads" : "diverged", local, local_dirty: false, cloud },
  }));
  t.on("conflict.resolve", () => ({ resolution: "x", head: "h", new_instance: null, preserved_local: null, uploaded: true }));
  return t;
}

describe("ConflictDialog", () => {
  it("shows both versions and says nothing is deleted", async () => {
    await renderWithStore(<ConflictDialog instance={instance()} onClose={() => {}} />, setup());
    expect(await screen.findByText(/never merges worlds automatically/)).toBeInTheDocument();
    expect(within(screen.getByRole("region", { name: "This computer" })).getByText("Desktop")).toBeInTheDocument();
    expect(within(screen.getByRole("region", { name: "In the cloud" })).getByText("Laptop")).toBeInTheDocument();
    expect(screen.getByText("Nothing is deleted:")).toBeInTheDocument();
  });

  it("keeps this computer's version by default and sends exactly that", async () => {
    const u = userEvent.setup();
    const t = setup();
    const onClose = vi.fn();
    const inst = instance();
    await renderWithStore(<ConflictDialog instance={inst} onClose={onClose} />, t);
    await screen.findByText(/never merges worlds/);
    await u.click(screen.getByRole("button", { name: "Apply this choice" }));
    await waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(t.calls.find((c) => c.method === "conflict.resolve")?.params).toEqual({ instance: inst.id, resolution: { choice: "keep_local" } });
  });

  it("uses the cloud's version, and asks which one when the cloud has several", async () => {
    const u = userEvent.setup();
    const t = setup([cloudA, cloudB]);
    const inst = instance();
    await renderWithStore(<ConflictDialog instance={inst} onClose={() => {}} />, t);
    await screen.findByText(/never merges worlds/);
    expect(screen.getByText("Two computers saved progress independently, so the cloud holds more than one version.", { exact: false })).toBeInTheDocument();
    await u.click(screen.getByRole("radio", { name: /Use the cloud's version/ }));
    const which = screen.getByLabelText("Which cloud version?");
    await u.selectOptions(which, cloudB.id);
    await u.click(screen.getByRole("button", { name: "Apply this choice" }));
    await waitFor(() => expect(t.calls.some((c) => c.method === "conflict.resolve")).toBe(true));
    expect(t.calls.find((c) => c.method === "conflict.resolve")?.params).toEqual({
      instance: inst.id,
      resolution: { choice: "keep_cloud", head: cloudB.id },
    });
  });

  it("splits into a separate instance with the name the person typed", async () => {
    const u = userEvent.setup();
    const t = setup();
    const inst = instance();
    await renderWithStore(<ConflictDialog instance={inst} onClose={() => {}} />, t);
    await screen.findByText(/never merges worlds/);
    await u.click(screen.getByRole("radio", { name: /Keep both, as separate instances/ }));
    const name = screen.getByLabelText("Name of the separate instance");
    expect(name).toHaveValue("Skyblock Pack (this computer)");
    await u.clear(name);
    await u.type(name, "Skyblock Experiments");
    await u.click(screen.getByRole("button", { name: "Apply this choice" }));
    await waitFor(() => expect(t.calls.some((c) => c.method === "conflict.resolve")).toBe(true));
    expect(t.calls.find((c) => c.method === "conflict.resolve")?.params).toMatchObject({
      resolution: { choice: "split_local", new_name: "Skyblock Experiments" },
    });
  });

  it("does not let an empty name through", async () => {
    const u = userEvent.setup();
    await renderWithStore(<ConflictDialog instance={instance()} onClose={() => {}} />, setup());
    await screen.findByText(/never merges worlds/);
    await u.click(screen.getByRole("radio", { name: /Keep both/ }));
    await u.clear(screen.getByLabelText("Name of the separate instance"));
    expect(screen.getByRole("button", { name: "Apply this choice" })).toBeDisabled();
  });

  it("tells the person when the conflict is already gone", async () => {
    const t = baseTransport();
    t.on("conflict.show", () => ({ conflict: false, report: { outcome: "up_to_date", head: "a" } }));
    await renderWithStore(<ConflictDialog instance={instance()} onClose={() => {}} />, t);
    expect(await screen.findByText("There is no conflict any more.")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Apply this choice" })).toBeNull();
  });

  it("shows why resolving failed and stays open", async () => {
    const u = userEvent.setup();
    const t = setup();
    t.on("conflict.resolve", () => {
      throw Object.assign(new Error("x"), { code: "game_running", message: "Minecraft is running", advice: "Close it first.", exit_code: 14 });
    });
    const onClose = vi.fn();
    await renderWithStore(<ConflictDialog instance={instance()} onClose={onClose} />, t);
    await screen.findByText(/never merges worlds/);
    await u.click(screen.getByRole("button", { name: "Apply this choice" }));
    expect(await screen.findByText("The game is running")).toBeInTheDocument();
    expect(onClose).not.toHaveBeenCalled();
  });
});

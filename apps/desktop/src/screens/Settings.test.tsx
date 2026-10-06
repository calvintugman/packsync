import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { baseTransport, settings } from "../test/fixtures";
import { renderWithStore } from "../test/render";
import { SettingsScreen } from "./Settings";

describe("SettingsScreen", () => {
  it("saves the command-line tool the launcher hooks call", async () => {
    const u = userEvent.setup();
    const t = baseTransport();
    t.on("settings.update", () => ({ settings: settings({ cli_path: "/opt/packsync/packsync" }), storage_problem: null }));
    await renderWithStore(<SettingsScreen />, t);
    const field = await screen.findByLabelText("PackSync command-line tool (used by the hooks)");
    expect(field).toHaveValue("");
    const section = screen.getByRole("region", { name: "Launcher hooks" });
    const save = section.querySelectorAll("button");
    expect(Array.from(save).some((b) => b.textContent === "Save" && b.hasAttribute("disabled"))).toBe(true);
    await u.type(field, "/opt/packsync/packsync");
    await u.click(Array.from(section.querySelectorAll("button")).find((b) => b.textContent === "Save")!);
    await waitFor(() => expect(t.count("settings.update")).toBe(1));
    expect(t.calls.find((c) => c.method === "settings.update")?.params).toEqual({ patch: { cli_path: "/opt/packsync/packsync" } });
  });

  it("applies the offline policy at once and saves it", async () => {
    const u = userEvent.setup();
    const t = baseTransport();
    let current = settings();
    t.on("settings.get", () => current);
    t.on("settings.update", () => {
      current = settings({ hook_offline_policy: "block" });
      return { settings: current, storage_problem: null };
    });
    await renderWithStore(<SettingsScreen />, t);
    const block = await screen.findByRole("radio", { name: /Don't start the game/ });
    await u.click(block);
    expect(block).toBeChecked();
    await waitFor(() => expect(t.calls.find((c) => c.method === "settings.update")?.params).toEqual({ patch: { hook_offline_policy: "block" } }));
  });

  it("shows the engine's reason when a setting is refused", async () => {
    const u = userEvent.setup();
    const t = baseTransport();
    t.on("settings.update", () => {
      throw Object.assign(new Error("x"), { code: "config", message: "upload_concurrency must be between 1 and 32", advice: null, exit_code: 1 });
    });
    await renderWithStore(<SettingsScreen />, t);
    const section = await screen.findByRole("region", { name: "Advanced" });
    const field = section.querySelector("input") as HTMLInputElement;
    await u.clear(field);
    await u.type(field, "0");
    await u.click(Array.from(section.querySelectorAll("button")).find((b) => b.textContent === "Save")!);
    expect(await screen.findByText(/must be between 1 and 32/)).toBeInTheDocument();
  });
});

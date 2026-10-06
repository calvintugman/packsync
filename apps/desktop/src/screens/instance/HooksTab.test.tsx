import { screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { HookState } from "../../api/types";
import { baseTransport, instance } from "../../test/fixtures";
import { renderWithStore } from "../../test/render";
import { HooksTab } from "./HooksTab";

async function show(state: Partial<HookState>) {
  const t = baseTransport();
  t.on("hooks.state", () => ({ installed: false, intact: false, partial: false, pre_launch: null, post_exit: null, ...state }));
  await renderWithStore(<HooksTab inst={instance()} />, t);
}

describe("HooksTab", () => {
  it("offers to install when nothing is installed", async () => {
    await show({});
    expect(await screen.findByText("Not installed")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Install hooks…" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Remove hooks…" })).toBeNull();
  });

  it("shows the commands that are installed and offers to remove them", async () => {
    await show({ installed: true, intact: true, pre_launch: '"packsync" hook pre-launch', post_exit: '"packsync" hook post-exit' });
    expect(await screen.findByText("Installed")).toBeInTheDocument();
    expect(screen.getByText(/hook pre-launch/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Remove hooks…" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Install hooks|Repair hooks/ })).toBeNull();
  });

  it("calls a half-replaced installation what it is and offers the repair", async () => {
    await show({ partial: true, pre_launch: "echo other", post_exit: '"packsync" hook post-exit' });
    expect(await screen.findByText("Partly installed")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Repair hooks…" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Remove hooks…" })).toBeInTheDocument();
  });

  it("notices commands that changed after the install", async () => {
    await show({ installed: true, intact: false, pre_launch: "a", post_exit: "b" });
    expect(await screen.findByText("Changed since installed")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Repair hooks…" })).toBeInTheDocument();
  });
});

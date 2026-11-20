import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { baseTransport } from "../test/fixtures";
import { renderWithStore } from "../test/render";
import { ActivityScreen } from "./Activity";

afterEach(() => {
  vi.restoreAllMocks();
});

describe("ActivityScreen", () => {
  it("hides detailed logs by default, expands/filters them, exports JSON, and clears logs", async () => {
    const u = userEvent.setup();
    let entries = [
      { ts_ms: Date.now() - 10_000, instance: null, level: "info", code: "sync_ok", message: "Synced cleanly" },
      { ts_ms: Date.now() - 5_000, instance: null, level: "warn", code: "retry", message: "Temporary timeout" },
    ];
    const t = baseTransport();
    t.on("events.recent", () => entries);
    t.on("events.clear", () => {
      const n = entries.length;
      entries = [];
      return { cleared: n };
    });

    const createObj = vi.spyOn(URL, "createObjectURL").mockReturnValue("blob:mock-logs");
    const revokeObj = vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
    const clickSpy = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => {});

    const { store } = await renderWithStore(<ActivityScreen />, t);

    expect(screen.getByRole("heading", { name: "Activity" })).toBeInTheDocument();
    expect(screen.queryByText("Synced cleanly")).not.toBeInTheDocument();

    await u.click(screen.getByRole("button", { name: "Show logs" }));
    expect(await screen.findByText("Synced cleanly")).toBeInTheDocument();
    expect(screen.getByText("Temporary timeout")).toBeInTheDocument();

    await u.click(screen.getByRole("button", { name: /Warnings & errors/ }));
    expect(screen.queryByText("Synced cleanly")).not.toBeInTheDocument();
    expect(screen.getByText("Temporary timeout")).toBeInTheDocument();

    await u.click(screen.getByRole("button", { name: "Export JSON" }));
    await waitFor(() => expect(createObj).toHaveBeenCalledTimes(1));
    expect(clickSpy).toHaveBeenCalledTimes(1);
    expect(revokeObj).toHaveBeenCalledWith("blob:mock-logs");
    expect(store.getState().toasts.some((x) => x.message === "Exported 2 log entries to JSON.")).toBe(true);

    await u.click(screen.getByRole("button", { name: "Clear logs" }));
    const dialog = await screen.findByRole("dialog", { name: "Clear detailed logs?" });
    await u.click(dialog.querySelector('button[class*="danger"]') as HTMLElement);
    await waitFor(() => expect(t.count("events.clear")).toBe(1));
    expect(await screen.findByText("No warnings or errors.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Clear logs" })).toBeDisabled();
  });
});

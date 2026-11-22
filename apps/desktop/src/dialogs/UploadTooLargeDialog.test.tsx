import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { baseTransport } from "../test/fixtures";
import { renderWithStore } from "../test/render";
import { UploadTooLargeDialog } from "./UploadTooLargeDialog";

describe("UploadTooLargeDialog", () => {
  it("explains that the world couldn't be uploaded and offers Google Drive, Cloudflare R2, or Cancel", async () => {
    const u = userEvent.setup();
    const onClose = vi.fn();
    await renderWithStore(
      <UploadTooLargeDialog
        instanceName="Big Modpack"
        detail="this world is too large and could not be uploaded to our servers (12.00 GB needed, server limit is 10.00 GB)"
        onClose={onClose}
      />,
      baseTransport(),
    );

    expect(screen.getByRole("dialog", { name: "World couldn't be uploaded to our servers" })).toBeInTheDocument();
    expect(screen.getByText(/Big Modpack is too large for PackSync Cloud's 10 GB server limit/)).toBeInTheDocument();
    expect(screen.getAllByText("Medium difficulty")).toHaveLength(2);
    expect(screen.getByRole("button", { name: "Set up Google Drive" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Set up Cloudflare R2" })).toBeInTheDocument();

    await u.click(screen.getByRole("button", { name: "Set up Google Drive" }));
    expect(window.location.hash).toBe("#/settings/google");
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("navigates to Cloudflare R2 setup when chosen", async () => {
    const u = userEvent.setup();
    const onClose = vi.fn();
    await renderWithStore(<UploadTooLargeDialog onClose={onClose} />, baseTransport());

    await u.click(screen.getByRole("button", { name: "Set up Cloudflare R2" }));
    expect(window.location.hash).toBe("#/settings/r2");
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("cancels cleanly and notifies the user", async () => {
    const u = userEvent.setup();
    const onClose = vi.fn();
    const { store } = await renderWithStore(<UploadTooLargeDialog instanceName="Skyblock" onClose={onClose} />, baseTransport());

    await u.click(screen.getAllByRole("button", { name: "Cancel upload" })[0]!);
    expect(onClose).toHaveBeenCalledTimes(1);
    expect(store.getState().toasts.some((t) => t.message.includes("Upload cancelled"))).toBe(true);
  });
});

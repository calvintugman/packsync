import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { baseTransport } from "../test/fixtures";
import { renderWithStore } from "../test/render";
import { AboutScreen } from "./About";

describe("AboutScreen", () => {
  it("renders avatar, profile handle, bio, and PackSync repository link", async () => {
    const u = userEvent.setup();
    const t = baseTransport();
    t.on("app.open_url", () => ({ opened: true }));

    await renderWithStore(<AboutScreen />, t);

    const avatar = screen.getByRole("img", { name: "calvintugman avatar" });
    expect(avatar).toHaveAttribute("src", "/dev-avatar.jpg");
    expect(screen.getByRole("heading", { name: "calvintugman" })).toBeInTheDocument();
    expect(screen.getByText("Data Science student")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "GitHub" })).toHaveAttribute("href", "https://github.com/calvintugman");
    expect(screen.getByRole("link", { name: /PackSync/ })).toHaveAttribute("href", "https://github.com/calvintugman/packsync");

    await u.click(screen.getByRole("link", { name: /PackSync/ }));
    await waitFor(() =>
      expect(t.calls.find((c) => c.method === "app.open_url")?.params).toEqual({
        url: "https://github.com/calvintugman/packsync",
      }),
    );
  });
});

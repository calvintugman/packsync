import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { ApiError } from "../api/errors";
import { ErrorNote } from "./ErrorNote";

const err = (code: string, message = "m", advice: string | null = null) => new ApiError({ code, message, advice });

describe("ErrorNote", () => {
  it("says what happened and what to do", () => {
    render(<ErrorNote error={err("quota_exceeded", "Drive is full", "Free some space.")} />);
    expect(screen.getByRole("alert")).toHaveTextContent("Google Drive is full");
    expect(screen.getByRole("alert")).toHaveTextContent("Drive is full");
    expect(screen.getByRole("alert")).toHaveTextContent("Free some space.");
  });

  it("offers 'Try again' only for problems that may pass by themselves", async () => {
    const u = userEvent.setup();
    const retry = vi.fn();
    const { rerender } = render(<ErrorNote error={err("offline")} onRetry={retry} />);
    await u.click(screen.getByRole("button", { name: "Try again" }));
    expect(retry).toHaveBeenCalledTimes(1);
    rerender(<ErrorNote error={err("corrupt_remote")} onRetry={retry} />);
    expect(screen.queryByRole("button", { name: "Try again" })).toBeNull();
  });

  it("offers to sign in again when the sign-in is the problem", async () => {
    const u = userEvent.setup();
    const signIn = vi.fn();
    render(<ErrorNote error={err("auth_revoked", "revoked")} onSignIn={signIn} />);
    await u.click(screen.getByRole("button", { name: "Sign in again" }));
    expect(signIn).toHaveBeenCalled();
  });

  it("shows extra actions the situation calls for", () => {
    render(<ErrorNote error={err("offline")} actions={<button>Play offline</button>} />);
    expect(screen.getByRole("button", { name: "Play offline" })).toBeInTheDocument();
  });

  it("shows nothing for no error", () => {
    const { container } = render(<ErrorNote error={null} />);
    expect(container).toBeEmptyDOMElement();
  });
});

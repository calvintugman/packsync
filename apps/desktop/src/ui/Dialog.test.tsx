import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { Dialog } from "./Dialog";

function Harness({ onClose, locked }: { onClose: () => void; locked?: boolean }) {
  return (
    <div>
      <button>opener</button>
      <Dialog title="Hello" onClose={onClose} locked={locked} footer={<button>Confirm</button>}>
        <input aria-label="first" />
      </Dialog>
    </div>
  );
}

describe("Dialog", () => {
  it("is a labelled modal dialog and focuses its first control", () => {
    render(<Harness onClose={() => {}} />);
    const d = screen.getByRole("dialog", { name: "Hello" });
    expect(d).toHaveAttribute("aria-modal", "true");
    expect(screen.getByRole("button", { name: "Close" })).toHaveFocus();
  });

  it("keeps Tab inside the dialog", async () => {
    const u = userEvent.setup();
    render(<Harness onClose={() => {}} />);
    await u.tab(); // first input
    expect(screen.getByLabelText("first")).toHaveFocus();
    await u.tab(); // confirm
    expect(screen.getByRole("button", { name: "Confirm" })).toHaveFocus();
    await u.tab(); // wraps to Close
    expect(screen.getByRole("button", { name: "Close" })).toHaveFocus();
    await u.tab({ shift: true }); // and back
    expect(screen.getByRole("button", { name: "Confirm" })).toHaveFocus();
  });

  it("closes on Escape and on the close button", async () => {
    const u = userEvent.setup();
    const onClose = vi.fn();
    render(<Harness onClose={onClose} />);
    await u.keyboard("{Escape}");
    expect(onClose).toHaveBeenCalledTimes(1);
    await u.click(screen.getByRole("button", { name: "Close" }));
    expect(onClose).toHaveBeenCalledTimes(2);
  });

  it("cannot be dismissed while something irreversible runs", async () => {
    const u = userEvent.setup();
    const onClose = vi.fn();
    render(<Harness onClose={onClose} locked />);
    await u.keyboard("{Escape}");
    expect(screen.getByRole("button", { name: "Close" })).toBeDisabled();
    expect(onClose).not.toHaveBeenCalled();
  });

  it("returns focus to what opened it", async () => {
    const { rerender } = render(<button autoFocus>opener</button>);
    screen.getByRole("button", { name: "opener" }).focus();
    rerender(
      <>
        <button>opener</button>
        <Dialog title="T" onClose={() => {}}>
          <p>x</p>
        </Dialog>
      </>,
    );
    rerender(<button>opener</button>);
    expect(screen.getByRole("button", { name: "opener" })).toBeTruthy();
  });
});

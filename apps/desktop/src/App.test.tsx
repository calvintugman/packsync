import { act, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { App } from "./App";
import { ApiError } from "./api/errors";
import { StoreProvider } from "./state/context";
import { AppStore } from "./state/store";
import { baseTransport, instance, storage, withPhase } from "./test/fixtures";

afterEach(() => {
  window.location.hash = "";
});

function mount(t = baseTransport()) {
  const store = new AppStore(t, { pollMs: 0 });
  render(
    <StoreProvider store={store}>
      <App />
    </StoreProvider>,
  );
  return { store, t };
}

describe("App", () => {
  it("shows the instances once everything is loaded", async () => {
    mount();
    expect(await screen.findByRole("heading", { name: "Instances" })).toBeInTheDocument();
    expect(screen.getByRole("article", { name: "Skyblock Pack" })).toBeInTheDocument();
    expect(screen.getByRole("navigation", { name: "Main" })).toBeInTheDocument();
  });

  it("explains why it cannot start and offers to try again", async () => {
    const t = baseTransport();
    t.on("app.info", () => {
      throw new ApiError({ code: "bridge_unreachable", message: "The PackSync development bridge is not reachable.", advice: "Is `packsync dev-bridge` still running?" });
    });
    mount(t);
    expect(await screen.findByRole("heading", { name: "PackSync can't start" })).toBeInTheDocument();
    expect(screen.getByText(/bridge is not reachable/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Try again" })).toBeInTheDocument();
  });

  it("starts with the setup when nothing is connected and nothing is synced", async () => {
    const t = baseTransport({ instances: [], storage: storage({ kind: "none", connected: false, detail: "Not connected" }) });
    mount(t);
    expect(await screen.findByRole("heading", { name: "Your modpacks, on every computer" })).toBeInTheDocument();
    expect(window.location.hash).toBe("#/setup");
  });

  it("does not drag a returning user into the setup", async () => {
    mount(baseTransport({ storage: storage({ kind: "none", connected: false }) }));
    expect(await screen.findByRole("heading", { name: "Instances" })).toBeInTheDocument();
    expect(window.location.hash).not.toBe("#/setup");
  });

  it("counts the instances that need a decision next to 'Instances'", async () => {
    mount(baseTransport({ instances: [withPhase("conflict"), instance({ id: "22222222-2222-4222-8222-222222222222", name: "Other" })] }));
    const nav = await screen.findByRole("navigation", { name: "Main" });
    expect(nav).toHaveTextContent("Instances1");
  });

  it("tells the person when it lost contact, and recovers", async () => {
    const { t } = mount();
    await screen.findByRole("heading", { name: "Instances" });
    await act(async () => t.setConnection("lost"));
    expect(await screen.findByText("Lost contact with PackSync")).toBeInTheDocument();
    await act(async () => t.setConnection("live"));
    expect(screen.queryByText("Lost contact with PackSync")).toBeNull();
  });

  it("shows where the cloud connection stands in the sidebar", async () => {
    mount(baseTransport({ storage: storage({ kind: "google", account: "me@example.com", detail: "me@example.com" }) }));
    const foot = await screen.findByLabelText("Connection");
    expect(foot).toHaveTextContent("Google Drive");
    expect(foot).toHaveTextContent("me@example.com");
  });
});

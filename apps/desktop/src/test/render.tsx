import { render } from "@testing-library/react";
import type { ReactElement } from "react";
import { StoreProvider } from "../state/context";
import { AppStore } from "../state/store";
import type { FakeTransport } from "./fakeTransport";

/** Render inside a store that talks to a fake application layer; waits for the first load. */
export async function renderWithStore(ui: ReactElement, transport: FakeTransport, opts: { pollMs?: number } = {}) {
  const store = new AppStore(transport, { pollMs: opts.pollMs ?? 0 });
  await store.start();
  const view = render(<StoreProvider store={store}>{ui}</StoreProvider>);
  return { store, transport, ...view };
}

import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { NoBridge } from "./NoBridge";
import { createTransport } from "./api/transport";
import { StoreProvider } from "./state/context";
import { AppStore } from "./state/store";
import "./styles/tokens.css";
import "./styles/base.css";
import "./styles/components.css";

const root = document.getElementById("root");
if (!root) throw new Error("#root is missing");

const transport = createTransport();
if (!transport) {
  createRoot(root).render(<NoBridge />);
} else {
  // The token was read from the address; drop it from the bar so it is not copied around by accident.
  if (transport.kind === "http" && window.location.hash.includes("token=")) {
    window.history.replaceState(null, "", window.location.pathname + "#/");
  }
  const store = new AppStore(transport);
  createRoot(root).render(
    <StrictMode>
      <StoreProvider store={store}>
        <App />
      </StoreProvider>
    </StrictMode>,
  );
}

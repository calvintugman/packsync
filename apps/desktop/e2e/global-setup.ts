import { execFileSync } from "node:child_process";
import { existsSync } from "node:fs";
import { CLI_BIN, DESKTOP_DIR, DIST, E2E_BIN, REPO_ROOT } from "./paths";

/** Make sure the UI is built and the Rust test world exists. CI builds them first and sets the skip flag. */
export default function globalSetup(): void {
  if (!process.env.PACKSYNC_E2E_SKIP_BUILD) {
    if (!existsSync(DIST)) {
      execFileSync("npx", ["vite", "build"], { cwd: DESKTOP_DIR, stdio: "inherit" });
    }
    execFileSync("cargo", ["build", "-p", "packsync-e2e", "-p", "packsync-cli"], { cwd: REPO_ROOT, stdio: "inherit" });
  }
  for (const p of [E2E_BIN, CLI_BIN, DIST]) {
    if (!existsSync(p)) throw new Error(`${p} does not exist; build it first (see CONTRIBUTING.md)`);
  }
}

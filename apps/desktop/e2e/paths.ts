import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));

export const DESKTOP_DIR = resolve(here, "..");
export const REPO_ROOT = resolve(DESKTOP_DIR, "..", "..");
export const DIST = join(DESKTOP_DIR, "dist");
export const E2E_BIN = process.env.PACKSYNC_E2E_BIN ?? join(REPO_ROOT, "target", "debug", "packsync-e2e");
export const CLI_BIN = process.env.PACKSYNC_CLI_BIN ?? join(REPO_ROOT, "target", "debug", "packsync");

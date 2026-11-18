import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const EXE = process.platform === "win32" ? ".exe" : "";

export const DESKTOP_DIR = resolve(here, "..");
export const REPO_ROOT = resolve(DESKTOP_DIR, "..", "..");
export const DIST = join(DESKTOP_DIR, "dist");
const BIN_DIR = process.env.PACKSYNC_BIN_DIR ?? join(REPO_ROOT, "target", "debug");
export const E2E_BIN = process.env.PACKSYNC_E2E_BIN ?? join(BIN_DIR, `packsync-e2e${EXE}`);
export const CLI_BIN = process.env.PACKSYNC_CLI_BIN ?? join(BIN_DIR, `packsync${EXE}`);

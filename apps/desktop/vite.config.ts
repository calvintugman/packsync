import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

// The UI is a static single-page app: Tauri loads `dist/`, and the development bridge
// (`packsync dev-bridge --ui-dir dist`) serves the same files for browser tests.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: { port: 1420, strictPort: true, host: "127.0.0.1" },
  build: { outDir: "dist", emptyOutDir: true, target: "es2022", sourcemap: false },
  test: {
    environment: "jsdom",
    globals: false,
    setupFiles: ["src/test/setup.ts"],
    include: ["src/**/*.test.{ts,tsx}"],
    css: false,
  },
});

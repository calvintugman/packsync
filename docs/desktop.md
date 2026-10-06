# The desktop app

A [Tauri 2](https://tauri.app/) shell around a React/TypeScript UI. Both are thin: all behaviour lives in
`packsync-app` (the application layer) and `packsync-core` (the engine), which the command-line tool uses too.

```
┌──────────────────────────── desktop window (system web view) ─────────────────────────────┐
│  React UI  ── typed API (src/api) ──┐                                                       │
│    screens · dialogs · store        │  Tauri commands: call(method, params)                 │
│                                     ├─ start_job(method, params) → job id                   │
│                                     └─ event "packsync://event"  ◄── application events     │
└─────────────────────────────────────────────┬───────────────────────────────────────────────┘
                                              │ same process
                              packsync-app::rpc (JSON method table) → Engine → storage, launcher
```

## How the UI talks to the engine

* **`packsync-app::rpc`** is a table of ~48 JSON methods (`app.info`, `instance.sync`, `restore.apply`,
  `conflict.resolve`, `hooks.plan_install`, …). Short methods return their result. **Long** methods
  (anything that reads many files or the network) run as **jobs**: the call returns a job id at once;
  progress and the final result arrive as events; a job can be cancelled.
* **`src/api/methods.ts`** is the typed copy of that table (parameters and result of every method, and which
  ones are long). A browser test compares it with the server's own list, so the two cannot drift apart.
  The DTOs in `src/api/types.ts` mirror the engine's `serde` types.
* **Transports** (`src/api/transport.ts`): `TauriTransport` (`invoke` + `listen`) in the desktop app;
  `HttpTransport` against the **development bridge** for browser tests and UI development. The bridge
  (`packsync dev-bridge`, hidden) binds `127.0.0.1`, needs a random token on every call (handed over in the
  URL fragment, never sent to a server) and checks the `Host` header. It is **not** part of the shipped app.
* **The store** (`src/state/store.ts`) holds the last known state, applies events, polls slowly for what other
  processes (launcher hooks, the CLI) change, and turns jobs into promises. It never guesses state: what the
  engine reports is what is shown. It was written test-first against races (an event arriving before the
  start call returns, a stale job listing arriving after the finished event, a dropped event stream).
* Errors cross as `{code, message, advice}` and are rendered with what happened and what to do.

## What is on screen

| Screen | Purpose |
|---|---|
| **Setup** (first run) | Choose Google Drive (visible folder or hidden app data) or a folder; Google sign-in with the OAuth-client explanation when none is configured; pick instances. |
| **Instances** | One card per synced instance with its sync *phase* and, while unfinished, the four-stage journey *saved here → uploading → published → ready on your other computers*; Play, Sync now; "Not syncing yet" list. |
| **Instance** | Status, local changes, History (restore with preview), *What is synced* (rules + "why is this file (not) synced?"), Launcher hooks (preview, install, repair, remove), Recovery points. |
| **Dialogs** | Preview-and-enable, conflict (three choices), crash (keep / go back), restore (preview, skip incompatible files), bring-from-cloud, hook plans, cloud cleanup (plan, then exactly that plan). |
| **Cloud** | Everything stored in the cloud; bring an instance to this computer. |
| **Activity** | Running and finished operations (cancel), recent events. |
| **Settings** | Computer name, cloud connection (test, change, disconnect), Prism location, behaviour when starting from Prism, advanced, cleanup, diagnostics (copy a redacted report). |

Accessibility: semantic HTML, labelled dialogs with a focus trap and Escape, keyboard-reachable everywhere,
`prefers-color-scheme` light and dark, `prefers-reduced-motion`, and WCAG 2.x A/AA checks (axe-core) in the
browser suite for every screen and dialog in both colour schemes.

## Running it

```sh
cd apps/desktop
npm ci

npm run tauri dev                      # the desktop app with hot reload (needs the system web-view libraries)

# the UI alone, in a browser, against a real engine:
cargo run -p packsync-cli -- dev-bridge --ui-dir apps/desktop/dist     # prints http://127.0.0.1:PORT/#token=…
```

`dev-bridge` uses **your real** PackSync settings and Prism. To play in a sandbox instead, build the test world
(two simulated computers, a shared cloud folder, a fake Prism and game — nothing real is touched):

```sh
cargo run -p packsync-e2e -- --dir /tmp/world --ui-dir apps/desktop/dist --cli target/debug/packsync [--connected]
cargo run -p packsync-e2e -- --prepare-only --dir /tmp/world --connected --enable "Skyblock Pack"
# then run the real app or CLI against it:
PACKSYNC_CONFIG_DIR=/tmp/world/desktop/app/config PACKSYNC_STATE_DIR=/tmp/world/desktop/app/state \
  apps/desktop/src-tauri/target/release/packsync-desktop
```

## Building and packaging

```sh
npm run tauri build -- --no-bundle     # just the binary: src-tauri/target/release/packsync-desktop
npm run tauri build                    # installers for this platform (deb, rpm, AppImage / nsis, msi / dmg, app)
```

* The shell is a **separate Cargo workspace** (`src-tauri/`), so building and testing the engine never needs
  the system web-view libraries.
* **Launcher hooks need the command-line tool.** For packaging, build `packsync` and let Tauri ship it next
  to the app as a sidecar: copy `target/release/packsync` to `src-tauri/binaries/packsync-<target-triple>`
  (`rustc -vV` prints the triple) and build with `--config src-tauri/tauri.bundle.conf.json`. The app finds a
  `packsync` next to its own executable; otherwise Settings can name one.
* **No installers are published and none are signed.** Code signing and notarisation need certificates that
  belong to whoever distributes the build. The `identifier` in `tauri.conf.json` is a placeholder
  (`org.example.packsync`); change it, the name and the icon (`src-tauri/icons/source.svg`) together — see
  [`rebranding.md`](rebranding.md).
* **What was actually built here (Linux):** the plain release binary, and `deb` and `rpm` packages with the
  command-line tool as the sidecar (`npm run tauri build -- --bundles deb,rpm --config src-tauri/tauri.bundle.conf.json`).
  The deb's contents were listed — `/usr/bin/packsync-desktop`, `/usr/bin/packsync` (next to it, where the app
  looks), icons and a `.desktop` entry — but the packages were **not installed or run**, and AppImage, `nsis`,
  `msi` and `dmg` were not attempted (they need their platform's tools; the manual `release-build` workflow is
  meant for that). Nothing is signed.

## Tests

| Suite | What it proves |
|---|---|
| `npm test` (Vitest + Testing Library, ~110 tests) | formatting and wording, phase mapping, error mapping, transports (token handling, failures, event stream), the store (loading, jobs, event races, polling), dialogs (focus trap), the instance card, conflict/crash/restore/hooks dialogs (they send exactly what the person chose), storage setup, the app shell. |
| `npm run e2e` (Playwright, Chromium, ~35 tests) | the **real UI against the real engine** in a test world: first run, a full journey across two computers, Play, conflicts (all three resolutions), crashes (both decisions), restore with preview and recovery points, hooks (install, chain, repair, remove), bring-from-cloud, settings, authentication of the bridge, accessibility (light, dark, dialogs). |
| Real shell | The release binary was started on Linux (WebKitGTK) against a prepared world and driven by keyboard through the compositor: *Sync now* (a job through `start_job`) and the preview dialog worked; the screenshot in `docs/images/desktop-app.png` is that window. This is manual — there is no automated test of the Tauri shell itself (the WebDriver tooling it needs is not available on every distribution). |

`SCREENSHOTS=1 npx playwright test e2e/screenshots.spec.ts` regenerates the pictures in `docs/images/`.

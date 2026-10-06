# Testing and status

## Run it

```sh
cargo test --workspace                                   # everything that needs no credentials, Prism or network
cargo test -p packsync-cli --features failpoints --test killed    # + real process kills at every transaction boundary

cd apps/desktop && npm ci
npm run typecheck && npm run lint && npm test            # UI unit and component tests
npm run build && npx playwright install chromium
npm run e2e                                              # the real UI against the real engine (builds the test world)

scripts/prism-smoke.sh                                   # the REAL Prism Launcher (Linux; manual, see below)
cargo build -p packsync-drive --features selftest        # live Google self-test (manual, never in CI; docs/live-validation.md)
cargo run -p packsync-bench --release -- … run|chunking  # benchmarks (docs/performance.md)
```

Development tests use generated fixtures and fakes only: no test touches a real Minecraft installation, your
Prism data or a Google account. Two Rust tests are `#[ignore]`d on purpose because they need the real thing:
the real-Prism smoke test and the real OS keyring round trip.

## What the tests establish

| Layer | Suites | What they establish |
|---|---|---|
| **Core engine** (`packsync-core`) | 131 unit tests + `journeys`, `crash`, `faults`, `hostile`, `sessions`, `concurrency`, `gc` | journeys on two computers (create, delete, rename, Unicode and long paths, conflicts and every resolution, restore on a second computer, catching up by changed blocks only); a crash or kill at **every** snapshot, upload and restore boundary (in process), and recovery that itself crashes; network faults (backoff, auth, quota, an ambiguous commit, listing lag); hostile data (corrupt/missing/bomb manifests, traversal, symlinks, case and Windows-name collisions, secrets never uploaded); sessions, leases, crash quarantine; simultaneous uploads; cleanup (grace period, tags, retention, merging, plan id) |
| **Drive adapter** (`packsync-drive`) | unit + `conformance`, `errors`, `uploads`, `duplicates_and_keys`, `oauth`, `engine` | the storage contract, retries, 401 and revocation, resumable uploads (including restart-resume), duplicate names, the OAuth flow and **secret hygiene** (no token in any log, error or `Debug` output) — **against a mock server**; the full sync engine through the adapter |
| **Prism adapter** (`packsync-prism`) | 62 unit tests + the fake-launcher testkit | ports of Qt's INI reader/writer and `QProcess::splitCommand`, discovery, registration, hook plan/apply/remove round trips (byte exact), partial installs, launch command |
| **Application layer** (`packsync-app`) | `launch`, `rpc` | launch orchestration with a fake launcher and fake game, hook entry points, offline policy, crash flows; the JSON API as the UI uses it; the bridge (token, host check, event stream) |
| **CLI** (`packsync-cli`) | `cli`, `hooks`, `killed` | the real binary end to end, `--json` and exit codes, hooks executed the way Prism executes them, **real process kills** |
| **Desktop UI, unit** | Vitest + Testing Library | formatting and wording, phase and error mapping, transports (token handling, failures, event stream), the store (loading, jobs, event races, polling), dialogs (focus trap), instance card, conflict/crash/restore/hooks dialogs (they send exactly what the person chose), storage setup, the app shell |
| **Desktop UI, browser** | Playwright against the **real engine** | first run, a journey across two computers, Play, conflicts (all three resolutions), crashes (both decisions), restore with preview and recovery points, hooks (install, chain, partial repair, remove), bring-from-cloud, settings, bridge authentication, API-table contract, accessibility (axe-core, light and dark, dialogs) |
| **Real Prism 11.0.3** | `prism_real` (ignored) via `scripts/prism-smoke.sh` | 22 checks: hook semantics, quoting and substitution, forwarding to a running Prism, install → launch → uninstall across Prism's own rewrite of `instance.cfg`, the crash flow, a restored instance launching, `packsync launch` through the real launcher — **all pass (2026-10-06, Linux)** |

### Counts when this was written (Linux, one machine)

| Suite | Result |
|---|---|
| `cargo test --workspace` | **358 passed, 0 failed, 2 ignored** |
| `cargo test -p packsync-cli --features failpoints --test killed` | 4 passed |
| UI unit and component tests | **108 passed** (14 files) |
| Playwright (Chromium) | **33 passed** (+7 screenshot generators that run only with `SCREENSHOTS=1`) |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean (also with `--features packsync-cli/failpoints`) |
| `cargo deny check` (engine workspace and desktop shell) | advisories, bans, licences and sources pass; one documented advisory exception in the shell (`docs/dependencies.md`) |
| `npm audit` | 0 vulnerabilities |
| `actionlint` on `.github/workflows/*.yml` | clean |

## The desktop shell on a real window

The Tauri shell has no automated test of its own: the usual route (`tauri-driver` + WebKitWebDriver) was not
available on the development machine. What was done instead, manually, on Linux (Hyprland/Wayland, WebKitGTK):
the **release binary was started against a prepared world** (`packsync-e2e --prepare-only`), the window was
screenshotted (`docs/images/desktop-app.png`), and it was driven **by keyboard through the compositor** —
*Sync now* ran a real job through `start_job` and showed its outcome, and *Start syncing…* opened the preview
dialog with real data. That shows the IPC commands, events and dialogs work in the shipped shell. It is a
one-off, not a regression test. (The same React code is exercised thoroughly in Chromium against the real
engine.)

## Status — implemented / tested locally / tested against real services / not done

**Implemented and tested locally (no credentials):** the sync engine (scan, chunking, packs, manifests, upload,
reconcile, journaled restore, recovery, sessions and crash quarantine, conflict resolution, cleanup and
retention), the Google Drive adapter and OAuth flow (against a mock), the Prism adapter, the application layer
(settings, jobs, launch, hooks, JSON API), the command-line tool, the desktop UI, benchmarks, safe logging.

**Tested against a real service:** **Prism Launcher 11.0.3 on Linux** (smoke test above, in a disposable
sandbox with a fake account, no network and a wrapper instead of Java — Minecraft itself never ran). The
desktop shell was run on Linux.

**NOT tested against real services:**

* **Google Drive and Google OAuth.** No credentials were available. The procedure and a self-test binary are
  in `docs/live-validation.md`. In particular **unverified**: that `drive.file` files created on one computer
  are visible to the same client on another; whether a Google client secret may be omitted for desktop
  clients; how long a fresh listing lags.
* The **operating-system credential store** on this machine (the code path is covered by an in-memory store
  and an ignored live test).
* **Windows and macOS**: nothing was built or run there. The CI workflows are written and linted but have
  **never run**; the Windows and macOS jobs are marked non-blocking until they have passed once.
* A **GUI Prism** noticing a restored instance (the smoke test is headless).
* **Installers**: only the plain release binary of the desktop app was built; deb/rpm/AppImage/nsis/msi/dmg
  bundles were not produced, and nothing is signed.

**Not done:** end-to-end encryption; deleting history (v1 releases the *content* of old snapshots but keeps
their manifests); auto-update; other launchers; a repack of partly-used packs.

## Bugs the tests found (all fixed)

Found by the engine and integration tests: a game's generic main-class pattern made one instance's running
game block syncing of every other instance; a hook's own start-up closed the session it was about to end
cleanly (now a grace period); a killed app kept "owning" its session for 30 minutes (now owner-process aware);
concurrent session starts could create two sessions (now atomic per launch token); a crash right after a
restore's files were applied let the next sync undo the restore (recovery now records it); restoring a
snapshot that was only local failed confusingly (clear error); the engine's `ps=1` tag was rejected by the
Drive adapter (found by the first run against the Drive mock); catch-up restores re-downloaded whole region
files (now delta from disk).

Found while building the desktop app: the uploader **re-downloaded the index it had just uploaded** on every
session (6.3 MB for a 4.3 GB instance; found by the benchmark, fixed with a metered regression test); a
**stale job listing could bring a finished job back to "running"** in the UI store (found by a unit test of
the event races); a **half-replaced hook installation** was reported as "not installed" although one of the
two commands was still active (now `partial`, with a repair path); a Drive tag test still asserted the old
contract after the tag change (found by the first complete test run); the Prism crate's own tests did not
build on their own (`cargo test -p packsync-prism`: a missing dev-dependency feature); toasts covered a
dialog's buttons; scrollable code blocks were not keyboard-focusable and faint text had insufficient contrast
(both found by axe-core); dialogs focused a close button that an accidental Enter would press.

## Reading a failure

* A failing **journey/crash/hostile** test means a guarantee in `docs/consistency.md` was broken — treat it as
  a data-loss bug until proven otherwise.
* A failing **browser** test names the screen and the person-visible behaviour; the HTML report
  (`apps/desktop/playwright-report`) has a screenshot and a trace.
* `PACKSYNC_FAILPOINTS=<name>[:count]` (builds with `--features failpoints`) reproduces a crash boundary by hand.

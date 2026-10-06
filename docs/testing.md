# Testing and status

## Run it

```sh
cargo test --workspace                                   # everything that needs no credentials, Prism or network
cargo test -p packsync-cli --features failpoints         # + real process kills at every transaction boundary
scripts/prism-smoke.sh                                   # the REAL Prism Launcher (Linux; see docs/dev/prism-adapter notes below)
cargo build -p packsync-drive --features selftest        # live Google self-test (never run in CI; docs/live-validation.md)
cargo run -p packsync-bench --release -- … run|chunking  # benchmarks (docs/performance.md)
```

Core development and tests use generated fixtures and fakes only; no test touches a real Minecraft
installation, your Prism data or a Google account.

## What the tests establish

| Layer | Tests | Establish |
|---|---|---|
| Core engine (`packsync-core`) | unit tests + `journeys`, `crash`, `faults`, `hostile`, `sessions`, `concurrency`, `gc` | journeys on two computers; crash/kill at every snapshot, upload and restore boundary (in process), recovery that itself crashes; network faults (backoff, auth, quota, ambiguous commit, listing lag); hostile data (corrupt/missing/bomb manifests, traversal, symlinks, case/Windows collisions, secrets never uploaded); sessions, leases, crash quarantine; simultaneous uploads; cleanup (grace period, tags, retention, merge, plan id) |
| Drive adapter | unit + `conformance`, `errors`, `uploads`, `duplicates_and_keys`, `oauth`, `engine` | the storage contract, retries/401/revocation, resumable uploads and restart-resume, duplicates, OAuth flow + secret hygiene — **against a mock server**; the full sync engine through the adapter |
| Prism adapter | unit + fake-launcher testkit | Qt INI/`splitCommand` ports, discovery, registration, hook plan/apply/remove round trips, launch command |
| App layer | `launch`, `rpc` | launch orchestration with a fake launcher and fake game, hook entry points, offline policy, crash flows; the JSON API as the UI uses it; the dev bridge (token, host check, SSE) |
| CLI | `cli`, `hooks`, `killed` | the real binary end to end, JSON/exit codes, hooks executed the way Prism executes them, real process kills |
| **Real Prism 11.0.3** | `prism_real` (ignored) via `scripts/prism-smoke.sh` | 22 checks: hook semantics, forwarding, install→launch→uninstall, crash flow, restored instance launches, `packsync launch` through the real launcher — **all pass (2026-10-06, Linux)** |

## Status — implemented / tested locally / tested against real services / future

**Implemented and tested locally (no credentials):** the sync engine (scan, chunking, packs, manifests, upload,
reconcile, journaled restore, recovery, sessions and crash quarantine, conflict resolution, cleanup/retention),
the Drive adapter and OAuth flow (against a mock), the Prism adapter, the application layer (settings, jobs,
launch, hooks, JSON API), the full CLI, benchmarks, safe logging.

**Tested against a real service:** **Prism Launcher 11.0.3 on Linux** (smoke test above, in a disposable
sandbox with a fake account, no network, a wrapper instead of Java — Minecraft itself never ran).

**NOT tested against real services:** **Google Drive and Google OAuth** (no credentials were available; the
procedure and a self-test binary are in `docs/live-validation.md`; the claim that `drive.file` works across
devices is unverified), the OS keyring on this machine, **Windows and macOS** (nothing was built or run there),
a GUI Prism (instance appearing after a rename into a running launcher).

**Not done yet:** the Tauri/React desktop app (UI, Playwright E2E, screenshots, packaging config), `README.md`,
`CONTRIBUTING.md`, issue templates, GitHub Actions workflows (no CI has ever run — none is claimed),
sample config files, a dependency/licence list and `cargo deny` gate, the independent code review, large
benchmark completion and the index-re-download fix (`docs/performance.md`), history *deletion* (v1 releases the
content of old snapshots but keeps their manifests), end-to-end encryption.

## Bugs the tests found (fixed)

A game's generic main-class pattern made one instance's running game block syncing of every other instance;
a hook's own start-up closed the session it was about to end cleanly (now a grace period); a killed app kept
"owning" its session for 30 minutes (now owner-process aware); concurrent session starts could create two
sessions (now atomic per launch token); a crash right after a restore's files were applied let the next sync
undo the restore (recovery now records it); restoring a snapshot that was only local failed confusingly (clear
error); the engine's `ps=1` tag was rejected by the Drive adapter (found by the first run against the Drive
mock); catch-up restores re-downloaded whole region files (now delta from disk).

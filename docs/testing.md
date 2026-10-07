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
| **Real Prism 11.0.3** | `prism_real` (ignored) via `scripts/prism-smoke.sh` | 20 checks: hook semantics, quoting and substitution, forwarding to a running Prism, install → launch → uninstall across Prism's own rewrite of `instance.cfg`, the crash flow, a restored instance launching, `packsync launch` through the real launcher — **all pass (re-run on the final code, 2026-10-07, Linux, from a fresh sandbox)** |

### The thirteen required scenarios and the tests that cover them

| # | Scenario | Tests |
|---|---|---|
| 1 | Initial upload and second-device restore reproduce all included bytes and exact pack identity, keeping local-only settings | `journeys::first_computer_uploads_second_computer_restores_identical_bytes`; CLI `two_computers_continue_each_others_progress`; app `app_launch_saves_uploads_and_a_second_computer_receives_the_progress`; Drive `the_sync_journeys_work_through_the_drive_adapter`; Prism hook/`instance.cfg` projection tests; Playwright *journey* and *cloud* |
| 2 | No-change sync uploads no content; sparse changes transfer only what is new | `journeys::no_change_sync_uploads_no_content_and_sparse_edits_transfer_little`, `catching_up_after_an_in_place_region_edit_downloads_only_the_changed_blocks`, `the_index_a_computer_uploaded_itself_is_not_downloaded_again`; benchmarks (`docs/performance.md`) |
| 3 | Same-size changes with coarse or preserved timestamps are found | `journeys::same_size_edit_with_restored_timestamp_is_detected`; scan unit tests `classify_covers_every_documented_reason`, `same_size_same_mtime_edit_is_found_by_ctime_during_a_real_scan` |
| 4 | Creation, deletion, rename, empty directories, Unicode, long paths, case collisions | `journeys::creation_deletion_rename_empty_dirs_unicode_and_long_paths_propagate`; `hostile::names_that_cannot_exist_on_the_target_platform_are_found_before_anything_is_touched`; path/collision unit tests |
| 5 | Offline changes on two devices keep both branches; resolution and later reconciliation destroy neither | `journeys::offline_changes_on_two_devices_become_a_conflict_and_both_versions_survive_each_resolution`; CLI `diverged_computers_are_never_merged_and_the_user_chooses`, `splitting_keeps_both_versions_as_two_instances`; Playwright *conflict* (all three resolutions) |
| 6 | Simultaneous uploads from one parent lose no branch; an unrelated instance cannot vanish in an index race | `concurrency::simultaneous_publications_from_one_parent_keep_both_branches`, `unrelated_instances_uploaded_at_the_same_time_all_survive`, `identical_content_is_deduplicated_across_instances`; `sessions::leases_are_advisory_stale_ones_are_ignored_and_simultaneous_starts_lose_nothing` |
| 7 | Interruption, timeout, 429/5xx, auth failure, quota failure and restart keep resumability and honest status | `faults.rs` (7 tests, e.g. `outage_mid_upload_leaves_honest_state_and_resumes_after_restart_without_reuploading`, `listing_lag_means_published_but_not_yet_ready_on_other_devices`); Drive `errors.rs` (15) and `uploads.rs` (12, resumable sessions); Playwright *offline* (offline, full Drive, revoked sign-in, cancel and retry) |
| 8 | Partial, missing or corrupt objects and manifests are rejected without touching the playable instance | `hostile::a_damaged_pack_in_the_cloud_is_rejected_and_the_instance_is_untouched`, `a_missing_pack_is_detected_at_restore_and_at_publish_time`, `garbage_bombs_and_foreign_objects_in_the_cloud_are_ignored_not_trusted`, `malicious_manifests_cannot_write_outside_the_instance_or_pass_without_data` |
| 9 | Termination during snapshot, upload, publish, restore and cleanup is recoverable; every destructive boundary tested | `crash::crash_at_every_snapshot_and_upload_boundary_is_resumable_and_never_exposes_a_partial_snapshot`, `crash_anywhere_in_an_in_place_restore_leaves_old_or_new_never_a_mix_and_converges`, `recovery_itself_can_crash_and_be_repeated`, `crash_during_new_directory_restore_leaves_no_half_instance`, `a_history_restore_interrupted_after_apply_still_sticks`; **real process kills** `killed.rs` (4); **cleanup** `gc::a_cleanup_killed_at_any_step_leaves_everything_restorable_and_can_simply_be_run_again` (six boundaries) |
| 10 | Live-world writers, duplicate jobs, stale leases, game crashes and watcher overflow never produce a claimed clean handoff | `sessions::nothing_mutates_an_instance_while_a_game_process_is_using_it`, `duplicate_hook_and_app_jobs_are_idempotent`, `a_stale_lease_is_ignored_while_a_fresh_one_only_warns`, `a_crashed_session_is_quarantined_never_promoted_automatically`, `a_session_killed_without_any_signal_is_found_after_restart_and_treated_as_unclean`; app `launch.rs` (12); Playwright *crash*. **Watcher overflow does not apply: v1 has no filesystem watcher** (`docs/architecture.md`); its safeguard — cached metadata is never trusted over a full hash when it could be stale — is the scan policy tested in scenario 3 |
| 11 | Retention keeps every reachable object, respects in-flight uploads, never deletes unrelated Drive files | `gc.rs` (8 tests); Drive `cleanup_works_on_drive_objects_tags_ages_and_trash`; CLI `cleanup_shows_a_plan_first_and_only_runs_exactly_that_plan` |
| 12 | Token redaction, path traversal, symlink escape, oversized manifests, decompression limits, malicious reconstruction mappings | Drive `oauth::no_secret_ever_appears_in_logs_errors_or_debug_output`; CLI `secrets_never_appear_in_json_output_or_diagnostics`; `hostile.rs` (8: traversal, symlinks, bombs, absurd sizes, credentials never uploaded); manifest and pack unit tests (overlapping/overflowing ranges) |
| 13 | The UI exposes real engine state: errors, cancel/retry, offline choice, conflicts; core journeys exercised through automated UI tests | Playwright (38 tests) against the real engine; Vitest (123) |

### Counts when this was written (Linux, one machine)

| Suite | Result |
|---|---|
| `cargo test --workspace` | **385 passed, 0 failed, 2 ignored** |
| `cargo test -p packsync-cli --features failpoints --test killed` | 4 passed |
| UI unit and component tests | **123 passed** (16 files) |
| Playwright (Chromium) | **38 passed** (+7 screenshot generators that run only with `SCREENSHOTS=1`) |
| `scripts/windows-check.sh` (Windows build, run under Wine) | **371 passed, 0 failed** in 22 test programs (the CLI hooks suite, which chains shell scripts, is Unix-only) |
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

## What the independent review found (all fixed, each with a test unless noted)

One reviewer read the code against these documents and the original requirements. Findings and fixes:

| | Finding | Fix |
|---|---|---|
| F1 | A snapshot could carry `.packsync/` (a forged restore journal, the ownership marker); startup recovery then moved files **outside** the instance | `.packsync` rejected in any path; journals validated before any rename; one bad journal no longer stops recovery of other instances |
| F2 | "Keep both as separate instances" published the fork without this computer's unique data | the branch is uploaded first; `uploaded` reflects reality |
| F3 | A computer back from a long break published progress whose unchanged files relied on data a retention cleanup had released | whole-tree completeness check before publishing; missing data uploaded again from this computer's files (`engine/repair.rs`) |
| F4 | A pack that vanished behind the local cache blocked every later upload, with an error naming a flag that did not exist | vanished packs are detected and forgotten; automatic repair; `verify --remote --repair` exists |
| F5 | A promoted crash save could be published before the crash save's data | the crash save is its parent; local dedup limited to the snapshot's own lineage |
| F6 | With lost index objects, cleanup planned to delete the head's only data | a pack no index describes is kept if its own footer shows needed data |
| F7 | "It will retry …" — nothing retried | the desktop app retries pending uploads while it is open |
| F8 | "Reinstall to repair" + uninstall deleted the user's own Prism command | receipts are merged; uninstall falls back to the chained command |
| F9 | A snapshot could set launch commands / Java path in `instance.cfg` | incoming projections re-filtered to portable keys |
| F10 | Wider rules in a cloud version deleted this computer's unsynced files (screenshots) | a restore never deletes what this instance does not sync |
| F11 | A game started during restore staging could be overwritten; a file that appeared was replaced silently | writer re-check right before applying; such files stop the restore |
| F12 | Renames were not flushed before "applied" was journaled | flushed per batch (not testable without cutting power) |
| F13 | Two silent-skip paths in staging | errors instead |
| F14 | Concurrent instance-row updates from two processes reverted each other | one IMMEDIATE transaction |
| F15 | Stale UI list responses; jobs with other parameters merged; incomplete Drive listings trusted; `--json` printed two documents on failure | each fixed |

## Reading a failure

* A failing **journey/crash/hostile** test means a guarantee in `docs/consistency.md` was broken — treat it as
  a data-loss bug until proven otherwise.
* A failing **browser** test names the screen and the person-visible behaviour; the HTML report
  (`apps/desktop/playwright-report`) has a screenshot and a trace.
* `PACKSYNC_FAILPOINTS=<name>[:count]` (builds with `--features failpoints`) reproduces a crash boundary by hand.

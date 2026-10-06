# Architecture

PackSync is a Rust workspace with a small, testable core and thin front ends. Nothing in the core knows
about Google, Prism, windows or terminals; those are adapters plugged in through two traits.

```
                       ┌──────────────────────────────┐
  desktop UI (React) ──┤  Tauri shell  ─┐             │
                       │                ├─ packsync-app ──────────────┐
  packsync (CLI) ──────┤                │  settings · providers ·     │
                       └────────────────┘  jobs · launch · hooks ·    │
                                           JSON API (rpc) · logging   │
                                                                      ▼
                                           packsync-core (the engine)
                                           ┌───────────────────────────────────────────────┐
                                           │ scan → snapshot → upload → reconcile → restore │
                                           │ sessions · journal/recovery · GC · rules       │
                                           │ SQLite state · pack/index/manifest formats      │
                                           └───────────┬─────────────────────┬──────────────┘
                                         Storage trait │                     │ Launcher trait
                                  ┌───────────────────┴──┐          ┌───────┴───────────────┐
                                  │ packsync-drive       │          │ packsync-prism        │
                                  │ Google Drive v3,     │          │ discovery · register  │
                                  │ OAuth, keyring       │          │ launch · hooks        │
                                  │ (+ local-dir, fake,  │          │ (+ generic launcher)  │
                                  │ fault-injecting)     │          └───────────────────────┘
                                  └──────────────────────┘
```

## Crates

| Crate | Responsibility |
|---|---|
| `packsync-core` | The engine. Public entry point: `engine::Engine`. Modules: `scan` (walk + change detection), `chunk`/`codec`/`pack` (content-defined/fixed chunking, compression, pack format), `manifest` (snapshot format), `snapshot`, `upload`, `remote` (cloud layout), `graph` (history DAG + reconcile decision), `restore` + `journal` (recoverable restore), `session`, `gc`, `rules`, `mixed` (mixed files), `db` (SQLite), `storage*` (trait + local/fault/meter providers), `testkit` (fixtures, harness, conformance suite — behind the `testkit` feature). |
| `packsync-drive` | `DriveStorage` (Drive v3 over blocking `ureq` + rustls), the installed-app OAuth flow (PKCE, loopback), keyring credential store, and a full **mock Drive/OAuth server** for tests and benchmarks. |
| `packsync-prism` | Prism Launcher adapter: instance discovery and validation, restore registration (atomic rename into the instances folder), launching (no shell), process detection, hook plan/apply/remove, a port of Qt's INI reader/writer and of `QProcess::splitCommand`, and a testkit with a fake `prismlauncher`. |
| `packsync-app` | Everything a front end needs without any UI: settings, provider wiring (local folder / Google), background **jobs** with progress events and cancellation, **launch orchestration**, hook installation, the **JSON API** (`rpc`), redacting logging, diagnostics, and the (dev-only) HTTP bridge. |
| `packsync-cli` | The `packsync` binary: argument grammar, rendering, confirmations, exit codes, the hook entry points. |
| `packsync-bench` | Reproducible benchmarks (`docs/performance.md`). |
| `apps/desktop` | The Tauri 2 shell and the React UI. It talks to `packsync-app::rpc` only. |

## The two extension points

* **`Storage`** (`storage.rs`): a flat key space with `list`/`stat`/`get(range)`/`put(CreateOnly|Replace)`/
  `delete`/`quota`, provider tags, and capability flags. It deliberately assumes **nothing** Drive does not
  give: no compare-and-swap, no unique names, no ordering, eventually consistent `list`. The engine is
  written against that contract; `testkit::conformance` is its executable definition and every provider (the
  local directory, the fault wrapper, `DriveStorage` against the mock, and manually real Drive) must pass it.
* **`Launcher`** (`launcher.rs`): discover/describe instances, contribute rules and mixed-file policies,
  register a restored instance safely, launch, and manage hooks. Prism is the first implementation; a
  second launcher means one new crate and no core change.

## Data flow of a play session

1. **Launch** (`app::launch`): close dead sessions → `reconcile` (fetch a newer cloud snapshot if this
   computer is clean) → `begin_session` → start the launcher → find the game process (cmdline/cwd under the
   instance, `-Djava.library.path=<root>/natives`) → heartbeat the advisory lease.
2. **End**: the game process disappears → `end_session` classifies the exit (exit status, crash reports and
   `hs_err` since the start, clean-exit signal) → **scan** (stat everything, hash what the cache cannot vouch
   for) → **snapshot** (chunk, compress, pack into `staging/`, write the manifest, one DB transaction) →
   **upload** (packs → index → verify → manifest → head record), resumable and idempotent.
3. **Next computer**: `reconcile` lists `commits/`, verifies the newest manifest, computes the history graph
   and decides: up to date / fast-forward / local ahead / conflict. A fast-forward is a journaled **restore**:
   plan → preflight → fetch only the needed blobs (unchanged blocks of replaced files come from disk) →
   verified assembly into a stage directory → apply with rollback.

## Concurrency model

* One process may run several jobs; each takes the **per-instance OS file lock** for mutations. A separate
  lock guards uploads so a long upload does not block a new snapshot. Uploads of different instances run in
  parallel; uploads within one use a bounded worker pool.
* All state is in SQLite (WAL, `synchronous=FULL`) plus files; the app, the CLI and launcher hooks are
  separate processes that coordinate through the database and locks. The **hook processes are short-lived**
  and open the database like any other client.
* Long operations take an `OpCtx` (progress sink + cancel token); cancellation is cooperative and always at a
  point where the on-disk state is consistent.

## Where state lives

| What | Where |
|---|---|
| Settings | `<config>/settings.json` (versioned, atomic writes; unknown keys ignored, invalid values rejected) |
| Database, staging, manifest cache, locks, signals, logs | `<state>/` |
| Refresh token | OS credential store (nothing on disk) |
| Per-instance bookkeeping | `<instance>/.packsync/` (ownership marker, transactions, recovery points; never synced) |
| Cloud | one root folder: `packs/ index/ instances/…` (`docs/remote-format.md`) |

## Testing architecture

Because the core depends only on the two traits, almost everything is tested without Google or Prism: a
multi-computer harness (several engines, one fake cloud), fault injection around any `Storage`, deterministic
crash points compiled in with the `failpoints` feature (in-process, and with real aborts in the CLI tests), a
hostile-data suite, a mock Drive/OAuth server, a fake `prismlauncher` plus game process — and, on top of
those, a smoke test that drives the **real Prism Launcher** in a disposable sandbox (`docs/testing.md`).

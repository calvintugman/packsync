# Command-line reference

`packsync` is the full interface: everything the desktop app does is available here, with stable
`--json` output and documented exit codes, so it can be scripted, run from launcher hooks, or used over SSH.

```
packsync [--json] [-q|--quiet] [-y|--yes] [-v|--verbose] [--config-dir DIR] [--state-dir DIR] <command> …
```

| Global option | Meaning |
|---|---|
| `--json` | Print **one JSON document** on stdout: `{"ok":true,"result":…}` or `{"ok":false,"error":{code,message,advice,exit_code}}`. Progress and hints go to stderr only. |
| `-q`, `--quiet` | No progress or hints on stderr. |
| `-y`, `--yes` | Answer yes to confirmations. **Required when stdin is not a terminal** — destructive commands never guess. |
| `-v`, `--verbose` | More detail in the log file (`<state>/logs/packsync.log`; secrets always redacted). |
| `--config-dir`, `--state-dir` | Where settings, and the database/staging area, live. Defaults: the platform config/state directories (`packsync doctor` prints them). Environment: `PACKSYNC_CONFIG_DIR`, `PACKSYNC_STATE_DIR`. |

Instances are named by their **ID** (or a unique prefix of at least 4 characters), their **name**
(case-insensitive) or the **path of their folder**. Snapshots by their ID or a unique prefix of at least
6 characters (see `packsync history`).

## Exit codes

| Code | Meaning | Typical advice |
|---:|---|---|
| 0 | OK | |
| 1 | Failure | read the message |
| 2 | Usage error | `--help` |
| 3 | Not found | instance/snapshot/object does not exist |
| 10 | Offline / storage temporarily unavailable / rate limited | retry; local data is safe |
| 11 | Authentication needed or revoked, permission problem | `packsync auth login` |
| 12 | **Conflict** — two versions need your decision | `packsync conflicts`, `packsync resolve` |
| 13 | Busy — another PackSync operation holds the instance | wait |
| 14 | A game (or another writer) is using the instance | close it |
| 15 | Local snapshot saved, cloud copy not published yet | `packsync upload` |
| 16 | Cloud data missing or damaged (`corrupt_remote`, `cloud_data_missing`) | `packsync verify INSTANCE --remote --repair` |
| 17 | Not enough disk space | free space |
| 18 | Cancelled (Ctrl-C) | |
| 19 | Safety guard (mass deletion, instance unavailable) **or a crash waiting for a decision** | `packsync crash show` |
| 20 | Google Drive is full | free space; the upload resumes |
| 30 | The game did not start | |

## Commands

### Look around

| Command | What it does |
|---|---|
| `status [INSTANCE] [--online]` | Sync state of one or all instances (local facts; `--online` also asks the cloud). Shows the storage connection too. |
| `instances` | Prism instances found on this computer and whether they are synced. |
| `cloud` | Instances stored in the cloud, and whether this computer has them. |
| `history INSTANCE [--offline]` | Snapshots: time, device, files, size, state, whether it is this computer's base / a cloud head / a crash save / expired. |
| `changes INSTANCE` | Local changes that are not in a snapshot yet. |
| `doctor [--online] [--out FILE]` | Environment checks and recent events; **secrets are redacted**; `--out` writes a file you can attach to a bug report. |
| `device [NAME]` | Show or set this computer's name (shown as the source device in history). |

### Start syncing

| Command | What it does |
|---|---|
| `preview PATH [--rules-file F] [--estimate-upload]` | What would be synced (by category), what is excluded and why, worlds found, problems. Changes nothing. |
| `enable PATH [--rules-file F] [--no-upload]` | Shows the preview, asks, takes the first snapshot and uploads it. |
| `disable INSTANCE` | Stop syncing here. Files and cloud data stay as they are. Versions saved on this computer but **not uploaded yet** are dropped from this computer's history (the files themselves stay); upload first (`packsync sync`) to keep them. |

### Day to day

| Command | What it does |
|---|---|
| `sync INSTANCE [--explain]` | Snapshot local changes, then bring this computer and the cloud in step. Exit 12 on a conflict, 15 if the upload is still pending. `--explain` adds where the time went: each step with its duration, HTTP requests and bytes, requests by kind, the costliest files (new data, bytes hashed) and the manifest size (also under `explain` in `--json`). |
| `snapshot INSTANCE [--force] [--note T] [--confirm-mass-delete]` | Save the current state locally. |
| `upload INSTANCE [--force]` | Upload saved snapshots (honours the persisted backoff unless `--force`). |
| `reconcile INSTANCE [--no-fast-forward] [--no-upload]` | Compare with the cloud: apply a newer version if this computer is clean, upload local snapshots, report conflicts. |
| `launch INSTANCE [--offline-player N] [--profile P] [--skip-sync] [--no-upload]` | Sync, start Prism, track the game process, save and upload when it ends. |
| `verify INSTANCE [--remote [--deep] [--repair [--discard-unrepairable]]] [--snapshot ID]` | Check local files against the last snapshot; with `--remote` check the cloud has everything (`--deep` downloads and hashes every object). `--repair` uploads again, from this computer's files, any data the cloud is missing (for that snapshot and for versions not uploaded yet) — nothing in the instance or its history changes. What this computer no longer has is listed; `--discard-unrepairable` then drops the versions that exist only here and can never be uploaded (files untouched; `packsync sync` saves them as they are). |

### Other computers, history, recovery

| Command | What it does |
|---|---|
| `restore-new [INSTANCE] [--snapshot ID] [--data-dir D] [--name N] [--java J] [--min-memory-mb M] [--max-memory-mb M]` | Bring a cloud instance here as a new Prism instance. Built hidden, verified, then renamed into place. |
| `restore INSTANCE SNAPSHOT [--preview] [--skip-incompatible]` | Restore a snapshot over an instance. Your current changes are saved first; replaced files are kept as a recovery point. |
| `recovery INSTANCE` | List recovery points (what restores replaced). |
| `conflicts INSTANCE` | Show the two versions of a diverged instance (exit 12). |
| `resolve INSTANCE keep-local \| keep-cloud [--head ID] \| split --name NEW` | Decide. Both versions stay in history. |
| `crash show \| promote \| discard INSTANCE` | Decide what to do with the save from a session that did not end cleanly. |

### Rules, hooks, storage

| Command | What it does |
|---|---|
| `rules show INSTANCE`, `rules explain INSTANCE PATH` | The effective rules; *why* a path is or is not synced. |
| `hooks status \| install \| uninstall INSTANCE [--cli PATH] [--plan]` | Optional Prism hooks (`docs/hooks.md`). `--plan` shows exactly what would change in `instance.cfg`. Refuses while Prism runs. |
| `storage status \| use-local DIR \| use-google [--space visible_folder\|app_data] \| test \| disconnect [--revoke]` | The storage connection. |
| `auth login [--no-browser] \| status \| logout [--revoke]` | Google sign-in (`docs/oauth-setup.md`). |
| `settings show`, `settings set KEY VALUE` | `device_name`, `hook_offline_policy` (`play_local`\|`block`), `hook_block_on_problem`, `upload_concurrency`, `recovery_point_days`, `lease_ttl_secs`, `cli_path`, `prism_executable`, `prism_data_dir`, `google_client_file`. |
| `gc plan [OPTIONS]`, `gc run PLAN_ID [OPTIONS]` | Cloud cleanup, manual only (below). |

### Cleanup (`gc`)

```sh
packsync gc plan                       # objects nothing needs (abandoned uploads), merged index objects
packsync gc plan --retain --keep-snapshots 20 --keep-days 30     # also release content of old snapshots
packsync gc run <plan-id> [same options]
```

`plan` changes nothing and prints a **plan id**. `run` recomputes the plan against the live cloud and
refuses (exit 12) if it no longer matches the id you were shown, then asks for confirmation. Only objects
PackSync created, older than `--grace-hours` (default 48, minimum 24), that no kept snapshot needs, are
deleted — never anything else in your Drive; Google Drive's trash keeps them for a while. Heads, branches,
crash saves (and their parents) and snapshots this computer has not uploaded yet are always kept.

## Hooks (internal)

`packsync hook pre-launch|post-exit --instance ID --marker packsync-hook [-- user command…]` is what Prism
runs once hooks are installed. It never fails a launch because *PackSync* is broken (it then just runs your
chained command), exits non-zero only on a deliberate decision (a conflict or a crash waiting for you, or the
strict offline policy), and always runs your own chained command with its exit status passed through.

## Environment

| Variable | Use |
|---|---|
| `PACKSYNC_CONFIG_DIR`, `PACKSYNC_STATE_DIR` | Override the default directories. |
| `PACKSYNC_GOOGLE_CLIENT_ID`, `PACKSYNC_GOOGLE_CLIENT_SECRET` | OAuth client (see `docs/oauth-setup.md`). |
| `PACKSYNC_LOG` | Log filter directives (`packsync_core=debug`); crates that can log credentials are pinned to `warn` and cannot be raised. |
| `PACKSYNC_LAUNCH_POLL_MS` | How often `launch` looks for the game process (default 500). |
| `PACKSYNC_FAILPOINTS` | Only in builds with `--features failpoints` (tests): inject a crash at a named boundary. |
| `PACKSYNC_TRANSFER_STATS`, `PACKSYNC_TRANSFER_LABEL` | Benchmarks: append one JSON line per run (wall-clock time, exit code, HTTP requests and bytes) to this file, labelled. |
| `PACKSYNC_PRISM_SEED` | Only for `scripts/prism-smoke.sh`: Prism data directory to copy read-only caches from. |

## JSON

With `--json`, success documents carry the engine's own types (`InstanceStatus`, `HistoryEntry`,
`SnapshotReport`, `ReconcileReport`, …); field names are `snake_case` and stable. Tagged enums use the
tag shown in the examples (`{"outcome":"fast_forwarded", …}`). Errors:

```json
{"ok":false,"error":{"code":"conflict","message":"…","advice":"Two devices changed this instance independently. …","exit_code":12}}
```

Every command prints exactly one JSON document. When a command produced a report before it failed (for
example `verify --remote` finding missing data), the report is included as `"result"` next to `"error"`.

Error `code` values: `io offline auth_required auth_revoked auth_scope quota_exceeded rate_limited
storage_unavailable storage_not_found storage_conflict storage_permission database invalid corrupt_remote
cloud_data_missing cancelled busy game_running conflict no_space instance_unavailable safety_guard not_found unsupported
config crash_pending internal`.

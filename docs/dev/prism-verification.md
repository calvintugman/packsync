# Prism Launcher — verified facts and design consequences

**Verification date:** 2026-10-06. **Version inspected:** Prism Launcher **11.0.3** (installed
here as `prismlauncher 11.0.3-2.1`, Arch). Authority: the 11.0.3 source tree
(`https://github.com/PrismLauncher/PrismLauncher/tree/11.0.3`, files named below) plus the
installed binary's `--help`. The public wiki
(<https://prismlauncher.org/wiki/getting-started/command-line-interface>) documents the CLI but
does **not** document hook semantics, environment variables, offline launch, the data-dir
environment variable or already-running-instance behaviour; the old
`getting-started/instance-settings/` URL returns 404. Everything below marked **[source]** was read
from code; items marked **[to test]** must be confirmed empirically by the smoke test
(`scripts/prism-smoke.sh`, results recorded in `docs/testing.md`).

## 1. Launch pipeline order [source: `launcher/minecraft/MinecraftInstance.cpp::createLaunchTask`]

`TextPrint` → `CreateGameFolders` → … → `MinecraftLoadAndCheck` (meta) → `AutoInstallJava` →
`CheckJava` → `VerifyJavaInstall` → **`PreLaunchCommand`** → `ClaimAccount` / update task
(libraries, assets) → `EnsureOfflineLibraries` → `ModMinecraftJar` → **`ScanModFolders`** →
`EnsureAvailableMemory` → `PrintInstanceInfo` → `ExtractNatives` → `ReconstructAssets` →
**`LauncherPartLaunch`** (the game) → **`PostLaunchCommand`** → `QuitAfterGameStop`.

Consequences: the pre-launch hook runs *before* Prism scans the mod folder, so a restore done by the
hook is picked up; it runs after Java verification, so a missing Java fails before our hook.

## 2. Pre-launch / post-exit semantics

* Command string handling [source: `launch/steps/PreLaunchCommand.cpp`]: Prism substitutes
  `$VAR` tokens in the whole string (`LaunchTask::substituteVariables`), then calls
  `QProcess::splitCommand(cmd)` — **no shell**, double quotes group arguments. The program is
  `args.takeFirst()`; working directory = game root.
  → Hook command lines must double-quote the executable path and avoid shell syntax. Spaces and
  Unicode in paths are legal when quoted. **[to test]** with a Unicode+space instance ID.
* **Nonzero exit of the pre-launch command fails the launch** [source: `PreLaunchCommand::on_state`
  → `emitFailed`; `LaunchTask::onStepFinished` → `finalizeSteps(false, …)`]: the game is *not*
  started. **[to test]** — the prompt forbids assuming this.
* **Post-exit command is not run after a crash** [source: `LauncherPartLaunch::on_state` →
  `emitFailed("Game crashed.")` for any nonzero exit/crash; `LaunchTask::finalizeSteps(false …)`
  skips remaining steps]. A post-exit hook therefore means "the game ended with exit code 0".
  A pre-launch with no matching post-exit means crash, kill or hook failure.
  `INST_EXITCODE` is commented out ("FIXME: make this work again") — the exit code is **not**
  available to hooks.
* Variables exported to hooks and substituted in command strings [source: `getVariables`]:
  `INST_NAME`, `INST_ID` (instance folder name), `INST_DIR` (instance root),
  `INST_MC_DIR` (game root), `INST_JAVA`, `INST_JAVA_ARGS`, `NO_COLOR=1`; plus the instance's
  custom `Env`.
* Hooks are only used when `OverrideCommands=true` in `instance.cfg` (legacy key
  `OverrideLaunchCmd`); otherwise the *global* `PreLaunchCommand`/`WrapperCommand`/`PostExitCommand`
  apply [source: `BaseInstance.cpp` `registerOverride`]. Instance keys: `PreLaunchCommand`,
  `WrapperCommand`, `PostExitCommand`.
* Wrapper command [source: `LauncherPartLaunch.cpp`]: `Commandline::splitArgs`, executable located with
  `QStandardPaths::findExecutable` (absolute path OK), started as `wrapper … javaPath args…`; if not
  found the launch fails. A wrapper sees the real exit code, but is part of the hot path (a machine
  without PackSync would be unable to launch) → **not used** in v1.

## 3. Game root and files

* `gameRoot()` is `<instance>/minecraft` if that directory exists, otherwise `<instance>/.minecraft`
  [source: `MinecraftInstance.cpp:350`]. Real instances on the dev machine use both.
* Instance root contents seen in the wild: `instance.cfg`, `mmc-pack.json`, `patches/`,
  `libraries/` (custom jars), `jarmods/`, `flame/` or `mrpack/` (pack-import metadata),
  `modlist.html`, `.packignore`, icon PNGs, `minecraft/` or `.minecraft/`.
* `instance.cfg` (INI, `[General]` + a `[UI]` table-state section) mixes portable metadata
  (`InstanceType`, `notes`, `ManagedPack*`, `Export*`) with machine-local settings
  (`JavaPath`, `Override*` toggles, `MinMemAlloc`/`MaxMemAlloc`, `JvmArgs`, `lastLaunchTime`,
  `totalTimePlayed`, `InstanceAccountId`, the whole `[UI]` section). Allow-list policy: only the
  portable keys are synced; everything else (including unknown future keys) stays local.
* Settings keys relevant to restore: `OverrideJavaLocation`/`JavaPath`, `OverrideMemory`/
  `MinMemAlloc`/`MaxMemAlloc`, `OverrideJavaArgs`/`JvmArgs`, `AutomaticJava`.

## 4. CLI and process behaviour [source: `Application.cpp`, installed `--help`]

* `prismlauncher [-d DIR] -l INSTANCE_ID [-s ADDR] [-w WORLD] [-a PROFILE] [-o OFFLINE_NAME]
  [--show-window] [-I URL]`; `--alive` writes `live.check`.
* Data directory: `--dir`, else environment variable `PRISMLAUNCHER_DATA_DIR`
  (`<LAUNCHER_NAME upper>_DATA_DIR`), else the platform default (`~/.local/share/PrismLauncher`,
  `%APPDATA%\PrismLauncher`, `~/Library/Application Support/PrismLauncher`), or portable mode
  (`UserData/` or `portable.txt` next to the executable). The instances directory is the
  `InstanceDir` setting (default `instances`).
* **If a Prism with the same data path is already running, `prismlauncher --launch X` only
  forwards a `launch` message to it and exits immediately** (`LocalPeer`, exit status success) —
  the child process lifetime says nothing about the game, and environment variables set by the
  caller do **not** reach the hooks. When Prism was not running, it runs the launch and exits with
  `0` if the launch task succeeded else `1` (`Application::controllerFinished`).
  → The game must be tracked through the process table; hook/launch coordination must go through
  the local DB + locks, not the environment.
* Separate data dirs (`--dir`) use separate `LocalPeer` IDs, so a disposable test Prism does not
  interact with a user's running Prism.
* Offline launch: `-o NAME` makes the session offline; an account entry must still exist
  [source: `LaunchController::decideAccount`]. The smoke test crafts an *offline-type* account in a
  disposable data dir; it never reads or copies the real `accounts.json`.

## 5. Registering a restored instance

Prism discovers instances as sub-folders of `InstanceDir` that contain `instance.cfg`. Folders whose
name starts with `.` are ignored by discovery, so a restore is staged in `.packsync-restore-<id>`
and atomically renamed to its final name when complete and validated — Prism never sees a
half-restored instance **[to test]** (rename while Prism runs; instance appears after refresh).

## 6. Design consequences (summary)

1. **Hooks are opt-in, additive and reversible**: instance-level `OverrideCommands=true` with
   PackSync's commands *chaining* any existing user command; a plan shows exact before/after values.
   `instance.cfg` is only edited while Prism is not running (Prism rewrites it from memory).
2. **Session ownership is DB/lock-based** (idempotent), because env vars do not reach hooks when a
   running Prism handles the launch, and the child's exit says nothing about the game.
3. **The game process is tracked via the process table** (cwd/cmdline under the instance root,
   `-Djava.library.path=<root>/natives`), not via the launcher process.
4. **Crash detection**: pre-launch without post-exit, a still-open session with no live game
   process, `crash-reports/` newer than the session start, `hs_err_pid*.log`.
5. A nonzero pre-launch exit is the *only* way a hook can stop a launch (e.g. unresolved conflict);
   everything else (offline) lets the game start and records the base snapshot.

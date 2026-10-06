# Prism Launcher — verified facts and design consequences

**Verification date:** 2026-10-06. **Version inspected:** Prism Launcher **11.0.3** (installed
here as `prismlauncher 11.0.3-2.1`, Arch). Authority: the 11.0.3 source tree
(`https://github.com/PrismLauncher/PrismLauncher/tree/11.0.3`, files named below) plus the
installed binary's `--help`. The public wiki
(<https://prismlauncher.org/wiki/getting-started/command-line-interface>) documents the CLI but
does **not** document hook semantics, environment variables, offline launch, the data-dir
environment variable or already-running-instance behaviour; the old
`getting-started/instance-settings/` URL returns 404. Everything below marked **[source]** was read
from code. Items marked **[verified]** were additionally confirmed on 2026-10-06 against the
real launcher by `scripts/prism-smoke.sh` (22 checks, all passing; Prism 11.0.3 on Linux; see
section 7 and `docs/dev/prism-adapter.md`). Items marked **[unverified]** have not been observed.

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
  Unicode in paths are legal when quoted. **[verified]** with a space + Unicode instance ID
  (`Smoke ünï 1`) and a hook program in a folder with spaces and Unicode: `"…/with space ünï/hook.sh"
  pre "$INST_NAME" "two words"` ran with `argc=3, argv=[pre, "Smoke ünï 1", "two words"]` — substitution
  happens first and a substituted value with spaces inside quotes stays one argument. Working
  directory of a hook is the game root (`<instance>/.minecraft`).
* **Nonzero exit of the pre-launch command fails the launch** [source: `PreLaunchCommand::on_state`
  → `emitFailed`; `LaunchTask::onStepFinished` → `finalizeSteps(false, …)`]: the game is *not*
  started. **[verified]**: a pre-launch command exiting 1 gave `Pre-Launch command failed with code 1.`,
  no wrapper/game ran, and Prism *stayed open* on its error dialog (it does not exit by itself; a
  `--launch` child therefore has no exit status in that case).
* **Post-exit command is not run after a crash** [source: `LauncherPartLaunch::on_state` →
  `emitFailed("Game crashed.")` for any nonzero exit/crash; `LaunchTask::finalizeSteps(false …)`
  skips remaining steps]. A post-exit hook therefore means "the game ended with exit code 0".
  A pre-launch with no matching post-exit means crash, kill or hook failure.
  `INST_EXITCODE` is commented out ("FIXME: make this work again") — the exit code is **not**
  available to hooks. **[verified]**: a game exit status of 1 and of 7 both gave `Game crashed.` and
  the post-exit command did **not** run; with status 0 pre-launch, the game and post-exit ran in that
  order and Prism exited 0. A post-exit command exiting 1 makes Prism report
  `Post-Launch command failed with code 1.` (a failing chained user command is therefore visible to
  the user, and PackSync's own post-exit part never fails the launch).
* Variables exported to hooks and substituted in command strings [source: `getVariables`]:
  `INST_NAME`, `INST_ID` (instance folder name), `INST_DIR` (instance root),
  `INST_MC_DIR` (game root), `INST_JAVA`, `INST_JAVA_ARGS`, `NO_COLOR=1`; plus the instance's
  custom `Env`. **[verified]**: `INST_ID`, `INST_NAME`, `INST_DIR`, `INST_MC_DIR` are set in both
  hooks; `INST_ID` is the instance *folder name* (not a PackSync ID), which is why hook command lines
  carry PackSync's own instance UUID.
* Hooks are only used when `OverrideCommands=true` in `instance.cfg` (legacy key
  `OverrideLaunchCmd`); otherwise the *global* `PreLaunchCommand`/`WrapperCommand`/`PostExitCommand`
  apply [source: `BaseInstance.cpp` `registerOverride`]. Instance keys: `PreLaunchCommand`,
  `WrapperCommand`, `PostExitCommand`.
* Wrapper command [source: `LauncherPartLaunch.cpp`]: `Commandline::splitArgs`, executable located with
  `QStandardPaths::findExecutable` (absolute path OK), started as `wrapper … javaPath args…`; if not
  found the launch fails. A wrapper sees the real exit code, but is part of the hot path (a machine
  without PackSync would be unable to launch) → **not used** in v1. **[verified]**: an *unquoted*
  wrapper path containing spaces is split at the spaces (`The wrapper command "/home/…/Coding"
  couldn't be found`) — quoting is required, unlike the pre/post commands this is not `splitCommand`
  behaviour that can be relied on for paths. The smoke test uses a wrapper that `exec`s a fake `java`
  with Prism's real arguments, so the process table looks exactly like a real game launch.

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
  `0` if the launch task succeeded else `1` (`Application::controllerFinished`). **[verified]**:
  success → exit 0; a *failed* launch (crash, failing pre/post command) leaves Prism open on an error
  dialog, so no exit status is ever observed for failures; a second `--launch` with the same `--dir`
  returned **exit 0 after 40 ms** and the second game's wrapper was a child of the *first* Prism
  process (forwarding confirmed; the process-table detection finds the running Prism by its
  `--dir` argument).
  → The game must be tracked through the process table; hook/launch coordination must go through
  the local DB + locks, not the environment.
* Separate data dirs (`--dir`) use separate `LocalPeer` IDs, so a disposable test Prism does not
  interact with a user's running Prism.
* Offline launch: `-o NAME` makes the session offline; an account entry must still exist
  [source: `LaunchController::decideAccount`]. **[verified]** details for the disposable test setup
  (none of this touches a real account): Prism's setup wizard blocks `--launch` in a headless run unless
  (a) `Language`, `IgnoreJavaWizard`, `LastHostname`, `UserAskedAboutAutomaticJavaDownload` are set in
  `prismlauncher.cfg`, and (b) one account *owns Minecraft* — `MinecraftAccount::ownsMinecraft()` is
  `type != Offline && entitlement.ownsMinecraft`, so an Offline-type account does **not** satisfy the
  login page. A fake MSA-type entry with `ownsMinecraft: true`, Prism's own `msa-client-id` and
  far-future token expiries is accepted; with no network its refresh fails as "offline" (a *server*
  refusal would be an error that blocks) and `--offline NAME` then launches. Prism also insists on
  reaching its metadata index, so the sandbox serves the cached metadata from a local file server via the
  `MetaURLOverride` setting while the launcher itself runs in an empty network namespace.

## 5. Registering a restored instance

Prism discovers instances as sub-folders of `InstanceDir` that contain `instance.cfg`. Folders whose
name starts with `.` are ignored by discovery, so a restore is staged in `.packsync-restore-<id>`
and atomically renamed to its final name when complete and validated — Prism never sees a
half-restored instance. **[verified]** that an instance restored by PackSync (including Prism's own
rewrite of its `instance.cfg`/`mmc-pack.json` on first load) launches in the real Prism and runs its
game. **[unverified]**: that an instance renamed into place while a *GUI* Prism is running appears
after Prism's next refresh (needs a display; the smoke test is headless).

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

## 7. Observed on the real launcher (smoke test, 2026-10-06)

`scripts/prism-smoke.sh` runs `crates/packsync-cli/tests/prism_real.rs` against Prism 11.0.3 in a
disposable data directory. Results (all PASS):

1. clean exit: Prism exits 0 and runs pre-launch, wrapper/game and post-exit in that order
2. quoting with spaces and Unicode, `$INST_NAME` with spaces stays one argument
3. `INST_ID`/`INST_NAME`/`INST_DIR`/`INST_MC_DIR` set; working directory is the game folder
4. the java arguments carry `-Djava.library.path=<instance>/natives` (the pattern PackSync matches games by)
5. non-zero pre-launch command stops the launch and Prism stays open
6. game exit status 1 and 7: post-exit is not run (`Game crashed.`)
7. failing post-exit command is reported as a failed launch
8. a second launch while Prism runs is forwarded (exit 0 in 40 ms, game runs under the first Prism)
9. PackSync discovery lists the real instances (name, `.minecraft`, Minecraft version)
10. hook installation is refused while the real Prism runs (its `--dir` is recognised)
11. PackSync's hooks run under Prism and chain the user's own commands with correct arguments
12. the post-exit hook saves the session and uploads in the background
13. another computer restores the progress into a new instance
14. Prism launches the restored instance
15. after a crash (no post-exit) the next launch is refused with exit status 19; status shows `crash_pending`
16. after `crash promote` the launch goes through again
17. uninstalling the hooks restores the user's commands (Prism had rewritten the file in between)
18. `packsync launch` through the real Prism finds the game process, sees exit status 0, saves and uploads

Also observed: after any launch Prism **rewrites `instance.cfg` completely** (alphabetical keys,
all defaults written out, values containing quotes stored as `\"…\"`, `lastLaunchTime` and
`totalTimePlayed` updated). Hook installation therefore edits only while Prism is closed, edits are
line-level, and removal restores exactly the keys PackSync touched (receipt + current-value check) — the
install → launch → uninstall round trip (check 17) proves it survives Prism's rewrite.

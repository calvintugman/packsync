# A companion mod or a Prism fork? (assessment, October 2026)

PackSync is meant to feel like Steam Cloud for Minecraft: each launcher instance — its mods, configs and
worlds together — follows you between computers. Two other shapes were considered for that: a companion
mod (Forge 1.20.1 first) and a fork of Prism Launcher with cloud saves built in. This page records why
neither was built now, and what would change that.

## What already exists: no background program

With PackSync's Prism hooks installed (`packsync hooks install`, or the desktop app's *Launcher
integration*), nothing of PackSync runs while you play or while your computer is idle:

* **Play pressed in Prism** → Prism runs `packsync hook pre-launch` (seconds): it brings in a newer version
  from the cloud if there is one, refuses to start on a conflict or an undecided crash, and exits.
* **Game closed** → Prism runs `packsync hook post-exit`: it saves a snapshot, uploads it in the
  background, and exits.
* The desktop app is optional: first-time setup, history, conflicts, restore. It can stay closed.

This works with every modpack Prism can run, because PackSync never needs to be inside the game. It was
verified against the real Prism 11.0.3 on Linux (`scripts/prism-smoke.sh`), and the same launch-tracking
code passes its tests as a Windows program under Wine (`scripts/windows-check.sh`, `docs/platforms.md`).

## A companion mod

**What it could add**

* A status line and a conflict or crash prompt on the title screen, and a "sync my worlds now" button.
* A precise signal that the game shut down cleanly.

**Why it is not the engine, and not needed for the goals**

* **Timing.** Mods, libraries and configs are loaded when Forge starts. A mod cannot install a newer
  mod set from inside the running game; that has to happen before launch, which is exactly when the
  pre-launch hook runs. Saving has to wait until the game has closed its region files, which is exactly when
  the post-exit hook runs. Inside the game only the worlds could be handled, and only between the title
  screen and loading a world.
* **"Track the chunks that changed."** It would not make anything noticeably faster. Region files are
  compared in 16 KiB blocks, so a play session already uploads only the changed parts (about 0.5–1 MB for
  a typical session in the benchmarks, `docs/performance.md`). Unchanged files are skipped by size and
  timestamps (0.04–0.08 s for 4,500 files). A mod would also miss whatever changes outside the game:
  mod updates, configs edited by hand, other launchers.
* **Modpack friction.** The mod would have to be added to every pack, could be removed by the next pack
  update from CurseForge or Modrinth, and would need a build for every Minecraft version and loader. The
  hooks work with any pack, any version and any loader, unchanged.
* **Sign-in in game** would still open the browser for Google's sign-in (the same loopback flow), and the
  refresh token would still belong in the OS credential store, not in the game folder.

**If it is ever built:** a thin Forge (later NeoForge/Fabric) client mod for the title screen. It should not
contain a second sync engine. Instead it calls the `packsync` program that is already installed: every
command has stable `--json` output and documented exit codes (`docs/cli.md`). The mod reads status and
shows prompts; the hooks keep doing the work. It is optional polish, not something the project needs.

## A fork of Prism Launcher

**Feasible on paper:** Prism is GPL-3.0 (a fork must stay GPL-3.0), written in C++/Qt, and a "Cloud" page
per instance could call the same engine (the `packsync` program, or the Rust library through a C interface).

**Why not now**

* **Signing in with Microsoft.** A fork has to use its own Microsoft (Azure) application, not Prism's. Newly
  created Azure applications must apply to Mojang for access to the Minecraft API, and are refused (HTTP 403)
  until approved. So "the same Minecraft login, with cloud saves" is not something a fork gets for free.
* **The login would not cover the cloud anyway.** Signing in to Minecraft gives no access to Google Drive.
  The same Microsoft application *could* also ask for OneDrive (Microsoft Graph) access, which would make
  OneDrive the natural storage for a fork. That would be a second storage adapter next to Google Drive (the
  engine is built for several, `docs/architecture.md`), not a reason to fork.
* **Upkeep and trust.** Prism releases often. A fork has to follow every release, build and sign installers
  for Windows (and macOS), and convince players to replace a launcher they trust. The hooks reach the same
  players with nothing to replace.
* **What the fork would add over the hooks** is mostly presentation: a cloud icon next to each instance and a
  status column. The desktop app shows the same.

**A better route than a fork:** offer Prism upstream a small, launcher-neutral extension point (for example
"run this command before launch and after exit, with the instance id" as a global setting with a visible
indicator). PackSync already uses exactly that through the per-instance commands.

## Decision

Keep the current architecture: a separate engine, the launcher hooks (no background program), and the
optional desktop app. Revisit:

* the companion mod, if title-screen status or prompts turn out to matter to players (thin, calling
  `packsync --json`);
* OneDrive as a second storage, if people would rather sign in with Microsoft than Google;
* a Prism contribution (an extension point), rather than a fork.

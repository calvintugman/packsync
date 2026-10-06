# What is synced: rules

PackSync syncs *game state*, not everything in the folder. What counts is decided by **rules** — ordered
patterns with a reason — so the preview can always tell you *why* a file is or is not included
(`packsync preview`, `packsync rules explain INSTANCE PATH`).

## How rules combine

1. **Sealed protections** (hard-coded, cannot be overridden): launcher account files
   (`accounts.json`, `launcher_accounts*.json`), `.env`, key material (`*.pem`, `*.key`, `*.jks`,
   `id_rsa*`, `.ssh/`), Prism's `prismlauncher.cfg` and PackSync's own `.packsync/`. They are excluded even
   if you select your whole home folder.
2. **The built-in defaults** (`crates/packsync-core/src/rules/defaults.toml`): Minecraft layout.
3. **The launcher's rules**: for Prism, instance-root files (`mmc-pack.json`, `patches/`, `instance.cfg` as a
   *mixed* file, custom `libraries/` and `jarmods/`, import metadata).
4. **Your overrides**, appended last (`--rules-file`).

For every path the **last matching rule wins**. A path no rule matches follows `unmatched` (default
`exclude`: unrecognised content is not uploaded, and the preview tells you which folders were not
recognised). The complete effective rules text is stored **inside every snapshot**, so all your
computers — and all versions of the app — agree, whatever their own defaults say.

## What the defaults include and exclude

| Included | Why |
|---|---|
| `mods/**`, `coremods/**` | the pack's code |
| `config/**`, `defaultconfigs/**`, `scripts/**`, `kubejs/**`, `groovy/**`, `crafttweaker/**`, `datapacks/**`, `resourcepacks/**`, `structures/**`, `patchouli_books/**` … | the pack's configuration |
| `saves/**` | single-player worlds: terrain, player data, per-world mod data, quest progress |
| waypoints (JourneyMap, VoxelMap, Xaero), schematics, blueprints | your progress outside worlds |
| `servers.dat`, `options.txt` (portable projection), `hotbar.nbt` | client settings you want everywhere |
| `mmc-pack.json`, `patches/**`, portable keys of `instance.cfg` | what the pack runs on |

| Excluded | Why |
|---|---|
| `logs/`, `crash-reports/`, `hs_err_pid*.log` | diagnostics (crash reports stay local) |
| caches (`.fabric/`, map tiles, remapped jars, LOD databases) | regenerated |
| `backups/`, `ftbbackups*/`, `saves/*.zip` | PackSync keeps its own history |
| `screenshots/`, `shaderpacks/` | large, opt-in |
| `natives/`, `*.dll`, `*.so`, `*.dylib`, `*.exe` | extracted per OS; another OS's binaries must not be restored |
| `versions/`, `assets/`, `libraries/`, `runtime/` (game folder) | launcher-managed |
| `session.lock`, `*.lock`, `*.tmp`, `*.swp` | machine/session specific |
| usernames/usercache | machine specific |
| graphics/performance settings tuned for one GPU (`optionsof.txt`, Sodium/Embeddium/Rubidium/Iris/Oculus/OptiFine/dynamic-fps configs) | machine specific (add an include rule to share them) |
| sealed credential files | secrets never leave the computer |

**Mixed files.** `options.txt` mixes preferences you want everywhere (language, key bindings, render
distance) with machine-specific values (GPU, window size, resource pack paths). PackSync stores a *portable
projection* and merges it into the local file on restore, leaving the local-only keys alone. Prism's
`instance.cfg` is the same: only an allow-list of portable keys (pack info, notes, export metadata) is
synced; Java path, memory, launch commands, account and window state — and any key PackSync does not know —
stay on each computer.

## Writing your own rules

`rules.toml` (format `version = 1`), passed with `packsync enable --rules-file` or `packsync preview --rules-file`:

```toml
version = 1

[[rule]]
id = "keep-my-screenshots"            # unique; shown in the preview
match = ["$GAME/screenshots/**"]      # globs anchored at the instance root; ** crosses directories
action = "include"                    # include | exclude
category = "screenshot"
reason = "I want my screenshots on every computer."

[[rule]]
id = "skip-big-map"
match = ["$GAME/saves/Archive*/**"]
action = "exclude"
category = "world"
reason = "Old archived worlds stay on this computer."
prune = true                          # (excludes only) skip the whole directory without walking it

[[rule]]
id = "region-blocks"
match = ["$GAME/saves/**/region/*.mca"]
action = "include"
strategy = "fixed:16384"              # auto | whole | fixed:<bytes> | cdc:<min>:<avg>:<max>
compress = "auto"                     # auto | never | always
```

* `$GAME` is the game directory relative to the instance root (`.minecraft`, `minecraft`, or nothing).
* `*` does not cross `/`; `**` does; `case_insensitive = true` makes a rule ignore case.
* `strategy` and `compress` only matter for included files; defaults are chosen per file kind (region
  files in fixed blocks, large mutable files with content-defined chunking, everything else whole), see
  `docs/performance.md`.
* Rules are validated when loaded (unknown keys, bad globs, bad strategies are errors with a message, not
  silently ignored).
* Changing rules affects **future** snapshots of that instance (they are recorded in each snapshot); files that
  stop matching are recorded as removed in the next snapshot, never deleted from your disk.

## Checking what a rule did

```sh
packsync preview "<instance folder>"                 # included by category, excluded with reasons, unrecognised folders
packsync rules explain MyPack ".minecraft/logs/latest.log"
# .minecraft/logs/latest.log: NOT synced — rule 'logs': Logs.
```

# Prism launcher hooks

Hooks are **optional**. Without them you start the game with `packsync launch` (or the app's Play button)
and PackSync syncs around it. With hooks installed, launching the instance **from Prism itself** syncs too.

## What gets installed

For one instance, in its `instance.cfg` (and nowhere else):

```
OverrideCommands=true
PreLaunchCommand="<packsync>" hook pre-launch  --instance <uuid> --marker packsync-hook [--config-dir … --state-dir …] [-- <your original command>]
PostExitCommand="<packsync>"  hook post-exit   --instance <uuid> --marker packsync-hook [--config-dir … --state-dir …] [-- <your original command>]
```

* **Opt-in and previewed.** `packsync hooks install INSTANCE --plan` (or the app's preview) shows the exact
  before/after values. Nothing is written without confirmation.
* **Additive.** Your existing pre-launch/post-exit commands are **chained**, not replaced: PackSync's part runs
  first, then yours with the arguments you wrote, and your command's exit status is passed through. If the
  instance did not override commands, the launcher-wide ones that would stop applying are carried over (and a
  launcher-wide wrapper command is copied) so installing hooks never changes what runs.
* **Reversible.** `hooks uninstall` restores the commands that were there before, key by key, even though
  Prism rewrites `instance.cfg` after every launch (verified by the real-launcher smoke test).
* **Only while Prism is closed.** Prism rewrites `instance.cfg` from memory, so installation refuses while the
  launcher runs (it recognises Prism by its `--dir`).
* **Safe quoting.** The program path is quoted for Prism's command splitter; paths containing a double quote or
  `$` (which Prism would substitute) are refused with an explanation. Hook commands run **without a shell**.
* **Non-default directories** (a custom `--state-dir`) are carried in the command line, so a hook finds the
  same database as the app that installed it.

## What the hooks do

| Hook | Behaviour |
|---|---|
| **pre-launch** | Closes sessions of earlier dead launches, reconciles with the cloud (fast-forwards if a newer version exists and this computer is clean), then begins a play session. If the instance is in conflict, a crash is waiting for a decision, or (strict policy) the cloud cannot be reached, it **exits non-zero**, which makes Prism not start the game, and prints why. Otherwise it lets the launch continue, even offline (the base snapshot is recorded). |
| **post-exit** | Prism runs this only after the game exited with status 0. It ends the session, saves a snapshot and starts the upload in a detached background process, so Prism is not kept waiting. |

If the **game crashes**, Prism does not run post-exit. The session stays open with no live game; the next launch
(or a passive check after a grace period) turns it into a quarantined *crash save* and the instance waits for
your decision (`packsync crash show`).

If **PackSync itself fails** inside a hook (corrupt settings, locked database, …) the hook prints a warning,
runs *your* chained command and lets the launch proceed — a broken sync tool must not stop you from playing.

If **PackSync started the game** (`packsync launch`, the Play button) the hooks stand aside and only leave a
clean-exit signal for the app to collect; two components never both sync the same launch.

## Settings

| Setting | Default | Meaning |
|---|---|---|
| `hook_offline_policy` | `play_local` | `play_local`: start from the local copy when the cloud is unreachable; `block`: refuse to start. |
| `hook_block_on_problem` | `true` | Block the launch on a conflict or a crash waiting for a decision. When `false` the problem is printed and the launch proceeds *without syncing* that instance. |

## Verified Prism behaviour

Hook semantics are not documented by Prism; they were read from the 11.0.3 source and **confirmed by running
the real launcher** (`scripts/prism-smoke.sh`, results in `docs/dev/prism-verification.md`): nonzero pre-launch
blocks the launch; a nonzero game exit means no post-exit; quoting and `$INST_*` substitution; environment and
working directory; forwarding of a second `--launch` to a running Prism; and the install → launch → uninstall
round trip across Prism's own rewrite of `instance.cfg`.

## Limits

* The game's **exit status is not available to hooks** (Prism does not export it), only "post-exit ran".
* When the game was started by an **already-running Prism** (a forwarded launch), the app cannot read its exit
  status either; without hooks that session is treated as unconfirmed (a quarantined save), with hooks the
  post-exit signal confirms a clean exit.
* Hooks are per instance and per computer: install them on each computer (the app offers it after a restore).
* macOS and Windows hook behaviour follows Prism's cross-platform code but was not exercised here (`docs/testing.md`).

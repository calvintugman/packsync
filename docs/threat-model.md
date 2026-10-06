# Threat model

PackSync moves a game folder between your computers through *your* Google Drive. This page lists what it
protects, from whom, how, and — plainly — what it does not.

## Assets

1. **Your instances and worlds** — integrity (no silent loss or corruption) and availability.
2. **Your Google access** — the OAuth refresh token and, if you use your own client, its client secret.
3. **Your local files** — a restore must never write outside the instance or destroy what it was not
   told to replace.
4. **Privacy of your game data** — what is in a world, your server list, your mod list.

## Trust boundaries

| Party | Trusted to… | Not trusted to… |
|---|---|---|
| You and your OS account | run PackSync, hold files and the credential store | — |
| Prism Launcher | start the game, run the hook commands you installed | its settings are never uploaded (sealed list + allow-list) |
| Google Drive / your Google account | store bytes and return them | be private from Google, be consistent (listings lag), or be free of other people's writes if the account is shared |
| Anyone who can write to the PackSync folder in your Drive | — (they are *you* or someone who took over your account/folder) | every byte they write is treated as hostile input |
| Other software on your computer | — | read the refresh token (OS credential store), call the dev bridge (token + loopback), tamper with an in-flight restore (instance lock) — within what an unprivileged same-user process can be prevented from doing |

## Threats and what PackSync does

### Hostile or damaged cloud content

*Someone with write access to the cloud folder (or a bit-flip, or an older/newer app version) supplies bad
manifests, indexes or packs.*

* Everything read from the cloud is **bounded and validated before use**: object sizes, entry counts,
  decompressed sizes (decompression is capped, so a "zip bomb" fails), pack/index structure (offsets,
  lengths, overlaps), and every path in a manifest (`docs/remote-format.md`).
* Paths are rejected if absolute, containing `..`, drive letters or control characters, too long/deep, or
  colliding on a case-insensitive target; Windows-reserved names are found **before anything is touched**
  (`hostile.rs`: names that cannot exist on the target platform are reported in the restore preview).
* **Symlinks are never followed** while scanning, and a restore refuses to write through a symlinked
  ancestor (a snapshot cannot place files outside the instance). A symlinked included folder blocks the
  snapshot instead of being recorded as "deleted".
* Every blob is verified against its BLAKE3 hash after decoding, every file against its whole-content hash
  before it is moved into place, and every manifest against its own name. A mismatch aborts the restore with
  the instance untouched.
* A manifest from another instance, or one that names a different instance than its folder, is refused.
* **What this does not stop:** a *valid* snapshot that contains a malicious mod. Minecraft mods are
  programs. PackSync cannot tell a legitimate update from a trojaned one. Mitigation: treat the cloud
  folder as sensitive as the account that owns it (strong password, 2-step verification), review the
  restore preview before applying history restores, and do not share the Drive folder with people you would
  not give your mods folder.

### Credential theft

* The refresh token is stored **only** in the OS credential store (Windows Credential Manager, macOS
  Keychain, Secret Service on Linux). If none is available PackSync fails with instructions; **there is no
  plaintext fallback**.
* Tokens, authorization codes, client secrets, `Authorization` headers and resumable-upload URLs are never
  written to logs, errors, `--json` output, diagnostics or the cloud: a redacting log writer, pinned
  third-party log levels (`ureq` prints headers at debug) and tests that scan logs/errors/`Debug` output for
  every secret a mock server issued (`crates/packsync-drive/tests/oauth.rs`).
* **Sealed exclusions**: launcher account files (`accounts.json`, `launcher_accounts*.json`), `.env`, key
  material (`*.pem`, `*.key`, `*.jks`, `id_rsa*`, `.ssh/`), Prism's `prismlauncher.cfg` (API keys) and
  PackSync's own state are excluded by a hard-coded layer that **no user rule can override**, even if the
  whole folder is selected (`credentials_never_enter_a_snapshot_even_when_the_whole_folder_is_selected`).
* The default scope is `drive.file`: PackSync can see only what it created, not your other Drive files.
* Sign-in uses the system browser with PKCE (S256), a random `state` (callbacks with another state are
  rejected), an ephemeral loopback port and a one-shot listener that serves a plain page with a strict CSP.
* Disconnecting forgets the token locally; `--revoke` also revokes it at Google.

### Data loss through bugs or bad input

* Never auto-merge, never auto-delete in the cloud, never treat "missing" as "deleted" (unreadable
  directories are *unknown*; an unmounted drive or replaced folder is detected by an ownership marker;
  mass deletions need confirmation).
* Journaled restores with rollback, recovery points on disk, per-instance locks, game-process and
  `session.lock` detection, stable-read checks. Failure behaviour is tested with injected crashes at every
  boundary and with real process kills (`docs/consistency.md`).

### Local tampering and other local software

* The instance lock is an OS advisory lock: it protects against *PackSync* processes (app, hooks, CLI) racing
  each other, not against a program that ignores locks.
* **Launcher hooks** are command lines in `instance.cfg`. Anyone who can write your instance folder can
  already do anything you can; PackSync's own part of a hook validates its arguments (an instance UUID) and
  never builds shell strings — hook commands are executed without a shell. Installing hooks is opt-in,
  previewed, reversible and refuses while the launcher runs.
* The development **dev bridge** (`packsync dev-bridge`, hidden, for UI tests) binds `127.0.0.1`, requires a
  random token on every API request and checks the `Host` header (DNS rebinding). It is not part of the
  shipped desktop app.

### Availability and abuse of limits

* Bounded retries with jittered backoff; `Retry-After` honoured; quota errors are not retried; uploads are
  resumable; listing storms are avoided by packing small files (a 2 GB instance is hundreds of requests).
* Resource caps on every untrusted input (see `docs/remote-format.md`), including recursion depth and
  entry counts.

## Privacy

* Your files are stored **unencrypted** in your Drive (end-to-end encryption is not implemented in v1, and
  PackSync does not roll its own cryptography). Google can read them, and so can anyone with access to the
  folder or account. File names and sizes are visible in Drive's web interface (the `PackSync` folder) with the
  default space; with `app_data` they are hidden from you and other apps but not from Google.
* Worlds can contain information you may not want to share: player names/UUIDs, coordinates, `servers.dat`
  (server addresses), mod lists. The preview before enabling shows what is synced and why; add exclude rules
  for anything you do not want uploaded (`docs/rules.md`).
* PackSync has no server, no analytics and makes no network requests except to Google (OAuth and Drive).

## Supply chain

* Dependencies are pinned by `Cargo.lock`. A licence/advisory gate (`cargo deny`, `npm audit`) and a generated dependency list are **planned but not yet in the repository**.
* There is no auto-update and no remote code loading. **No signed installers or release artifacts exist
  yet**; the README says how to build from source, and this repository does not claim otherwise.

## Out of scope (stated, not solved)

* Protection against a compromised operating system or a malicious administrator.
* Protection of data *in use* (Minecraft itself) and of the launcher.
* Multi-user sharing of one instance between different people (the design is one person, many computers).
* End-to-end encryption, secret sharing, hardware-key storage of the token. A future version could add
  client-side encryption with a user-held key; the object format has room for it, but it is not in v1.

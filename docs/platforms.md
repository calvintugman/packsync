# Platform support and limitations

PackSync was developed and tested on **Linux x86_64**. **Windows and Linux are the primary targets.** The
Windows build is cross-compiled on Linux and its test programs run under Wine (`scripts/windows-check.sh`);
it has **not** been run on a real Windows installation, and macOS has never been built. This page says
precisely what exists, what is verified how, and what to expect.

| | Linux | Windows | macOS |
|---|---|---|---|
| Engine, CLI, tests | built, tested, run | **cross-built; all 380 tests of the Windows build pass under Wine**, including launching through a (fake) Prism, crash handling, hooks, the JSON API and the `packsync.exe` end-to-end suite; never run on real Windows | not built |
| Desktop app (Tauri 2) | release binary built and run (WebKitGTK, Wayland) | not built (needs WebView2 and MSVC or a Windows host) | not built |
| Prism adapter | **verified against real Prism 11.0.3** (headless, sandboxed) | paths and quoting implemented, unverified | paths and quoting implemented, unverified |
| Credential store | Secret Service (code + live test that is `#[ignore]`d; not run here) | Credential Manager (unverified) | Keychain (unverified) |
| Installers | none produced | `nsis`/`msi` configured, none produced | `dmg`/`app` configured, none produced |
| CI | `ubuntu-latest`: blocking | `windows-latest`: **non-blocking until its first green run** | `macos-latest`: **non-blocking until its first green run** |

### What the Windows check found (and fixed)

* **Long paths were broken.** PackSync prefixes absolute paths with `\\?\` so they may exceed 260
  characters, but in that form Windows stops interpreting `/`, `.` and `..`, and any path that contained a
  `/` failed with "invalid name" — restores and the restore journal would have failed on Windows. The path is
  now rebuilt from its components (`fsutil::long_path`, unit-tested on every platform).
* **A missing `windows-sys` feature** (`Win32_Security`, needed by `CreateFileW`) only compiled because another
  crate happened to enable it.
* Tests that assumed Unix (paths like `/opt/x`, names Windows cannot hold, directory order, Qt writing CRLF
  on Windows) were made platform-correct. The test fake of Prism and of the game was a pair of shell scripts;
  on Windows it is now one small program (`crates/packsync-fake-prism`), so the launch, hook, JSON-API and
  CLI suites run there too. Only the CLI test that chains the user's own *shell scripts* stays Unix-only.

Wine is not Windows: its NTFS emulation (change times, file ids, mandatory locks) and process table differ in
details, so the change-time and lock behaviour below remains unverified on real Windows.

## What is platform-specific in the code

* **Paths and names** (`packsync-core/src/paths.rs`, `restore.rs`, `fsutil.rs`):
  * Windows: absolute paths are prefixed `\\?\` (and `\\?\UNC\`) so paths longer than 260 characters work; names
    that cannot exist on Windows (`CON`, `NUL`, `COM1`… with or without an extension, trailing dots or spaces,
    `<>:"|?*`) are found in a restore **preview, before anything is touched**; case-insensitive collisions are
    detected; the long-path and reserved-name logic is unit-tested on Linux against the *rules*, not against NTFS.
  * macOS: collisions are checked with case **and** Unicode-normalisation folding (APFS/HFS+ keep names
    normalised).
  * Everywhere: the file system itself is probed for case-insensitivity (a case-insensitive volume on Linux
    behaves like Windows), unless overridden.
* **File metadata** (`fsutil.rs`): Unix uses `mtime`, `ctime`, `dev:ino`; Windows uses NTFS write time,
  ChangeTime and the volume-serial/file-id (via `GetFileInformationByHandleEx`, `windows-sys`) so a replaced file
  is noticed even when size and mtime are preserved. The Windows branch has not been compiled by the author.
* **Game-folder locks:** `session.lock` is probed with POSIX `F_GETLK` on Unix; on Windows by opening it
  read-only and reading one byte, because Java's lock there is a mandatory byte-range lock: the file still
  opens, but reading the locked range fails (OS error 33; a sharing violation on open, 32, also means "held").
  The probe never writes or takes a lock itself. Both are best effort: process detection (below) is the main
  guard, and old game versions (1.7.10, so GregTech: New Horizons) do not lock the file at all.
* **Process detection** (`proc.rs`, Prism adapter): the game is found by its command line and working directory
  under the instance (`-Djava.library.path=<instance>/natives`), via `sysinfo`; Prism is recognised by `--dir`.
  This was verified with the real launcher on Linux; the Windows/macOS process tables behave differently in
  details (for example `javaw.exe`, command-line length limits) and are untested.
* **Prism** (`packsync-prism/src/discovery.rs`, `env.rs`): data folders — Linux `~/.local/share/PrismLauncher` and
  the Flatpak `~/.var/app/org.prismlauncher.PrismLauncher/data/PrismLauncher`, Windows `%APPDATA%\PrismLauncher`
  and the portable layout (a `UserData` folder or `portable.txt` beside the executable), macOS
  `~/Library/Application Support/PrismLauncher`; `PRISMLAUNCHER_DATA_DIR` is honoured.
  Only the Linux ones were exercised against a real installation.
* **Hook commands** (`packsync-prism/src/{cmdline,hooks}.rs`): Prism stores commands in `instance.cfg` and splits
  them with Qt's rules; PackSync ports the INI reader/writer and `QProcess::splitCommand` (`cmdline.rs`,
  `qtini.rs`) so quoting is byte-exact for spaces, Unicode and quotes. Verified with real Prism on Linux; the Qt
  port is the same code on every OS, but Windows' own argument conventions for the *called program* were not tested.
  Paths that contain a double quote or `$…` (which Prism substitutes) are refused with an explanation.
* **Credentials** (`packsync-drive/src/credentials.rs`): the `keyring` crate with native stores per OS.
  There is **no plaintext fallback** on any platform.
* **Launching** (`packsync-prism`): `prismlauncher` (Linux/macOS binary or Flatpak: `flatpak run …`) or
  `prismlauncher.exe`; no shell is involved.

## Known platform limitations and unknowns

* **Windows:** antivirus and indexer software can hold files open during a restore (a locked file makes that file
  fail, not the whole restore: the journal rolls back); renaming a folder that something has open fails; OneDrive
  "known folders" and network drives were not considered; symlinks and junctions are never followed (reported,
  not synced). None of this was observed — it is design reasoning.
* **macOS:** an **unsigned, un-notarised** app is blocked by Gatekeeper until the user allows it; signing needs an
  Apple developer identity that this repository does not have. Instances kept in iCloud-synchronised folders ("Desktop &
  Documents") were not considered and are best avoided.
* **Linux:** needs a Secret Service provider (GNOME Keyring, KWallet, KeePassXC) for Google sign-in — without one
  PackSync says so and refuses to store a token (a folder cloud needs no credentials). On Wayland without a
  desktop portal the native folder picker may be unavailable; the path can be typed.
* **Any platform:** cross-platform restores check *the target* before touching it: a snapshot made on Linux
  that contains a file Windows cannot hold is reported in the preview, and can be restored with that file skipped.
  Native libraries and executables are never synced, so an instance created on one OS is run with the other OS's
  natives, which Prism extracts itself.

## What to do on Windows or macOS

1. Build as in the README (`cargo build --release -p packsync-cli`; `npm run tauri build -- --no-bundle`).
   From Linux, `scripts/windows-check.sh` cross-builds and runs the test programs under Wine.
2. Run `cargo test --workspace` and the browser tests (`npm run e2e`). Any failure is informative: please
   report it with the output of `packsync doctor`.
3. Before trusting it with a world: use a **folder** as the cloud and a copy of an instance, check *History →
   Restore*, and the Prism hooks on a throw-away instance (`docs/hooks.md`).
4. Once the Windows and macOS CI jobs pass, remove `continue-on-error` from `.github/workflows/ci.yml`.

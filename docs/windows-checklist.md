# Windows verification checklist

**Status: PackSync has not been run on a real Windows installation.** The Windows build is cross-compiled on
Linux and its tests pass under Wine (`scripts/windows-check.sh`), but Wine is not Windows: it does not
enforce byte-range locks on reads, its file IDs and change times are emulated, and its process table
differs. This page is how to close that gap. It takes about 20 to 30 minutes, most of it compiling.

Part 1 is automatic and uses no Google account and none of your instances. Part 2 uses the real game.

## Before you start

* Windows 10 or 11, with [Rust](https://rustup.rs/) (the default MSVC toolchain; the installer offers the
  Visual Studio Build Tools it needs) and [Git for Windows](https://git-scm.com/download/win).
* Get the repository onto the machine with `git clone` (not a zip copied from Linux, so that Git applies its
  line-ending rules).

## Part 1: the script

In PowerShell, from the repository root:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\windows-verify.ps1
```

What it does, in order:

| Section | What it establishes |
|---|---|
| System | Windows version, toolchain, `core.autocrlf`, whether long paths are enabled in the registry |
| Line endings | No text file was checked out with CRLF (`.gitattributes` works) |
| Build | `packsync.exe` builds natively with MSVC |
| Test suite | `cargo test --workspace` on NTFS, including the `session.lock` probe test that Wine cannot run meaningfully |
| Functional run | Two "computers" (two state folders) and a plain folder as the cloud: enable, restore, in-place edit, catch up, verify. The instance contains a path over 260 characters, non-ASCII names, a file with CRLF content and an empty file; the restored tree must be byte-identical |
| File locking | (1) with a byte-range lock on a world's `session.lock`, as Minecraft 1.16+ holds it, saving must refuse with exit code 14; (2) a changed file opened exclusively by another program makes the sync fail cleanly and succeed afterwards; (3) a restore whose target file is in use fails, **changes nothing**, and succeeds afterwards |
| Doctor | `packsync doctor` output (redacted by design) |

Everything it creates is under `%TEMP%\packsync-verify`.

**Send back:** the file `windows-report.txt` from the repository root. If the test suite section says
anything failed, also send `windows-test-full.log`. Neither contains credentials; the report does contain
your Windows user name inside paths.

## Part 2: with the real game (10 minutes)

This checks process detection and the lock against a real `javaw.exe`, which the script can only imitate.
Use a **copy** of an instance (in Prism: right-click, *Copy instance*), never your only copy of a world.

```powershell
$ps = "target\release\packsync.exe"
& $ps storage use-local "$env:TEMP\packsync-manual-cloud"
& $ps instances                                   # 1. is the copy listed, with the right folder?
& $ps --yes enable "<path of the copied instance folder>"
```

2. Start the copy from Prism and **open a world**. While you are standing in the world:

```powershell
& $ps snapshot "<instance name>"; "exit code: $LASTEXITCODE"
& $ps status "<instance name>"
```

   Expected: the snapshot is refused, exit code **14**, and the message names the running game
   (a `javaw.exe` process and/or "world ... is open").

3. Place a few blocks, save and quit to the title screen, **close the game completely**, then:

```powershell
& $ps sync "<instance name>"; "exit code: $LASTEXITCODE"
& $ps verify "<instance name>"
```

   Expected: a new snapshot is saved and uploaded (exit code 0), and verify reports
   `0 changed, 0 missing`.

**Send back:** the output of the commands in steps 1 to 3, the Minecraft version of the instance, and
whether Prism is the installed or the portable version.

To clean up afterwards: `& $ps disable "<instance name>"`, delete `%TEMP%\packsync-manual-cloud`, and delete
the copied instance in Prism.

## What stays unverified even after this

* Google sign-in on Windows (the token is kept in the Windows Credential Manager) is a separate check:
  `packsync storage use-google`, `packsync auth login`, `packsync storage test`.
* The desktop app (Tauri) has not been built on Windows.
* Antivirus products that hold files open for longer than a moment, OneDrive-redirected folders and network
  drives were not considered.

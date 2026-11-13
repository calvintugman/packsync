# PackSync

PackSync syncs [Prism Launcher](https://prismlauncher.org/) instances between computers: mods, configs and
single-player worlds. Play on your desktop, then press Play on your laptop and pick up where you left off.

![PackSync showing two synced instances](docs/images/instances.png)

## How it works

You turn on sync on one computer and join the others by typing a short pairing code. With Prism's launch hooks
installed, pressing Play downloads the newest version and closing the game uploads yours.

- Only changed blocks of a file are uploaded, so a typical play session uploads a few megabytes.
- Everything is encrypted on your computer before upload. You can also use your own Google Drive or
  Cloudflare R2 bucket instead of the PackSync cloud.
- Worlds are never merged. If two computers played the same world, you choose which version to keep, and the
  other one stays in the history.
- A save from a game that crashed is held back until you decide what to do with it.
- If you lose every computer, a passkey or a recovery key gets your account back.

## Under the hood

- The sync engine is written in Rust (about 54k lines). It only talks to two traits, `Storage` and `Launcher`,
  so almost all of it can be tested without a network or a real launcher. It uses content-defined and
  fixed-size chunking, zstd compression, BLAKE3 content addressing, journaled restores that survive a crash
  at any point, and SQLite state shared by several processes.
- The desktop app is Tauri 2 with React; there is also a command-line tool.
- The PackSync cloud is a Cloudflare Worker with R2 and D1. Data is encrypted with XChaCha20-Poly1305 on the
  client, pairing uses SPAKE2 so the server never learns the code, and passkey recovery uses WebAuthn PRF.
- With a 5.4 GB GregTech: New Horizons pack on Cloudflare R2, the check at game launch takes 0.5 s, a sync
  with nothing changed 0.7–0.9 s, and a sync after a scripted world edit 3.4–3.8 s.
- There are 453 Rust tests, 133 UI tests and 38 Playwright end-to-end tests. They cover crashes at every write,
  corrupt or malicious cloud data, network failures, and a conformance suite that every storage backend runs.

## Install

Download a build from [Releases](https://github.com/calvintugman/packsync/releases).

- Linux: unpack the `.tar.gz` and run `./install.sh`, or install the `.deb`.
- Windows: unzip it somewhere permanent and run `packsync-desktop.exe`. The build is not signed, so
  SmartScreen will warn you first.

To build it yourself you need Rust 1.89+ and Node.js 22+:

```sh
cargo build --release -p packsync-cli                              # command-line tool
cd apps/desktop && npm ci && npm run tauri build -- --no-bundle    # desktop app
```

PackSync is a pre-release that I use on Linux and Windows. MIT licensed; see [CONTRIBUTING.md](CONTRIBUTING.md)
if you want to help.

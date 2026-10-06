# Dependencies and licences

PackSync is MIT-licensed ([`LICENSE`](../LICENSE)). It builds on open-source packages; none of them is
copyleft in a way that affects distributing PackSync, and none is modified. Nothing is downloaded at run time
and there is no auto-update.

## How it is enforced

* **Rust:** [`deny.toml`](../deny.toml) + `cargo deny check` (CI): RustSec advisories and yanked crates,
  licences (an allow-list of permissive licences), banned wildcard versions, and sources (crates.io only,
  no git dependencies). Run it for both Cargo workspaces:
  ```sh
  cargo install cargo-deny --locked
  cargo deny check
  cargo deny --manifest-path apps/desktop/src-tauri/Cargo.toml check
  ```
* **npm:** `package-lock.json` pins every version; CI runs `npm audit --omit=dev --audit-level=high`.
* **Lock files are committed** (`Cargo.lock`, `apps/desktop/src-tauri/Cargo.lock`, `apps/desktop/package-lock.json`).

At the time of writing both `cargo deny` runs pass, with one documented exception below.

## Rust

| | Third-party crates | Notes |
|---|---:|---|
| Engine, CLI, adapters, benchmarks, test world | 344 | |
| + desktop shell (Tauri 2, GTK/WebKit bindings, …) | 557 in total | |

Licences present (by crate count): MIT, MIT OR Apache-2.0 and Apache-2.0 OR MIT dominate (~450);
then Unicode-3.0, Zlib, ISC, BSD-3-Clause, Unlicense, CC0-1.0 and dual-licensed combinations of these. The ones
worth naming:

| Crate(s) | Licence | Why it is fine |
|---|---|---|
| `cssparser`, `cssparser-macros`, `selectors`, `dtoa-short` (desktop shell, via Tauri), `option-ext` (via `directories`) | MPL-2.0 | File-level copyleft: it only requires that changes to *those files* stay open. They are used unmodified as dependencies. |
| `webpki-roots` | CDLA-Permissive-2.0 | Mozilla's CA bundle data; permissive. |
| `xxhash-rust` | BSL-1.0 | Boost licence; permissive. |
| a crate offering `MIT OR Apache-2.0 OR LGPL-2.1-or-later` | choose MIT | LGPL is only one of the options. |

**System libraries (not distributed by PackSync):** on Linux the desktop app links the system's GTK 3,
WebKitGTK and libsoup dynamically (LGPL). Windows uses WebView2, macOS uses WKWebView.

**Advisory exception** (`deny.toml`): `RUSTSEC-2024-0370` — `proc-macro-error` is *unmaintained* (not a
vulnerability). It is a build-time proc macro used by gtk-rs 0.18, which Tauri 2 uses for its Linux window;
only the desktop shell is affected and it is not ours to replace. It will go away with a Tauri/gtk-rs update.

## JavaScript (desktop UI)

Shipped in the app (5 packages): `react`, `react-dom`, `scheduler` (MIT); `@tauri-apps/api`
(Apache-2.0 OR MIT); `@tauri-apps/plugin-dialog` (MIT OR Apache-2.0).

Build- and test-time only (not shipped): Vite, TypeScript, ESLint, Vitest, Testing Library, jsdom,
Playwright, axe-core (MPL-2.0), the Tauri CLI and their dependencies — mostly MIT, with some Apache-2.0, ISC,
BSD, BlueOak, MPL-2.0 and CC-BY-4.0 (browser-support data).

## Regenerating the numbers

```sh
cargo metadata --format-version 1 --locked > meta.json     # then count `license` fields
cd apps/desktop && npm ls --omit=dev                        # what ships in the UI
```

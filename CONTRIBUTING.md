# Contributing to PackSync

Thank you for helping. PackSync moves people's **worlds** between computers, so the bar for a change is
"could this lose or corrupt someone's data?" before "is it faster / prettier?". This page explains how to
work on it and which rules are not up for negotiation.

By contributing you agree that your contribution is licensed under the project's [MIT licence](LICENSE).

## Ground rules (the invariants)

These are why the project exists; a change that weakens one needs a strong, written justification.

1. **Immutable, content-addressed cloud data.** Objects are never overwritten; there is no mutable global
   "latest" pointer (`docs/remote-format.md`, `docs/consistency.md`).
2. **Never merge worlds automatically**, and never delete the loser of a conflict.
3. **Unknown is not deleted.** An unreadable folder, a missing drive or an empty listing never means "delete".
4. **A crash is not a clean exit.** An unclean session is quarantined until a person decides.
5. **Restores are recoverable** (journaled; rolled back or completed after any interruption), and replaced
   files are kept as a recovery point.
6. **Cloud data is untrusted input.** Bound it, validate it, never follow its paths outside the instance.
7. **Secrets never leave the credential store**, are never logged, and never go into a snapshot.
   There is no plaintext fallback.
8. **Honest status.** Never show "synced" for something that is only saved locally.
9. **Cleanup is manual and previewed**, and touches only objects PackSync created.
10. **One component owns a launch's sync** (the app *or* the hooks, never both).

## What you can and cannot put in the repository

* **Never** commit credentials, access or refresh tokens, OAuth client secrets, `.env` files, downloaded mod
  JARs, Minecraft files, Prism account files, or anyone's real world data. `.gitignore` excludes the obvious
  names; you are the last line of defence. Test fixtures are **generated** (`packsync_core::testkit`).
* **Never** make a test depend on a real Google account, a real Prism installation or the network. Tests use
  the in-memory/local-directory storage, the mock Drive server, the fault-injecting wrapper and the fake
  `prismlauncher`. Real-service checks are documented procedures for a human with a designated test account
  (`docs/live-validation.md`, `scripts/prism-smoke.sh`) — and they must never touch real worlds.
* Do not add `unwrap()`/`expect()` on data that comes from the cloud or the disk in library code.

## Setting up

You need Rust (1.89+; `rustup`) and, for the desktop app, Node.js 22+. On Linux the desktop shell also needs
the web-view libraries listed in `docs/desktop.md`. The engine, CLI and their tests need none of that.

```sh
cargo test --workspace                       # everything that needs no credentials, Prism or network
cargo test -p packsync-cli --features failpoints --test killed
cd apps/desktop && npm ci && npm test && npm run typecheck && npm run lint
npx playwright install chromium && npm run e2e
```

Before sending a change, make these pass (CI runs the same):

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check                              # licences, advisories, sources (cargo install cargo-deny --locked)
( cd apps/desktop && npm run typecheck && npm run lint && npm test && npm run build && npm run e2e )
```

## How the code is organised

See [`docs/architecture.md`](docs/architecture.md). In short: `packsync-core` (the engine) knows nothing about
Google, Prism or windows; it talks to a `Storage` trait and a `Launcher` trait. Front ends (CLI, desktop)
only call `packsync-app`'s JSON API. Adding a launcher means a new crate implementing `Launcher`; adding a
cloud means a new `Storage` that passes `testkit::conformance`.

## Tests we expect with a change

* **Bug fix:** a test that fails without the fix. Prefer the lowest level that can show it, and add a
  journey/crash/hostile-data test when the bug was about a sequence of events.
* **Anything touching restore, upload, snapshot, sessions or recovery:** think about *when the power fails*.
  The engine has named failpoints (`failpoint!("…")`, enabled with the `failpoints` feature and
  `PACKSYNC_FAILPOINTS`); add one at the new boundary and a test that crashes there.
* **Anything reading cloud or disk data:** a hostile-data test (corrupt, oversized, traversal, symlink,
  case/Windows collisions) in `crates/packsync-core/tests/hostile.rs`.
* **UI changes:** a Vitest test for logic and components; a Playwright test (`apps/desktop/e2e`) for a
  user-visible flow, which runs the real UI against the real engine. Keep it keyboard-accessible: the
  axe-core checks run on every screen and dialog.
* **A claim in the docs or README** is also a thing that can be wrong: if you change behaviour, change the
  docs; if you cannot test something (a platform, a real service), say so there rather than implying it works.

## Style

* Rust: `rustfmt` (config in `rustfmt.toml`) and `clippy -D warnings`. Comments explain *why*, not what;
  write user-facing errors as "what happened, what you can do" (`Error::advice`).
* TypeScript: strict mode, ESLint (`npm run lint`), no `any`. User-visible text is plain, specific and calm —
  say what will happen to the person's files.
* Keep dependencies few and boring; a new one must pass `cargo deny` / `npm audit` and be mentioned in the PR.

## Commits and pull requests

* Small, focused commits with a message that says *why*. Describe how you tested it, and on which platform.
* Use the pull-request template. If the change affects what is synced, what is stored in the cloud or what is
  shown as a state, update `docs/` in the same PR.
* Be honest in the description about what you did **not** verify.

## Reporting problems

Use the issue templates. Run `packsync doctor --out report.txt` and attach the report (secrets are redacted,
but read it before posting). Security problems: see [`SECURITY.md`](SECURITY.md) — please do not open a public
issue with exploit details.

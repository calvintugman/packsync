# Contributing

PackSync moves people's worlds between computers, so the first question for any change is whether it could
lose or corrupt someone's data.

## Building and testing

You need Rust 1.89+ and, for the desktop app, Node.js 22+. On Linux the app also needs WebKitGTK 4.1
(`webkit2gtk-4.1` on Arch, `libwebkit2gtk-4.1-dev` on Debian/Ubuntu).

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p packsync-cli --features failpoints --test killed   # kills the real process at every boundary

cd apps/desktop && npm ci
npm run typecheck && npm run lint && npm test
npm run build && npx playwright install chromium && npm run e2e  # the real UI against the real engine
```

CI runs the same commands. Tests never use a real Google account, a real Prism install or the network.

## Code layout

`packsync-core` is the engine and knows nothing about Prism or any cloud: it uses the `Storage` and
`Launcher` traits. The CLI and the desktop app only call `packsync-app`. A new cloud is a new `Storage` that
passes `testkit::conformance`; a new launcher is a new crate implementing `Launcher`.

## Rules for changes

- Never merge worlds automatically, and never delete the losing side of a conflict.
- A missing or unreadable folder is unknown, not deleted.
- Cloud data is untrusted input: bound its sizes, check its hashes, keep its paths inside the instance.
- Credentials stay in the OS credential store and never appear in logs or snapshots.
- A bug fix comes with a test that fails without it. Changes to saving, uploading or restoring should add a
  failpoint and a test that crashes there.

Never commit credentials, mod JARs, Minecraft files or real world data.

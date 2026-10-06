# Rebranding

Product names live in a few constants so a fork can rename the product without touching logic:

* `crates/packsync-core/src/brand.rs` — product name, CLI name, application ID (a placeholder: no ID is
  registered by anyone), keyring service, default Drive folder, state directory name, environment prefix.
* `apps/desktop/src-tauri/tauri.conf.json` and `apps/desktop/ui/src/brand.ts` (when the desktop app exists).

**Do not change** the `packsync.*` format identifiers (`packsync.snapshot`, `packsync.index`, …) or the
`ps=1` object tag: they are protocol names (`docs/remote-format.md`); changing them makes existing cloud data
unrecognisable. Changing the keyring service or Drive folder name makes an existing sign-in/folder
unreachable for the renamed build — migrate deliberately.

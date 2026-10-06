# Rebranding

Product names live in a few places so a fork can rename the product without touching logic:

| Where | What |
|---|---|
| `crates/packsync-core/src/brand.rs` | product name, CLI name, application ID, keyring service, default Drive folder, state-directory name, environment prefix, hook marker |
| `apps/desktop/src/brand.ts` | the name and tagline shown in the UI |
| `apps/desktop/src-tauri/tauri.conf.json` | `productName`, `identifier`, window title, bundle metadata |
| `apps/desktop/src-tauri/icons/source.svg` | the icon; regenerate the platform set with `npx tauri icon src-tauri/icons/source.png -o src-tauri/icons` (render the SVG to a 1024 px PNG first, e.g. `rsvg-convert -w 1024 source.svg -o source.png`) |
| `README.md`, `docs/` | prose |

The application identifier `org.example.packsync` is a **placeholder**: nobody has registered it. A
distributor must choose a reverse-DNS identifier they control.

**Do not change** the `packsync.*` format identifiers (`packsync.snapshot`, `packsync.index`, …) or the `ps=1`
object tag: they are protocol names (`docs/remote-format.md`), and changing them makes existing cloud data
unrecognisable. Changing the keyring service, the Drive folder name or the hook marker makes an existing
sign-in, an existing cloud folder or already-installed launcher hooks unreachable for the renamed build —
migrate deliberately.

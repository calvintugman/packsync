# Changelog

## 0.2.0

### Sign-in, security and PackSync Cloud
- Replaced passkeys and anonymous signups with **Sign in with Minecraft** (Microsoft OAuth 2.0 device-code authorization via `microsoft.com/link`), so signing in with the same Microsoft account on another PC automatically reconnects to your encrypted cloud vault.
- Hosted Worker verifies Microsoft `RS256` ID tokens against Microsoft's JWKS endpoint, hashes `(tid, sub)` into an irreversible salted `ms_hash` without storing display names or emails in D1, wraps each account's 32-byte encryption key with `AES-256-GCM` under `VAULT_SECRET`, and supports up to 10 concurrent device sessions per account.
- Added abuse protections and operational limits on PackSync Cloud: **5 GiB** default quota and **10,000 objects** per account, **3,000 write/list ops** and **25,000 read ops** per day, **2 new accounts/IP/day** (**40/day** globally), security response headers (`HSTS`, `X-Content-Type-Options`, `X-Frame-Options`), and a daily scheduled cleanup job that removes empty accounts after 7 days and inactive accounts after 180 days.
- Published the PackSync Cloud Privacy Policy & Terms of Use in [PRIVACY.md](PRIVACY.md) and served them at `/privacy` and `/terms` on the hosted service.
- Deleting a PackSync Cloud account (`Delete cloud account & data` in Settings or `packsync storage delete-account`) now automatically clears stale local sync registrations and Prism launch hooks tied to that deleted account without touching local Minecraft files.

### Desktop app and CLI
- Redesigned the desktop app in dark mode with slim icon-rail navigation, instance status cards with accent bars, a dashboard statistics strip, and a cloud overview rail.
- Expanded storage setup in the UI to support **PackSync Cloud (Minecraft sign-in)** alongside **Google Drive**, **Synced Folder**, and **Cloudflare R2**.
- Added a 1-click **Remove old sync link** action on instance cards that are bound to a deleted or different storage account.
- Simplified the **Activity** screen to show recent operations first and keep detailed logs tucked behind a collapsible **Show logs** toggle with a `Warnings & errors` filter, **Export JSON**, and **Clear logs**.
- Added an **About** screen (`#/about`) and new CLI commands `packsync storage login` and `packsync storage delete-account`.

## 0.1.0-beta.2

### Security and PackSync cloud
- Pairing confirms the shared key before the first computer uploads encrypted account credentials, so a mistyped code fails immediately without writing an offer.
- Plaintext `http://` storage endpoints are restricted to loopback (`localhost`, `127.0.0.1`, `[::1]`) for PackSync cloud and S3/R2.
- Restoring an instance strips launcher hook/wrapper commands and machine-specific Java paths from `instance.cfg`, and enforces mixed-file policies by path even if a manifest omits the policy marker.
- Mixed INI merging handles bare `\r`, `\r\n`, UTF-8 BOMs, and case-insensitive section and key names.
- Recovery keys (`packsync1-...`) are redacted in logs and `doctor` reports, and sensitive key buffers are zeroed when dropped.
- Hosted Worker adds atomic quota reservation and rollback, IPv6 `/64` pairing rate limiting, a per-account limit on active pairings, early D1 revocation when deleting an account, and D1 indexes.

### Sync engine, storage and launchers
- Faster cloud `HEAD` checks, process checks, and Windows directory scans.
- Case-only file and folder renames on Windows and case-insensitive filesystems restore and roll back without overwriting backups.
- Local SQLite state (schema v4) tracks staged packs per `(snapshot_id, id)` so snapshots sharing a pack record uploads accurately, and prunes finished job history past the latest 2,000 entries.
- Instance locks write holder details to a companion `.info` file so lock-holder messages work reliably with Windows byte-range locks; process checks match instance roots on path boundaries.
- Clearer errors for cross-device moves and read-only files versus a running game, and provider-neutral troubleshooting advice.
- Google Drive OAuth saves rotated refresh tokens when Google issues a new token during refresh.
- S3 / Cloudflare R2 supports single-PUT packs above 64 MiB, accepts `aws:kms` server-side encryption ETags, and parses XML elements with attributes or self-closing tags.
- Prism launch hooks use `flatpak-spawn --host` with explicit config/state directories for Flatpak instances, skip ephemeral `/tmp/.mount_*` AppImage paths when locating the CLI, and run chained `pre-launch` commands before starting the background lease.

### Desktop app
- Connecting to Cloudflare R2 or PackSync cloud runs a storage preflight check before replacing existing settings or credentials.
- Runtime settings (`upload_concurrency`, `recovery_point_days`, `lease_ttl_secs`) apply immediately without restarting PackSync.
- Unreadable or missing instance folders ("unknown" state) show a banner and resolution dialog in the instance screen, and crash/unknown resolution toasts distinguish local saves from completed uploads.
- Direct PackSync cloud controls in Settings, Cancel buttons during setup, file pickers for executable and OAuth client files, instance-aware rule previews, `Home`/`End` keyboard navigation in tabs, and dialog focus-trap fixes.
- Application identifier updated to `io.github.calvintugman.packsync`.

## 0.1.0-beta.1

- Initial public beta for Linux (`x86_64` tarball and `.deb`) and Windows (`x86_64` portable `.zip`).
- End-to-end encrypted PackSync cloud with SPAKE2 device pairing and recovery keys, alongside Cloudflare R2, Google Drive and local/synced folder storage.
- Prism Launcher discovery, world/mod/config sync rules, conflict history without automatic world merging, crash-save holdback, and opt-in launch hooks.

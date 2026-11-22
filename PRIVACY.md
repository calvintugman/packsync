# PackSync Privacy Policy & Terms of Use

Last updated: November 2026

This policy describes how **PackSync Cloud** (the optional hosted storage service in `services/hosted`, also served at `/privacy` and `/terms` on the service endpoint) handles data, along with what the PackSync desktop app and CLI store locally.

## 1. What PackSync Cloud stores

PackSync Cloud is designed to avoid collecting personal profile data. **No email address, display name, password, or usage analytics are stored on the server.** (Your Microsoft display name from the ID token is returned only in the immediate sign-in response to your client and is never persisted in the database.)

- **Account identifier & sign-in link:** An irreversible salted `SHA-256` hash (`packsync-ms-v1:<tid>:<sub>`) of your Microsoft tenant and subject ID so signing in with the same Minecraft (Microsoft) account on another computer reconnects to your own vault, a random 26-character account ID, hashed per-device session tokens (up to 10 active sessions per account), creation and last-seen timestamps, and byte, object, and daily operation counters.
- **Encrypted modpack files & server-wrapped vault key:** Modpacks, configs, and single-player worlds are encrypted locally with `XChaCha20-Poly1305` before leaving your computer. To allow seamless multi-PC sign-in without manual recovery steps, your 32-byte vault encryption key is stored wrapped with `AES-256-GCM` under a server secret (`VAULT_SECRET`, bound with additional authenticated data to your account ID). Cloudflare and the service operator can observe encrypted object sizes, object key paths, and timestamps, while file contents remain encrypted at rest.
- **Ephemeral device pairing & rate limiting:** Optional SPAKE2 pairing messages expire within 10 minutes. Salted `SHA-256` hashes of client IPv4 addresses or IPv6 `/64` prefixes (used strictly to enforce registration and sign-in rate limits) and daily operation counters (`op_usage`) are automatically purged after 48 hours (`day < today - 2`).

## 2. Microsoft sign-in, third-party infrastructure & alternative backends

- **Microsoft Entra ID device-code sign-in:** PackSync Cloud authenticates via Microsoft's OAuth 2.0 Device Authorization Grant (`openid profile offline_access`). Your sign-in interaction on `microsoft.com/link` is subject to Microsoft's privacy statement.
- **Infrastructure provider:** PackSync Cloud runs on Cloudflare Workers, Cloudflare R2 object storage, and Cloudflare D1 database infrastructure.
- **Other storage backends:** If you configure Google Drive, your own Cloudflare R2 bucket, or a local/synced folder instead of PackSync Cloud, files are transferred directly between your computer and your chosen storage provider; PackSync operates no intermediate server for those backends.
- **Local app data:** Settings, SQLite sync state, OS credential-store entries, and redacted local logs stay on your own computer.

## 3. Retention & deletion

- **Instant self-service deletion:** You can permanently delete your PackSync Cloud account, all active device sessions, usage rows, and every stored object at any time from **Settings → Cloud → Delete cloud account & data** in the desktop app, or by running `packsync storage delete-account` in the CLI.
- **Automatic cleanup:** A daily scheduled cleanup task (`scheduled()`) automatically deletes expired pairing mailboxes, rate-limit/usage rows older than 2 days, empty accounts (`0` stored bytes) inactive for more than 7 days, and inactive accounts with no activity for more than 180 days (`INACTIVE_DAYS`).

## 4. Quotas, rate limits & acceptable use

- **Default limits:** **10 GiB** storage per account, **10,000 objects** per account, **3,000 write/list operations per day**, **25,000 read operations per day**, **200 GiB** total service storage ceiling, and **20 sign-in attempts per hour** per network (with no daily new-user cap).
- **Acceptable use:** Automated abuse, non-PackSync bulk file hosting, or attempts to bypass quotas or rate limits may result in immediate account removal.

## 5. No warranty (Terms of Use)

PackSync Cloud is provided free of charge on a best-effort, **"as-is"** basis without warranty, uptime guarantee, or service-level agreement. Always keep local copies of irreplaceable worlds on your own computer or connect your own Google Drive or Cloudflare R2 storage if you prefer full administrative control.

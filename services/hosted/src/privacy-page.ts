// Privacy Policy and Terms of Use served at /privacy and /terms.
export const PRIVACY_PAGE = `<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>PackSync Cloud — Privacy Policy &amp; Terms</title>
<style>
  :root { color-scheme: dark; }
  body {
    margin: 0; padding: 40px 20px; background: #0b0d13; color: #f1f5f9;
    font: 15px/1.65 system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
  }
  main {
    max-width: 720px; margin: 0 auto; background: #121620;
    border: 1px solid #232b3d; border-radius: 16px; padding: 32px 36px;
  }
  h1 { margin: 0 0 8px; font-size: 24px; }
  h2 { margin: 26px 0 8px; font-size: 17px; color: #c4b5fd; }
  p, ul { margin: 0 0 12px; color: #cbd5e1; }
  ul { padding-left: 22px; }
  li { margin-bottom: 6px; }
  code { background: #181d2a; padding: 2px 6px; border-radius: 5px; font-size: 13px; color: #e2e8f0; }
  .muted { color: #94a3b8; font-size: 13.5px; }
</style>
<main>
  <h1>PackSync Cloud — Privacy Policy &amp; Terms</h1>
  <p class="muted">Last updated: November 2026</p>

  <h2>1. What data PackSync Cloud stores</h2>
  <p>PackSync Cloud avoids collecting personal profile data. No email address, name, password, or usage analytics are stored on the server.</p>
  <ul>
    <li><strong>Account identifier &amp; login link:</strong> an irreversible salted <code>SHA-256</code> hash of your Microsoft account subject ID (so signing in on another computer reconnects to your own vault), a random 26-character account ID, hashed per-device session tokens, creation and last-active timestamps, and byte/object counters.</li>
    <li><strong>Encrypted modpack files:</strong> uploaded modpacks, configs, and worlds are encrypted locally with <code>XChaCha20-Poly1305</code> before leaving your computer. Your 32-byte encryption key is stored wrapped with <code>AES-256-GCM</code> under a server secret so signing in on a second computer requires no manual recovery step. Cloudflare and the operator can see encrypted object sizes, pack key paths, and timestamps, but pack contents remain encrypted.</li>
    <li><strong>Ephemeral device pairing &amp; rate-limiting:</strong> short-lived SPAKE2 pairing messages expire within 10 minutes. Salted <code>SHA-256</code> hashes of client IPv4 addresses or IPv6 <code>/64</code> prefixes are kept for up to 48 hours strictly to enforce registration and login rate limits.</li>
  </ul>

  <h2>2. Infrastructure provider</h2>
  <p>PackSync Cloud runs on Cloudflare Workers, Cloudflare R2 object storage, and Cloudflare D1 database infrastructure.</p>

  <h2>3. Retention &amp; deletion</h2>
  <ul>
    <li><strong>Instant self-service deletion:</strong> you can permanently delete your account, session tokens, and every stored object at any time from <em>Settings → Cloud → Delete cloud account &amp; data</em> (or <code>packsync storage delete-account</code>).</li>
    <li><strong>Automatic cleanup:</strong> empty accounts with 0 stored bytes older than 7 days and accounts with no sync activity for 180 days are automatically purged by a daily scheduled cleanup job.</li>
  </ul>

  <h2>4. Acceptable use &amp; service limits</h2>
  <ul>
    <li>Default storage quota is <strong>5 GiB per account</strong>, with up to <strong>10,000 objects</strong>, <strong>3,000 writes/listings per day</strong>, and <strong>25,000 reads per day</strong>, plus an overall service storage ceiling to keep the free hosted tier sustainable.</li>
    <li>Automated abuse, non-PackSync bulk hosting, or attempts to bypass rate limits or quotas may result in immediate account removal.</li>
  </ul>

  <h2>5. No warranty</h2>
  <p>PackSync Cloud is provided free of charge on a best-effort, <strong>"as-is"</strong> basis without warranty, uptime guarantee, or SLA. Always keep local copies of irreplaceable worlds on your own computer or connect your own Google Drive / Cloudflare R2 storage if you prefer full control.</p>
</main>
</html>`;

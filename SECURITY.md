# Security

Please report security problems privately with GitHub's "Report a vulnerability" button on the Security tab,
not in a public issue. Include the PackSync version, your operating system and steps to reproduce.

Things worth reporting:

- tokens, keys or passwords showing up in a log, an error message, a diagnostics report or a snapshot;
- cloud data that makes PackSync read, write or delete files outside the instance folder, or crash;
- PackSync deleting or overwriting data it was not asked to;
- any way for the PackSync cloud, or anyone in between, to silently change encrypted data, bypass
  authentication or authorization, or learn a SPAKE2 pairing code.

Not vulnerabilities: data in your own Google Drive or R2 bucket is not encrypted by PackSync (the app says
so), the PackSync cloud can see object sizes, key paths and times, and a malicious mod in a modpack is out of scope.

Trust boundary note for PackSync Cloud: modpacks, configs, and worlds are encrypted locally with
`XChaCha20-Poly1305` before upload, and optional SPAKE2 device pairing runs in the native client so the server
cannot observe the pairing code. Account sign-in uses Microsoft OAuth 2.0 device-code authorization: the Hosted
Worker verifies the Microsoft `RS256` ID token against Microsoft's public JWKS keys, stores only an
irreversible salted `SHA-256` hash (`ms_hash`) of `(tid, sub)` (no email or display name is stored in D1),
and wraps/unwraps your account's 32-byte vault key with `AES-256-GCM` under the Worker secret `VAULT_SECRET`
(bound via AAD to your account ID) so signing in on another computer restores your encrypted vault without
manual recovery steps. Because the Hosted Worker holds `VAULT_SECRET`, using PackSync Cloud requires
trusting the Worker operator with key wrapping; anyone who prefers strict client-only key custody can
self-host `services/hosted` or connect their own Cloudflare R2 bucket or Google Drive instead. See
[PRIVACY.md](PRIVACY.md) for data retention, quotas, and service limits.

# Google Drive adapter (`crates/packsync-drive`)

Design notes for maintainers. Facts about Google's API and why each choice was made are in
`docs/dev/google-drive-verification.md` (verified 2026-10-06); this page is the map of the code and the
invariants that tests pin down.

## Map

| Module | What it does |
|---|---|
| `config` | `DriveConfig`, `DriveSpace` (visible folder / app data), `OAuthClientConfig` (client JSON, environment, compiled-in), endpoints and timeouts. |
| `oauth` | `Authenticator`: installed-app flow with PKCE (S256), `state` check, loopback listener, silent refresh (single-flight), revoke/disconnect. `flow.rs` is the browser/loopback part. The refresh token lives in the `CredentialStore` (OS keychain) — there is no file fallback. |
| `credentials` | `KeyringCredentialStore` (Secret Service / Keychain / Credential Manager). |
| `storage` | `DriveStorage`, the `packsync_core::Storage` implementation on Drive v3 with blocking `ureq` + rustls (no async runtime). `api` = request building + retries, `upload` = multipart/resumable, `download` = ranged reads with resume. |
| `mock` (feature `mock`) | In-process Drive v3 + OAuth server with fault injection, used by every test and the benchmarks. |
| `bin/selftest` (feature `selftest`) | Live conformance run against a real account; see `docs/live-validation.md`. |

## Invariants (each has tests in `crates/packsync-drive/tests`)

* **Logical keys, not paths.** Callers use keys like `packs/<hash>.pack`; the adapter maps them to
  folder IDs and file names and persists IDs, because Drive names are not unique. Duplicates are
  tolerated, reported and **never deleted automatically**; the canonical copy is the oldest by
  `createdTime`, then the lowest ID (`duplicates_and_keys.rs`).
* **No assumption of atomic primitives.** There is no compare-and-swap in Drive v3; `CreateOnly` is
  implemented with pre-generated IDs (`files.generateIds`) so an ambiguous create is retried idempotently
  (409 → verify size and checksum). `list` is treated as eventually consistent (`list_wait` in the
  conformance options).
* **Uploads are verified.** Every stored object's size and `md5Checksum` (`sha256Checksum` where present)
  is compared with what was sent; a mismatch removes the bad object and reports corruption
  (`uploads.rs: checksum_mismatch_*`). Downloads of whole objects verify the checksum too.
* **Resumable uploads survive process death.** The session URI and confirmed offset are persisted through
  `TransferStateStore` (the engine's SQLite database); a restarted process resumes from the offset the
  server reports; expired sessions (404 after a week) restart cleanly. Session URIs are secrets: never
  logged (`oauth.rs: no_secret_ever_appears_…`).
* **Retry policy.** `RetryPolicy::network()`: full jitter, bounded attempts, `Retry-After` honoured as a
  floor and returned to the caller when it exceeds `max_retry_after` (so the UI can say "try again in 40
  minutes" instead of freezing). Quota-exceeded and permission errors are *not* retried.
* **Token handling.** One refresh in flight at a time (`errors.rs: concurrent_401s_share_one_refresh`);
  `invalid_grant` becomes `AuthProblem::Revoked`; a token that could break HTTP header framing is
  rejected before use.
* **Secret hygiene.** Tokens, client secrets, authorization codes and upload sessions never appear in
  `Debug`/`Display` output, error messages or PackSync's own log events (`oauth.rs`), and the application's
  log setup pins `ureq` & co. to `warn` because they print full headers at `debug`
  (`crates/packsync-app/src/logging.rs`).

## Running the tests

```sh
cargo test -p packsync-drive                       # mock server, no network, no credentials
cargo test -p packsync-drive -- --ignored          # also touches the OS credential store (live_keyring_roundtrip)
cargo build -p packsync-drive --features selftest  # the live self-test (never run in CI)
```

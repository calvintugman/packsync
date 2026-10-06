# Google OAuth / Drive API — verified facts and design consequences

**Verification date:** 2026-10-06. Every page below was fetched on that date; the "last updated"
dates are the ones shown on the pages themselves. Google moves its documentation around; if a link
404s, search for the page title. Nothing here has been validated against a *live* Google account
yet — see `docs/live-validation.md` for the exact steps and the list of unverified claims.

## 1. Desktop OAuth (installed application)

Source: <https://developers.google.com/identity/protocols/oauth2/native-app> (updated 2026-09-14)

| Fact | Consequence for PackSync |
|---|---|
| Supported redirect for desktop apps: loopback IP `http://127.0.0.1:<port>` or `http://[::1]:<port>`; `localhost` works but may trip firewalls. | We listen on `127.0.0.1` on an ephemeral port. |
| Custom URI schemes: "no longer supported due to the risk of app impersonation". Out-of-band (OOB) flow: "no longer supported". | Neither is implemented. No embedded browser: the system browser is used. |
| PKCE: `code_challenge_method=S256` recommended (verifier 43–128 chars of `[A-Za-z0-9-._~]`). | Always S256. |
| `state` is "recommended"; the client "must prevent CSRF". | We generate a random `state` per attempt and reject any callback whose `state` differs. |
| `client_secret` is listed as optional for installed apps (not applicable to Android/iOS/Chrome clients). Desktop-type clients are issued one in the Cloud console. | We send it when configured and tolerate its absence. **Unverified live.** |
| Endpoints: auth `https://accounts.google.com/o/oauth2/v2/auth`, token `https://oauth2.googleapis.com/token`, revoke `https://oauth2.googleapis.com/revoke`. | Constants, overridable for tests. |
| "refresh tokens are always returned for installed applications". | We still send `access_type=offline` and `prompt=consent` on interactive login so a re-login after revocation reliably yields a refresh token. |

### Refresh-token lifetime (important for the developer configuration path)

Source: <https://developers.google.com/identity/protocols/oauth2> (section "Refresh token expiration")

* A Cloud project whose OAuth consent screen is **external** and in publishing status **Testing**
  issues refresh tokens that **expire after 7 days**, unless the only scopes are a subset of
  `openid`/email/profile. `drive.file` and `drive.appdata` are *not* exempt.
  → For real use, move the consent screen to **In production**. Do not ship an app whose users
  silently lose access weekly.
* Other expiry reasons: user revoked access; token unused for six months; the account exceeded
  **100 live refresh tokens per client ID**; admin policy changes.
* `invalid_grant` on refresh = token expired/revoked → we surface `AuthProblem::Revoked` and ask the
  user to reconnect. Cloud data is never touched by this.

## 2. Scope choice

Source: <https://developers.google.com/workspace/drive/api/guides/api-specific-auth> (updated 2026-09-03)

* `drive.file` — **non-sensitive**. Described as "Create new Drive files, or modify existing files,
  that you open with an app or that the user shares with an app while using the Google Picker API".
* `drive.appdata` — **non-sensitive**. "View and manage the app's own configuration data".
* `drive`, `drive.readonly`, `drive.metadata*` — **restricted** (security assessment). Not requested.
* Google: "choose the most narrowly focused scope possible".

**Decision:** default to `drive.file` with an app-created, *visible* root folder (`PackSync/`), so
users can see (and delete) what the app stores. `drive.appdata` is supported as an alternative
(`DriveSpace::AppData`): hidden from the user and from other apps
(<https://developers.google.com/workspace/drive/api/guides/appdata>), deleted when the user
uninstalls the app, files cannot be trashed (only deleted), and the user cannot browse or back it up.

**Unverified claim that the whole design depends on:** files *created by the app* stay accessible to
the same OAuth client on another device/session under `drive.file`. The current page describes
opened/picked files; older official text and community answers say "per-file access to files created
or opened by the app". Live validation step 4 in `docs/live-validation.md` tests this with two
machines and one client ID. **If it fails, switch to `drive.appdata`** (same code path, one config
value).

## 3. Uploads, IDs, downloads

Sources: <https://developers.google.com/workspace/drive/api/guides/manage-uploads>,
<https://developers.google.com/workspace/drive/api/guides/create-file>,
<https://developers.google.com/workspace/drive/api/reference/rest/v3/files/generateIds>,
<https://developers.google.com/workspace/drive/api/guides/manage-downloads>

* Multipart upload for ≤ 5 MB with metadata; **resumable** for larger files / flaky networks:
  `POST https://www.googleapis.com/upload/drive/v3/files?uploadType=resumable` (PATCH for update),
  optional `X-Upload-Content-Type`/`X-Upload-Content-Length`, session URI in the `Location` header.
* Chunks must be multiples of 256 KiB except the last; `Content-Range: bytes a-b/total`; `308`
  means "continue"; ask for progress with `Content-Range: bytes */total`.
* A session URI **expires after one week** (404 afterwards) → persist it, resume while valid, then
  restart. Session URIs are bearer-like secrets: stored only in the local DB, **never logged**.
* Max file size 5 TB; **750 GB/day** upload cap (<https://developers.google.com/workspace/drive/api/guides/limits>, updated 2026-09-11).
* `files.generateIds` returns IDs that can be passed as `id` in `files.create`; "subsequent retries
  return a 409 Conflict … and duplicate files aren't created". The File resource states arbitrary
  client-chosen IDs are **not** supported — IDs must come from `generateIds`.
  → We pre-generate an ID per object and persist it with the upload state, which makes an
  ambiguous create (timeout after commit) safely retryable and idempotent.
* Blob files expose `md5Checksum`, `sha1Checksum`, `sha256Checksum` (output-only; populated for
  content stored in Drive) → used to verify uploads without re-downloading ("verified according to
  the provider's capabilities").
* `files.get?alt=media` supports `Range: bytes=a-b` partial downloads.

## 4. What Drive does NOT give us

Source: File resource reference <https://developers.google.com/workspace/drive/api/reference/rest/v3/files>

* **No `etag`/conditional write in v3.** `version` is output-only. There is **no compare-and-swap**.
* **File names are not unique** within a folder; folders are objects with IDs, paths do not exist.
* No transactions across files, no locks.
* `files.list`: `pageSize` 1–1000, results may include `incompleteSearch`, trashed files are
  included unless `trashed=false`, page tokens "valid for several hours". No statement about
  read-after-write consistency of `list` → we treat `list` as **eventually consistent** and never
  use it alone to prove absence.
* `changes.list` / `getStartPageToken` exist, but the docs do not say how they behave under
  `drive.file` visibility → **not relied upon**. Remote discovery lists the small `commits/` and
  `heads/` folders instead.

## 5. Errors and backoff

Sources: <https://developers.google.com/workspace/drive/api/guides/handle-errors>, limits page above

| Response | Meaning | PackSync mapping |
|---|---|---|
| 401 `authError` | token expired/invalid | refresh once, else `AuthProblem::Expired/Revoked` |
| 403 `rateLimitExceeded`, `userRateLimitExceeded`, `sharingRateLimitExceeded`, `dailyLimitExceeded` | quota/rate | retryable → `RateLimited` (+ `Retry-After`) |
| 429 | too many requests | retryable → `RateLimited` |
| 403 `storageQuotaExceeded` | Drive full | **not** retryable → `QuotaExceeded` |
| 403 `activeItemCreationLimitExceeded` | 500M item limit | not retryable → `QuotaExceeded` |
| 403 `insufficientFilePermissions`, `appNotAuthorizedToFile`, `domainPolicy` | no access | `Permission` |
| 404 | not found / not visible to this app under `drive.file` | `NotFound` |
| 409 on create with a pre-generated ID | already created | success after verifying size+checksum |
| 5xx | backend | retryable → `Transient` |

Backoff: "truncated exponential backoff" with a 32–64 s ceiling → `RetryPolicy::network()` (full jitter,
6 attempts, 32 s cap). Quota units (limits page): `files.list` 100, `files.get` 5, download 200,
`files.update` 50; 325 000 units/min/user/project → sustained use far below the limit, but bursts of
thousands of tiny `list` calls would not be — another reason objects are packed.

## 6. Properties

Source: <https://developers.google.com/workspace/drive/api/guides/properties>

* `appProperties` are private to the OAuth client (visible only with a token from that client ID);
  ≤ **124 bytes per key+value (UTF-8)**, ≤ 30 private properties per app per file, ≤ 100 in total.
  Queryable: `appProperties has { key='k' and value='v' }`.
* Used for: a `ps=1` marker on everything PackSync creates (retention only ever touches marked
  objects) and small tags (`kind`, parent snapshot IDs of commits). Never as authority: manifests are.

## 7. Quota display

`about.get?fields=storageQuota` → `limit` (absent when unlimited), `usage`, `usageInDrive`,
`usageInDriveTrash` (<https://developers.google.com/workspace/drive/api/reference/rest/v3/about>).

## 8. Design consequences (summary)

1. Immutable, content-addressed objects + immutable commit manifests; **no mutable global pointer**.
2. Remote "heads" are discovered by listing commits (graph computed locally); per-device head records
   are single-writer optimisations only.
3. Logical keys ≠ Drive names: persist file IDs, tolerate duplicates, choose a canonical copy
   deterministically (oldest `createdTime`, then lowest ID), verify bytes by hash on every read.
4. Pack small objects (≥ 16 MiB packs) to keep request counts low; resumable uploads with persisted
   sessions for large packs; pre-generated IDs for idempotent creates.
5. Treat `list` as eventually consistent; `ready on another device` is only claimed after the
   discovery path (a fresh listing) shows the snapshot.

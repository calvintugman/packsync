# Validating PackSync against the real Google Drive

**Status: nothing in this repository has been run against a live Google account.** Everything the
Drive adapter does is covered by tests against an in-process *mock* of the Drive v3 and OAuth endpoints
(`crates/packsync-drive/src/mock`, including fault injection), and the mock encodes what Google's
documentation says (`docs/dev/google-drive-verification.md`). The claims in the table below are exactly the
ones documentation cannot settle. This page is the procedure for settling them. **Please do it with a
throwaway Google account** — the self-test only touches one folder, but a test account makes that a
guarantee.

## What is unverified

| # | Claim | Why it matters | How it is checked |
|---|---|---|---|
| 1 | Files created by the app under `drive.file` stay visible to the same OAuth client on **another computer** | The whole design (a second PC restores what the first uploaded) | two-computer check (below) |
| 2 | The client secret is optional for Desktop clients | Whether `client_secret` can be left out | sign-in works with a client JSON that has the secret removed |
| 3 | `files.list` right after `files.create` shows the new file (and how long it lags) | "ready on another device" is only claimed after a fresh listing shows the snapshot | `list_wait` in the conformance report |
| 4 | Concurrent creates of the same name produce duplicates that PackSync resolves deterministically | Two computers uploading at once | conformance `racing creators` + duplicate tests |
| 5 | `appProperties` (`ps=1`, `kind`, parent IDs) are stored, queryable and ≤ 124 bytes per key+value | Retention only touches tagged objects | conformance `tags` |
| 6 | A resumable upload session can be resumed from the persisted URI after the process died | Large packs over flaky links | interrupt the self-test during the large upload (Ctrl-C), run it again |
| 7 | `sha256Checksum`/`md5Checksum` are present soon after upload | Uploads are verified without re-downloading | conformance `checksums` |
| 8 | `files.generateIds` + `create` with that ID returns 409 on a repeat | Idempotent create after an ambiguous failure | conformance `idempotent create` |
| 9 | Trashed files stay recoverable; permanent delete works for files the app created | `gc` uses the trash | conformance `delete` + look in Drive's Trash |
| 10 | In *Testing* publishing status refresh tokens expire after 7 days | The README's OAuth warning | observe, or trust Google's page |
| 11 | Request counts and throughput for a real 2 GB initial sync | `docs/performance.md` was measured against the mock | the end-to-end check at the bottom |

## 0. Prepare

1. Create a throwaway Google account (or use a spare one) and an OAuth client as in
   `docs/oauth-setup.md`. For the 7-day expiry observation leave the consent screen in **Testing** and add
   the account as a test user; for everything else **In production** is more convenient.
2. Build the self-test (it is not part of the default build):

```sh
cargo build -p packsync-drive --features selftest --release
export PACKSYNC_GOOGLE_CLIENT_ID=…apps.googleusercontent.com PACKSYNC_GOOGLE_CLIENT_SECRET=…   # or --client-file
ST=target/release/packsync-drive-selftest
```

The tool refuses to run without `--i-understand-this-uses-my-drive`, keeps its refresh token in the OS
credential store under its own service name (`packsync-selftest`), and creates everything inside one folder
named `PackSync-Test`, tagged `ps=1`.

## 1. The conformance suite

```sh
$ST --i-understand-this-uses-my-drive run            # add --no-large to skip the 12 MiB resumable test
```

It signs in (browser), runs ~15 checks (create-only races, replace, ranged reads, pagination with 60+
objects, large resumable upload, duplicate handling, tags, delete, quota), prints PASS/FAIL per check, the
HTTP request counts and your Drive usage, and then deletes **exactly the objects it created** (`--keep` leaves
them). Any FAIL, or a failed cleanup, makes the exit status non-zero. Record the output in `docs/testing.md`
(date, account type, PASS/FAIL list).

Resumable-upload check (#6): run it again with the large test, press Ctrl-C while the 12 MiB object is
uploading, and run it once more. The adapter persists the session URI in the transfer-state store; with the
self-test's in-memory store the resume happens within one run, so for the cross-*process* check use the
application path in section 3.

## 2. Two computers: does `drive.file` work across devices? (#1)

On **computer A**:

```sh
$ST --i-understand-this-uses-my-drive cross-device-write
```

On **computer B** (same Google account, **same OAuth client**, i.e. the same client JSON or environment):

```sh
$ST --i-understand-this-uses-my-drive cross-device-read
```

* `PASS` → files created by the app on A are readable on B under the default scope. Done.
* `FAIL` → under `drive.file` B cannot see what A created. Repeat both steps with `--space app_data`. If that
  passes, set `packsync storage use-google --space app_data` for real use and report the finding (the README's
  scope paragraph should then recommend app data).

Clean up afterwards: `$ST … cross-device-cleanup`, then delete the empty `PackSync-Test` folder in Drive
if you like, and `$ST … logout --revoke`.

## 3. The application end to end (#6, #11)

```sh
packsync storage use-google && packsync auth login
packsync enable "<your Prism instance folder>"        # first computer: snapshot + upload
packsync status --online
# second computer (same account and client):
packsync storage use-google && packsync auth login
packsync cloud && packsync restore-new
```

Watch `packsync doctor --online` and the log (`<state>/logs/packsync.log`; secrets are redacted) for
backoff messages. To check #6 across processes, start `packsync upload` on a large pack, kill the process,
start it again and confirm from the log that it resumed instead of restarting.

Use the benchmark tool against the real service for #11 (`cargo run -p packsync-bench --release -- … --storage
google`, see `docs/performance.md`) and compare its request counts with the mock's.

## 4. Recording results

Add a dated section to `docs/testing.md` ("Live Google Drive validation") with: date, account type (free /
Workspace), consent-screen status, the self-test's PASS/FAIL list and request counts, the two-computer result,
and anything surprising. Until that section exists the project's status for Google Drive is **implemented and
tested against a mock only**, and the README says so.

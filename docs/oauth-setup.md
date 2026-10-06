# Connecting Google Drive: OAuth setup

PackSync stores your instances in **your own Google Drive**. There is no PackSync server and no shared
account: the app signs in as you, with a permission you grant in your browser, and talks to Google
directly. To do that it needs an *OAuth client* — an identity for the application that Google's consent
screen shows. This page explains how to create one, what permission is requested and what the limits are.

> **Why do I have to create a client?** A client ID identifies an *application* to Google and is tied to
> a Google Cloud project that someone is responsible for. This repository does not ship one (nobody has
> registered one on behalf of the project). A maintainer who publishes official builds can compile their
> client ID in (see "Packaged builds" below); until then every user — or one person for a family or
> friend group — creates their own, which takes about ten minutes and is free.

## What PackSync asks for

| | |
|---|---|
| **Scope** | `https://www.googleapis.com/auth/drive.file` (default) — *non-sensitive*. |
| What it allows | Files and folders **created by PackSync** (and files you explicitly open with it). It cannot see the rest of your Drive. |
| Where data lives | A visible folder named **PackSync** in "My Drive" — you can look at it, move it, or delete it. |
| Alternative | `drive.appdata` (*non-sensitive*): a hidden per-app folder. Pick it with `packsync storage use-google --space app_data`. Hidden from you and other apps, deleted by Google if you uninstall the app's access, and files cannot go to the trash. |
| Not requested | `drive`, `drive.readonly`, `drive.metadata*` (restricted scopes that need a security assessment). |

No data is encrypted by PackSync (yet): your files are stored as-is in your Drive, protected by Google's
account security. See `docs/threat-model.md`.

**Open question (verified live only by you):** Google's documentation describes `drive.file` as access to
files "that you open with an app or that the user shares with an app". PackSync relies on *files it
created* staying accessible to the same OAuth client on your other computers. This is how the scope has
always behaved, and the documentation of this exact guarantee is thin; `docs/live-validation.md` contains
a two-computer check (`packsync-drive-selftest cross-device-write/read`). If it fails for you, use
`--space app_data` — the code path is the same.

## Creating the client (about ten minutes)

Google moves its console around; if a menu below is not where it says, search the console for the name.
Current documentation: <https://developers.google.com/identity/protocols/oauth2/native-app> and
<https://support.google.com/cloud/answer/6158849>.

1. Open the [Google Cloud console](https://console.cloud.google.com/) and **create a project** (any name,
   for example `PackSync`).
2. **Enable the Google Drive API**: *APIs & Services → Library → Google Drive API → Enable*.
3. **Configure the consent screen** (*Google Auth Platform* / *APIs & Services → OAuth consent screen*):
   * User type **External**.
   * App name `PackSync`, your e-mail as support and developer contact.
   * **Data access / Scopes**: add `…/auth/drive.file` (and only that).
4. **Publishing status — read this.** A new external app starts in **Testing**. In Testing, Google issues
   refresh tokens that **expire after 7 days**, so PackSync would ask you to sign in again every week, and
   only the *test users* you list can sign in. For real use choose **Publish app → In production**. Because
   `drive.file` is a non-sensitive scope no security review is required for it; Google may still show the
   "unverified app" screen until the app's branding is verified, which is fine for a personal client — click
   *Advanced → continue*.
5. **Create the client**: *Credentials → Create credentials → OAuth client ID → Application type
   **Desktop app***. Download the JSON file (`client_secret_….json`).

## Giving the client to PackSync

Any one of these (checked in this order):

1. `packsync settings set google_client_file /path/to/client_secret_….json` (or the same field in the
   desktop app's Settings → Storage).
2. Save the JSON as `google-oauth.json` in PackSync's config folder (`packsync doctor` prints it).
3. Environment: `PACKSYNC_GOOGLE_CLIENT_ID` (and optionally `PACKSYNC_GOOGLE_CLIENT_SECRET`).
4. A client compiled into the binary (packaged builds only).

For desktop clients Google issues a "client secret" and states it is not confidential in installed apps;
PackSync sends it when present and works without it. It is never written to logs, to PackSync's settings
file or to the cloud — `packsync doctor` and `--json` output never contain it.

Then:

```sh
packsync storage use-google            # choose Google Drive (visible PackSync folder)
packsync auth login                    # opens your browser; add --no-browser to print the address instead
packsync storage test                  # shows the connection and your free space
```

Signing in uses Google's *installed application* flow: the system browser (never an embedded web view),
PKCE (S256), a random `state` check and a one-shot loopback listener on `127.0.0.1`. The only thing
stored is the **refresh token**, in your operating system's credential store (Windows Credential Manager,
macOS Keychain, or a Secret Service such as GNOME Keyring / KWallet on Linux). **There is no plaintext
fallback**: if no secure store is available PackSync says so and refuses to sign in rather than writing the
token to a file.

## Signing out, revoking, and what happens when access stops working

* `packsync auth logout` forgets the token on this computer. Your cloud data is not touched.
* `packsync auth logout --revoke` additionally asks Google to invalidate the grant.
* Revoking access in <https://myaccount.google.com/permissions>, a password change that revokes sessions,
  six months without use, or (in Testing status) seven days all end the grant. PackSync then shows
  *"Google no longer accepts the saved sign-in"* and offers **Reconnect**; nothing is deleted and pending
  uploads resume afterwards.
* Deleting data is a separate, explicit operation (`packsync gc …`); disconnecting never deletes anything.

## Limits worth knowing

* Drive's upload limit is 750 GB per day per user; a single object may be up to 5 TB. PackSync packs small
  files into ~16 MiB objects, so a 2 GB modpack is a few hundred requests, not thousands.
* Free accounts have 15 GB shared with Gmail and Photos. When Drive is full PackSync stops uploading,
  explains why, keeps your local snapshot and resumes later (`QuotaExceeded`, exit code 20).
* A Cloud project's refresh tokens are capped at 100 live tokens per client ID and account: if you share
  one client between many people, each person's *own* token is separate, but the same person signing in on
  more than 100 devices would revoke the oldest.

## Packaged builds

A maintainer who distributes binaries can compile a client in with
`PACKSYNC_GOOGLE_CLIENT_ID=… PACKSYNC_GOOGLE_CLIENT_SECRET=… cargo build --release` (the values are read by
`option_env!` at build time, see `crates/packsync-drive/src/config.rs`). Users of such a build skip steps
1–5. The maintainer is then the owner of the Cloud project and responsible for its consent-screen
branding; PackSync itself never sees anyone's data on the way.

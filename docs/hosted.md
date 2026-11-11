# The PackSync cloud

The PackSync cloud is the storage that needs no setup from the person using PackSync. There is no sign-up,
no e-mail and no password:

1. **First computer:** *Start on this computer* (`packsync storage start`). The app creates an account on the
   service and a random encryption key, and keeps both in the computer's credential store.
2. **Every other computer:** on a computer that is already in, *Show a pairing code* (`packsync storage pair`);
   on the new one, *Join from another computer* and type it (`packsync storage join 4821-K7QM-X2PD`).
3. **If every computer is lost:** the *recovery key* (`packsync storage recovery-key`, `packsync storage
   recover`) restores access. It is the only way back, so the app asks you to keep it somewhere safe.

It is run by whoever builds and distributes PackSync, on their Cloudflare account: a small Worker
(`services/hosted`) in front of one R2 bucket, with a D1 database for accounts.

## Privacy and security

* **End-to-end encrypted.** Every object is encrypted on the computer before upload, with a key that never
  leaves your computers: XChaCha20-Poly1305 in 64 KiB segments, each bound to its object and position, so the
  service can neither read nor undetectably alter, reorder, truncate or swap data
  (`crates/packsync-hosted/src/crypto.rs`). A wrong or damaged object is reported as corrupt, never used.
* **What the operator can see:** that an account exists, when it was last used, object keys (content hashes
  and random instance IDs; no file, world or instance names), object sizes and times, and network addresses
  of requests. Identical content uploaded twice under the same key encrypts identically (this keeps
  retries idempotent).
* **Pairing** uses SPAKE2, a password-authenticated key exchange, through a mailbox on the service: the
  two computers derive a shared key only if both typed the same code, and nobody watching (the service
  included) can test guesses of the code offline. A mailbox accepts one joiner and expires after 10
  minutes, so a guessed code gets one try at 40 bits. The account's token and key travel sealed under that
  shared key (`crates/packsync-hosted/src/pairing.rs`).
* **The account token** is a 256-bit random value; the service stores only its SHA-256.
* **Losing all computers and the recovery key** means the data cannot be decrypted by anyone, including
  the operator. That is the price of the service not being able to read it.

## Limits

| | Default (`services/hosted/wrangler.toml`) |
|---|---|
| Space per account | 20 GiB (the GregTech: New Horizons copy with history uses about 6–10 GB) |
| Space of all accounts together | 200 GiB: uploads beyond it are refused, so the operator's bill is bounded |
| New accounts per network address per day | 5 |
| Largest single upload | 90 MB (Workers accept up to 100 MB; PackSync's packs are 16 MiB) |

When an account is full, uploads fail with "this account's space is full" and nothing already stored is
affected; `packsync gc` frees space taken by old history.

## Costs for the operator

Cloudflare R2: 10 GB stored free, then $0.015 per GB-month; downloads are free. One GTNH-sized player costs
roughly $0.10–0.15 a month beyond the free tier. Workers' free plan allows 100,000 requests a day (a first
upload of a 5.4 GB instance is about 400 requests; a sync 1–15); D1's free plan is far more than accounts
need. The total-space cap is the safety net against surprises.

## Deploying it (operator)

Requirements: a Cloudflare account (free), Node.js.

```sh
cd services/hosted
npm install
npx wrangler login        # opens the browser once
./setup.sh                # creates the R2 bucket and D1 database, applies schema.sql, deploys; prints the URL
```

Then build PackSync with that address built in, so users never type it:

```sh
PACKSYNC_HOSTED_URL=https://packsync.<your-subdomain>.workers.dev cargo build --release -p packsync-cli
cd apps/desktop && PACKSYNC_HOSTED_URL=https://… npm run tauri build
```

(`PACKSYNC_HOSTED_URL` at run time overrides the built-in address; `--service <url>` on `storage start` and
`storage join` does too.)

After deploying, check the real service with the same tests the mock passes (creates three throwaway
accounts and deletes them):

```sh
PACKSYNC_TEST_HOSTED_URL=https://packsync.<your-subdomain>.workers.dev \
  cargo test -p packsync-hosted --test service against_the_real_service_when_running
```

## Developing

```sh
cd services/hosted
npm install
npm run db:init:local     # local D1
npm run dev               # the Worker on http://localhost:8787 with a local R2 and D1
PACKSYNC_TEST_HOSTED_URL=http://localhost:8787 cargo test -p packsync-hosted
```

`.dev.vars` (not committed) can raise `REGISTRATIONS_PER_DAY` for local testing. The Rust tests also run
against an in-process stand-in (`packsync_hosted::mock`), without Node.

## What has been verified

* The storage contract (the conformance suite every provider passes), plain and encrypted, against the
  in-process stand-in and against the Worker running locally (`wrangler dev`, local R2 and D1).
* Pairing (right code, wrong code, a used code), encryption properties, quotas, recovery keys.
* The command-line tool end to end against the local Worker: start, enable, pair, join, restore on the second
  computer (identical files), an edit synced back, recovery on a third.
* **Not yet:** the Worker on Cloudflare itself. R2's conditional writes (`If-None-Match: *`, `If-Match`) are
  the documented behaviour, and the local runtime implements them, but run the check above after deploying.

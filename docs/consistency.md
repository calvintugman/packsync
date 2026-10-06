# Consistency model and failure analysis

PackSync keeps one or more computers' copies of an instance convergent through Google Drive, which is
**not** a database: there is no compare-and-swap, no transaction across files, no unique names, and
`files.list` may lag behind writes (`docs/dev/google-drive-verification.md`). This page states what
PackSync guarantees on top of that, how, and — as importantly — what it does not.

## Guarantees

1. **No lost update.** Two computers that change the same instance from the same snapshot produce two
   snapshots with the same parent: an explicit divergence. Nothing is overwritten; the user decides
   (`keep local` / `keep cloud` / `split into a new instance`), and the other side is preserved in
   history. World files are never merged automatically.
2. **A published snapshot is complete.** A manifest is uploaded *last*, after every pack it needs was
   uploaded and verified and the index was written. A reader that sees a manifest can restore it; a reader
   that does not see it (yet) simply has the previous state.
3. **A crash does not become the truth.** A session that ended without a clean exit produces a
   *quarantined* snapshot (`kind: crash`): uploaded and kept, never a head, never an automatic
   fast-forward target. The user promotes it or discards it.
4. **Restores are recoverable.** A restore is journaled and by default rolled back if interrupted before
   it was fully applied; after that point recovery rolls forward. At every moment the instance is exactly
   the old version, exactly the new one, or an in-progress state that the next start repairs — never a
   silently mixed one. (This is *recoverable*, not atomic: it is not claimed to be one rename.)
5. **Missing is not deleted.** A folder that is unreadable, an unmounted drive or a vanished instance is
   "unknown", never "the user deleted everything"; a snapshot that would delete more than half of the
   files requires explicit confirmation; a different drive mounted at the same path is refused by an
   ownership marker.
6. **Nothing is deleted in the cloud on its own.** Disconnecting, signing out and revoked access never
   delete cloud data. Cleanup is manual, previewed, confirmed by plan id, restricted to objects PackSync
   created, and conservative about in-flight uploads.
7. **Secrets stay local.** A sealed exclusion list (account files, tokens, keys, launcher settings with
   API keys) cannot be overridden by any rule; the refresh token lives only in the OS credential store.

## How, in short

| Mechanism | Why |
|---|---|
| Content-addressed immutable objects (packs, indexes, manifests) | idempotent, retryable and cacheable operations; no write conflicts between devices |
| History is a DAG of manifests (`parents`); **heads are computed by listing commits** | no shared mutable pointer to race on; the per-device head file is a hint only |
| Manifest uploaded last; uploads idempotent and resumable | partial uploads are invisible and harmless; a retry converges |
| Decisions come from verified manifests, never from tags or listings alone | a lagging or lying listing cannot cause an overwrite |
| `list` treated as eventually consistent; "ready on another device" is only claimed after a **fresh listing** shows the snapshot | no false "synced" |
| Instance lock (OS file lock) + game-process detection + `session.lock` probes + stable-read check | no snapshot or restore while a writer is active; a torn read is detected |
| Journal + trash + recovery points for restores | rollback by default, roll-forward after the point of no return, user-visible recovery points |
| Sessions, ownership and a grace period | a crash is told apart from "the game just exited and the post-exit hook is about to run" |
| Advisory leases | a warning that another device seems to be playing; deliberately not a lock |

## What happens when…

| Event | Result |
|---|---|
| Process killed during snapshot creation | Either no snapshot (nothing committed) or a complete local one; the next run continues. Tested at every failpoint (`crates/packsync-core/tests/crash.rs`) and with real process kills (`crates/packsync-cli/tests/killed.rs`). |
| Killed during upload | Packs may exist in the cloud without a manifest — invisible orphans, reused by the retry (existing packs are detected by name and size), removable by cleanup after the grace period. |
| Killed after the manifest, before the head record | The snapshot is published; the head record is rewritten later (or never needed: discovery lists commits). |
| Killed during an in-place restore | Before `Applied`: rolled back, the instance is exactly as before. After: rolled forward and, for a restore the user asked for, recorded as a new snapshot so it sticks. Recovery itself can be killed and repeated. |
| Killed during a new-instance restore | The hidden staging folder (ignored by the launcher) is recognised by its ownership marker and removed on the next start; the instance is never registered half-restored. |
| Two computers upload simultaneously | Two heads. Reconcile reports a conflict; both snapshots are kept. |
| A laptop that was offline for months syncs | Its base is an ancestor of the head (fast-forward) or not (conflict); retention never makes that undecidable because manifests are kept and only *content* of old snapshots is released. |
| Drive duplicates a file name (concurrent create) | Tolerated; the canonical copy is the oldest, then the lowest ID; duplicates are reported, never auto-deleted. |
| Upload response lost after the server committed | The pre-generated Drive file ID makes the retry a no-op (409 → verify size/checksum). |
| Cloud object corrupted | Detected by hash on read; the restore refuses to apply it and nothing local is touched; `verify --remote` reports it. |
| Disk full / permission denied during restore | Preflight checks space and names first; a failure during apply rolls back. |
| Access revoked / token expired | Operations fail with an authentication error; local data, pending snapshots and cloud data are untouched; reconnecting resumes. |
| Drive full | Upload stops with `QuotaExceeded` (not retried); the snapshot stays local and uploads later. |
| Clock skew between computers | Never used for ordering decisions; only the grace period and display use time, with generous margins. |

## Honest limits

* **Not safe against a hostile cloud account.** Anyone who can write to your Drive folder can add
  snapshots; PackSync validates and bounds everything it reads, and a malicious snapshot cannot write
  outside the instance, but it can *contain* content you then run (mods). Treat the folder like the
  account that owns it. There is no end-to-end encryption in v1 (`docs/threat-model.md`).
* **Mid-session snapshots are not taken.** A snapshot happens when the game is not running; a computer
  that dies during a session loses at most that session's unsaved progress (the world's own autosaves are
  captured by the crash/interrupted-session snapshot at the next start).
* **Restore is recoverable, not atomic.** There is a window in which files are being replaced; recovery
  guarantees the end state, not the absence of that window.
* **Cleanup is pack-granular.** Space is reclaimed only for packs none of whose blobs is needed;
  partly-used packs are reported but not repacked (future work).
* **A tiny window remains** between a restore finishing its journal and recording itself as a snapshot
  when the process is killed in exactly that instant: the files are the restored version, and the next
  sync may fast-forward away from it (recovery point and history keep both). No data is lost.
* **Leases are advisory.** Two computers started at the same moment both play; the result is a conflict
  to resolve, not corruption.

# How PackSync works, in plain language

This page explains the five ideas the sync engine is built on, what each one buys, and where the code is.
It is written to be read top to bottom once; the file pointers are for looking things up afterwards.

The one-sentence version: **PackSync cuts every file into pieces, names each piece by a fingerprint of its
content, uploads only pieces the cloud does not have yet, and records each save as a small list of
fingerprints that points back at the save it was made from.**

## The problem

A modded Minecraft instance is several gigabytes. After an hour of play, a few megabytes of it have changed,
scattered through files that are hundreds of megabytes each. Uploading the whole thing after every session
is too slow to use. Uploading "the files that changed" is not much better, because the files that change are
the big ones. So the engine has to find the changed *parts* of files, and it has to do that without ever
putting a half-written or mixed-up world in front of the player.

## 1. Chunking: cutting files into pieces

A file is split into **chunks** (the code calls them *blobs*). If one chunk changes, only that chunk is
uploaded. How the cuts are placed matters, and the engine uses three strategies, chosen per file
(`resolve_strategy` in `crates/packsync-core/src/chunk.rs`):

| Strategy | Used for | How it cuts |
|---|---|---|
| **Fixed blocks** | Region files (`*.mca`, `*.mcr`) | Every 16 KiB, counted from the start of the file |
| **Content-defined (FastCDC)** | Other large files that are edited in place (over 4 MiB, not an archive) | Where the *content* says so; average 64 KiB, between 16 KiB and 256 KiB |
| **Whole** | Everything else (mod jars, configs, images) | One piece per file, split at 8 MiB only because that is the size limit of a piece |

**Why fixed blocks for region files.** A region file stores Minecraft chunks in 4 KiB sectors, and the game
rewrites a changed chunk *in place*: the rest of the file does not move. Fixed 16 KiB blocks line up with
that. Walk around and place a few blocks, and only the 16 KiB blocks covering the touched sectors are new.
Nothing shifts, so nothing else looks changed.

**Why content-defined chunking for other files.** Fixed blocks have one weakness: insert one byte near the
start of a file and every later block boundary shifts by one byte, so *every* block looks new. Content-defined
chunking fixes this. It slides a small window over the data, computes a rolling hash of the window, and cuts
wherever that hash matches a pattern. The cut points depend on the bytes around them, not on the distance
from the start of the file. Insert a byte, and the cut points after it still land on the same content, so the
chunks after the insertion are byte-for-byte the same chunks as before and are not uploaded again.
FastCDC is a specific, fast algorithm for this; PackSync uses the `fastcdc` crate (`StreamCDC` in
`chunk_stream`).

The honest summary for an interview: *the engine supports both. Region files, which are most of a world,
use fixed blocks because the file format already guarantees in-place edits; content-defined chunking is for
the files where data can shift.* The defaults were picked by measuring both on region-file edits
(`packsync-bench chunking`, `docs/performance.md`).

Memory stays bounded: chunkers are streaming iterators, so at most one chunk (8 MiB at most) per file being
processed is in memory at a time.

## 2. BLAKE3: naming things by their content

Every chunk gets a **BLAKE3 hash**: a 32-byte fingerprint computed from its bytes
(`crates/packsync-core/src/hash.rs`). The same bytes always give the same hash, and it is not feasible to
find two different chunks with the same hash. The engine uses the hash as the chunk's *name*. This is called
**content addressing**, and three things follow from it:

* **Deduplication.** Before uploading a chunk the engine asks "does the cloud already have a chunk with this
  name?" If yes, it uploads nothing. A renamed mod jar, the same library in two modpacks, and the unchanged
  99 % of a region file all cost zero bytes.
* **Verification.** After downloading a chunk, hash it. If the hash is not the name it was asked for, the
  data is damaged or wrong and is rejected before it gets anywhere near the instance (`verify_blob` in
  `remote.rs`).
* **Immutability.** A name can only ever mean one content, so nothing in the cloud is ever overwritten. Two
  computers uploading at once cannot corrupt each other: they either upload the same thing or different
  things with different names.

Hashes are used at four levels, and each is checked on the way back in:

| What | Named by |
|---|---|
| A chunk | BLAKE3 of its raw bytes |
| A whole file (in the snapshot) | BLAKE3 of the file's content |
| A pack (a bundle of chunks, see below) | BLAKE3 of the pack file |
| A snapshot | BLAKE3 of its manifest |

Why BLAKE3 and not SHA-256: it is several times faster on modern CPUs and can use many cores, and the first
sync of an instance has to hash every byte of it.

**Avoiding the hashing.** Hashing 5 GB takes seconds, so the scan does not hash what it can prove unchanged.
For every file it keeps size, modification time, change time and file identity in a local database
(`scan.rs`, `db.rs`). If all of them match the last scan, the old hash is reused. Anything that does not
match, or cannot be trusted (first scan, after a restore, after a crash), is hashed again. Change time
(`ctime`) matters because, unlike modification time, a program cannot set it back: a same-size edit with a
restored timestamp is still noticed.

## 3. Zstandard: compressing what is worth compressing

Each chunk is stored either raw or compressed with **zstd**, decided per chunk by measuring
(`encode_blob` in `crates/packsync-core/src/codec.rs`):

* File types that are already compressed (jars, zips, PNGs, and region files, whose chunks the game has
  already compressed) are stored raw. Compressing them again burns CPU for nothing.
* Everything else is compressed, and the compressed form is kept only if it is at least 10 % smaller.

Decompression is treated as hostile input. Every chunk records its uncompressed size; the decoder refuses to
produce more than that, so a tiny file in the cloud cannot expand into gigabytes (a "decompression bomb").

**Packs.** Uploading 100,000 small chunks as 100,000 files would be slow and would hit Google's rate limits.
Chunks are therefore bundled into **pack files** of about 16 MiB (`pack.rs`). A pack ends with a table saying
which chunk sits at which offset. An **index** file per upload lists all of that, so another computer can find
a chunk without opening packs. To read one chunk the engine asks the cloud for just that byte range of the
pack.

A file that was cut into several chunks also gets a **recipe**: the ordered list of its chunk hashes. The
recipe is itself stored as a chunk.

## 4. Snapshots: what one save looks like

A **snapshot** is the state of the whole instance at one moment. It is a single document, the **manifest**
(`manifest.rs`, written by `snapshot.rs`), containing:

* every synced file: path, size, hash, and its recipe if it has one;
* the **parent**: the ID of the snapshot this one was made from;
* which computer made it, when, and whether the game exited cleanly;
* the sync rules that were in effect.

The manifest holds no file data, only fingerprints, so it is small. The snapshot's ID is the BLAKE3 of the
manifest. Because the manifest contains the parent's ID, a snapshot ID pins down the entire history behind
it. This is the same structure as a git commit.

Every manifest lists the *complete* instance, not a difference from the previous one. Any snapshot can be
restored on its own, and the cost is tiny because unchanged files are just the same hashes again.

**Upload order is what makes it safe** (`upload.rs`): packs first, then the index, then a check that
everything arrived, and only then the manifest. A snapshot does not exist for other computers until its
manifest does. If the network drops halfway, the cloud holds some extra packs and no manifest; nobody can see
a half-uploaded save, and the next attempt skips what is already there.

**Restoring is journaled** (`restore.rs`, `journal.rs`). New files are downloaded and verified into a staging
folder first. Then each replaced file is moved aside and the new one moved in, with every step written to a
journal. If the process dies at any point, the next run reads the journal and either finishes or undoes it.
The old files are kept as a recovery point.

## 5. Conflict detection: who is ahead of whom

Each computer remembers its **base**: the snapshot its files currently correspond to. It also knows whether
its files have changed since (*dirty*). The cloud has **heads**: snapshots that no other snapshot names as a
parent. With parents forming a chain, the question "which is newer" becomes "is one an ancestor of the
other", which has an exact answer. The whole decision is one function, `decide` in
`crates/packsync-core/src/graph.rs`:

| Situation | Decision |
|---|---|
| The cloud head *is* my base, and I changed nothing | Up to date |
| The cloud head is my base, and I have changes | I am ahead: upload |
| My base is an ancestor of the cloud head, and I changed nothing | The cloud is ahead: download (a "fast-forward") |
| The cloud head is an ancestor of my base | I am ahead: upload |
| My base is an ancestor of the cloud head, **but I also have changes** | **Conflict** |
| Neither is an ancestor of the other | **Conflict** |
| The cloud has two heads | **Conflict** |

A conflict means two computers both played from the same starting point. PackSync never tries to merge two
worlds; a merged region file would be a corrupt world. It shows both versions and the player picks: keep
mine, take the cloud's, or keep both as separate instances (`engine/conflict.rs`). Whichever is not chosen
stays in the history and can still be restored.

Two details make this robust:

* **No clocks.** The decision uses only the parent links. A computer with a wrong clock cannot make an old
  save look new.
* **No shared "latest" pointer.** There is no single file in the cloud saying "this is the current save"
  that two computers could both overwrite. Each computer only *adds* manifests; the heads are computed by
  reading them. Two computers uploading at the same moment produce two heads, which is exactly a conflict,
  and nothing is lost.

A separate safeguard covers crashes (`session.rs`): if the game did not exit cleanly, the save is stored as a
*crash save*, which is never treated as a head, and the player decides whether to keep it.

## Putting it together: one play session

1. **Before the game starts**, PackSync reads the cloud's manifests and runs `decide`. If the cloud is ahead,
   it downloads only the chunks this computer is missing and restores them through the journal.
2. **You play.** PackSync is not running.
3. **After the game closes**, it scans the instance (reusing hashes where it can), chunks and hashes the
   changed files, compresses the new chunks into packs, and writes a manifest whose parent is the base.
4. **Upload:** new packs, the index, a check, then the manifest. The base moves to the new snapshot.

## Where to look

| Topic | File |
|---|---|
| Chunking strategies | `crates/packsync-core/src/chunk.rs` |
| Hashing | `crates/packsync-core/src/hash.rs` |
| Compression and bomb protection | `crates/packsync-core/src/codec.rs` |
| Pack file format | `crates/packsync-core/src/pack.rs` |
| Change detection | `crates/packsync-core/src/scan.rs` |
| Building a snapshot | `crates/packsync-core/src/snapshot.rs`, `manifest.rs` |
| Upload order and resume | `crates/packsync-core/src/upload.rs` |
| History graph and the sync decision | `crates/packsync-core/src/graph.rs` |
| Conflict resolution | `crates/packsync-core/src/engine/conflict.rs` |
| Journaled restore | `crates/packsync-core/src/restore.rs`, `journal.rs` |
| Sessions and crash saves | `crates/packsync-core/src/session.rs` |
| Google Drive adapter | `crates/packsync-drive/src/storage/` |
| Exact cloud format | `docs/remote-format.md` |
| What is guaranteed, and the tests for it | `docs/consistency.md` |

## Questions you may be asked

* **Why not just use rsync or Google Drive's own sync?** Drive's client uploads whole files and, on a
  conflict, keeps a renamed copy of single files, which leaves a world that is a mix of two saves. rsync needs
  a server that runs code. PackSync needs only "store a file, read a byte range, list files", so any dumb
  storage works.
* **What happens if two chunks have the same hash but different content?** With a 256-bit hash this does not
  happen in practice; the same assumption underlies git and every content-addressed backup tool.
* **What if the upload is interrupted?** Nothing is visible until the manifest is uploaded, and it goes
  last. Uploaded packs are found again by name and skipped.
* **What is the cost of this design?** Space. History keeps old chunks until a cleanup removes them, and a
  chunk inside a pack that is still partly needed is not reclaimed. Cleanup is manual and previewed.
* **Is the data encrypted?** No. It sits in the user's own Drive under their Google account. End-to-end
  encryption is not implemented.

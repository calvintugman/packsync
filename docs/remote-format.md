# Remote format (version 1)

Everything PackSync stores in the cloud is described here. The format is a **protocol**: its names
(`packsync.*`) are not branding and must not change when the product is renamed (`docs/rebranding.md`),
or existing cloud data stops being recognised.

Design rules that shaped it (why, in `docs/consistency.md`):

* every object is **immutable and named by its content hash** — there is no mutable pointer that two
  computers could fight over;
* nothing is trusted: every size, count, offset and path read from the cloud is bounded and validated
  before use, and every blob is verified against its hash after decoding;
* readers ignore objects they do not recognise, and refuse (with an upgrade message) a *newer* major
  `version` rather than guess.

## Logical keys

A *storage provider* exposes a flat key space (`a/b/c`); components are `[A-Za-z0-9._-]`, at most 8
levels, 300 bytes. Google Drive maps keys to folders and file names under one root folder (default
`PackSync`); a local directory maps them to paths.

```
packs/<blake3-hex>.pack                                  content packs (immutable)
index/<blake3-hex>.idx                                   blob location indexes (immutable)
instances/<instance-uuid>/commits/<snapshot-id>.snap     snapshot manifests (immutable)
instances/<instance-uuid>/heads/<device-uuid>.json       single-writer "my latest snapshot" hint
instances/<instance-uuid>/leases/<device-uuid>.json      advisory "someone is playing" marker
instances/<instance-uuid>/expired/<snapshot-id>.json     marker: this snapshot's content was released
```

Everything PackSync creates carries the provider tag `ps=1` (Drive `appProperties`). Retention and
cleanup only ever touch objects with that tag, and only objects whose names match the patterns above.
Other tags (`kind`, and for commits `p0..p3` parents, `np` the number of parents, `dev`, `ts`) are *hints*: a
manifest is the authority, tags are only used to avoid reading manifests for display. A commit with more than
four parents (`np` > 4) is never read from its tags.

## Pack — `packs/<id>.pack`

```
header   16 B   "PSPACK1\0" + 8 zero bytes
blobs    …      stored blob bytes back to back (raw or zstd, see below)
footer   …      zstd( JSON { "format":"packsync.pack", "version":1, "entries":[{h,o,s,r,c,k}, …] } )
trailer  44 B   u32 LE footer_stored_len | BLAKE3(footer_stored) (32 B) | "PSPKEND1"
```

* `<id>` is the BLAKE3 of the **whole** pack file. A pack is only ever created, never modified.
* Entry fields: `h` blob hash (BLAKE3 of the *raw* bytes), `o` offset, `s` stored length, `r` raw length,
  `c` codec (`0` raw, `1` zstd), `k` kind (`0` data, `1` recipe).
* A blob is stored zstd-compressed only when it saves ≥ 10 % (and the file type is not already
  compressed: jars, zips, png, …); otherwise raw. The choice is per blob.
* Limits (a pack is untrusted input): ≤ 256 MiB, ≤ 1 000 000 entries, footer ≤ 32 MiB stored / 128 MiB
  raw, a blob ≤ 8 MiB, offsets/lengths inside the blob area, no overlaps.
* Packs are filled to a target size (default 16 MiB, one blob over); small blobs from many files share
  packs, so a 2 GB instance is a few hundred requests rather than thousands.
* **Layout (not part of the format, a property of the writer):** the blocks of one file are written next to
  each other (in pieces of up to 8 MiB), and files appear in roughly the order they were hashed. A reader can
  therefore fetch a whole file with one ranged request, and a whole restore with about one request per pack.
  Readers must not depend on it — any order is valid — but a writer that interleaves the blocks of many files
  makes restores slow (a request per block): see `docs/performance.md`.
* The footer makes a pack **self-describing**: an index can always be rebuilt from the packs.

## Index — `index/<id>.idx`

`zstd(JSON { "format":"packsync.index", "version":1, "packs":[{ "id","size","blobs":[entries…] }], "supersedes":[<index id>…] })`,
named by the BLAKE3 of the uncompressed JSON. One index is written per upload, after its packs and
before the manifest. It lets a computer find a blob (`pack + offset + length`) without reading
footers. A consolidated index lists many packs and names the indexes it replaces in `supersedes`.

A computer merges the blobs of an index only for packs that **exist with the recorded size** in the
cloud; a damaged or unreadable index is ignored (with a log entry), never fatal.

## Snapshot manifest — `instances/<id>/commits/<snapshot-id>.snap`

`zstd(JSON Manifest)`; the **snapshot ID is the BLAKE3 of the uncompressed canonical JSON**, so a
manifest read from the cloud is verified by hashing it.

```jsonc
{
  "format": "packsync.snapshot", "version": 1,
  "instance": { "id": "<uuid>", "name": "My Pack" },
  "device":   { "id": "<uuid>", "name": "desktop" },
  "created":  "2026-10-06T12:34:56.789Z",
  "parents":  ["<snapshot-id>", …],        // 0 = first snapshot, 1 = normal, 2+ = merge
  "kind":     "normal" | "crash",           // crash saves are never heads
  "fork_of":  { "instance": "<uuid>", "snapshot": "<id>" },   // only for "split" conflict resolution
  "session":  { "id", "started", "ended", "outcome": "clean|crashed|interrupted", "base", "offline" },
  "layout":   { "launcher": "prism", "game_root": ".minecraft" },
  "pack":     { "minecraft_version": "1.20.1", "loader": {…}, "components": […] },
  "worlds":   [ { "folder": "World", "name": "…", "last_played_ms": … } ],
  "rules":    { "version": 1, "hash": "<blake3>", "text": "<the complete effective rules>" },
  "stats":    { "files": 25, "dirs": 6, "bytes": 1028977 },
  "note":     "restored from snapshot …",
  "entries":  [ { "p": ".minecraft/config/a.toml", "t": "f", "s": 123, "m": 1700000000000,
                  "x": false, "h": "<blake3>", "r": "<recipe blob>", "mx": "<mixed policy id>" },
                { "p": ".minecraft/config/empty", "t": "d" }, … ]
}
```

* `entries` is the complete tree (no deltas): a snapshot is restorable on its own. Paths are
  instance-root-relative, `/`-separated, validated on read (no `..`, no absolute paths, no drive
  letters, no control characters, bounded length/depth/count, unique by exact path *and* by case-folded
  path on case-insensitive targets).
* `h` is the BLAKE3 of the whole file content; `s` its size; a file with `r` is split into chunks
  listed by the **recipe** blob (below), a file without `r` is the single blob `h`. Timestamps and
  the executable bit restore fidelity only; they are never part of a file's identity.
* `mx` marks a *mixed* file (for example `options.txt`, Prism's `instance.cfg`): the stored content is
  the **portable projection**; machine-local keys stay local.
* The effective **rules travel in every snapshot** (`rules.text`), so all computers and all app versions
  agree on what is synced; a device uses its *base snapshot's* rules, not its own defaults.
* Limits: ≤ 3 000 000 entries, ≤ 256 MiB raw, ≤ 8 parents.

## Recipe blob (kind 1)

```
"PSR1" | u32 LE n | n × ( 32 B chunk BLAKE3 | u32 LE chunk length )
```

A multi-chunk file's content is the concatenation of its chunks; lengths must sum to the file size and
each chunk is non-empty and ≤ 8 MiB. Chunking strategies (chosen per rule, default by file kind):
`whole`, `fixed:<bytes>` (region files `*.mca` — default 16 KiB blocks, see `docs/performance.md`),
`cdc:<min>:<avg>:<max>` (FastCDC for large mutable files), `auto`.

## Head and lease records

* `heads/<device>.json` — `{format:"packsync.head", version, device_id, device_name, updated, head,
  recent:[…up to 50 recent commits…]}`. **Single writer per file** (the device named in it). An
  optimisation and a UI aid only: discovery lists `commits/` and computes the history graph itself.
* `leases/<device>.json` — `{format:"packsync.lease", version, device_id, device_name, session, started,
  heartbeat, ttl_secs, base}`. Advisory: a fresh lease from another device produces a *warning*; a stale
  one (heartbeat older than `ttl_secs`) is ignored. Never used for exclusion.

## Expiry marker — `instances/<id>/expired/<snapshot-id>.json`

`{format:"packsync.expired", version:1, instance, snapshot, expired_at, device}`. Written by a cleanup
(`packsync gc run … --retain`) after it decided that a snapshot's content may be released. The manifest
stays, so the history graph and every ancestry decision are unchanged; computers that see the marker
mark the entry "expired" and refuse to restore it. Markers are separate immutable objects, never a
shared list, so two cleanups cannot overwrite each other.

## Versioning and compatibility

* Every document carries `format` and `version`. A reader accepts versions it knows and fails with an
  actionable message (update the app) for a newer one; it ignores unknown *fields* and unknown *objects*.
* A change that older readers could misread requires a new `version`; additive changes (new optional
  fields) do not. Version 1 has no deprecated fields yet.
* The local database (`state.db`) has its own schema version; opening a database written by a newer
  PackSync is refused rather than risked.

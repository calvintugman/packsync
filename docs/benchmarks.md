# Benchmarks: a real modpack against real storage

Everything on this page was measured on a copy of a real, heavily played **GregTech: New Horizons 2.9**
instance (Minecraft 1.7.10, 341 mods), synced with the `packsync` command-line tool. Unlike
`docs/performance.md` (generated fixtures, local folder and Drive mock), these numbers include real
network latency. Each table says how many runs it is; where there are several, the range is given, not
the best run.

## The test data

| | |
|---|---|
| Instance | copy of a GregTech: New Horizons 2.9.0-RC-1 instance (Prism Launcher 11.0.3) |
| Synced (Implementation 1 rules) | **6.84 GB in 19,634 files**: world 5.76 GB (3,645 files), config 390 MB (15,690 files), mods 684 MB (271 jars) |
| Synced (Implementation 2 rules) | **5.4 GB in 19,577 files**: the same minus Distant Horizons' 1.2 GB level-of-detail cache (see below) |
| Largest single file | `saves/Home/data/DistantHorizons.sqlite`, 1.2 GB, partly rewritten every session |
| Machine | AMD Ryzen 7 5700X3D, 16 GB RAM, NVMe (btrfs), CachyOS Linux; release build, rustc 1.99 |
| Network | home connection; measured upload to Google Drive ≈ 28 MB/s |

## What "a small edit" is

Two kinds, both reported:

* **Real play sessions:** start the game, open the world, change a few blocks, save and quit. The game
  rewrites far more than the blocks: about 190 files per session (region files around the player,
  `level.dat`, ~55 saved-data files, the 1.2 GB Distant Horizons database).
* **The synthetic edit** (`scripts/bench-world-edit.py --seed N`): the same *kind* of change, made
  identically every time so that two builds can be compared fairly: a few chunks in the three largest
  region files, `level.dat`/`level.dat_old`, every gzip-compressed saved-data file, and one SQLite
  transaction in the Distant Horizons database. The world stays valid and playable. A new seed per run
  (reusing a seed rewrites bytes that are already in the cloud).

`packsync sync --explain` reports every step with its time, HTTP requests and bytes; the numbers below
come from it, and "whole process" times from timing the command.

## Baseline (before Implementation 1), Google Drive

| | Result |
|---|---|
| First snapshot (scan, chunk, compress, pack 6.84 GB) | 29.3 s, 298,517 chunks in 392 packs |
| First upload | **239.4 s**, 6.76 GB sent, 2,830 HTTP requests, 0 retries (≈ 28 MB/s) |
| Full restore on a second computer (profile) | 439.2 s, 6.89 GB received, 4,600 requests; **byte-identical** to the original (`diff -r`) |
| Sync after a real play session (3 sessions) | **34.7–36.8 s**, 257–268 requests, 31–65 MB sent |
| Sync after the synthetic edit (1 run) | **36.0 s**, 243 requests, 7.9 MB sent |
| Sync with nothing changed | 2.3 s, 10 requests |

Where the 36.0 s went (`--explain`): **22.6 s and 215 requests looking up, one at a time, each pack the
changed files' unchanged blocks live in** — right after the upload had listed every pack anyway; 2.1 s
scanning the instance a second time; 2.2 s refreshing the index cache; six requests per run only to
look up folder IDs by name; ~0.5 s per Drive write, one after another.

Interrupting an upload (`kill -9`, twice, during 1.2 GB and 0.6 GB uploads): nothing was shown as
synced; the next run skipped the finished packs, resumed the half-sent ones from their saved upload
sessions, and `verify --remote` found nothing missing or damaged.

## Implementation 1: removing the request overhead (Google Drive)

What changed (commits "Publishing trusts the fresh pack listing…", "sync does not scan the instance a
second time…", "Drive: reuse confirmed folder IDs…"):

* dependencies the fresh pack listing shows are trusted; direct lookups only for packs it did not show,
  after cloud data was lost, and on every 20th publication (`verify --remote` stays the full check);
* no second scan after saving; no existence check before uploading a new pack; the index uploaded
  alongside its packs; the remaining lookups 16 at a time over one keep-alive connection pool;
* folder IDs confirmed well after their creation are reused across runs.

| Synthetic edit, Google Drive | Time | HTTP requests | Sent |
|---|---:|---:|---:|
| Baseline (1 run) | 36.0 s | 243 | 7.9 MB |
| **Implementation 1 (3 runs, seeds 104–106)** | **8.44–8.77 s** | **22–23** | 7.5–7.6 MB |

What is left on Drive: hashing the 1.2 GB database (1.7 s), the 860 KB manifest, and Drive writes at
~0.5 s each in sequence (pack, manifest, head record) plus listings that Drive needs because it has no
atomic compare-and-swap.

## Implementation 2: the R2 fast path

What changed (commits "Cloudflare R2 / S3 storage provider", "Sub-second sync on R2…", "Rules: Distant
Horizons…"):

* a **Cloudflare R2** provider (S3 API, AWS Signature V4, conditional writes);
* a **HEAD pointer** moved only by compare-and-swap (`If-Match`): while a computer's last known HEAD is
  still the cloud's, "nothing changed" is **one read** and a new snapshot is **four requests** (packs and
  index side by side, the manifest, the HEAD swap) with no listing; any disagreement falls back to the
  full path;
* the Distant Horizons database is excluded as a regenerable cache; the file index is updated by
  difference; no repeated completeness check right after saving.

Measured **against a local S3 server** (the mock in `crates/packsync-s3`, network time ≈ 0), i.e. the
local floor of the fast path:

| Synthetic edit, local S3 server | Whole process | Traced | HTTP requests | Sent |
|---|---:|---:|---:|---:|
| Implementation 2 (2 runs, seeds 202–203) | **0.68–0.71 s** | 0.66 s | **4** | 4.5 MB |
| Nothing changed (1 run) | — | 0.74 s | **1** | 0 |

Where the 0.66 s goes: walking 19.5k files 0.24 s, the "is a game running?" process check 0.10 s,
reading the base manifest 0.07 s, hashing the changed files (40.7 MB) 0.07 s, encoding the manifest
0.07 s, the database 0.06 s, the four requests 0.05 s.

### Through Prism's hooks

The same, as Prism runs it: the exact `PreLaunchCommand`/`PostExitCommand` that `packsync hooks install`
writes (the GTNH copy, local S3 server, 3 rounds, seeds 401–403; "play" = the synthetic edit):

| Step | Time |
|---|---:|
| Pre-launch hook, cloud unchanged (one read of HEAD) | **0.14–0.24 s** |
| Post-exit hook, until Prism gets control back (snapshot saved; upload started in the background) | **0.68–1.13 s** |
| Background upload finished, counted from the game's exit | **0.86–1.30 s** |
| Pre-launch hook on a second profile after the first one played (downloads the 40 changed files; full path) | 4.1 s (1 run) |

After the rounds, `verify --remote` found nothing missing or damaged, and the second profile's world was
identical to the first's except the Distant Horizons caches (excluded; each computer regenerates its own).

**Real Cloudflare R2: not measured yet.** On R2 the four requests become three sequential round trips
plus the upload of ~4.5 MB; the table below is filled in from the run in the next section.

| Synthetic edit, Cloudflare R2 | Whole process | HTTP requests | Sent |
|---|---:|---:|---:|
| Implementation 2 (3 runs) | *to be measured* | | |
| Nothing changed (3 runs) | *to be measured* | | |

## Reproducing

```sh
cargo build --release -p packsync-cli
pk() { target/release/packsync --config-dir "$HOME/bench/config" --state-dir "$HOME/bench/state" "$@"; }
cp -a --reflink=always "<a copy of an instance>" "$HOME/bench/inst" && rm -rf "$HOME/bench/inst/.packsync"

pk storage use-r2 --account-id <ID> --bucket <bucket> --access-key-id <key ID>   # or: storage use-google + auth login
pk storage test
pk --yes enable "$HOME/bench/inst"               # first upload
pk sync "<instance name>"                       # first full sync (creates HEAD on R2)
pk sync --explain "<instance name>"             # nothing changed
for seed in 301 302 303; do
  python3 -I scripts/bench-world-edit.py --world "$HOME/bench/inst/.minecraft/saves/<World>" \
      --seed $seed --i-know-this-edits-the-world
  time pk sync --explain "<instance name>"       # "whole process" = the shell's real time
done
```

`PACKSYNC_TRANSFER_STATS=<file>` additionally appends one JSON line per run (time, requests, bytes).
Never point `bench-world-edit.py` at a world you care about.

# Performance

Measured with `packsync-bench` (`cargo run -p packsync-bench --release -- --dir target/bench run --size small|large
--storage local|mock-drive`, and `… chunking [--big]`). Fixtures are generated (deterministic seeds), the work
directory is on a real disk, and every number comes from running the real engine end to end. **Single runs,
one machine — timings are indicative, byte counts and request counts are exact.**

| | |
|---|---|
| CPU | AMD Ryzen 7 5700X3D (8 cores / 16 threads) |
| RAM | 16.7 GB |
| OS | CachyOS Linux (kernel 7.2.9) |
| Disk | btrfs on NVMe (solid state), page cache warm |
| Toolchain | rustc 1.99.0, **release** build |

"Logical requests" are storage operations as the engine issues them; "Drive HTTP requests" are counted by the
adapter against the in-process **Drive mock** (no latency, no real Google — see `docs/live-validation.md`).

## Small fixture (108 synced files, 50.9 MB: 12 region files 37.2 MB, 20 jars 13.1 MB)

| scenario | time | sent | received | logical req. | Drive HTTP req. (mock) | peak RSS |
|---|---:|---:|---:|---:|---:|---:|
| initial: scan + pack | 0.20 s | – | – | – | – | 80 MB |
| initial: upload | 0.10 s | 50.8 MB | – | 15 | 64 | 80 MB (271 MB with the mock in-process) |
| no change: scan (warm cache) | 0.04 s | 0 | 0 | 0 | 0 | 117 MB |
| no change: snapshot | 0.08 s | 0 | 0 | 0 | 0 | |
| no change: cloud check (a launch) | 0.03 s | 0 | 515 B | 4 | 5 | |
| no change: cold re-hash of everything | 0.08 s | 0 | 0 | 0 | 0 | |
| small edit (10 configs, options.txt, servers.dat) | 0.14 s | 14.4 KB | 97.7 KB | 13 | 19 | |
| region edit (6 chunks in 3 region files) | 0.14 s | 475 KB | 0.7 KB | 14 | 20 | |
| 5 mod jars replaced (new content) | 0.14 s | 3.9 MB | 1.4 KB | 15 | 22 | |
| 5 jars renamed (dedup) | 0.12 s | 10.7 KB | 0.4 KB | 12 | 16 | |
| restore on a second computer | 0.62 s | – | 55.2 MB | 27 | 60 | 126 MB |
| catch up after one play session | 0.27 s | – | 314 KB | 10 | 17 | |

Full-archive baseline (all synced files through zstd-3, re-uploaded after every change — a *better-than-ZIP*
stand-in): 50.5 MB each time. PackSync's incremental uploads are 0.01–0.5 MB for the same edits except the
jar replacement (3.9 MB = the new jars, incompressible). Cloud space used after all scenarios: 55.5 MB.

## Large fixture (4,501 synced files, **4.3 GB**: 180 region files 2.3 GB, 300 jars 1.9 GB) — *partial run*

The run was stopped before the restore and baseline scenarios (end of the working session); these are the
scenarios that completed (local-directory backend):

| scenario | time | sent | received | logical req. | peak RSS |
|---|---:|---:|---:|---:|---:|
| initial: scan + chunk + compress + pack | 32.7 s (≈ 130 MB/s) | – | – | – | 190 MB |
| initial: upload to a local directory | 66.2 s | 4.3 GB | – | 705 | 218 MB |
| no change: scan (warm cache), 4,501 files | 0.07 s | 0 | 0 | 0 | 200 MB |
| no change: snapshot | 0.17 s | 0 | 0 | 0 | |
| no change: cloud check | 0.04 s | 0 | 518 B | 4 | |
| no change: cold re-hash of 4.3 GB (page cache warm) | 7.4 s | 0 | 0 | 0 | 209 MB |
| small edit | 3.65 s | 229 KB | **6.3 MB** | 10 | 227 MB |
| region edit (6 chunks in 3 files) | 8.65 s | 854 KB | 0.7 KB | 54 | 209 MB |
| 5 jars replaced | 4.21 s | 27.6 MB | 1.7 KB | 57 | 217 MB |
| 5 jars renamed | 0.80 s | 225 KB | 0.7 KB | 11 | 217 MB |

Memory stays below ~230 MB for a 4.3 GB instance. A no-change launch costs 0.07 s of scanning and four
storage requests. **Observed inefficiency:** the small edit *downloaded 6.3 MB* — the engine re-fetches the
index object it uploaded itself with the first snapshot (it is not marked as already seen). Harmless at this
size but wasteful; fixing it means recording the index as seen right after upload (open item).

## Chunking region files (fixed blocks vs content-defined), 206 MB of regions

Edit model: per save, 3 region files × 8 chunks rewritten in place (same sector count, new payload), twice.

| chunking | blobs | index+manifest metadata | uploaded for 311 KB of changes | write amplification | restore time* |
|---|---:|---:|---:|---:|---:|
| fixed 4 KiB | 50,344 | 2.1 MB | 668 KB | 2.1× | 5.6 s |
| fixed 8 KiB | 25,190 | 1.0 MB | 599 KB | 1.9× | 3.1 s |
| **fixed 16 KiB (default)** | 12,613 | 522 KB | 743 KB | 2.4× | 4.5 s |
| fixed 32 KiB | 6,326 | 261 KB | 1.1 MB | 3.6× | 1.1 s |
| fixed 64 KiB | 3,183 | 133 KB | 2.0 MB | 6.4× | 0.8 s |
| CDC 4/16/64 KiB | 10,400 | 503 KB | 1.1 MB | 3.4× | 10.6 s |
| CDC 8/32/128 KiB | 5,197 | 252 KB | 2.0 MB | 6.5× | 1.7 s |
| whole file | 64 | 8.8 KB | 38.8 MB | 125× | 1.2 s |

\* single noisy run; restore time grows with the number of blobs (per-blob file operations).

**Decision: 16 KiB fixed blocks for `*.mca`.** 8 KiB uploads ~140 KB less per session but doubles the blob count
(index size, database rows, restore time); 32 KiB and above lose the delta advantage; content-defined chunking
is slower and no better for the in-place edits Minecraft makes (region chunks are sector-aligned and rewritten
in place). Caveats: the edit model has no data shifts (a chunk that outgrows its sectors relocates in real
region files, which fixed blocks handle less well than CDC) and the fixture payloads are incompressible; the
rule `strategy = "cdc:…"` lets a user choose otherwise per path (`docs/rules.md`).

## Restore

Unchanged blocks of files being replaced are read from disk instead of downloaded (catch-up after a play
session fetched 314 KB instead of two whole region files; test
`catching_up_after_an_in_place_region_edit_downloads_only_the_changed_blocks`). A first restore of a large
instance is limited by the network and by per-blob file operations (≈ 45 MB/s locally at 16 KiB blocks); the
large-fixture restore was not measured.

## Not measured (open)

Large-fixture restore and baseline; the large chunking set (`chunking --big`); real Google Drive throughput
and request counts (the mock has no latency or quotas); Windows and macOS; a slow-disk (rotational) run.

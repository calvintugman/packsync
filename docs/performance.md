# Performance

Everything here was measured with `packsync-bench` on generated fixtures: the real engine, end to end, against
a local directory and against the in-process Drive mock. **Each number is one run on one machine.** Byte
counts and request counts are exact and repeatable; times are indicative (the disk and page cache move them
by tens of percent). Nothing here says anything about real Google Drive latency, quotas or throughput — that
has not been measured (`docs/live-validation.md`).

## Machine and method

| | |
|---|---|
| CPU | AMD Ryzen 7 5700X3D (8 cores / 16 threads) |
| RAM | 16.7 GB |
| OS | CachyOS Linux (kernel 7.2.9) |
| Disk | btrfs on NVMe (solid state); page cache warm |
| Toolchain | rustc 1.99.0, **release** build |

```sh
cargo build --release -p packsync-bench
packsync-bench --dir target/bench hardware
packsync-bench --dir target/bench run --size small --storage local|mock-drive      # also: tiny, medium, large
packsync-bench --dir target/bench run --size large --storage local
packsync-bench --dir target/bench chunking [--big]
```

* **Fixtures are generated** (fixed seeds; nothing from a real Minecraft installation): mod jars of
  random (incompressible) bytes, Anvil-format region files with realistic in-place chunk rewrites, configs,
  Unicode names, and the junk a game folder accumulates (logs, caches, natives) that the rules exclude.
* **Scenarios** run in order on one instance: initial snapshot and upload, no change (warm scan, snapshot,
  cloud check, cold re-hash), small edit (10 configs + `options.txt` + `servers.dat`), region edit (6 chunks
  in each of 3 region files), 5 mod jars replaced, 5 renamed, restore on a second computer, and that
  computer catching up after one play session.
* "**Logical requests**" are storage operations as the engine issues them. "**Drive HTTP requests**" are what
  the Drive adapter really sends to the mock (resumable-session steps count): this is the number of calls a
  real Drive would see, without its latency.
* The disk is flushed (`sync`, not timed) before each scenario so a scenario does not pay for the previous
  one's writeback — without it a small scenario can look 10× slower than it is on a 4 GB fixture.
* The **baseline** is the obvious alternative: compress all synced files into one `zstd -3` stream and upload
  it after every change.

## Small fixture — 108 synced files, 50.9 MB (12 region files 37.2 MB, 20 jars 13.1 MB)

| scenario | time | sent | received | logical req. | Drive HTTP req. (mock) | peak RSS |
|---|---:|---:|---:|---:|---:|---:|
| initial: scan + pack | 0.30 s | – | – | – | – | 125 MB |
| initial: upload | 0.19 s | 50.8 MB | – | 18 | 64 | 125 MB (272 MB in the Drive run: the mock server shares the process) |
| **no change**: scan (warm cache) | 0.04 s | 0 | 0 | 0 | 0 | 133 MB |
| no change: snapshot | 0.09 s | 0 | 0 | 0 | 0 | |
| no change: cloud check (what a launch does) | 0.04 s | 0 | 515 B | 4 | 5 | |
| no change: cold re-hash of everything | 0.08 s | 0 | 0 | 0 | 0 | |
| small edit (13 files) | 0.14 s | **14.4 KB** | 0 | 13 | 17 | |
| region edit (6 chunks × 3 files) | 0.14 s | **475 KB** | 0 | 14 | 18 | |
| 5 jars replaced (new content) | 0.16 s | 3.9 MB | 0 | 15 | 20 | |
| 5 jars renamed (dedup) | 0.13 s | **10.7 KB** | 0 | 12 | 14 | |
| restore on a second computer | 0.51 s | – | 55.2 MB | 30 | 62 | 135 MB |
| catch up after one play session | 0.30 s | – | 249 KB | 11 | 17 | |

Baseline (full archive after every change): **50.5 MB each time**; cloud space used by PackSync after all
scenarios: 55.5 MB for 50.9 MB of data.

## Large fixture — 4,501 synced files, **4.3 GB** (180 region files 2.3 GB, 300 jars 1.9 GB)

Local-directory backend:

| scenario | time | sent | received | logical req. | peak RSS |
|---|---:|---:|---:|---:|---:|
| initial: scan + chunk + compress + pack | 31.6 s (≈ 136 MB/s) | – | – | – | 229 MB |
| initial: upload | 59.0 s | 4.3 GB | – | 714 | 252 MB |
| **no change**: scan (warm cache), 4,501 files | 0.08 s | 0 | 0 | 0 | 234 MB |
| no change: snapshot | 0.14 s | 0 | 0 | 0 | |
| no change: cloud check | 0.04 s | 0 | 518 B | 4 | |
| no change: cold re-hash of 4.3 GB (page cache warm) | 4.15 s | 0 | 0 | 0 | 235 MB |
| small edit | 0.75 s | **229 KB** | 0 | 9 | |
| region edit (6 chunks × 3 files) | 0.82 s | **854 KB** | 0 | 18 | |
| 5 jars replaced | 0.98 s | 27.6 MB | 0 | 22 | |
| 5 jars renamed | 0.81 s | **225 KB** | 0 | 20 | |
| restore on a second computer | 73.7 s (≈ 58 MB/s) | – | 4.4 GB | 830 | 312 MB |
| catch up after one play session | 7.8 s | – | 562 KB | 11 | 313 MB |

Baseline (full archive after every change): **4.2 GB each time**; cloud space used by PackSync after all
scenarios: 4.3 GB for 4.3 GB of data. Memory stays near 235 MB for the whole 4.3 GB instance.

### What that means

| Change | Baseline uploads | PackSync uploads | Reduction |
|---|---:|---:|---:|
| nothing changed | 4.2 GB | 0 bytes of content (4 requests, 518 B) | – |
| a few config files | 4.2 GB | 229 KB | ≈ 18,000× |
| a game save (region edit) | 4.2 GB | 854 KB | ≈ 4,900× |
| five mod jars replaced | 4.2 GB | 27.6 MB (the new jars, incompressible) | ≈ 150× |
| five jars renamed | 4.2 GB | 225 KB | ≈ 18,000× |

(For the small fixture the same changes are 3,500×, 106×, 13× and 4,700×.) These ratios hold for these edit
models; real saves rewrite more chunks, and the region-edit result depends on the chunking (below). The
"catch up" row downloads 562 KB rather than two region files because unchanged blocks of the files being replaced
are taken from the disk (test `catching_up_after_an_in_place_region_edit_downloads_only_the_changed_blocks`).
Its 7.8 s is the one-off re-hash the first sync after any restore performs (4.15 s cold for 4.3 GB) plus the apply.

## Chunking decision — region files

Edit model: per save, 3 region files × 8 chunks rewritten in place (same sector count, new payload), twice.
**Fixed 16 KiB blocks are the default for `*.mca`**; everything else is whole-file or content-defined.

Small set — 206 MB of region data:

| chunking | blobs | initial metadata | uploaded for 311 KB of changes | write amplification | restore |
|---|---:|---:|---:|---:|---:|
| fixed 4 KiB | 50,344 | 2.1 MB | 669 KB | 2.1× | 5.9 s |
| fixed 8 KiB | 25,190 | 1.1 MB | 599 KB | 1.9× | 4.9 s |
| **fixed 16 KiB (default)** | 12,613 | 527 KB | 743 KB | 2.4× | 1.9 s |
| fixed 32 KiB | 6,326 | 264 KB | 1.1 MB | 3.6× | 1.5 s |
| fixed 64 KiB | 3,183 | 134 KB | 2.0 MB | 6.4× | 1.4 s |
| content-defined 4/16/64 KiB | 10,400 | 502 KB | 1.1 MB | 3.4× | 2.7 s |
| content-defined 8/32/128 KiB | 5,197 | 252 KB | 2.0 MB | 6.5× | 4.1 s |
| whole file | 64 | 8.8 KB | 38.8 MB | 125× | 1.5 s |

The 928 MB set gives the same upload numbers (the amplification is a property of the edit, not of the size)
and restore times of 42 s (4 KiB), 29 s (8 KiB), 14.5–15.5 s (16–64 KiB), 8–20 s (content-defined) and 9 s
(whole file) — single noisy runs; the small-block variants pay for their many temporary files.

**Why 16 KiB:** smaller blocks upload a little less per session (8 KiB: ~140 KB less) but double the number
of objects (index size, database rows, restore time); 32 KiB and above lose the delta advantage;
content-defined chunking costs more CPU and is no better for the edits Minecraft makes (a region file is
rewritten in place, sector-aligned, so fixed boundaries line up with the changes). Caveats: the edit model has
no data shifts (a chunk that outgrows its sectors moves the rest of the file, which fixed blocks handle worse than
content-defined chunking), and the fixture payloads are incompressible. A rule can choose per path
(`strategy = "cdc:…"`, `docs/rules.md`).

## What the benchmarks found (and the fixes)

1. **The uploader re-downloaded its own index** on every session (6.3 MB for a 4.3 GB instance) — it was never
   marked as already known. Fixed; test `the_index_a_computer_uploaded_itself_is_not_downloaded_again`.
2. **A restore read the packs over and over.** Files are hashed by several threads, and their blocks were
   interleaved in the packs, so one file's blocks were scattered and a restore working through the files in
   batches made 33,978 requests and read 6.65 GB to obtain 4.27 GB. Now each file's blocks are written
   together and a restore downloads files in the order they are stored: **830 requests, 4.4 GB**. Test
   `restoring_reads_each_pack_about_once_even_though_files_are_hashed_in_parallel` (98 MB in 2,010 requests →
   63 MB in 61 requests at test scale). On real Drive, where each request costs a round trip, this is the
   difference between minutes of request latency and seconds.
3. **A restore flushed every file to disk separately** (4,500 fsyncs on the large set; ~6 ms each on this
   btrfs). Files are now assembled on several threads and the file system is flushed once per batch (`syncfs`
   on Linux, per-file flush elsewhere), still **before** anything is renamed over a live file or the new
   instance appears. A 750 MB restore went from 10–14 s to 3.9 s; the large restore from 149 s to 74 s.
4. **Benchmark artefact:** without flushing the disk between scenarios, whichever small scenario happened to
   fsync first waited ~8 s behind the previous scenarios' writeback. The harness now flushes first.

## Known limits and open work

* **Restore throughput is ~60–190 MB/s locally** (58 MB/s on the 4.3 GB set, where page-cache and writeback
  pressure dominate; ~190 MB/s on the 750 MB set). A remaining cost is that every fetched block is written to a
  temporary file and read back; assembling files directly from the downloaded ranges would remove it. Over a
  real network the download is expected to dominate, but that has not been measured.
* **The first sync after a restore re-hashes the instance** (4 s per 4.3 GB when cached, longer from a cold
  disk): after a restore the cached file metadata is not trusted.
* **Drive request counts are real, latency is not:** the mock answers instantly. A no-change launch costs 4–5
  requests; the initial upload of 51 MB is 64 requests (resumable sessions included).
* **Cleanup frees space per pack object**; partly-used packs are reported, not repacked.
* **Not measured:** real Google Drive; Windows and macOS; a rotational disk; low-memory machines. The
  benchmark writes ~13 GB for the large run (source, cloud copy, restored copy): keep 20 GB free.

#!/usr/bin/env python3
"""Make the kind of change one short play session makes to a world, the same way every time.

Used by docs/benchmarks.md to measure a sync after "a small in-game change" without starting the game,
so that two builds can be compared on an identical workload. The world stays valid and playable: only
bytes that Minecraft ignores are changed, or changes are made through the file's own format.

What it changes (modelled on a real one-block session of GregTech: New Horizons, where the game rewrote
about 190 files):
  * 3 region files (the largest in <world>/region): a few chunks' unused sector padding and their
    timestamp entries, which is what an in-place chunk rewrite looks like to a sync tool;
  * level.dat is copied to level.dat_old, and the gzip timestamp of level.dat changes (the game rewrites
    both on every save);
  * every gzip-compressed *.dat file directly in <world>/data gets a new gzip timestamp (the game
    rewrites these "saved data" files on every save);
  * data/DistantHorizons.sqlite, if present: one real SQLite transaction that writes a few dozen pages.

Usage:
    python3 -I scripts/bench-world-edit.py --world <path to saves/WorldName> --seed N --i-know-this-edits-the-world

Never run it on a world you care about: use a copy (the benchmark uses a copied instance).
"""

import argparse
import gzip
import os
import random
import shutil
import sqlite3
import struct
import sys

SECTOR = 4096


def edit_region(path, rng, chunks=4):
    """Write random bytes into the padding after a few chunks' data, and bump their timestamps."""
    size = os.path.getsize(path)
    with open(path, "r+b") as f:
        header = f.read(2 * SECTOR)
        if len(header) < 2 * SECTOR:
            return 0
        candidates = []
        for i in range(1024):
            off, count = struct.unpack(">I", b"\0" + header[4 * i : 4 * i + 3])[0], header[4 * i + 3]
            if off < 2 or count == 0 or (off + count) * SECTOR > size:
                continue
            f.seek(off * SECTOR)
            length = struct.unpack(">I", f.read(4))[0]
            start = off * SECTOR + 4 + length
            end = (off + count) * SECTOR
            if end - start >= 64:
                candidates.append((i, start, end))
        rng.shuffle(candidates)
        done = 0
        for i, start, end in candidates[:chunks]:
            n = min(end - start, 512)
            f.seek(start)
            f.write(rng.randbytes(n))
            f.seek(SECTOR + 4 * i)  # the timestamp table
            f.write(struct.pack(">I", rng.randrange(1_700_000_000, 2_000_000_000)))
            done += 1
        return done


def touch_gzip(path, rng):
    """Change a gzip file's header timestamp (bytes 4..8); readers ignore it."""
    with open(path, "r+b") as f:
        magic = f.read(2)
        if magic != b"\x1f\x8b":
            return False
        f.seek(4)
        f.write(struct.pack("<I", rng.randrange(1_700_000_000, 2_000_000_000)))
    return True


def edit_sqlite(path, rng):
    con = sqlite3.connect(path)
    try:
        con.execute("CREATE TABLE IF NOT EXISTS packsync_bench (id INTEGER PRIMARY KEY, v BLOB)")
        con.execute("DELETE FROM packsync_bench")
        con.executemany("INSERT INTO packsync_bench (v) VALUES (?)", [(rng.randbytes(3000),) for _ in range(30)])
        con.commit()
    finally:
        con.close()


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--world", required=True)
    ap.add_argument("--seed", type=int, required=True)
    ap.add_argument("--i-know-this-edits-the-world", action="store_true")
    a = ap.parse_args()
    if not a.i_know_this_edits_the_world:
        sys.exit("refusing: pass --i-know-this-edits-the-world (and only ever point it at a copy)")
    w = a.world
    if not os.path.isfile(os.path.join(w, "level.dat")):
        sys.exit(f"{w} is not a world folder (no level.dat)")
    rng = random.Random(a.seed)

    regions = sorted(
        (os.path.join(w, "region", n) for n in os.listdir(os.path.join(w, "region")) if n.endswith(".mca")),
        key=os.path.getsize,
        reverse=True,
    )[:3]
    edited = {os.path.relpath(r, w): edit_region(r, rng) for r in regions}

    shutil.copyfile(os.path.join(w, "level.dat"), os.path.join(w, "level.dat_old"))
    touch_gzip(os.path.join(w, "level.dat"), rng)

    data = os.path.join(w, "data")
    touched = 0
    if os.path.isdir(data):
        for n in sorted(os.listdir(data)):
            p = os.path.join(data, n)
            if n.endswith(".dat") and os.path.isfile(p) and touch_gzip(p, rng):
                touched += 1
        dh = os.path.join(data, "DistantHorizons.sqlite")
        if os.path.isfile(dh):
            edit_sqlite(dh, rng)

    print(f"regions: {edited}; level.dat + level.dat_old; {touched} data files; "
          f"DistantHorizons.sqlite: {'yes' if os.path.isfile(os.path.join(data, 'DistantHorizons.sqlite')) else 'absent'}")


if __name__ == "__main__":
    main()

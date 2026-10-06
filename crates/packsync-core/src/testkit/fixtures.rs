//! Generated, deterministic Minecraft-instance fixtures: mods, configs, valid `level.dat`,
//! Anvil-style region files and realistic in-place edits. Also includes the junk a real game
//! directory accumulates (logs, caches, screenshots, native libraries) that rules must exclude.
//! Used by tests and by the benchmark tool (`packsync-bench`).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::fsutil::set_mtime_ms;

#[derive(Debug, Clone)]
pub struct FixtureSpec {
    pub seed: u64,
    pub mods: usize,
    /// Mod jar size range in bytes.
    pub mod_size: (usize, usize),
    pub configs: usize,
    pub worlds: usize,
    pub regions_per_world: usize,
    /// Populated chunk slots per region file (max 1024).
    pub chunks_per_region: usize,
    /// Sectors (4 KiB) per chunk payload.
    pub chunk_sectors: (u32, u32),
    pub junk: bool,
    /// `.minecraft` below the root (Prism layout) instead of a plain game folder.
    pub dot_minecraft: bool,
}

impl FixtureSpec {
    /// A handful of files: fast unit tests.
    pub fn tiny() -> Self {
        Self {
            seed: 1,
            mods: 3,
            mod_size: (20_000, 80_000),
            configs: 6,
            worlds: 1,
            regions_per_world: 2,
            chunks_per_region: 40,
            chunk_sectors: (1, 3),
            junk: true,
            dot_minecraft: false,
        }
    }
    /// ~40 MB: integration tests that want more than a toy.
    pub fn small() -> Self {
        Self {
            seed: 7,
            mods: 20,
            mod_size: (100_000, 1_500_000),
            configs: 60,
            worlds: 2,
            regions_per_world: 6,
            chunks_per_region: 300,
            chunk_sectors: (1, 4),
            junk: true,
            dot_minecraft: false,
        }
    }
    /// ~0.75 GB: a quicker benchmark and profiling size.
    pub fn medium() -> Self {
        Self {
            seed: 21,
            mods: 100,
            mod_size: (100_000, 6_000_000),
            configs: 800,
            worlds: 2,
            regions_per_world: 20,
            chunks_per_region: 800,
            chunk_sectors: (1, 6),
            junk: true,
            dot_minecraft: false,
        }
    }
    /// ~4 GB: benchmarks only.
    pub fn large() -> Self {
        Self {
            seed: 42,
            mods: 300,
            mod_size: (200_000, 12_000_000),
            configs: 4000,
            worlds: 3,
            regions_per_world: 60,
            chunks_per_region: 900,
            chunk_sectors: (1, 6),
            junk: true,
            dot_minecraft: false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct FixtureInfo {
    pub root: PathBuf,
    pub game_dir: PathBuf,
    /// Files that PackSync should sync / exclude under the default rules.
    pub synced_files: u64,
    pub synced_bytes: u64,
    pub junk_files: u64,
    pub regions: Vec<PathBuf>,
    pub mods: Vec<PathBuf>,
    pub configs: Vec<PathBuf>,
    pub worlds: Vec<String>,
}

fn rng(seed: u64) -> fastrand::Rng {
    fastrand::Rng::with_seed(seed)
}

fn write(path: &Path, data: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
}

fn random_bytes(r: &mut fastrand::Rng, n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    r.fill(&mut v);
    v
}

/// Text that compresses like real mod config files.
fn config_text(r: &mut fastrand::Rng, n: usize) -> Vec<u8> {
    let words = [
        "enabled",
        "true",
        "false",
        "maxStack",
        "recipe",
        "tier",
        "energy",
        "capacity",
        "multiplier",
        "oreGen",
        "veinSize",
        "rarity",
        "dimension",
        "biome",
    ];
    let mut s = String::new();
    while s.len() < n {
        s.push_str(words[r.usize(..words.len())]);
        s.push_str(if r.bool() { " = " } else { ": " });
        s.push_str(&r.u32(..100_000).to_string());
        s.push('\n');
    }
    s.truncate(n);
    s.into_bytes()
}

pub const SECTOR: usize = 4096;

/// An Anvil-like region file: 8 KiB header (locations + timestamps) followed by sector-aligned
/// chunk payloads (`u32 length`, `u8 compression`, random "compressed" bytes).
pub fn make_region(r: &mut fastrand::Rng, chunks: usize, sectors: (u32, u32)) -> Vec<u8> {
    let mut locations = vec![0u8; SECTOR];
    let mut timestamps = vec![0u8; SECTOR];
    let mut body: Vec<u8> = vec![];
    let mut next_sector = 2u32;
    let mut slots: Vec<usize> = (0..1024).collect();
    r.shuffle(&mut slots);
    for &slot in slots.iter().take(chunks.min(1024)) {
        let n = r.u32(sectors.0..=sectors.1.max(sectors.0));
        let len = n as usize * SECTOR;
        let mut payload = random_bytes(r, len);
        payload[..4].copy_from_slice(&((len - 4) as u32).to_be_bytes());
        payload[4] = 2;
        body.extend_from_slice(&payload);
        let entry = (next_sector << 8) | n;
        locations[slot * 4..slot * 4 + 4].copy_from_slice(&entry.to_be_bytes());
        timestamps[slot * 4..slot * 4 + 4].copy_from_slice(&(1_700_000_000u32 + r.u32(..1_000_000)).to_be_bytes());
        next_sector += n;
    }
    let mut out = locations;
    out.extend(timestamps);
    out.extend(body);
    out
}

/// Rewrite `n` populated chunk slots in place (same sector count, fresh payload, new timestamp):
/// what the game does when a few chunks change. Returns the number of bytes modified.
pub fn edit_region(path: &Path, n: usize, seed: u64) -> usize {
    let mut data = fs::read(path).unwrap();
    let mut r = rng(seed);
    let mut populated: Vec<usize> = (0..1024).filter(|s| data[s * 4..s * 4 + 4] != [0, 0, 0, 0]).collect();
    r.shuffle(&mut populated);
    let mut touched = 0;
    for &slot in populated.iter().take(n) {
        let e = u32::from_be_bytes(data[slot * 4..slot * 4 + 4].try_into().unwrap());
        let (off, cnt) = ((e >> 8) as usize * SECTOR, (e & 0xFF) as usize);
        let len = cnt * SECTOR;
        if off + len > data.len() {
            continue;
        }
        let mut payload = random_bytes(&mut r, len);
        payload[..4].copy_from_slice(&((len - 4) as u32).to_be_bytes());
        payload[4] = 2;
        data[off..off + len].copy_from_slice(&payload);
        data[SECTOR + slot * 4..SECTOR + slot * 4 + 4]
            .copy_from_slice(&(1_800_000_000u32 + r.u32(..1_000_000)).to_be_bytes());
        touched += len + 4;
    }
    fs::write(path, data).unwrap();
    touched
}

/// Overwrite a file with different bytes of the *same size* and restore its modification time:
/// the edit that fools size+mtime change detection.
pub fn same_size_edit_preserving_mtime(path: &Path, seed: u64) {
    let md = fs::metadata(path).unwrap();
    let mtime = md.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let mut data = fs::read(path).unwrap();
    let mut r = rng(seed);
    for _ in 0..8 {
        let i = r.usize(..data.len().max(1));
        if !data.is_empty() {
            data[i] ^= 0x5A;
        }
    }
    fs::write(path, data).unwrap();
    set_mtime_ms(path, mtime).unwrap();
}

/// Build a generated instance under `root`.
pub fn generate(root: &Path, spec: &FixtureSpec) -> FixtureInfo {
    let game = if spec.dot_minecraft { root.join(".minecraft") } else { root.to_path_buf() };
    fs::create_dir_all(&game).unwrap();
    let mut r = rng(spec.seed);
    let mut info = FixtureInfo { root: root.to_path_buf(), game_dir: game.clone(), ..Default::default() };
    let count = |info: &mut FixtureInfo, p: &Path| {
        info.synced_files += 1;
        info.synced_bytes += fs::metadata(p).unwrap().len();
    };

    for i in 0..spec.mods {
        let size = r.usize(spec.mod_size.0..=spec.mod_size.1.max(spec.mod_size.0));
        let p = game.join(format!("mods/mod-{i:03}-1.{}.{}.jar", i % 20, i % 7));
        write(&p, &random_bytes(&mut r, size));
        count(&mut info, &p);
        info.mods.push(p);
    }
    for i in 0..spec.configs {
        let ext = ["toml", "json", "cfg"][i % 3];
        let p = game.join(format!("config/mod{}/settings-{i}.{ext}", i % 25));
        let n = r.usize(300..20_000);
        write(&p, &config_text(&mut r, n));
        count(&mut info, &p);
        info.configs.push(p);
    }
    for (rel, data) in [
        (
            "options.txt",
            "version:3465\nlang:en_us\nrenderDistance:12\nfullscreen:false\nkey_key.jump:key.keyboard.space\n"
                .as_bytes()
                .to_vec(),
        ),
        ("servers.dat", random_bytes(&mut r, 600)),
        ("kubejs/server_scripts/recipes.js", config_text(&mut r, 2_000)),
        ("resourcepacks/pack.zip", random_bytes(&mut r, 30_000)),
        ("journeymap/data/sp/Fixture World~/waypoints/wp.json", config_text(&mut r, 500)),
    ] {
        let p = game.join(rel);
        write(&p, &data);
        count(&mut info, &p);
    }
    for w in 0..spec.worlds {
        let name = format!("World {w}");
        let wd = game.join("saves").join(&name);
        write(
            &wd.join("level.dat"),
            &crate::worlds::build_level_dat(&format!("My World {w} ünï"), 1_700_000_000_000 + w as i64),
        );
        count(&mut info, &wd.join("level.dat"));
        for (rel, n) in [
            ("playerdata/00000000-0000-0000-0000-000000000001.dat", 900),
            ("data/raids.dat", 120),
            ("datapacks/tweaks.zip", 4_000),
            ("stats/p1.json", 700),
        ] {
            let p = wd.join(rel);
            write(&p, &random_bytes(&mut r, n));
            count(&mut info, &p);
        }
        for i in 0..spec.regions_per_world {
            let p = wd.join(format!("region/r.{}.{}.mca", i % 8, i / 8));
            write(&p, &make_region(&mut r, spec.chunks_per_region, spec.chunk_sectors));
            count(&mut info, &p);
            info.regions.push(p);
        }
        // present but never synced
        write(&wd.join("session.lock"), b"\x00\x00\x00\x00\x01\x8b\x00\x00");
        info.worlds.push(name);
    }
    if spec.junk {
        for (rel, n) in [
            ("logs/latest.log", 20_000),
            ("crash-reports/crash-2026-01-01_00.00.00-client.txt", 5_000),
            ("screenshots/2026-01-01_00.00.00.png", 40_000),
            ("shaderpacks/BSL.zip", 60_000),
            ("backups/world-backup.zip", 50_000),
            (".fabric/remappedJars/x.jar", 10_000),
            ("natives/liblwjgl.so", 8_000),
            ("mods/native-helper.dll", 3_000),
        ] {
            write(&game.join(rel), &random_bytes(&mut r, n));
            info.junk_files += 1;
        }
    }
    // an empty directory in an included area, and a Unicode path
    fs::create_dir_all(game.join("config/empty-folder")).unwrap();
    let uni = game.join("config/ünï cödé 🎮/settings.json");
    write(&uni, b"{\"ok\":true}");
    count(&mut info, &uni);
    info
}

/// Read every synced file under `root` (the instance's included content) into a sorted listing
/// `(relative path, content hash)` — used to compare two devices byte for byte.
pub fn tree_digest(root: &Path, include: impl Fn(&str) -> bool) -> Vec<(String, crate::hash::Hash)> {
    let mut v = vec![];
    for e in walkdir::WalkDir::new(root).into_iter().flatten() {
        if e.file_type().is_file() {
            let rel = e.path().strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            if include(&rel) {
                v.push((rel, crate::hash::Hash::of(&fs::read(e.path()).unwrap())));
            }
        } else if e.file_type().is_dir() && e.path() != root && fs::read_dir(e.path()).unwrap().next().is_none() {
            let rel = e.path().strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            if include(&format!("{rel}/")) {
                v.push((format!("{rel}/"), crate::hash::Hash::ZERO));
            }
        }
    }
    v.sort();
    v
}

#[allow(dead_code)]
fn _w(w: &mut dyn Write) {
    let _ = w.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_regions_look_like_anvil_and_edits_are_local() {
        let d = tempfile::tempdir().unwrap();
        let info = generate(d.path(), &FixtureSpec::tiny());
        let p = &info.regions[0];
        let before = fs::read(p).unwrap();
        assert!(before.len() > 2 * SECTOR && before.len().is_multiple_of(SECTOR));
        let touched = edit_region(p, 5, 99);
        let after = fs::read(p).unwrap();
        assert_eq!(before.len(), after.len());
        let diff_sectors = (0..before.len() / SECTOR)
            .filter(|i| before[i * SECTOR..(i + 1) * SECTOR] != after[i * SECTOR..(i + 1) * SECTOR])
            .count();
        assert!(diff_sectors > 0 && touched > 0);
        assert!(
            diff_sectors < before.len() / SECTOR / 2,
            "a few chunk edits dirty a small part of the file ({diff_sectors} sectors)"
        );
    }

    #[test]
    fn same_size_edit_changes_content_but_not_size_or_mtime() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.json");
        fs::write(&p, vec![b'x'; 1000]).unwrap();
        let m0 =
            fs::metadata(&p).unwrap().modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
        same_size_edit_preserving_mtime(&p, 1);
        let m1 =
            fs::metadata(&p).unwrap().modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
        assert_eq!(fs::metadata(&p).unwrap().len(), 1000);
        assert_eq!(m0, m1);
        assert_ne!(fs::read(&p).unwrap(), vec![b'x'; 1000]);
    }
}

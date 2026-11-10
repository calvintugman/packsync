//! World discovery: reads `LevelName` / `LastPlayed` from `saves/*/level.dat` (gzip-compressed
//! NBT) with a tiny, bounds-checked parser. `level.dat` files come from disk *and* from restored
//! snapshots, so the parser treats them as untrusted: bounded decompression, bounded recursion,
//! no panics.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use flate2::read::GzDecoder;

use crate::launcher::WorldInfo;

const MAX_DECOMPRESSED: u64 = 64 * 1024 * 1024;
const MAX_DEPTH: u32 = 32;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct LevelData {
    pub name: Option<String>,
    pub last_played_ms: Option<i64>,
}

struct Cursor<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.i.checked_add(n)?;
        let s = self.b.get(self.i..end)?;
        self.i = end;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|s| s[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|s| u16::from_be_bytes([s[0], s[1]]))
    }
    fn i32(&mut self) -> Option<i32> {
        self.take(4).map(|s| i32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn i64(&mut self) -> Option<i64> {
        self.take(8).map(|s| i64::from_be_bytes(s.try_into().unwrap()))
    }
    fn string(&mut self) -> Option<String> {
        let n = self.u16()? as usize;
        self.take(n).map(decode_mutf8)
    }
}

/// Java "modified UTF-8": like UTF-8 but NUL is `C0 80` and supplementary characters are encoded as
/// two 3-byte surrogates.
fn decode_mutf8(b: &[u8]) -> String {
    let mut units: Vec<u16> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c < 0x80 {
            units.push(c as u16);
            i += 1;
        } else if c & 0xE0 == 0xC0 && i + 1 < b.len() {
            units.push((((c & 0x1F) as u16) << 6) | (b[i + 1] & 0x3F) as u16);
            i += 2;
        } else if c & 0xF0 == 0xE0 && i + 2 < b.len() {
            units.push((((c & 0x0F) as u16) << 12) | (((b[i + 1] & 0x3F) as u16) << 6) | (b[i + 2] & 0x3F) as u16);
            i += 3;
        } else {
            units.push(0xFFFD);
            i += 1;
        }
    }
    String::from_utf16_lossy(&units)
}

fn skip_payload(c: &mut Cursor, tag: u8, depth: u32) -> Option<()> {
    if depth > MAX_DEPTH {
        return None;
    }
    match tag {
        1 => c.take(1).map(|_| ()),
        2 => c.take(2).map(|_| ()),
        3 | 5 => c.take(4).map(|_| ()),
        4 | 6 => c.take(8).map(|_| ()),
        7 => {
            let n = c.i32()?;
            c.take(usize::try_from(n).ok()?).map(|_| ())
        }
        8 => {
            let n = c.u16()? as usize;
            c.take(n).map(|_| ())
        }
        9 => {
            let t = c.u8()?;
            let n = c.i32()?;
            for _ in 0..usize::try_from(n).ok()? {
                if t == 0 {
                    break;
                }
                skip_payload(c, t, depth + 1)?;
            }
            Some(())
        }
        10 => loop {
            let t = c.u8()?;
            if t == 0 {
                return Some(());
            }
            let _name = c.string()?;
            skip_payload(c, t, depth + 1)?;
        },
        11 => {
            let n = c.i32()?;
            c.take(usize::try_from(n).ok()?.checked_mul(4)?).map(|_| ())
        }
        12 => {
            let n = c.i32()?;
            c.take(usize::try_from(n).ok()?.checked_mul(8)?).map(|_| ())
        }
        _ => None,
    }
}

/// Parse the root compound's `Data` compound for the two fields we need.
pub fn parse_level_nbt(nbt: &[u8]) -> Option<LevelData> {
    let mut c = Cursor { b: nbt, i: 0 };
    if c.u8()? != 10 {
        return None;
    }
    let _root_name = c.string()?;
    let mut out = LevelData::default();
    loop {
        let t = c.u8()?;
        if t == 0 {
            return Some(out);
        }
        let name = c.string()?;
        if t == 10 && name == "Data" {
            loop {
                let t2 = c.u8()?;
                if t2 == 0 {
                    break;
                }
                let n2 = c.string()?;
                match (t2, n2.as_str()) {
                    (8, "LevelName") => out.name = Some(c.string()?),
                    (4, "LastPlayed") => out.last_played_ms = Some(c.i64()?),
                    _ => skip_payload(&mut c, t2, 1)?,
                }
                if out.name.is_some() && out.last_played_ms.is_some() {
                    return Some(out);
                }
            }
            return Some(out);
        }
        skip_payload(&mut c, t, 1)?;
    }
}

pub fn read_level_dat(path: &Path) -> Option<LevelData> {
    let f = File::open(crate::fsutil::long_path(path)).ok()?;
    let mut raw = Vec::new();
    GzDecoder::new(f).take(MAX_DECOMPRESSED).read_to_end(&mut raw).ok()?;
    parse_level_nbt(&raw)
}

/// Worlds under `<game_root>/saves`, most recently played first. A folder counts as a world when
/// it has a `level.dat` (even an unreadable one: the folder name is still reported).
pub fn list_worlds(game_root: &Path) -> Vec<WorldInfo> {
    let mut out = vec![];
    let Ok(rd) = std::fs::read_dir(crate::fsutil::long_path(&game_root.join("saves"))) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() || !p.join("level.dat").is_file() {
            continue;
        }
        let Some(folder) = e.file_name().to_str().map(|s| s.to_string()) else { continue };
        let lv = read_level_dat(&p.join("level.dat")).unwrap_or_default();
        out.push(WorldInfo { folder, name: lv.name, last_played_ms: lv.last_played_ms });
    }
    out.sort_by(|a, b| b.last_played_ms.cmp(&a.last_played_ms).then(a.folder.cmp(&b.folder)));
    out
}

#[cfg(any(test, feature = "testkit"))]
pub fn build_level_dat(name: &str, last_played: i64) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;
    // Java "modified UTF-8": supplementary characters become two 3-byte surrogates, NUL is C0 80.
    fn s(out: &mut Vec<u8>, v: &str) {
        let mut enc = Vec::new();
        for u in v.encode_utf16() {
            match u {
                0 => enc.extend([0xC0, 0x80]),
                1..=0x7F => enc.push(u as u8),
                0x80..=0x7FF => enc.extend([0xC0 | (u >> 6) as u8, 0x80 | (u & 0x3F) as u8]),
                _ => enc.extend([0xE0 | (u >> 12) as u8, 0x80 | ((u >> 6) & 0x3F) as u8, 0x80 | (u & 0x3F) as u8]),
            }
        }
        out.extend((enc.len() as u16).to_be_bytes());
        out.extend(enc);
    }
    let mut n = vec![10u8];
    s(&mut n, ""); // root name
    n.push(10);
    s(&mut n, "Data");
    // some unrelated tags first to exercise skipping
    n.push(3);
    s(&mut n, "version");
    n.extend(19133i32.to_be_bytes());
    n.push(9); // list of 2 ints
    s(&mut n, "SpawnList");
    n.push(3);
    n.extend(2i32.to_be_bytes());
    n.extend(1i32.to_be_bytes());
    n.extend(2i32.to_be_bytes());
    n.push(8);
    s(&mut n, "LevelName");
    s(&mut n, name);
    n.push(4);
    s(&mut n, "LastPlayed");
    n.extend(last_played.to_be_bytes());
    n.push(0); // end Data
    n.push(0); // end root
    let mut gz = GzEncoder::new(vec![], Compression::fast());
    gz.write_all(&n).unwrap();
    gz.finish().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_name_and_last_played() {
        let d = tempfile::tempdir().unwrap();
        let w = d.path().join("saves/folder-a");
        std::fs::create_dir_all(&w).unwrap();
        std::fs::write(w.join("level.dat"), build_level_dat("Мой мир 🌍", 1_700_000_000_123)).unwrap();
        let w2 = d.path().join("saves/older");
        std::fs::create_dir_all(&w2).unwrap();
        std::fs::write(w2.join("level.dat"), build_level_dat("Older", 1_600_000_000_000)).unwrap();
        let broken = d.path().join("saves/broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("level.dat"), b"not gzip").unwrap();
        std::fs::create_dir_all(d.path().join("saves/not-a-world")).unwrap();

        let worlds = list_worlds(d.path());
        assert_eq!(worlds.len(), 3);
        assert_eq!(worlds[0].folder, "folder-a");
        assert_eq!(worlds[0].name.as_deref(), Some("Мой мир 🌍"));
        assert_eq!(worlds[0].last_played_ms, Some(1_700_000_000_123));
        assert_eq!(worlds[1].folder, "older");
        assert_eq!(worlds[2].folder, "broken");
        assert_eq!(worlds[2].name, None);
    }

    #[test]
    fn parser_never_panics_on_garbage_or_truncation() {
        let good = {
            let gz = build_level_dat("x", 5);
            let mut raw = Vec::new();
            GzDecoder::new(&gz[..]).read_to_end(&mut raw).unwrap();
            raw
        };
        for cut in 0..good.len() {
            let _ = parse_level_nbt(&good[..cut]);
        }
        let mut rng = fastrand::Rng::with_seed(7);
        for _ in 0..2000 {
            let len = rng.usize(0..64);
            let junk: Vec<u8> = (0..len).map(|_| rng.u8(..)).collect();
            let _ = parse_level_nbt(&junk);
        }
        // deeply nested lists must hit the depth limit, not the stack
        let mut deep = vec![10u8, 0, 0, 10, 0, 4, b'D', b'a', b't', b'a'];
        for _ in 0..10_000 {
            deep.extend([9u8, 0, 1, b'x', 9, 0, 0, 0, 1]);
        }
        let _ = parse_level_nbt(&deep);
    }

    #[test]
    fn mutf8_surrogate_pairs_decode() {
        // U+1F30D encoded as the surrogate pair D83C DF0D, each as 3 bytes
        let b = [0xED, 0xA0, 0xBC, 0xED, 0xBC, 0x8D];
        assert_eq!(decode_mutf8(&b), "🌍");
        assert_eq!(decode_mutf8(&[0xC0, 0x80]), "\0");
    }
}

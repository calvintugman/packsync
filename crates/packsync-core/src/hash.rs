//! Content hashing. BLAKE3 everywhere; hashes are 32 bytes, rendered as 64 lowercase hex chars.

use std::fmt;
use std::io::{self, Read};
use std::str::FromStr;

use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

/// A BLAKE3-256 digest.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    pub const LEN: usize = 32;
    pub const ZERO: Hash = Hash([0u8; 32]);

    pub fn of(data: &[u8]) -> Hash {
        Hash(*blake3::hash(data).as_bytes())
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// First 12 hex characters, for display.
    pub fn short(&self) -> String {
        self.to_hex()[..12].to_string()
    }

    pub fn from_hex(s: &str) -> Result<Hash, HashParseError> {
        if s.len() != 64 {
            return Err(HashParseError);
        }
        // Accept lowercase only: a single canonical spelling keeps IDs comparable as strings.
        if !s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return Err(HashParseError);
        }
        let mut out = [0u8; 32];
        hex::decode_to_slice(s, &mut out).map_err(|_| HashParseError)?;
        Ok(Hash(out))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HashParseError;

impl fmt::Display for HashParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "expected 64 lowercase hex characters")
    }
}
impl std::error::Error for HashParseError {}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.short())
    }
}
impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}
impl FromStr for Hash {
    type Err = HashParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Hash::from_hex(s)
    }
}

impl Serialize for Hash {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}
impl<'de> Deserialize<'de> for Hash {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        Hash::from_hex(&s).map_err(de::Error::custom)
    }
}

/// Incremental hasher.
#[derive(Default)]
pub struct Hasher(blake3::Hasher);

impl Hasher {
    pub fn new() -> Self {
        Hasher(blake3::Hasher::new())
    }
    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }
    pub fn finalize(&self) -> Hash {
        Hash(*self.0.finalize().as_bytes())
    }
}

/// A reader adapter that hashes (BLAKE3) and counts everything read through it.
pub struct HashingReader<R> {
    inner: R,
    hasher: Hasher,
    count: u64,
}

impl<R: Read> HashingReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner, hasher: Hasher::new(), count: 0 }
    }
    pub fn finish(self) -> (Hash, u64) {
        (self.hasher.finalize(), self.count)
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.count += n as u64;
        Ok(n)
    }
}

/// Hash a file by streaming it (bounded memory).
pub fn hash_file(path: &std::path::Path) -> io::Result<(Hash, u64)> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Hasher::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut n_total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        n_total += n as u64;
    }
    Ok((h.finalize(), n_total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip_and_strictness() {
        let h = Hash::of(b"abc");
        assert_eq!(Hash::from_hex(&h.to_hex()).unwrap(), h);
        assert!(Hash::from_hex(&h.to_hex().to_uppercase()).is_err(), "uppercase must be rejected");
        assert!(Hash::from_hex("abc").is_err());
        assert!(Hash::from_hex(&"g".repeat(64)).is_err());
        let json = serde_json::to_string(&h).unwrap();
        assert_eq!(serde_json::from_str::<Hash>(&json).unwrap(), h);
    }

    #[test]
    fn hashing_reader_matches_oneshot() {
        let data = vec![7u8; 100_000];
        let mut r = HashingReader::new(&data[..]);
        let mut sink = Vec::new();
        io::copy(&mut r, &mut sink).unwrap();
        let (h, n) = r.finish();
        assert_eq!(n, data.len() as u64);
        assert_eq!(h, Hash::of(&data));
    }
}

//! End-to-end encryption as a [`Storage`] wrapper: everything is encrypted on this computer before it is
//! uploaded, with a key the service never sees.
//!
//! Format of a stored object: the plaintext is cut into segments of [`SEGMENT`] bytes (at least one, so
//! an empty object is one empty segment); each is stored as `nonce (24) ‖ ciphertext ‖ tag (16)`,
//! sealed with XChaCha20-Poly1305. The additional data binds the segment to its object key, its index
//! and whether it is the last one, so segments cannot be moved between objects, reordered, dropped
//! from the end or replaced with another object's. Because every segment stands alone, a range of
//! the plaintext is read by fetching only the segments that cover it.
//!
//! Nonces are derived from a keyed hash of the content (and key and index): the same plaintext under the
//! same key encrypts to the same bytes. That keeps create-only uploads idempotent (a retry uploads
//! identical bytes) and reveals only that an object was rewritten with identical content.
//!
//! What the service can still see: object keys (content hashes and instance IDs, no names), sizes
//! and times.

use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use packsync_core::hash::Hash;
use packsync_core::progress::OpCtx;
use packsync_core::storage::*;
use parking_lot::Mutex;

/// Plaintext bytes per segment.
pub const SEGMENT: u64 = 64 * 1024;
/// Bytes a segment adds: nonce and tag.
pub const OVERHEAD: u64 = 24 + 16;
const AAD_PREFIX: &[u8] = b"packsync object segment v1";

/// The secret every computer of an account shares. Derived keys are used, never this one directly.
#[derive(Clone)]
pub struct MasterKey([u8; 32]);

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey(<redacted>)")
    }
}

impl MasterKey {
    pub fn generate() -> MasterKey {
        let mut k = [0u8; 32];
        getrandom::getrandom(&mut k).expect("the operating system's random number generator");
        MasterKey(k)
    }
    pub fn from_bytes(b: [u8; 32]) -> MasterKey {
        MasterKey(b)
    }
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
    fn derive(&self, purpose: &str) -> [u8; 32] {
        blake3::derive_key(purpose, &self.0)
    }
}

/// Stored size of a plaintext of `n` bytes.
pub fn sealed_len(n: u64) -> u64 {
    let segments = n.div_ceil(SEGMENT).max(1);
    n + segments * OVERHEAD
}

/// Plaintext size of a stored object of `c` bytes, `None` if no plaintext seals to that size.
pub fn plain_len(c: u64) -> Option<u64> {
    let segments = c.div_ceil(SEGMENT + OVERHEAD).max(1);
    let n = c.checked_sub(segments * OVERHEAD)?;
    (sealed_len(n) == c).then_some(n)
}

struct Keys {
    aead: XChaCha20Poly1305,
    nonce: [u8; 32],
}

impl Keys {
    fn new(master: &MasterKey) -> Keys {
        Keys {
            aead: XChaCha20Poly1305::new(&master.derive("packsync 2026 object encryption").into()),
            nonce: master.derive("packsync 2026 object nonces"),
        }
    }

    fn aad(key: &str, index: u64, last: bool) -> Vec<u8> {
        let mut a = Vec::with_capacity(AAD_PREFIX.len() + key.len() + 13);
        a.extend_from_slice(AAD_PREFIX);
        a.extend_from_slice(&(key.len() as u32).to_le_bytes());
        a.extend_from_slice(key.as_bytes());
        a.extend_from_slice(&index.to_le_bytes());
        a.push(last as u8);
        a
    }

    fn seal(&self, key: &str, plain: &[u8]) -> Vec<u8> {
        let content = blake3::keyed_hash(&self.nonce, plain);
        let segments = (plain.len() as u64).div_ceil(SEGMENT).max(1);
        let mut out = Vec::with_capacity(sealed_len(plain.len() as u64) as usize);
        for i in 0..segments {
            let start = (i * SEGMENT) as usize;
            let end = (start + SEGMENT as usize).min(plain.len());
            let mut h = blake3::Hasher::new_keyed(&self.nonce);
            h.update(b"nonce");
            h.update(&(key.len() as u32).to_le_bytes());
            h.update(key.as_bytes());
            h.update(content.as_bytes());
            h.update(&i.to_le_bytes());
            let nonce: [u8; 24] = h.finalize().as_bytes()[..24].try_into().expect("24 bytes");
            let aad = Self::aad(key, i, i + 1 == segments);
            let sealed = self
                .aead
                .encrypt(XNonce::from_slice(&nonce), Payload { msg: &plain[start..end], aad: &aad })
                .expect("encryption of an in-memory buffer cannot fail");
            out.extend_from_slice(&nonce);
            out.extend_from_slice(&sealed);
        }
        out
    }

    /// Open consecutive segments starting at segment `first`. With `whole`, the data must end with the
    /// object's last segment; otherwise the last segment read may or may not be the object's last.
    fn open(&self, key: &str, sealed: &[u8], first: u64, whole: bool) -> StorageResult<Vec<u8>> {
        let damaged = || {
            StorageError::Corrupt(format!(
                "{key} could not be decrypted: it was damaged, or it was written with another account's key"
            ))
        };
        let step = (SEGMENT + OVERHEAD) as usize;
        let count = sealed.len().div_ceil(step).max(1);
        let mut out = Vec::with_capacity(sealed.len());
        for j in 0..count {
            let part = &sealed[(j * step).min(sealed.len())..((j + 1) * step).min(sealed.len())];
            if part.len() < OVERHEAD as usize {
                return Err(damaged());
            }
            let (nonce, body) = part.split_at(24);
            let index = first + j as u64;
            let final_here = j + 1 == count;
            // a full-size final segment of a range read may or may not be the object's last
            let candidates: &[bool] = match (final_here, whole, part.len() == step) {
                (false, _, _) => &[false],
                (true, true, _) | (true, false, false) => &[true],
                (true, false, true) => &[false, true],
            };
            let opened = candidates.iter().find_map(|last| {
                let aad = Self::aad(key, index, *last);
                self.aead.decrypt(XNonce::from_slice(nonce), Payload { msg: body, aad: &aad }).ok()
            });
            out.extend_from_slice(&opened.ok_or_else(damaged)?);
        }
        Ok(out)
    }
}

/// A [`Storage`] that encrypts everything written to `inner` and decrypts everything read.
pub struct EncryptedStorage {
    inner: Arc<dyn Storage>,
    keys: Keys,
    /// Stored sizes seen (listings, `stat`, uploads), so that a range read needs no extra request.
    sizes: Mutex<HashMap<String, u64>>,
}

impl EncryptedStorage {
    pub fn new(inner: Arc<dyn Storage>, master: &MasterKey) -> EncryptedStorage {
        EncryptedStorage { inner, keys: Keys::new(master), sizes: Mutex::new(HashMap::new()) }
    }

    fn remember(&self, key: &str, stored: Option<u64>) {
        let mut s = self.sizes.lock();
        if s.len() > 200_000 {
            s.clear();
        }
        match stored {
            Some(n) => s.insert(key.to_string(), n),
            None => s.remove(key),
        };
    }

    /// The caller's view of a stored object: plaintext size, no server checksums (they describe the
    /// encrypted bytes).
    fn plain_info(&self, mut info: ObjectInfo) -> ObjectInfo {
        if !info.is_dir {
            self.remember(&info.key, Some(info.size));
            info.size = plain_len(info.size).unwrap_or(0);
            info.checksums = Checksums::default();
        }
        info
    }

    fn stored_size(&self, key: &str, ctx: &OpCtx) -> StorageResult<u64> {
        if let Some(n) = self.sizes.lock().get(key).copied() {
            return Ok(n);
        }
        let info = self.inner.stat(key, ctx)?.ok_or_else(|| StorageError::NotFound(key.to_string()))?;
        self.remember(key, Some(info.size));
        Ok(info.size)
    }
}

impl Storage for EncryptedStorage {
    fn provider_id(&self) -> String {
        format!("{}+e2e", self.inner.provider_id())
    }

    fn capabilities(&self) -> Capabilities {
        let mut c = self.inner.capabilities();
        c.name = format!("{} (end-to-end encrypted)", c.name);
        c.server_checksums = vec![];
        c.max_object_size = c.max_object_size.map(|m| {
            let segments = m.div_ceil(SEGMENT + OVERHEAD);
            m.saturating_sub(segments * OVERHEAD)
        });
        c
    }

    fn list(&self, prefix: &str, ctx: &OpCtx) -> StorageResult<Vec<ObjectInfo>> {
        Ok(self.inner.list(prefix, ctx)?.into_iter().map(|i| self.plain_info(i)).collect())
    }

    fn stat(&self, key: &str, ctx: &OpCtx) -> StorageResult<Option<ObjectInfo>> {
        Ok(self.inner.stat(key, ctx)?.map(|i| self.plain_info(i)))
    }

    fn get(&self, key: &str, range: Option<(u64, u64)>, ctx: &OpCtx) -> StorageResult<Box<dyn Read + Send>> {
        match range {
            None => {
                let mut sealed = Vec::new();
                self.inner.get(key, None, ctx)?.read_to_end(&mut sealed).map_err(StorageError::io)?;
                self.remember(key, Some(sealed.len() as u64));
                Ok(Box::new(std::io::Cursor::new(self.keys.open(key, &sealed, 0, true)?)))
            }
            Some((_, 0)) => Ok(Box::new(std::io::empty())),
            Some((start, len)) => {
                let stored = self.stored_size(key, ctx)?;
                let plain =
                    plain_len(stored).ok_or_else(|| StorageError::Corrupt(format!("{key} has an impossible size")))?;
                if start + len > plain {
                    return Err(StorageError::Other(format!("range outside object {key}")));
                }
                let first = start / SEGMENT;
                let last = (start + len - 1) / SEGMENT;
                let step = SEGMENT + OVERHEAD;
                let from = first * step;
                let to = ((last + 1) * step).min(stored);
                let mut sealed = Vec::with_capacity((to - from) as usize);
                self.inner
                    .get(key, Some((from, to - from)), ctx)?
                    .read_to_end(&mut sealed)
                    .map_err(StorageError::io)?;
                if sealed.len() as u64 != to - from {
                    return Err(StorageError::Corrupt(format!("{key}: short read")));
                }
                let plain_part = self.keys.open(key, &sealed, first, to == stored)?;
                let skip = (start - first * SEGMENT) as usize;
                Ok(Box::new(std::io::Cursor::new(plain_part[skip..skip + len as usize].to_vec())))
            }
        }
    }

    fn get_with_info(&self, key: &str, max: u64, ctx: &OpCtx) -> StorageResult<Option<(Vec<u8>, ObjectInfo)>> {
        let Some((sealed, info)) = self.inner.get_with_info(key, sealed_len(max), ctx)? else { return Ok(None) };
        let plain = self.keys.open(key, &sealed, 0, true)?;
        if plain.len() as u64 > max {
            return Err(StorageError::Corrupt(format!("object {key} exceeds the {max} byte limit")));
        }
        Ok(Some((plain, self.plain_info(info))))
    }

    fn put(&self, key: &str, src: PutSource<'_>, opts: &PutOptions, ctx: &OpCtx) -> StorageResult<ObjectInfo> {
        let plain: Vec<u8> = match &src {
            PutSource::Bytes(b) => b.to_vec(),
            PutSource::File { path, len } => {
                let mut v = Vec::with_capacity(*len as usize);
                std::fs::File::open(path)?.take(len + 1).read_to_end(&mut v)?;
                if v.len() as u64 != *len {
                    return Err(StorageError::Corrupt(format!(
                        "source changed while uploading: expected {len} bytes, read {}",
                        v.len()
                    )));
                }
                v
            }
        };
        if let Some(d) = &opts.digest
            && (d.size != plain.len() as u64 || d.blake3 != Hash::of(&plain))
        {
            return Err(StorageError::Corrupt(format!("{key}: content does not match the declared digest")));
        }
        let sealed = self.keys.seal(key, &plain);
        let inner_opts = PutOptions {
            mode: opts.mode.clone(),
            digest: Some(ContentDigest { size: sealed.len() as u64, blake3: Hash::of(&sealed) }),
            tags: opts.tags.clone(),
        };
        self.remember(key, None);
        let info = self.inner.put(key, PutSource::Bytes(&sealed), &inner_opts, ctx)?;
        let mut info = self.plain_info(info);
        info.checksums.blake3 = opts.digest.map(|d| d.blake3.to_hex());
        Ok(info)
    }

    fn delete(&self, key: &str, mode: DeleteMode, ctx: &OpCtx) -> StorageResult<()> {
        self.remember(key, None);
        self.inner.delete(key, mode, ctx)
    }

    fn quota(&self, ctx: &OpCtx) -> StorageResult<Option<Quota>> {
        self.inner.quota(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_round_trip() {
        for n in [0, 1, 100, SEGMENT - 1, SEGMENT, SEGMENT + 1, 3 * SEGMENT, 3 * SEGMENT + 7, 16 << 20] {
            assert_eq!(plain_len(sealed_len(n)), Some(n), "{n}");
        }
        assert_eq!(plain_len(0), None);
        assert_eq!(plain_len(OVERHEAD - 1), None);
        assert_eq!(plain_len(SEGMENT + OVERHEAD + 3), None, "a segment shorter than its overhead");
    }

    #[test]
    fn sealing_is_deterministic_and_bound_to_key_index_and_end() {
        let k = Keys::new(&MasterKey::from_bytes([7; 32]));
        let data: Vec<u8> = (0..(2 * SEGMENT + 10)).map(|i| (i % 251) as u8).collect();
        let a = k.seal("packs/a", &data);
        assert_eq!(a, k.seal("packs/a", &data), "same content, same bytes");
        assert_eq!(k.open("packs/a", &a, 0, true).unwrap(), data);
        assert!(k.open("packs/b", &a, 0, true).is_err(), "another key");
        let step = (SEGMENT + OVERHEAD) as usize;
        // dropping the last segment is detected
        assert!(k.open("packs/a", &a[..2 * step], 0, true).is_err());
        // swapping two segments is detected
        let mut swapped = a.clone();
        let (x, y) = swapped.split_at_mut(step);
        x.swap_with_slice(&mut y[..step]);
        assert!(k.open("packs/a", &swapped, 0, true).is_err());
        // one flipped bit
        let mut bad = a.clone();
        bad[100] ^= 1;
        assert!(k.open("packs/a", &bad, 0, true).is_err());
        // another master key
        let other = Keys::new(&MasterKey::from_bytes([8; 32]));
        assert!(other.open("packs/a", &a, 0, true).is_err());
        // the middle segment alone
        assert_eq!(
            k.open("packs/a", &a[step..2 * step], 1, false).unwrap(),
            &data[SEGMENT as usize..2 * SEGMENT as usize]
        );
    }
}

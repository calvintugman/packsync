//! `MeteredStorage`: counts logical storage requests and bytes, per top-level key prefix, so
//! tests and benchmarks can assert *what* was transferred (payload packs vs metadata/lease traffic).

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::Serialize;

use crate::progress::OpCtx;
use crate::storage::*;

#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
pub struct PrefixStats {
    pub puts: u64,
    pub put_bytes: u64,
    pub gets: u64,
    pub get_bytes: u64,
    pub lists: u64,
    pub stats: u64,
    pub deletes: u64,
}

#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
pub struct MeterSnapshot {
    pub requests: u64,
    pub puts: u64,
    pub gets: u64,
    pub lists: u64,
    pub stats: u64,
    pub deletes: u64,
    pub bytes_up: u64,
    pub bytes_down: u64,
    /// Keyed by the first key component (`packs`, `index`, `instances`, ...).
    pub by_prefix: BTreeMap<String, PrefixStats>,
}

impl MeterSnapshot {
    /// Content payload uploads: new pack objects. This is what "no-change sync uploads nothing" is about.
    pub fn payload_puts(&self) -> u64 {
        self.by_prefix.get("packs").map(|p| p.puts).unwrap_or(0)
    }
    pub fn payload_bytes_up(&self) -> u64 {
        self.by_prefix.get("packs").map(|p| p.put_bytes).unwrap_or(0)
    }
    /// Everything that is not a pack: manifests, indexes, head/lease records.
    pub fn metadata_puts(&self) -> u64 {
        self.puts - self.payload_puts()
    }
    pub fn metadata_bytes_up(&self) -> u64 {
        self.bytes_up - self.payload_bytes_up()
    }
    /// Difference `self - earlier` (counters are monotonic).
    pub fn since(&self, earlier: &MeterSnapshot) -> MeterSnapshot {
        let mut by_prefix = BTreeMap::new();
        for (k, v) in &self.by_prefix {
            let e = earlier.by_prefix.get(k).cloned().unwrap_or_default();
            by_prefix.insert(
                k.clone(),
                PrefixStats {
                    puts: v.puts - e.puts,
                    put_bytes: v.put_bytes - e.put_bytes,
                    gets: v.gets - e.gets,
                    get_bytes: v.get_bytes - e.get_bytes,
                    lists: v.lists - e.lists,
                    stats: v.stats - e.stats,
                    deletes: v.deletes - e.deletes,
                },
            );
        }
        MeterSnapshot {
            requests: self.requests - earlier.requests,
            puts: self.puts - earlier.puts,
            gets: self.gets - earlier.gets,
            lists: self.lists - earlier.lists,
            stats: self.stats - earlier.stats,
            deletes: self.deletes - earlier.deletes,
            bytes_up: self.bytes_up - earlier.bytes_up,
            bytes_down: self.bytes_down - earlier.bytes_down,
            by_prefix,
        }
    }
}

#[derive(Default)]
pub struct Meter(Mutex<MeterSnapshot>);

impl Meter {
    pub fn new() -> Arc<Meter> {
        Arc::new(Meter::default())
    }
    pub fn snapshot(&self) -> MeterSnapshot {
        self.0.lock().clone()
    }
    fn bump(&self, key: &str, f: impl FnOnce(&mut MeterSnapshot, &mut PrefixStats)) {
        let prefix = key.split('/').next().unwrap_or("").to_string();
        let mut g = self.0.lock();
        g.requests += 1;
        let mut ps = g.by_prefix.remove(&prefix).unwrap_or_default();
        f(&mut g, &mut ps);
        g.by_prefix.insert(prefix, ps);
    }
}

pub struct MeteredStorage {
    inner: Arc<dyn Storage>,
    meter: Arc<Meter>,
}

impl MeteredStorage {
    pub fn new(inner: Arc<dyn Storage>) -> (Arc<MeteredStorage>, Arc<Meter>) {
        let meter = Meter::new();
        (Arc::new(MeteredStorage { inner, meter: meter.clone() }), meter)
    }
}

struct CountingReader {
    inner: Box<dyn Read + Send>,
    meter: Arc<Meter>,
    prefix_key: String,
}
impl Read for CountingReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            let meter = self.meter.clone();
            meter.bump_bytes_down(&self.prefix_key, n as u64);
        }
        Ok(n)
    }
}
impl Meter {
    fn bump_bytes_down(&self, key: &str, n: u64) {
        let prefix = key.split('/').next().unwrap_or("").to_string();
        let mut g = self.0.lock();
        g.bytes_down += n;
        g.by_prefix.entry(prefix).or_default().get_bytes += n;
    }
}

impl Storage for MeteredStorage {
    fn provider_id(&self) -> String {
        self.inner.provider_id()
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn list(&self, prefix: &str, ctx: &OpCtx) -> StorageResult<Vec<ObjectInfo>> {
        self.meter.bump(prefix, |g, p| {
            g.lists += 1;
            p.lists += 1;
        });
        self.inner.list(prefix, ctx)
    }
    fn stat(&self, key: &str, ctx: &OpCtx) -> StorageResult<Option<ObjectInfo>> {
        self.meter.bump(key, |g, p| {
            g.stats += 1;
            p.stats += 1;
        });
        self.inner.stat(key, ctx)
    }
    fn get(&self, key: &str, range: Option<(u64, u64)>, ctx: &OpCtx) -> StorageResult<Box<dyn Read + Send>> {
        self.meter.bump(key, |g, p| {
            g.gets += 1;
            p.gets += 1;
        });
        let r = self.inner.get(key, range, ctx)?;
        Ok(Box::new(CountingReader { inner: r, meter: self.meter.clone(), prefix_key: key.to_string() }))
    }
    fn put(&self, key: &str, src: PutSource<'_>, opts: &PutOptions, ctx: &OpCtx) -> StorageResult<ObjectInfo> {
        let n = src.len();
        let r = self.inner.put(key, src, opts, ctx);
        // Count attempts that reached the provider; bytes only when the put succeeded.
        self.meter.bump(key, |g, p| {
            g.puts += 1;
            p.puts += 1;
            if r.is_ok() {
                g.bytes_up += n;
                p.put_bytes += n;
            }
        });
        r
    }
    fn delete(&self, key: &str, mode: DeleteMode, ctx: &OpCtx) -> StorageResult<()> {
        self.meter.bump(key, |g, p| {
            g.deletes += 1;
            p.deletes += 1;
        });
        self.inner.delete(key, mode, ctx)
    }
    fn quota(&self, ctx: &OpCtx) -> StorageResult<Option<Quota>> {
        self.inner.quota(ctx)
    }
}

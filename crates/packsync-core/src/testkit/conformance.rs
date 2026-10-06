//! Storage conformance suite: the executable definition of the [`Storage`] contract.
//!
//! Every provider — the local fake, the fault-injecting wrapper (with no faults armed), the Google
//! Drive adapter against its mock server and (manually, with credentials) against real Drive —
//! must pass it. All keys live under a unique namespace so the suite can run in a shared test
//! folder.

use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::hash::Hash;
use crate::progress::OpCtx;
use crate::storage::*;

#[derive(Debug, Clone)]
pub struct ConformanceOptions {
    /// Number of small objects for the listing test (use > the provider's page size to cover pagination).
    pub list_count: usize,
    /// Size of the large-object test; `None` skips it.
    pub large_bytes: Option<u64>,
    /// Concurrent writers for the race tests.
    pub writers: usize,
    /// How long to keep polling `list` for eventually consistent providers.
    pub list_wait: Duration,
}

impl Default for ConformanceOptions {
    fn default() -> Self {
        Self { list_count: 60, large_bytes: Some(12 * 1024 * 1024), writers: 8, list_wait: Duration::ZERO }
    }
}

struct Env {
    ctx: OpCtx,
    ns: String,
    opts: ConformanceOptions,
    caps: Capabilities,
}

pub fn pattern(len: usize, seed: u64) -> Vec<u8> {
    let mut rng = fastrand::Rng::with_seed(seed);
    let mut v = vec![0u8; len];
    rng.fill(&mut v);
    v
}

fn digest(b: &[u8]) -> ContentDigest {
    ContentDigest { size: b.len() as u64, blake3: Hash::of(b) }
}

fn read_all(mut r: Box<dyn Read + Send>) -> Result<Vec<u8>, String> {
    let mut v = Vec::new();
    r.read_to_end(&mut v).map_err(|e| format!("read failed: {e}"))?;
    Ok(v)
}

fn list_until(
    st: &dyn Storage,
    env: &Env,
    prefix: &str,
    pred: impl Fn(&[ObjectInfo]) -> bool,
) -> Result<Vec<ObjectInfo>, String> {
    let start = Instant::now();
    loop {
        let items = st.list(prefix, &env.ctx).map_err(|e| format!("list {prefix}: {e}"))?;
        if pred(&items) || start.elapsed() >= env.opts.list_wait {
            return Ok(items);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

type Check = fn(&dyn Storage, &Env) -> Result<(), String>;

fn roundtrip(st: &dyn Storage, env: &Env) -> Result<(), String> {
    let data = pattern(100_003, 1);
    let key = format!("{}rt/bytes.bin", env.ns);
    let info = st
        .put(&key, PutSource::Bytes(&data), &PutOptions::create_only(Some(digest(&data))), &env.ctx)
        .map_err(|e| e.to_string())?;
    if info.size != data.len() as u64 {
        return Err(format!("put returned size {} != {}", info.size, data.len()));
    }
    let got = read_all(st.get(&key, None, &env.ctx).map_err(|e| e.to_string())?)?;
    if got != data {
        return Err("whole-object read differs".into());
    }
    let st_info = st.stat(&key, &env.ctx).map_err(|e| e.to_string())?.ok_or("stat after put is None")?;
    if st_info.size != data.len() as u64 {
        return Err("stat size mismatch".into());
    }
    // ranges
    let cases: [(u64, u64); 5] = [(0, 10), (50_000, 4096), (data.len() as u64 - 1, 1), (0, data.len() as u64), (7, 0)];
    for (start, len) in cases {
        let got =
            read_all(st.get(&key, Some((start, len)), &env.ctx).map_err(|e| format!("range {start}+{len}: {e}"))?)?;
        if got != data[start as usize..(start + len) as usize] {
            return Err(format!("range {start}+{len} returned wrong bytes"));
        }
    }
    if st.get(&key, Some((data.len() as u64 - 4, 100)), &env.ctx).is_ok() {
        return Err("range past the end must fail, not silently truncate".into());
    }
    // file source
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let fp = dir.path().join("src.bin");
    let fdata = pattern(777_777, 2);
    std::fs::write(&fp, &fdata).map_err(|e| e.to_string())?;
    let fkey = format!("{}rt/file.bin", env.ns);
    st.put(
        &fkey,
        PutSource::File { path: &fp, len: fdata.len() as u64 },
        &PutOptions::create_only(Some(digest(&fdata))),
        &env.ctx,
    )
    .map_err(|e| e.to_string())?;
    if read_all(st.get(&fkey, None, &env.ctx).map_err(|e| e.to_string())?)? != fdata {
        return Err("file-sourced object differs".into());
    }
    Ok(())
}

fn empty_object(st: &dyn Storage, env: &Env) -> Result<(), String> {
    let key = format!("{}empty/zero", env.ns);
    st.put(&key, PutSource::Bytes(&[]), &PutOptions::create_only(Some(digest(&[]))), &env.ctx)
        .map_err(|e| e.to_string())?;
    let i = st.stat(&key, &env.ctx).map_err(|e| e.to_string())?.ok_or("empty object missing")?;
    if i.size != 0 {
        return Err("empty object has size != 0".into());
    }
    if !read_all(st.get(&key, None, &env.ctx).map_err(|e| e.to_string())?)?.is_empty() {
        return Err("empty object read non-empty".into());
    }
    Ok(())
}

fn create_only_idempotent(st: &dyn Storage, env: &Env) -> Result<(), String> {
    let key = format!("{}co/obj.bin", env.ns);
    let a = pattern(5000, 3);
    let o = PutOptions::create_only(Some(digest(&a)));
    st.put(&key, PutSource::Bytes(&a), &o, &env.ctx).map_err(|e| format!("first put: {e}"))?;
    // A retry after an ambiguous failure must succeed and must not duplicate.
    st.put(&key, PutSource::Bytes(&a), &o, &env.ctx).map_err(|e| format!("idempotent re-put must succeed: {e}"))?;
    let items = list_until(st, env, &format!("{}co/", env.ns), |i| !i.is_empty())?;
    let n = items.iter().filter(|i| i.key == key).count();
    if n != 1 {
        return Err(format!("expected exactly one listing entry for the key, got {n}"));
    }
    if items[0].duplicates != 0 {
        return Err("sequential idempotent create produced duplicates".into());
    }
    // Different content under an existing CreateOnly key is a conflict and must not clobber.
    let b = pattern(5000, 4);
    match st.put(&key, PutSource::Bytes(&b), &PutOptions::create_only(Some(digest(&b))), &env.ctx) {
        Err(StorageError::Conflict(_)) => {}
        other => return Err(format!("different content on CreateOnly must be Conflict, got {other:?}")),
    }
    if read_all(st.get(&key, None, &env.ctx).map_err(|e| e.to_string())?)? != a {
        return Err("conflicting put clobbered the original".into());
    }
    Ok(())
}

fn replace_semantics(st: &dyn Storage, env: &Env) -> Result<(), String> {
    let key = format!("{}rep/head.json", env.ns);
    st.put(&key, PutSource::Bytes(b"v1"), &PutOptions::replace(), &env.ctx)
        .map_err(|e| format!("replace-create: {e}"))?;
    st.put(&key, PutSource::Bytes(b"version-two"), &PutOptions::replace(), &env.ctx)
        .map_err(|e| format!("replace-overwrite: {e}"))?;
    let got = read_all(st.get(&key, None, &env.ctx).map_err(|e| e.to_string())?)?;
    if got != b"version-two" {
        return Err(format!("replace did not take effect: {:?}", String::from_utf8_lossy(&got)));
    }
    let n = list_until(st, env, &format!("{}rep/", env.ns), |_| true)?.iter().filter(|i| i.key == key).count();
    if n != 1 {
        return Err(format!("replace must not create a second entry (got {n})"));
    }
    Ok(())
}

fn digest_mismatch_rejected(st: &dyn Storage, env: &Env) -> Result<(), String> {
    let key = format!("{}dg/bad.bin", env.ns);
    let data = pattern(2048, 5);
    let wrong = ContentDigest { size: data.len() as u64, blake3: Hash::of(b"something else") };
    match st.put(&key, PutSource::Bytes(&data), &PutOptions::create_only(Some(wrong)), &env.ctx) {
        Err(StorageError::Corrupt(_)) => {}
        other => return Err(format!("wrong digest must fail with Corrupt, got {other:?}")),
    }
    if st.stat(&key, &env.ctx).map_err(|e| e.to_string())?.is_some() {
        return Err("object with a failed digest check is visible".into());
    }
    let wrong_size = ContentDigest { size: 7, blake3: Hash::of(&data) };
    if st.put(&key, PutSource::Bytes(&data), &PutOptions::create_only(Some(wrong_size)), &env.ctx).is_ok() {
        return Err("wrong declared size must fail".into());
    }
    Ok(())
}

fn missing_semantics(st: &dyn Storage, env: &Env) -> Result<(), String> {
    let key = format!("{}missing/none.bin", env.ns);
    if st.stat(&key, &env.ctx).map_err(|e| e.to_string())?.is_some() {
        return Err("stat of a missing key must be None".into());
    }
    match st.get(&key, None, &env.ctx) {
        Err(StorageError::NotFound(_)) => {}
        Ok(_) => return Err("get of a missing key succeeded".into()),
        Err(e) => return Err(format!("get of a missing key must be NotFound, got {e}")),
    }
    st.delete(&key, DeleteMode::Permanent, &env.ctx).map_err(|e| format!("delete of a missing key must be Ok: {e}"))?;
    let items = st.list(&format!("{}missing/", env.ns), &env.ctx).map_err(|e| e.to_string())?;
    if !items.is_empty() {
        return Err("missing directory must list as empty".into());
    }
    Ok(())
}

fn delete_and_recreate(st: &dyn Storage, env: &Env) -> Result<(), String> {
    let key = format!("{}del/x.bin", env.ns);
    let a = pattern(300, 6);
    st.put(&key, PutSource::Bytes(&a), &PutOptions::create_only(Some(digest(&a))), &env.ctx)
        .map_err(|e| e.to_string())?;
    st.delete(&key, DeleteMode::Permanent, &env.ctx).map_err(|e| e.to_string())?;
    if st.stat(&key, &env.ctx).map_err(|e| e.to_string())?.is_some() {
        return Err("object still present after delete".into());
    }
    let b = pattern(301, 7);
    st.put(&key, PutSource::Bytes(&b), &PutOptions::create_only(Some(digest(&b))), &env.ctx)
        .map_err(|e| format!("re-create after delete must work: {e}"))?;
    if read_all(st.get(&key, None, &env.ctx).map_err(|e| e.to_string())?)? != b {
        return Err("re-created object has stale content".into());
    }
    Ok(())
}

fn listing_directories(st: &dyn Storage, env: &Env) -> Result<(), String> {
    for k in ["ls/a/x.bin", "ls/a/y.bin", "ls/b/z.bin", "ls/top.bin"] {
        let d = k.as_bytes();
        st.put(&format!("{}{k}", env.ns), PutSource::Bytes(d), &PutOptions::create_only(Some(digest(d))), &env.ctx)
            .map_err(|e| e.to_string())?;
    }
    let root = list_until(st, env, &format!("{}ls/", env.ns), |i| i.len() >= 3)?;
    let dirs: Vec<_> = root.iter().filter(|i| i.is_dir).map(|i| i.key.clone()).collect();
    let files: Vec<_> = root.iter().filter(|i| !i.is_dir).map(|i| i.key.clone()).collect();
    if dirs.len() != 2 || !dirs.iter().all(|d| d.ends_with("/")) {
        return Err(format!("expected 2 directory entries ending in '/', got {dirs:?}"));
    }
    if files != vec![format!("{}ls/top.bin", env.ns)] {
        return Err(format!("non-recursive listing returned {files:?}"));
    }
    let a = list_until(st, env, &format!("{}ls/a/", env.ns), |i| i.len() >= 2)?;
    if a.len() != 2 {
        return Err(format!("expected 2 entries under a/, got {}", a.len()));
    }
    Ok(())
}

fn listing_many(st: &dyn Storage, env: &Env) -> Result<(), String> {
    let n = env.opts.list_count;
    let pool = Arc::new(parking_lot::Mutex::new(0usize));
    std::thread::scope(|s| -> Result<(), String> {
        let mut hs = vec![];
        for t in 0..4 {
            let pool = pool.clone();
            hs.push(s.spawn(move || -> Result<(), String> {
                let mut i = t;
                while i < n {
                    let d = format!("object-{i}").into_bytes();
                    st.put(
                        &format!("{}many/o{i:05}", env.ns),
                        PutSource::Bytes(&d),
                        &PutOptions::create_only(Some(digest(&d))),
                        &env.ctx,
                    )
                    .map_err(|e| e.to_string())?;
                    *pool.lock() += 1;
                    i += 4;
                }
                Ok(())
            }));
        }
        for h in hs {
            h.join().map_err(|_| "writer thread panicked".to_string())??;
        }
        Ok(())
    })?;
    let items = list_until(st, env, &format!("{}many/", env.ns), |i| i.len() >= n)?;
    if items.len() != n {
        return Err(format!("listing returned {} of {n} objects (pagination?)", items.len()));
    }
    let mut keys: Vec<_> = items.iter().map(|i| i.key.clone()).collect();
    keys.dedup();
    if keys.len() != n {
        return Err("listing contains duplicate keys".into());
    }
    Ok(())
}

fn invalid_keys_rejected(st: &dyn Storage, env: &Env) -> Result<(), String> {
    for bad in ["../escape", "a/../../b", "/abs", "a b", "a//b", ""] {
        if st.put(bad, PutSource::Bytes(b"x"), &PutOptions::replace(), &env.ctx).is_ok() {
            return Err(format!("put accepted invalid key {bad:?}"));
        }
        if st.get(bad, None, &env.ctx).is_ok() {
            return Err(format!("get accepted invalid key {bad:?}"));
        }
    }
    if st.list("no-trailing-slash", &env.ctx).is_ok() {
        return Err("list accepted a prefix without trailing '/'".into());
    }
    Ok(())
}

fn cancellation(st: &dyn Storage, env: &Env) -> Result<(), String> {
    let ctx = OpCtx::detached();
    ctx.cancel.cancel();
    let key = format!("{}cancel/x.bin", env.ns);
    match st.put(&key, PutSource::Bytes(&pattern(1000, 8)), &PutOptions::replace(), &ctx) {
        Err(StorageError::Cancelled) => {}
        other => return Err(format!("cancelled put must return Cancelled, got {other:?}")),
    }
    if st.stat(&key, &env.ctx).map_err(|e| e.to_string())?.is_some() {
        return Err("cancelled put left an object behind".into());
    }
    Ok(())
}

fn concurrent_same_content(st: &dyn Storage, env: &Env) -> Result<(), String> {
    let key = format!("{}race/same.bin", env.ns);
    let data = pattern(200_000, 9);
    let d = digest(&data);
    std::thread::scope(|s| -> Result<(), String> {
        let hs: Vec<_> = (0..env.opts.writers)
            .map(|_| s.spawn(|| st.put(&key, PutSource::Bytes(&data), &PutOptions::create_only(Some(d)), &env.ctx)))
            .collect();
        for h in hs {
            h.join()
                .map_err(|_| "panic".to_string())?
                .map_err(|e| format!("concurrent identical create failed: {e}"))?;
        }
        Ok(())
    })?;
    let got = read_all(st.get(&key, None, &env.ctx).map_err(|e| e.to_string())?)?;
    if got != data {
        return Err("content corrupted by concurrent identical creates".into());
    }
    let items = list_until(st, env, &format!("{}race/", env.ns), |i| !i.is_empty())?;
    let entry = items.iter().find(|i| i.key == key).ok_or("key missing from listing")?;
    if env.caps.unique_keys && (items.len() != 1 || entry.duplicates != 0) {
        return Err(format!(
            "provider claims unique keys but listed {} entries / {} duplicates",
            items.len(),
            entry.duplicates
        ));
    }
    Ok(())
}

fn concurrent_distinct_keys(st: &dyn Storage, env: &Env) -> Result<(), String> {
    let n = env.opts.writers * 3;
    std::thread::scope(|s| -> Result<(), String> {
        let hs: Vec<_> = (0..n)
            .map(|i| {
                s.spawn(move || {
                    let d = pattern(10_000 + i, 100 + i as u64);
                    st.put(
                        &format!("{}race/d{i:03}", env.ns),
                        PutSource::Bytes(&d),
                        &PutOptions::create_only(Some(digest(&d))),
                        &env.ctx,
                    )
                })
            })
            .collect();
        for h in hs {
            h.join().map_err(|_| "panic".to_string())?.map_err(|e| e.to_string())?;
        }
        Ok(())
    })?;
    for i in 0..n {
        let d = pattern(10_000 + i, 100 + i as u64);
        let got = read_all(st.get(&format!("{}race/d{i:03}", env.ns), None, &env.ctx).map_err(|e| e.to_string())?)?;
        if got != d {
            return Err(format!("object d{i:03} differs after concurrent writes"));
        }
    }
    Ok(())
}

fn large_object(st: &dyn Storage, env: &Env) -> Result<(), String> {
    let Some(size) = env.opts.large_bytes else { return Ok(()) };
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let fp = dir.path().join("large.bin");
    let data = pattern(size as usize, 11);
    std::fs::write(&fp, &data).map_err(|e| e.to_string())?;
    let key = format!("{}large/big.bin", env.ns);
    st.put(&key, PutSource::File { path: &fp, len: size }, &PutOptions::create_only(Some(digest(&data))), &env.ctx)
        .map_err(|e| e.to_string())?;
    let i = st.stat(&key, &env.ctx).map_err(|e| e.to_string())?.ok_or("large object missing")?;
    if i.size != size {
        return Err(format!("large object size {} != {size}", i.size));
    }
    let back = read_all(st.get(&key, None, &env.ctx).map_err(|e| e.to_string())?)?;
    if Hash::of(&back) != Hash::of(&data) {
        return Err("large object content differs".into());
    }
    let mid = size / 2;
    let part = read_all(st.get(&key, Some((mid, 4096)), &env.ctx).map_err(|e| e.to_string())?)?;
    if part != data[mid as usize..mid as usize + 4096] {
        return Err("range read inside a large object returned wrong bytes".into());
    }
    Ok(())
}

fn quota_call(st: &dyn Storage, env: &Env) -> Result<(), String> {
    st.quota(&env.ctx).map(|_| ()).map_err(|e| format!("quota(): {e}"))
}

/// Run every check; returns `(name, result)` pairs. Never panics on a failing check.
pub fn run_all(st: &dyn Storage, opts: ConformanceOptions) -> Vec<(&'static str, Result<(), String>)> {
    let env = Env {
        ctx: OpCtx::detached(),
        ns: format!("conformance-{}/", uuid::Uuid::new_v4().simple()),
        caps: st.capabilities(),
        opts,
    };
    let checks: &[(&'static str, Check)] = &[
        ("roundtrip_bytes_ranges_and_files", roundtrip),
        ("empty_object", empty_object),
        ("create_only_is_idempotent_and_conflicts_on_different_content", create_only_idempotent),
        ("replace_overwrites_single_writer_keys", replace_semantics),
        ("digest_mismatch_is_rejected_and_invisible", digest_mismatch_rejected),
        ("missing_object_semantics", missing_semantics),
        ("delete_then_recreate", delete_and_recreate),
        ("listing_is_non_recursive_with_directories", listing_directories),
        ("listing_many_objects", listing_many),
        ("invalid_keys_are_rejected", invalid_keys_rejected),
        ("cancelled_put_leaves_nothing", cancellation),
        ("concurrent_identical_creates", concurrent_same_content),
        ("concurrent_distinct_creates", concurrent_distinct_keys),
        ("large_object_and_range_inside_it", large_object),
        ("quota_is_queryable", quota_call),
    ];
    checks.iter().map(|(n, f)| (*n, f(st, &env))).collect()
}

/// Panics with a readable report if any check fails.
pub fn assert_conformance(st: &dyn Storage, opts: ConformanceOptions) {
    let results = run_all(st, opts);
    let failed: Vec<_> = results.iter().filter(|(_, r)| r.is_err()).collect();
    if !failed.is_empty() {
        let mut msg = format!("{} of {} storage conformance checks failed:\n", failed.len(), results.len());
        for (n, r) in failed {
            msg.push_str(&format!("  ✗ {n}: {}\n", r.as_ref().unwrap_err()));
        }
        panic!("{msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage_fault::{FaultPlan, FaultyStorage};
    use crate::storage_local::LocalDirStorage;
    use crate::storage_meter::MeteredStorage;

    #[test]
    fn local_dir_storage_conforms() {
        let dir = tempfile::tempdir().unwrap();
        let st = LocalDirStorage::new(dir.path()).unwrap();
        assert_conformance(&st, ConformanceOptions::default());
    }

    #[test]
    fn metered_and_unarmed_fault_wrappers_conform() {
        let dir = tempfile::tempdir().unwrap();
        let local: Arc<dyn Storage> = Arc::new(LocalDirStorage::new(dir.path()).unwrap());
        let (metered, meter) = MeteredStorage::new(local);
        let faulty = FaultyStorage::new(metered, FaultPlan::new(1));
        assert_conformance(&*faulty, ConformanceOptions { large_bytes: Some(2 << 20), ..Default::default() });
        assert!(meter.snapshot().puts > 20);
    }
}

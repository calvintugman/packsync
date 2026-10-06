//! `FaultyStorage`: deterministic, scriptable fault injection around any [`Storage`].
//!
//! A [`FaultPlan`] holds rules (`op` × `key filter` × `when` → `fault`). Every decision is
//! reproducible: probabilistic rules use a seeded RNG, and nothing sleeps unless a latency fault
//! asks for it. The faults model what real networks and Drive do to a client:
//! timeouts, 429/5xx, auth expiry/revocation, quota exhaustion, **ambiguous failures** (the request
//! was applied but the response was lost), truncated or corrupted reads, and eventual-consistency
//! windows during which new objects are missing from `list`.

use std::collections::BTreeSet;
use std::io::{self, Read};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use crate::progress::OpCtx;
use crate::storage::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    Any,
    List,
    Stat,
    Get,
    Put,
    Delete,
    Quota,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum When {
    /// Exactly the n-th matching call (1-based).
    Nth(u32),
    /// Every n-th matching call.
    EveryNth(u32),
    /// All matching calls from the n-th on (e.g. "network goes down after the 3rd upload").
    FromNth(u32),
    Always,
    /// Seeded random with this probability.
    Probability(f64),
}

#[derive(Debug, Clone)]
pub enum Fault {
    /// Fail before the operation touches the backing store.
    Error(StorageError),
    /// Apply the operation, then report this error (response lost / timeout after commit).
    ErrorAfterApply(StorageError),
    /// `get` returns the stream with one byte flipped.
    CorruptRead,
    /// `get` stream ends after `keep` bytes without an error (silent truncation).
    TruncatedRead {
        keep: u64,
    },
    Latency(Duration),
    /// Newly written keys are invisible to `list` for the next `lists` list calls.
    HideFromList {
        lists: u32,
    },
    /// `std::process::abort()` — simulated hard termination (subprocess tests only).
    Abort,
}

#[derive(Debug, Clone)]
pub struct FaultRule {
    pub op: OpKind,
    pub key_contains: Option<String>,
    pub when: When,
    pub fault: Fault,
    /// Stop firing after this many triggers.
    pub max_triggers: Option<u32>,
}

impl FaultRule {
    pub fn new(op: OpKind, when: When, fault: Fault) -> Self {
        Self { op, key_contains: None, when, fault, max_triggers: None }
    }
    pub fn on_key(mut self, s: impl Into<String>) -> Self {
        self.key_contains = Some(s.into());
        self
    }
    pub fn max(mut self, n: u32) -> Self {
        self.max_triggers = Some(n);
        self
    }
}

struct RuleState {
    rule: FaultRule,
    matches: u32,
    triggers: u32,
}

pub struct FaultPlan {
    rules: Mutex<Vec<RuleState>>,
    rng: Mutex<fastrand::Rng>,
    hidden: Mutex<Vec<(String, u32)>>,
    log: Mutex<Vec<String>>,
    /// While `true` every operation fails as `Offline` (hard network outage switch).
    offline: Mutex<bool>,
}

impl FaultPlan {
    pub fn new(seed: u64) -> Arc<Self> {
        Arc::new(Self {
            rules: Mutex::new(vec![]),
            rng: Mutex::new(fastrand::Rng::with_seed(seed)),
            hidden: Mutex::new(vec![]),
            log: Mutex::new(vec![]),
            offline: Mutex::new(false),
        })
    }
    pub fn add(&self, rule: FaultRule) -> &Self {
        self.rules.lock().push(RuleState { rule, matches: 0, triggers: 0 });
        self
    }
    pub fn clear(&self) {
        self.rules.lock().clear();
    }
    pub fn set_offline(&self, v: bool) {
        *self.offline.lock() = v;
    }
    /// Human-readable record of every injected fault, in order.
    pub fn log(&self) -> Vec<String> {
        self.log.lock().clone()
    }
    pub fn triggered(&self) -> usize {
        self.log.lock().len()
    }

    fn pick(&self, op: OpKind, key: &str) -> Vec<Fault> {
        let mut out = vec![];
        let mut rules = self.rules.lock();
        for st in rules.iter_mut() {
            let r = &st.rule;
            if r.op != OpKind::Any && r.op != op {
                continue;
            }
            if let Some(s) = &r.key_contains
                && !key.contains(s.as_str())
            {
                continue;
            }
            st.matches += 1;
            if st.rule.max_triggers.is_some_and(|m| st.triggers >= m) {
                continue;
            }
            let fire = match st.rule.when {
                When::Nth(n) => st.matches == n,
                When::EveryNth(n) => n > 0 && st.matches % n == 0,
                When::FromNth(n) => st.matches >= n,
                When::Always => true,
                When::Probability(p) => self.rng.lock().f64() < p,
            };
            if fire {
                st.triggers += 1;
                self.log.lock().push(format!("{op:?} {key}: {:?}", st.rule.fault));
                out.push(st.rule.fault.clone());
            }
        }
        out
    }
}

pub struct FaultyStorage {
    inner: Arc<dyn Storage>,
    plan: Arc<FaultPlan>,
}

impl FaultyStorage {
    pub fn new(inner: Arc<dyn Storage>, plan: Arc<FaultPlan>) -> Arc<Self> {
        Arc::new(Self { inner, plan })
    }

    fn check_offline(&self) -> StorageResult<()> {
        if *self.plan.offline.lock() {
            return Err(StorageError::Offline("simulated network outage".into()));
        }
        Ok(())
    }

    /// Run faults that apply *before* the operation. Returns faults to apply after.
    fn pre(&self, op: OpKind, key: &str) -> StorageResult<Vec<Fault>> {
        self.check_offline()?;
        let faults = self.plan.pick(op, key);
        let mut after = vec![];
        for f in faults {
            match f {
                Fault::Error(e) => return Err(e),
                Fault::Latency(d) => std::thread::sleep(d),
                Fault::Abort => std::process::abort(),
                other => after.push(other),
            }
        }
        Ok(after)
    }
}

struct TruncatingReader {
    inner: Box<dyn Read + Send>,
    left: u64,
}
impl Read for TruncatingReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Ok(0);
        }
        let max = (buf.len() as u64).min(self.left) as usize;
        let n = self.inner.read(&mut buf[..max])?;
        self.left -= n as u64;
        Ok(n)
    }
}
struct CorruptingReader {
    inner: Box<dyn Read + Send>,
    done: bool,
}
impl Read for CorruptingReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 && !self.done {
            buf[0] ^= 0x01;
            self.done = true;
        }
        Ok(n)
    }
}

impl Storage for FaultyStorage {
    fn provider_id(&self) -> String {
        self.inner.provider_id()
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn list(&self, prefix: &str, ctx: &OpCtx) -> StorageResult<Vec<ObjectInfo>> {
        let after = self.pre(OpKind::List, prefix)?;
        let mut items = self.inner.list(prefix, ctx)?;
        // eventual-consistency window
        {
            let mut hidden = self.plan.hidden.lock();
            let hide: BTreeSet<String> = hidden.iter().map(|(k, _)| k.clone()).collect();
            items.retain(|i| !hide.contains(&i.key));
            for h in hidden.iter_mut() {
                h.1 = h.1.saturating_sub(1);
            }
            hidden.retain(|h| h.1 > 0);
        }
        for f in after {
            if let Fault::ErrorAfterApply(e) = f {
                return Err(e);
            }
        }
        Ok(items)
    }

    fn stat(&self, key: &str, ctx: &OpCtx) -> StorageResult<Option<ObjectInfo>> {
        let after = self.pre(OpKind::Stat, key)?;
        let r = self.inner.stat(key, ctx)?;
        for f in after {
            if let Fault::ErrorAfterApply(e) = f {
                return Err(e);
            }
        }
        Ok(r)
    }

    fn get(&self, key: &str, range: Option<(u64, u64)>, ctx: &OpCtx) -> StorageResult<Box<dyn Read + Send>> {
        let after = self.pre(OpKind::Get, key)?;
        let mut r = self.inner.get(key, range, ctx)?;
        for f in after {
            match f {
                Fault::ErrorAfterApply(e) => return Err(e),
                Fault::CorruptRead => r = Box::new(CorruptingReader { inner: r, done: false }),
                Fault::TruncatedRead { keep } => r = Box::new(TruncatingReader { inner: r, left: keep }),
                _ => {}
            }
        }
        Ok(r)
    }

    fn put(&self, key: &str, src: PutSource<'_>, opts: &PutOptions, ctx: &OpCtx) -> StorageResult<ObjectInfo> {
        let after = self.pre(OpKind::Put, key)?;
        let r = self.inner.put(key, src, opts, ctx)?;
        for f in after {
            match f {
                Fault::ErrorAfterApply(e) => return Err(e),
                Fault::HideFromList { lists } => self.plan.hidden.lock().push((key.to_string(), lists)),
                _ => {}
            }
        }
        Ok(r)
    }

    fn delete(&self, key: &str, mode: DeleteMode, ctx: &OpCtx) -> StorageResult<()> {
        let after = self.pre(OpKind::Delete, key)?;
        self.inner.delete(key, mode, ctx)?;
        for f in after {
            if let Fault::ErrorAfterApply(e) = f {
                return Err(e);
            }
        }
        Ok(())
    }

    fn quota(&self, ctx: &OpCtx) -> StorageResult<Option<Quota>> {
        self.pre(OpKind::Quota, "")?;
        self.inner.quota(ctx)
    }
}

//! Local persistent state (SQLite, WAL, versioned migrations).
//!
//! The database is **never authoritative for file content**: the file index is an optimisation that
//! the scan policy distrusts (see `docs/consistency.md`), remote-object caches are re-validated
//! against listings before they are relied on, and the cloud is the source of truth for history.
//! What the DB *does* own: which instances are registered, the local snapshot lifecycle
//! (`local_complete → uploading → published → visible`), play sessions, idempotent job requests and
//! resumable-transfer state.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::hash::Hash;
use crate::ids::{DeviceId, InstanceId, SessionId, SnapshotId};
use crate::manifest::{SessionOutcome, SnapshotKind};
use crate::pack::BlobEntry;
use crate::storage::TransferStateStore;

/// Each entry migrates from `user_version == index` to `index + 1`. Never edit a shipped entry.
const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

CREATE TABLE instances (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  root TEXT NOT NULL,
  launcher TEXT NOT NULL,
  launcher_ref TEXT,
  game_root TEXT NOT NULL,
  sync_enabled INTEGER NOT NULL DEFAULT 0,
  base_snapshot TEXT,
  pending_rules TEXT,
  marker TEXT NOT NULL,
  root_dev TEXT,
  provider_id TEXT,
  state TEXT NOT NULL DEFAULT 'ok',
  state_detail TEXT,
  created_ms INTEGER NOT NULL,
  last_scan_ms INTEGER,
  last_full_verify_ms INTEGER,
  last_synced_ms INTEGER,
  needs_full_scan INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE file_index (
  instance_id TEXT NOT NULL,
  path TEXT NOT NULL,
  size INTEGER NOT NULL,
  mtime_ns INTEGER NOT NULL,
  ctime_ns INTEGER,
  file_id TEXT,
  hash BLOB NOT NULL,
  recipe BLOB,
  mixed TEXT,
  content_size INTEGER NOT NULL,
  hashed_at_ns INTEGER NOT NULL,
  verified_at_ns INTEGER NOT NULL,
  PRIMARY KEY (instance_id, path)
) WITHOUT ROWID;

CREATE TABLE snapshots (
  id TEXT NOT NULL,
  instance_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  parents TEXT NOT NULL,
  parents_known INTEGER NOT NULL DEFAULT 1,
  device_id TEXT,
  device_name TEXT,
  created TEXT,
  origin TEXT NOT NULL,
  state TEXT NOT NULL,
  files INTEGER NOT NULL DEFAULT 0,
  bytes INTEGER NOT NULL DEFAULT 0,
  note TEXT,
  session_outcome TEXT,
  published_ms INTEGER,
  visible_ms INTEGER,
  has_manifest INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (instance_id, id)
);
CREATE INDEX snapshots_by_id ON snapshots(id);

CREATE TABLE remote_blobs (
  hash BLOB PRIMARY KEY,
  pack BLOB NOT NULL,
  offset INTEGER NOT NULL,
  stored_len INTEGER NOT NULL,
  raw_len INTEGER NOT NULL,
  codec INTEGER NOT NULL,
  kind INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX remote_blobs_by_pack ON remote_blobs(pack);

CREATE TABLE remote_packs (
  id BLOB PRIMARY KEY,
  size INTEGER NOT NULL,
  provider_ref TEXT,
  created_ms INTEGER,
  seen_ms INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE remote_indexes (
  id BLOB PRIMARY KEY,
  seen_ms INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE staged_packs (
  id BLOB PRIMARY KEY,
  snapshot_id TEXT NOT NULL,
  path TEXT NOT NULL,
  size INTEGER NOT NULL,
  uploaded INTEGER NOT NULL DEFAULT 0,
  attempts INTEGER NOT NULL DEFAULT 0,
  next_attempt_ms INTEGER,
  last_error TEXT,
  last_error_code TEXT,
  created_ms INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE sessions (
  id TEXT PRIMARY KEY,
  instance_id TEXT NOT NULL,
  owner TEXT NOT NULL,
  token TEXT NOT NULL,
  base_snapshot TEXT,
  started_ms INTEGER NOT NULL,
  ended_ms INTEGER,
  state TEXT NOT NULL,
  outcome TEXT,
  game_pid INTEGER,
  game_start_ms INTEGER,
  offline INTEGER NOT NULL DEFAULT 0,
  result_snapshot TEXT,
  note TEXT,
  dead_seen_ms INTEGER,
  owner_pid INTEGER,
  owner_start_ms INTEGER
);
CREATE INDEX sessions_by_instance ON sessions(instance_id, started_ms);
-- one open session per launch token: concurrent requests (hook + app) for the same launch resolve to one
CREATE UNIQUE INDEX sessions_open_token ON sessions(instance_id, token) WHERE state IN ('starting','running');

CREATE TABLE jobs (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  instance_id TEXT,
  request_key TEXT UNIQUE,
  state TEXT NOT NULL,
  created_ms INTEGER NOT NULL,
  updated_ms INTEGER NOT NULL,
  progress TEXT,
  error TEXT
);

CREATE TABLE events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  ts_ms INTEGER NOT NULL,
  instance_id TEXT,
  level TEXT NOT NULL,
  code TEXT NOT NULL,
  message TEXT NOT NULL
);

CREATE TABLE transfer_state (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL,
  updated_ms INTEGER NOT NULL
) WITHOUT ROWID;
"#];

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub struct Db {
    conn: Mutex<Connection>,
    path: Option<PathBuf>,
}

fn blob_hash(b: Vec<u8>) -> rusqlite::Result<Hash> {
    <[u8; 32]>::try_from(b.as_slice())
        .map(Hash)
        .map_err(|_| rusqlite::Error::InvalidColumnType(0, "hash".into(), rusqlite::types::Type::Blob))
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(format!("creating {}", parent.display()), e))?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn, Some(path.to_path_buf()))
    }

    pub fn open_memory() -> Result<Db> {
        Self::init(Connection::open_in_memory()?, None)
    }

    fn init(mut conn: Connection, path: Option<PathBuf>) -> Result<Db> {
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let version = conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))? as usize;
        if version > MIGRATIONS.len() {
            return Err(Error::Unsupported(format!(
                "the local database was created by a newer PackSync (schema {version}, this build supports {}); update the app",
                MIGRATIONS.len()
            )));
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version) {
            let tx = conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", (i + 1) as i64)?;
            tx.commit()?;
        }
        Ok(Db { conn: Mutex::new(conn), path })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn schema_version() -> usize {
        MIGRATIONS.len()
    }

    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let g = self.conn.lock();
        f(&g)
    }

    pub fn tx<T>(&self, f: impl FnOnce(&Transaction) -> Result<T>) -> Result<T> {
        let mut g = self.conn.lock();
        let tx = g.transaction()?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    // ------------------------------------------------------------------ meta
    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        self.with(|c| Ok(c.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0)).optional()?))
    }
    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [key, value],
            )?;
            Ok(())
        })
    }

    pub fn meta_delete(&self, key: &str) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM meta WHERE key=?1", [key])?;
            Ok(())
        })
    }

    // ------------------------------------------------------------------ lost cloud data
    /// Cloud packs this computer knew about have disappeared (a cleanup on another computer, files removed
    /// from Drive). Every instance re-reads its files at its next snapshot — so data that is gone is offered
    /// and uploaded again instead of being assumed present — and checks its next publication in full.
    pub fn mark_cloud_data_lost(&self) -> Result<()> {
        self.with(|c| {
            c.execute("UPDATE instances SET needs_full_scan=1", [])?;
            c.execute(
                "INSERT INTO meta(key,value) VALUES('cloud_data_lost_ms',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [now_ms().to_string()],
            )?;
            Ok(())
        })
    }

    /// When cloud data was last found missing, if ever.
    pub fn cloud_data_lost_ms(&self) -> Result<Option<i64>> {
        Ok(self.meta_get("cloud_data_lost_ms")?.and_then(|v| v.parse().ok()))
    }

    /// Whether `inst`'s next publication must check its whole tree against the cloud (data was lost after
    /// the last such check). Returns the loss time to record once the check passed.
    pub fn deep_check_due(&self, inst: InstanceId) -> Result<Option<i64>> {
        let Some(lost) = self.cloud_data_lost_ms()? else { return Ok(None) };
        let checked: i64 =
            self.meta_get(&format!("deep_checked_ms:{inst}"))?.and_then(|v| v.parse().ok()).unwrap_or(i64::MIN);
        Ok((checked < lost).then_some(lost))
    }

    pub fn set_deep_checked(&self, inst: InstanceId, lost_ms: i64) -> Result<()> {
        self.meta_set(&format!("deep_checked_ms:{inst}"), &lost_ms.to_string())
    }

    /// Why the last publication of `inst` was refused (shown in its status until a publication succeeds).
    pub fn publish_blocked(&self, inst: InstanceId) -> Result<Option<String>> {
        self.meta_get(&format!("publish_blocked:{inst}"))
    }
    pub fn set_publish_blocked(&self, inst: InstanceId, why: Option<&str>) -> Result<()> {
        let key = format!("publish_blocked:{inst}");
        match why {
            Some(w) => self.meta_set(&key, w),
            None => self.meta_delete(&key),
        }
    }

    /// This installation's identity, created on first use.
    pub fn device(&self) -> Result<(DeviceId, String)> {
        if let Some(id) = self.meta_get("device_id")? {
            let id = DeviceId::parse(&id).map_err(|e| Error::Db(format!("stored device id is invalid: {e}")))?;
            let name = self.meta_get("device_name")?.unwrap_or_else(default_device_name);
            return Ok((id, name));
        }
        let id = DeviceId::new();
        // A name chosen before the identity existed (EngineConfig.device_name) must survive.
        let name = self.meta_get("device_name")?.unwrap_or_else(default_device_name);
        self.meta_set("device_id", &id.to_string())?;
        self.meta_set("device_name", &name)?;
        Ok((id, name))
    }

    pub fn set_device_name(&self, name: &str) -> Result<()> {
        self.meta_set("device_name", name)
    }

    // ------------------------------------------------------------------ instances
    pub fn instance_insert(&self, r: &InstanceRow) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO instances(id,name,root,launcher,launcher_ref,game_root,sync_enabled,base_snapshot,pending_rules,marker,root_dev,provider_id,state,state_detail,created_ms,last_scan_ms,last_full_verify_ms,last_synced_ms,needs_full_scan)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
                params![
                    r.id.to_string(),
                    r.name,
                    r.root.to_string_lossy(),
                    r.launcher,
                    r.launcher_ref,
                    r.game_root,
                    r.sync_enabled,
                    r.base_snapshot.map(|s| s.to_hex()),
                    r.pending_rules,
                    r.marker,
                    r.root_dev,
                    r.provider_id,
                    r.state.as_str(),
                    r.state_detail,
                    r.created_ms,
                    r.last_scan_ms,
                    r.last_full_verify_ms,
                    r.last_synced_ms,
                    r.needs_full_scan,
                ],
            )?;
            Ok(())
        })
    }

    pub fn instance_get(&self, id: InstanceId) -> Result<Option<InstanceRow>> {
        self.with(|c| {
            Ok(c.query_row(
                &format!("SELECT {INSTANCE_COLS} FROM instances WHERE id=?1"),
                [id.to_string()],
                instance_from_row,
            )
            .optional()?)
        })
    }

    pub fn instances(&self) -> Result<Vec<InstanceRow>> {
        self.with(|c| {
            let mut st = c.prepare(&format!("SELECT {INSTANCE_COLS} FROM instances ORDER BY name COLLATE NOCASE"))?;
            let rows = st.query_map([], instance_from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    pub fn instance_by_root(&self, root: &Path) -> Result<Option<InstanceRow>> {
        self.with(|c| {
            Ok(c.query_row(
                &format!("SELECT {INSTANCE_COLS} FROM instances WHERE root=?1"),
                [root.to_string_lossy()],
                instance_from_row,
            )
            .optional()?)
        })
    }

    pub fn instance_update(&self, id: InstanceId, f: impl FnOnce(&mut InstanceRow)) -> Result<InstanceRow> {
        let mut row = self.instance_get(id)?.ok_or_else(|| Error::NotFound(format!("instance {id}")))?;
        f(&mut row);
        self.with(|c| {
            c.execute(
                "UPDATE instances SET name=?2, root=?3, launcher=?4, launcher_ref=?5, game_root=?6, sync_enabled=?7, base_snapshot=?8, pending_rules=?9, marker=?10, root_dev=?11, provider_id=?12, state=?13, state_detail=?14, last_scan_ms=?15, last_full_verify_ms=?16, last_synced_ms=?17, needs_full_scan=?18 WHERE id=?1",
                params![
                    row.id.to_string(),
                    row.name,
                    row.root.to_string_lossy(),
                    row.launcher,
                    row.launcher_ref,
                    row.game_root,
                    row.sync_enabled,
                    row.base_snapshot.map(|s| s.to_hex()),
                    row.pending_rules,
                    row.marker,
                    row.root_dev,
                    row.provider_id,
                    row.state.as_str(),
                    row.state_detail,
                    row.last_scan_ms,
                    row.last_full_verify_ms,
                    row.last_synced_ms,
                    row.needs_full_scan,
                ],
            )?;
            Ok(())
        })?;
        Ok(row)
    }

    /// Remove the registration and all local bookkeeping. Cloud data is untouched.
    pub fn instance_remove(&self, id: InstanceId) -> Result<()> {
        self.tx(|tx| {
            let k = id.to_string();
            tx.execute("DELETE FROM file_index WHERE instance_id=?1", [&k])?;
            tx.execute("DELETE FROM sessions WHERE instance_id=?1", [&k])?;
            tx.execute("DELETE FROM staged_packs WHERE snapshot_id IN (SELECT id FROM snapshots WHERE instance_id=?1 AND state IN ('local_complete','uploading'))", [&k])?;
            tx.execute("DELETE FROM snapshots WHERE instance_id=?1", [&k])?;
            tx.execute("DELETE FROM instances WHERE id=?1", [&k])?;
            Ok(())
        })
    }

    // ------------------------------------------------------------------ file index
    pub fn index_load(&self, id: InstanceId) -> Result<HashMap<String, IndexRow>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT path,size,mtime_ns,ctime_ns,file_id,hash,recipe,mixed,content_size,hashed_at_ns,verified_at_ns FROM file_index WHERE instance_id=?1",
            )?;
            let rows = st
                .query_map([id.to_string()], |r| {
                    Ok(IndexRow {
                        path: r.get(0)?,
                        size: r.get::<_, i64>(1)? as u64,
                        mtime_ns: r.get(2)?,
                        ctime_ns: r.get(3)?,
                        file_id: r.get(4)?,
                        hash: blob_hash(r.get(5)?)?,
                        recipe: r.get::<_, Option<Vec<u8>>>(6)?.map(blob_hash).transpose()?,
                        mixed: r.get(7)?,
                        content_size: r.get::<_, i64>(8)? as u64,
                        hashed_at_ns: r.get(9)?,
                        verified_at_ns: r.get(10)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows.into_iter().map(|r| (r.path.clone(), r)).collect())
        })
    }

    /// Replace the index of an instance atomically with exactly `rows`.
    pub fn index_replace(&self, id: InstanceId, rows: &[IndexRow]) -> Result<()> {
        self.tx(|tx| {
            let k = id.to_string();
            tx.execute("DELETE FROM file_index WHERE instance_id=?1", [&k])?;
            let mut st = tx.prepare(
                "INSERT INTO file_index(instance_id,path,size,mtime_ns,ctime_ns,file_id,hash,recipe,mixed,content_size,hashed_at_ns,verified_at_ns) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            )?;
            for r in rows {
                st.execute(params![
                    k,
                    r.path,
                    r.size as i64,
                    r.mtime_ns,
                    r.ctime_ns,
                    r.file_id,
                    r.hash.0.to_vec(),
                    r.recipe.map(|h| h.0.to_vec()),
                    r.mixed,
                    r.content_size as i64,
                    r.hashed_at_ns,
                    r.verified_at_ns
                ])?;
            }
            Ok(())
        })
    }

    pub fn index_clear(&self, id: InstanceId) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM file_index WHERE instance_id=?1", [id.to_string()])?;
            Ok(())
        })
    }

    // ------------------------------------------------------------------ snapshots
    /// Insert or update a snapshot row (keeps the more advanced local lifecycle state).
    pub fn snapshot_upsert(&self, r: &SnapshotRow) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO snapshots(id,instance_id,kind,parents,parents_known,device_id,device_name,created,origin,state,files,bytes,note,session_outcome,published_ms,visible_ms,has_manifest)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)
                 ON CONFLICT(instance_id,id) DO UPDATE SET
                   kind=excluded.kind, parents=excluded.parents, parents_known=MAX(parents_known, excluded.parents_known),
                   device_id=COALESCE(excluded.device_id, device_id), device_name=COALESCE(excluded.device_name, device_name),
                   created=COALESCE(excluded.created, created),
                   files=excluded.files, bytes=excluded.bytes, note=COALESCE(excluded.note, note),
                   session_outcome=COALESCE(excluded.session_outcome, session_outcome),
                   has_manifest=MAX(has_manifest, excluded.has_manifest)",
                params![
                    r.id.to_hex(),
                    r.instance_id.to_string(),
                    kind_str(r.kind),
                    serde_json::to_string(&r.parents.iter().map(|p| p.to_hex()).collect::<Vec<_>>())?,
                    r.parents_known,
                    r.device_id.map(|d| d.to_string()),
                    r.device_name,
                    r.created,
                    r.origin.as_str(),
                    r.state.as_str(),
                    r.files as i64,
                    r.bytes as i64,
                    r.note,
                    r.session_outcome.map(outcome_str),
                    r.published_ms,
                    r.visible_ms,
                    r.has_manifest,
                ],
            )?;
            Ok(())
        })
    }

    pub fn snapshot_get(&self, instance: InstanceId, id: SnapshotId) -> Result<Option<SnapshotRow>> {
        self.with(|c| {
            Ok(c.query_row(
                &format!("SELECT {SNAP_COLS} FROM snapshots WHERE instance_id=?1 AND id=?2"),
                params![instance.to_string(), id.to_hex()],
                snapshot_from_row,
            )
            .optional()?)
        })
    }

    pub fn snapshots(&self, instance: InstanceId) -> Result<Vec<SnapshotRow>> {
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {SNAP_COLS} FROM snapshots WHERE instance_id=?1 ORDER BY created DESC, id"
            ))?;
            let rows =
                st.query_map([instance.to_string()], snapshot_from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Forget a cached snapshot row (e.g. a damaged or vanished remote commit). Never touches the cloud.
    pub fn snapshot_remove(&self, instance: InstanceId, id: SnapshotId) -> Result<()> {
        self.with(|c| {
            c.execute(
                "DELETE FROM snapshots WHERE instance_id=?1 AND id=?2",
                params![instance.to_string(), id.to_hex()],
            )?;
            Ok(())
        })
    }

    /// Move a registration to a new instance identity (forking a local copy into a separate
    /// instance). The old identity's cached snapshot rows are dropped; cloud data is untouched.
    pub fn instance_rekey(
        &self,
        old: InstanceId,
        new: InstanceId,
        name: &str,
        base: SnapshotId,
        marker: &str,
    ) -> Result<()> {
        self.tx(|tx| {
            let (o, n) = (old.to_string(), new.to_string());
            tx.execute(
                "INSERT INTO instances(id,name,root,launcher,launcher_ref,game_root,sync_enabled,base_snapshot,pending_rules,marker,root_dev,provider_id,state,state_detail,created_ms,last_scan_ms,last_full_verify_ms,last_synced_ms,needs_full_scan)
                 SELECT ?2,?3,root,launcher,launcher_ref,game_root,sync_enabled,?4,NULL,?5,root_dev,provider_id,'ok',NULL,created_ms,last_scan_ms,last_full_verify_ms,NULL,1 FROM instances WHERE id=?1",
                params![o, n, name, base.to_hex(), marker],
            )?;
            tx.execute("DELETE FROM file_index WHERE instance_id=?1", [&o])?;
            tx.execute("DELETE FROM sessions WHERE instance_id=?1", [&o])?;
            tx.execute("DELETE FROM snapshots WHERE instance_id=?1", [&o])?;
            tx.execute("DELETE FROM instances WHERE id=?1", [&o])?;
            Ok(())
        })
    }

    pub fn snapshot_set_state(&self, instance: InstanceId, id: SnapshotId, state: SnapshotState) -> Result<()> {
        self.with(|c| {
            let now = now_ms();
            let n = c.execute(
                "UPDATE snapshots SET state=?3,
                    published_ms = CASE WHEN ?3 IN ('published','visible') AND published_ms IS NULL THEN ?4 ELSE published_ms END,
                    visible_ms = CASE WHEN ?3 = 'visible' AND visible_ms IS NULL THEN ?4 ELSE visible_ms END
                 WHERE instance_id=?1 AND id=?2",
                params![instance.to_string(), id.to_hex(), state.as_str(), now],
            )?;
            if n == 0 {
                return Err(Error::NotFound(format!("snapshot {}", id.short())));
            }
            Ok(())
        })
    }

    pub fn snapshots_in_state(&self, instance: InstanceId, states: &[SnapshotState]) -> Result<Vec<SnapshotRow>> {
        Ok(self.snapshots(instance)?.into_iter().filter(|s| states.contains(&s.state)).collect())
    }

    /// The single commit point of "local snapshot complete": snapshot row, staged-pack rows, the new
    /// file index and the instance's base pointer all change in ONE transaction (or none do).
    pub fn commit_local_snapshot(&self, c: &LocalSnapshotCommit<'_>) -> Result<()> {
        self.tx(|tx| {
            let r = c.row;
            tx.execute(
                "INSERT INTO snapshots(id,instance_id,kind,parents,parents_known,device_id,device_name,created,origin,state,files,bytes,note,session_outcome,published_ms,visible_ms,has_manifest)
                 VALUES (?1,?2,?3,?4,1,?5,?6,?7,'local','local_complete',?8,?9,?10,?11,NULL,NULL,1)
                 ON CONFLICT(instance_id,id) DO NOTHING",
                params![
                    r.id.to_hex(),
                    r.instance_id.to_string(),
                    kind_str(r.kind),
                    serde_json::to_string(&r.parents.iter().map(|p| p.to_hex()).collect::<Vec<_>>())?,
                    r.device_id.map(|d| d.to_string()),
                    r.device_name,
                    r.created,
                    r.files as i64,
                    r.bytes as i64,
                    r.note,
                    r.session_outcome.map(outcome_str),
                ],
            )?;
            for (id, path, size) in c.staged {
                tx.execute(
                    "INSERT INTO staged_packs(id,snapshot_id,path,size,created_ms) VALUES (?1,?2,?3,?4,?5)
                     ON CONFLICT(id) DO UPDATE SET snapshot_id=excluded.snapshot_id, path=excluded.path",
                    params![id.0.to_vec(), r.id.to_hex(), path.to_string_lossy(), *size as i64, now_ms()],
                )?;
            }
            if let Some((inst, rows)) = c.index {
                let k = inst.to_string();
                tx.execute("DELETE FROM file_index WHERE instance_id=?1", [&k])?;
                let mut st = tx.prepare(
                    "INSERT INTO file_index(instance_id,path,size,mtime_ns,ctime_ns,file_id,hash,recipe,mixed,content_size,hashed_at_ns,verified_at_ns) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                )?;
                for ix in rows {
                    st.execute(params![
                        k,
                        ix.path,
                        ix.size as i64,
                        ix.mtime_ns,
                        ix.ctime_ns,
                        ix.file_id,
                        ix.hash.0.to_vec(),
                        ix.recipe.map(|h| h.0.to_vec()),
                        ix.mixed,
                        ix.content_size as i64,
                        ix.hashed_at_ns,
                        ix.verified_at_ns
                    ])?;
                }
                let now = now_ms();
                tx.execute(
                    "UPDATE instances SET base_snapshot=?2, last_scan_ms=?3, needs_full_scan=0,
                        last_full_verify_ms = CASE WHEN ?4 THEN ?3 ELSE last_full_verify_ms END WHERE id=?1",
                    params![k, r.id.to_hex(), now, c.full_scan],
                )?;
            }
            Ok(())
        })
    }

    // ------------------------------------------------------------------ remote caches
    pub fn blobs_put(&self, pack: Hash, entries: &[BlobEntry]) -> Result<()> {
        self.tx(|tx| {
            let mut st = tx.prepare(
                "INSERT INTO remote_blobs(hash,pack,offset,stored_len,raw_len,codec,kind) VALUES (?1,?2,?3,?4,?5,?6,?7)
                 ON CONFLICT(hash) DO NOTHING",
            )?;
            for e in entries {
                st.execute(params![
                    e.hash.0.to_vec(),
                    pack.0.to_vec(),
                    e.offset as i64,
                    e.stored_len,
                    e.raw_len,
                    e.codec,
                    e.kind
                ])?;
            }
            Ok(())
        })
    }

    pub fn blob_lookup(&self, h: Hash) -> Result<Option<(Hash, BlobEntry)>> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT pack,offset,stored_len,raw_len,codec,kind FROM remote_blobs WHERE hash=?1",
                [h.0.to_vec()],
                |r| {
                    Ok((
                        blob_hash(r.get(0)?)?,
                        BlobEntry {
                            hash: h,
                            offset: r.get::<_, i64>(1)? as u64,
                            stored_len: r.get(2)?,
                            raw_len: r.get(3)?,
                            codec: r.get(4)?,
                            kind: r.get(5)?,
                        },
                    ))
                },
            )
            .optional()?)
        })
    }

    pub fn blob_known(&self, h: Hash) -> Result<bool> {
        self.with(|c| {
            Ok(c.query_row("SELECT 1 FROM remote_blobs WHERE hash=?1", [h.0.to_vec()], |_| Ok(()))
                .optional()?
                .is_some())
        })
    }

    pub fn blob_count(&self) -> Result<u64> {
        self.with(|c| Ok(c.query_row("SELECT COUNT(*) FROM remote_blobs", [], |r| r.get::<_, i64>(0))? as u64))
    }

    /// `(hash, stored length)` of every blob the cached indexes place in `pack`.
    pub fn pack_blobs(&self, pack: Hash) -> Result<Vec<(Hash, u32)>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT hash,stored_len FROM remote_blobs WHERE pack=?1")?;
            let rows = st
                .query_map([pack.0.to_vec()], |r| Ok((blob_hash(r.get(0)?)?, r.get::<_, i64>(1)? as u32)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Forget every cached blob location that points into `pack` (the pack vanished remotely).
    pub fn blobs_forget_pack(&self, pack: Hash) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM remote_blobs WHERE pack=?1", [pack.0.to_vec()])?;
            c.execute("DELETE FROM remote_packs WHERE id=?1", [pack.0.to_vec()])?;
            Ok(())
        })
    }

    /// Cached packs with the time a listing last showed them (or this computer uploaded them).
    pub fn packs_seen(&self) -> Result<Vec<(Hash, i64)>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT id,seen_ms FROM remote_packs")?;
            let rows = st
                .query_map([], |r| Ok((blob_hash(r.get(0)?)?, r.get::<_, i64>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    pub fn packs_known(&self) -> Result<HashMap<Hash, u64>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT id,size FROM remote_packs")?;
            let rows = st
                .query_map([], |r| Ok((blob_hash(r.get(0)?)?, r.get::<_, i64>(1)? as u64)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows.into_iter().collect())
        })
    }

    pub fn pack_put(&self, id: Hash, size: u64, provider_ref: Option<&str>, created_ms: Option<i64>) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO remote_packs(id,size,provider_ref,created_ms,seen_ms) VALUES (?1,?2,?3,?4,?5)
                 ON CONFLICT(id) DO UPDATE SET size=excluded.size, provider_ref=excluded.provider_ref, seen_ms=excluded.seen_ms",
                params![id.0.to_vec(), size as i64, provider_ref, created_ms, now_ms()],
            )?;
            Ok(())
        })
    }

    pub fn index_seen(&self, id: Hash) -> Result<bool> {
        self.with(|c| {
            Ok(c.query_row("SELECT 1 FROM remote_indexes WHERE id=?1", [id.0.to_vec()], |_| Ok(()))
                .optional()?
                .is_some())
        })
    }
    pub fn index_mark_seen(&self, id: Hash) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT OR IGNORE INTO remote_indexes(id,seen_ms) VALUES (?1,?2)",
                params![id.0.to_vec(), now_ms()],
            )?;
            Ok(())
        })
    }

    /// Drop all remote-object caches (they are re-derived from the cloud on the next sync).
    pub fn remote_cache_clear(&self) -> Result<()> {
        self.tx(|tx| {
            tx.execute("DELETE FROM remote_blobs", [])?;
            tx.execute("DELETE FROM remote_packs", [])?;
            tx.execute("DELETE FROM remote_indexes", [])?;
            Ok(())
        })
    }

    // ------------------------------------------------------------------ staged packs (upload queue)
    pub fn staged_put(&self, id: Hash, snapshot: SnapshotId, path: &Path, size: u64) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO staged_packs(id,snapshot_id,path,size,created_ms) VALUES (?1,?2,?3,?4,?5)
                 ON CONFLICT(id) DO UPDATE SET snapshot_id=excluded.snapshot_id, path=excluded.path",
                params![id.0.to_vec(), snapshot.to_hex(), path.to_string_lossy(), size as i64, now_ms()],
            )?;
            Ok(())
        })
    }
    pub fn staged_for(&self, snapshot: SnapshotId) -> Result<Vec<StagedPack>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT id,snapshot_id,path,size,uploaded,attempts,next_attempt_ms,last_error,last_error_code FROM staged_packs WHERE snapshot_id=?1 ORDER BY created_ms, id",
            )?;
            let rows = st
                .query_map([snapshot.to_hex()], |r| {
                    Ok(StagedPack {
                        id: blob_hash(r.get(0)?)?,
                        snapshot: r.get::<_, String>(1)?.parse().map_err(|_| rusqlite::Error::InvalidQuery)?,
                        path: PathBuf::from(r.get::<_, String>(2)?),
                        size: r.get::<_, i64>(3)? as u64,
                        uploaded: r.get(4)?,
                        attempts: r.get(5)?,
                        next_attempt_ms: r.get(6)?,
                        last_error: r.get(7)?,
                        last_error_code: r.get(8)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }
    pub fn staged_mark_uploaded(&self, id: Hash) -> Result<()> {
        self.with(|c| {
            c.execute("UPDATE staged_packs SET uploaded=1, last_error=NULL, last_error_code=NULL, next_attempt_ms=NULL WHERE id=?1", [id.0.to_vec()])?;
            Ok(())
        })
    }
    pub fn staged_record_failure(&self, id: Hash, code: &str, msg: &str, next_attempt_ms: i64) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE staged_packs SET attempts=attempts+1, last_error=?2, last_error_code=?3, next_attempt_ms=?4 WHERE id=?1",
                params![id.0.to_vec(), msg, code, next_attempt_ms],
            )?;
            Ok(())
        })
    }
    pub fn staged_remove_snapshot(&self, snapshot: SnapshotId) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM staged_packs WHERE snapshot_id=?1", [snapshot.to_hex()])?;
            Ok(())
        })
    }

    // ------------------------------------------------------------------ sessions
    pub fn session_insert(&self, s: &SessionRow) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO sessions(id,instance_id,owner,token,base_snapshot,started_ms,ended_ms,state,outcome,game_pid,game_start_ms,offline,result_snapshot,note,dead_seen_ms,owner_pid,owner_start_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
                params![
                    s.id.to_string(),
                    s.instance_id.to_string(),
                    s.owner.as_str(),
                    s.token,
                    s.base_snapshot.map(|b| b.to_hex()),
                    s.started_ms,
                    s.ended_ms,
                    s.state.as_str(),
                    s.outcome.map(outcome_str),
                    s.game_pid,
                    s.game_start_ms,
                    s.offline,
                    s.result_snapshot.map(|b| b.to_hex()),
                    s.note,
                    s.dead_seen_ms,
                    s.owner_pid,
                    s.owner_start_ms,
                ],
            )?;
            Ok(())
        })
    }

    /// Insert unless an open session with the same (instance, token) exists. `true` = inserted.
    pub fn session_insert_if_absent(&self, s: &SessionRow) -> Result<bool> {
        self.with(|c| {
            let n = c.execute(
                "INSERT INTO sessions(id,instance_id,owner,token,base_snapshot,started_ms,ended_ms,state,outcome,game_pid,game_start_ms,offline,result_snapshot,note,dead_seen_ms,owner_pid,owner_start_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)
                 ON CONFLICT DO NOTHING",
                params![
                    s.id.to_string(),
                    s.instance_id.to_string(),
                    s.owner.as_str(),
                    s.token,
                    s.base_snapshot.map(|b| b.to_hex()),
                    s.started_ms,
                    s.ended_ms,
                    s.state.as_str(),
                    s.outcome.map(outcome_str),
                    s.game_pid,
                    s.game_start_ms,
                    s.offline,
                    s.result_snapshot.map(|b| b.to_hex()),
                    s.note,
                    s.dead_seen_ms,
                    s.owner_pid,
                    s.owner_start_ms,
                ],
            )?;
            Ok(n == 1)
        })
    }

    pub fn session_update(&self, id: SessionId, f: impl FnOnce(&mut SessionRow)) -> Result<SessionRow> {
        let mut s = self.session_get(id)?.ok_or_else(|| Error::NotFound(format!("session {id}")))?;
        f(&mut s);
        self.with(|c| {
            c.execute(
                "UPDATE sessions SET ended_ms=?2,state=?3,outcome=?4,game_pid=?5,game_start_ms=?6,offline=?7,result_snapshot=?8,note=?9,base_snapshot=?10,owner=?11,dead_seen_ms=?12,owner_pid=?13,owner_start_ms=?14 WHERE id=?1",
                params![
                    s.id.to_string(),
                    s.ended_ms,
                    s.state.as_str(),
                    s.outcome.map(outcome_str),
                    s.game_pid,
                    s.game_start_ms,
                    s.offline,
                    s.result_snapshot.map(|b| b.to_hex()),
                    s.note,
                    s.base_snapshot.map(|b| b.to_hex()),
                    s.owner.as_str(),
                    s.dead_seen_ms,
                    s.owner_pid,
                    s.owner_start_ms,
                ],
            )?;
            Ok(())
        })?;
        Ok(s)
    }

    pub fn session_get(&self, id: SessionId) -> Result<Option<SessionRow>> {
        self.with(|c| {
            Ok(c.query_row(
                &format!("SELECT {SESSION_COLS} FROM sessions WHERE id=?1"),
                [id.to_string()],
                session_from_row,
            )
            .optional()?)
        })
    }

    pub fn sessions_for(&self, instance: InstanceId, limit: usize) -> Result<Vec<SessionRow>> {
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {SESSION_COLS} FROM sessions WHERE instance_id=?1 ORDER BY started_ms DESC LIMIT ?2"
            ))?;
            let rows = st
                .query_map(params![instance.to_string(), limit as i64], session_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Sessions that have not ended (`starting`, `running`).
    pub fn sessions_open(&self, instance: InstanceId) -> Result<Vec<SessionRow>> {
        self.with(|c| {
            let mut st = c.prepare(&format!("SELECT {SESSION_COLS} FROM sessions WHERE instance_id=?1 AND state IN ('starting','running') ORDER BY started_ms"))?;
            let rows = st.query_map([instance.to_string()], session_from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    // ------------------------------------------------------------------ jobs (idempotent requests)
    /// Register a job. If `request_key` already exists the existing job is returned (`created == false`).
    pub fn job_register(
        &self,
        id: crate::ids::JobId,
        kind: &str,
        instance: Option<InstanceId>,
        request_key: Option<&str>,
    ) -> Result<(JobRow, bool)> {
        self.with(|c| {
            let now = now_ms();
            let n = c.execute(
                "INSERT INTO jobs(id,kind,instance_id,request_key,state,created_ms,updated_ms) VALUES (?1,?2,?3,?4,'running',?5,?5)
                 ON CONFLICT(request_key) DO NOTHING",
                params![id.to_string(), kind, instance.map(|i| i.to_string()), request_key, now],
            )?;
            let row = if n == 1 {
                c.query_row(&format!("SELECT {JOB_COLS} FROM jobs WHERE id=?1"), [id.to_string()], job_from_row)?
            } else {
                c.query_row(&format!("SELECT {JOB_COLS} FROM jobs WHERE request_key=?1"), [request_key], job_from_row)?
            };
            Ok((row, n == 1))
        })
    }

    pub fn job_finish(&self, id: crate::ids::JobId, state: &str, error: Option<&str>) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE jobs SET state=?2, error=?3, updated_ms=?4 WHERE id=?1",
                params![id.to_string(), state, error, now_ms()],
            )?;
            Ok(())
        })
    }

    pub fn jobs_recent(&self, limit: usize) -> Result<Vec<JobRow>> {
        self.with(|c| {
            let mut st = c.prepare(&format!("SELECT {JOB_COLS} FROM jobs ORDER BY created_ms DESC LIMIT ?1"))?;
            let rows = st.query_map([limit as i64], job_from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Mark jobs left `running` by a previous process as `interrupted` (called at startup).
    pub fn jobs_mark_interrupted(&self) -> Result<usize> {
        self.with(|c| {
            Ok(c.execute("UPDATE jobs SET state='interrupted', updated_ms=?1 WHERE state='running'", [now_ms()])?)
        })
    }

    // ------------------------------------------------------------------ events
    pub fn event(&self, instance: Option<InstanceId>, level: &str, code: &str, message: &str) {
        let msg = crate::redact::redact(message).into_owned();
        let _ = self.with(|c| {
            c.execute(
                "INSERT INTO events(ts_ms,instance_id,level,code,message) VALUES (?1,?2,?3,?4,?5)",
                params![now_ms(), instance.map(|i| i.to_string()), level, code, msg],
            )?;
            c.execute("DELETE FROM events WHERE id <= (SELECT MAX(id) FROM events) - 5000", [])?;
            Ok(())
        });
    }

    pub fn events_recent(&self, instance: Option<InstanceId>, limit: usize) -> Result<Vec<EventRow>> {
        self.with(|c| {
            let map = |r: &rusqlite::Row| -> rusqlite::Result<EventRow> {
                Ok(EventRow { ts_ms: r.get(0)?, instance_id: r.get(1)?, level: r.get(2)?, code: r.get(3)?, message: r.get(4)? })
            };
            let rows = match instance {
                Some(i) => c
                    .prepare("SELECT ts_ms,instance_id,level,code,message FROM events WHERE instance_id=?1 ORDER BY id DESC LIMIT ?2")?
                    .query_map(params![i.to_string(), limit as i64], map)?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
                None => c
                    .prepare("SELECT ts_ms,instance_id,level,code,message FROM events ORDER BY id DESC LIMIT ?1")?
                    .query_map([limit as i64], map)?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            };
            Ok(rows)
        })
    }
}

/// The DB doubles as the persistent store for resumable transfers (Drive session URIs, offsets).
impl TransferStateStore for Db {
    fn load(&self, key: &str) -> Option<String> {
        self.with(|c| {
            Ok(c.query_row("SELECT value FROM transfer_state WHERE key=?1", [key], |r| r.get(0)).optional()?)
        })
        .ok()
        .flatten()
    }
    fn save(&self, key: &str, state: &str) {
        let _ = self.with(|c| {
            c.execute(
                "INSERT INTO transfer_state(key,value,updated_ms) VALUES (?1,?2,?3) ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_ms=excluded.updated_ms",
                params![key, state, now_ms()],
            )?;
            Ok(())
        });
    }
    fn clear(&self, key: &str) {
        let _ = self.with(|c| {
            c.execute("DELETE FROM transfer_state WHERE key=?1", [key])?;
            Ok(())
        });
    }
}

pub fn default_device_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("COMPUTERNAME").ok().filter(|s| !s.is_empty()))
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "this computer".into())
}

// ---------------------------------------------------------------------------- row types

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceState {
    Ok,
    /// A session ended abnormally and has not been resolved.
    CrashPending,
    /// Divergent histories await a decision.
    Conflict,
    /// A restore transaction is in progress or needs recovery.
    Restoring,
    /// The instance folder is missing / on an unmounted drive.
    Unavailable,
}

impl InstanceState {
    pub fn as_str(&self) -> &'static str {
        match self {
            InstanceState::Ok => "ok",
            InstanceState::CrashPending => "crash_pending",
            InstanceState::Conflict => "conflict",
            InstanceState::Restoring => "restoring",
            InstanceState::Unavailable => "unavailable",
        }
    }
    pub fn parse(s: &str) -> InstanceState {
        match s {
            "crash_pending" => InstanceState::CrashPending,
            "conflict" => InstanceState::Conflict,
            "restoring" => InstanceState::Restoring,
            "unavailable" => InstanceState::Unavailable,
            _ => InstanceState::Ok,
        }
    }
}

#[derive(Debug, Clone)]
pub struct InstanceRow {
    pub id: InstanceId,
    pub name: String,
    pub root: PathBuf,
    pub launcher: String,
    pub launcher_ref: Option<String>,
    pub game_root: String,
    pub sync_enabled: bool,
    pub base_snapshot: Option<SnapshotId>,
    pub pending_rules: Option<String>,
    pub marker: String,
    pub root_dev: Option<String>,
    pub provider_id: Option<String>,
    pub state: InstanceState,
    pub state_detail: Option<String>,
    pub created_ms: i64,
    pub last_scan_ms: Option<i64>,
    pub last_full_verify_ms: Option<i64>,
    pub last_synced_ms: Option<i64>,
    pub needs_full_scan: bool,
}

const INSTANCE_COLS: &str = "id,name,root,launcher,launcher_ref,game_root,sync_enabled,base_snapshot,pending_rules,marker,root_dev,provider_id,state,state_detail,created_ms,last_scan_ms,last_full_verify_ms,last_synced_ms,needs_full_scan";

fn instance_from_row(r: &rusqlite::Row) -> rusqlite::Result<InstanceRow> {
    let bad = |i: usize| rusqlite::Error::InvalidColumnType(i, "id".into(), rusqlite::types::Type::Text);
    Ok(InstanceRow {
        id: InstanceId::parse(&r.get::<_, String>(0)?).map_err(|_| bad(0))?,
        name: r.get(1)?,
        root: PathBuf::from(r.get::<_, String>(2)?),
        launcher: r.get(3)?,
        launcher_ref: r.get(4)?,
        game_root: r.get(5)?,
        sync_enabled: r.get(6)?,
        base_snapshot: r.get::<_, Option<String>>(7)?.map(|s| s.parse().map_err(|_| bad(7))).transpose()?,
        pending_rules: r.get(8)?,
        marker: r.get(9)?,
        root_dev: r.get(10)?,
        provider_id: r.get(11)?,
        state: InstanceState::parse(&r.get::<_, String>(12)?),
        state_detail: r.get(13)?,
        created_ms: r.get(14)?,
        last_scan_ms: r.get(15)?,
        last_full_verify_ms: r.get(16)?,
        last_synced_ms: r.get(17)?,
        needs_full_scan: r.get(18)?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRow {
    pub path: String,
    /// On-disk size.
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: Option<i64>,
    pub file_id: Option<String>,
    pub hash: Hash,
    pub recipe: Option<Hash>,
    pub mixed: Option<String>,
    /// Size of the stored content (differs from `size` for mixed files).
    pub content_size: u64,
    /// Wall-clock time (ns) when the file started being read for hashing (racy-timestamp guard).
    pub hashed_at_ns: i64,
    /// Wall-clock time (ns) of the last time this entry was verified by a full content hash.
    pub verified_at_ns: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotState {
    /// Manifest and staged packs exist locally; nothing (or not everything) is in the cloud yet.
    LocalComplete,
    /// Upload in progress or being retried.
    Uploading,
    /// Every object and the manifest are stored and verified in the cloud.
    Published,
    /// A fresh discovery listing shows the snapshot: other devices can see it.
    Visible,
    /// Discovered in the cloud (not produced on this device).
    Remote,
}

impl SnapshotState {
    pub fn as_str(&self) -> &'static str {
        match self {
            SnapshotState::LocalComplete => "local_complete",
            SnapshotState::Uploading => "uploading",
            SnapshotState::Published => "published",
            SnapshotState::Visible => "visible",
            SnapshotState::Remote => "remote",
        }
    }
    pub fn parse(s: &str) -> SnapshotState {
        match s {
            "local_complete" => SnapshotState::LocalComplete,
            "uploading" => SnapshotState::Uploading,
            "published" => SnapshotState::Published,
            "visible" => SnapshotState::Visible,
            _ => SnapshotState::Remote,
        }
    }
    pub fn is_in_cloud(&self) -> bool {
        matches!(self, SnapshotState::Published | SnapshotState::Visible | SnapshotState::Remote)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Local,
    Remote,
}
impl Origin {
    pub fn as_str(&self) -> &'static str {
        match self {
            Origin::Local => "local",
            Origin::Remote => "remote",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotRow {
    pub id: SnapshotId,
    pub instance_id: InstanceId,
    pub kind: SnapshotKind,
    pub parents: Vec<SnapshotId>,
    /// `false` for remote commits whose manifest has not been read yet (parents unknown).
    pub parents_known: bool,
    pub device_id: Option<DeviceId>,
    pub device_name: Option<String>,
    pub created: Option<String>,
    pub origin: Origin,
    pub state: SnapshotState,
    pub files: u64,
    pub bytes: u64,
    pub note: Option<String>,
    pub session_outcome: Option<SessionOutcome>,
    pub published_ms: Option<i64>,
    pub visible_ms: Option<i64>,
    pub has_manifest: bool,
}

const SNAP_COLS: &str = "id,instance_id,kind,parents,parents_known,device_id,device_name,created,origin,state,files,bytes,note,session_outcome,published_ms,visible_ms,has_manifest";

fn kind_str(k: SnapshotKind) -> &'static str {
    match k {
        SnapshotKind::Normal => "normal",
        SnapshotKind::Crash => "crash",
    }
}
fn outcome_str(o: SessionOutcome) -> &'static str {
    match o {
        SessionOutcome::Clean => "clean",
        SessionOutcome::Crashed => "crashed",
        SessionOutcome::Interrupted => "interrupted",
    }
}
fn outcome_parse(s: &str) -> Option<SessionOutcome> {
    match s {
        "clean" => Some(SessionOutcome::Clean),
        "crashed" => Some(SessionOutcome::Crashed),
        "interrupted" => Some(SessionOutcome::Interrupted),
        _ => None,
    }
}

fn snapshot_from_row(r: &rusqlite::Row) -> rusqlite::Result<SnapshotRow> {
    let bad = |i: usize| rusqlite::Error::InvalidColumnType(i, "snapshot".into(), rusqlite::types::Type::Text);
    let parents: Vec<String> = serde_json::from_str(&r.get::<_, String>(3)?).map_err(|_| bad(3))?;
    Ok(SnapshotRow {
        id: r.get::<_, String>(0)?.parse().map_err(|_| bad(0))?,
        instance_id: InstanceId::parse(&r.get::<_, String>(1)?).map_err(|_| bad(1))?,
        kind: if r.get::<_, String>(2)? == "crash" { SnapshotKind::Crash } else { SnapshotKind::Normal },
        parents: parents.iter().map(|p| p.parse().map_err(|_| bad(3))).collect::<rusqlite::Result<_>>()?,
        parents_known: r.get(4)?,
        device_id: r.get::<_, Option<String>>(5)?.and_then(|s| DeviceId::parse(&s).ok()),
        device_name: r.get(6)?,
        created: r.get(7)?,
        origin: if r.get::<_, String>(8)? == "local" { Origin::Local } else { Origin::Remote },
        state: SnapshotState::parse(&r.get::<_, String>(9)?),
        files: r.get::<_, i64>(10)? as u64,
        bytes: r.get::<_, i64>(11)? as u64,
        note: r.get(12)?,
        session_outcome: r.get::<_, Option<String>>(13)?.and_then(|s| outcome_parse(&s)),
        published_ms: r.get(14)?,
        visible_ms: r.get(15)?,
        has_manifest: r.get(16)?,
    })
}

/// Arguments of [`Db::commit_local_snapshot`].
pub struct LocalSnapshotCommit<'a> {
    pub row: &'a SnapshotRow,
    /// `(pack id, path, size)` of each staged pack.
    pub staged: &'a [(Hash, PathBuf, u64)],
    /// When `Some`, replace this instance's file index and advance its base to the new snapshot.
    pub index: Option<(InstanceId, &'a [IndexRow])>,
    pub full_scan: bool,
}

#[derive(Debug, Clone)]
pub struct StagedPack {
    pub id: Hash,
    pub snapshot: SnapshotId,
    pub path: PathBuf,
    pub size: u64,
    pub uploaded: bool,
    pub attempts: u32,
    pub next_attempt_ms: Option<i64>,
    pub last_error: Option<String>,
    pub last_error_code: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionOwner {
    /// Started by the app / `packsync launch`.
    App,
    /// Started by a launcher hook.
    Hook,
    /// Detected game activity nobody announced.
    External,
}
impl SessionOwner {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionOwner::App => "app",
            SessionOwner::Hook => "hook",
            SessionOwner::External => "external",
        }
    }
    pub fn parse(s: &str) -> SessionOwner {
        match s {
            "app" => SessionOwner::App,
            "hook" => SessionOwner::Hook,
            _ => SessionOwner::External,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Starting,
    Running,
    Ended,
    /// Left open by a process that died; resolved by recovery.
    Interrupted,
}
impl SessionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionState::Starting => "starting",
            SessionState::Running => "running",
            SessionState::Ended => "ended",
            SessionState::Interrupted => "interrupted",
        }
    }
    pub fn parse(s: &str) -> SessionState {
        match s {
            "starting" => SessionState::Starting,
            "running" => SessionState::Running,
            "interrupted" => SessionState::Interrupted,
            _ => SessionState::Ended,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: SessionId,
    pub instance_id: InstanceId,
    pub owner: SessionOwner,
    pub token: String,
    pub base_snapshot: Option<SnapshotId>,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
    pub state: SessionState,
    pub outcome: Option<SessionOutcome>,
    pub game_pid: Option<u32>,
    pub game_start_ms: Option<i64>,
    pub offline: bool,
    pub result_snapshot: Option<SnapshotId>,
    pub note: Option<String>,
    /// When a passive observer first saw that the session's game is gone. A session is only closed as
    /// interrupted after a grace period, so that the launcher's own post-exit hook can still end it
    /// cleanly (see `SessionMgr::stale_open_sessions`).
    pub dead_seen_ms: Option<i64>,
    /// The process that started an app-owned session (and start time, against pid reuse). A session
    /// whose owner is gone is not "owned" any more: launcher hooks take over.
    pub owner_pid: Option<u32>,
    pub owner_start_ms: Option<i64>,
}

const SESSION_COLS: &str = "id,instance_id,owner,token,base_snapshot,started_ms,ended_ms,state,outcome,game_pid,game_start_ms,offline,result_snapshot,note,dead_seen_ms,owner_pid,owner_start_ms";

fn session_from_row(r: &rusqlite::Row) -> rusqlite::Result<SessionRow> {
    let bad = |i: usize| rusqlite::Error::InvalidColumnType(i, "session".into(), rusqlite::types::Type::Text);
    Ok(SessionRow {
        id: SessionId::parse(&r.get::<_, String>(0)?).map_err(|_| bad(0))?,
        instance_id: InstanceId::parse(&r.get::<_, String>(1)?).map_err(|_| bad(1))?,
        owner: SessionOwner::parse(&r.get::<_, String>(2)?),
        token: r.get(3)?,
        base_snapshot: r.get::<_, Option<String>>(4)?.map(|s| s.parse().map_err(|_| bad(4))).transpose()?,
        started_ms: r.get(5)?,
        ended_ms: r.get(6)?,
        state: SessionState::parse(&r.get::<_, String>(7)?),
        outcome: r.get::<_, Option<String>>(8)?.and_then(|s| outcome_parse(&s)),
        game_pid: r.get(9)?,
        game_start_ms: r.get(10)?,
        offline: r.get(11)?,
        result_snapshot: r.get::<_, Option<String>>(12)?.map(|s| s.parse().map_err(|_| bad(12))).transpose()?,
        note: r.get(13)?,
        dead_seen_ms: r.get(14)?,
        owner_pid: r.get(15)?,
        owner_start_ms: r.get(16)?,
    })
}

#[derive(Debug, Clone)]
pub struct JobRow {
    pub id: crate::ids::JobId,
    pub kind: String,
    pub instance_id: Option<String>,
    pub request_key: Option<String>,
    pub state: String,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub error: Option<String>,
}
const JOB_COLS: &str = "id,kind,instance_id,request_key,state,created_ms,updated_ms,error";
fn job_from_row(r: &rusqlite::Row) -> rusqlite::Result<JobRow> {
    Ok(JobRow {
        id: crate::ids::JobId::parse(&r.get::<_, String>(0)?).map_err(|_| rusqlite::Error::InvalidQuery)?,
        kind: r.get(1)?,
        instance_id: r.get(2)?,
        request_key: r.get(3)?,
        state: r.get(4)?,
        created_ms: r.get(5)?,
        updated_ms: r.get(6)?,
        error: r.get(7)?,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct EventRow {
    pub ts_ms: i64,
    pub instance_id: Option<String>,
    pub level: String,
    pub code: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inst(name: &str) -> InstanceRow {
        InstanceRow {
            id: InstanceId::new(),
            name: name.into(),
            root: PathBuf::from(format!("/tmp/{name}")),
            launcher: "prism".into(),
            launcher_ref: None,
            game_root: ".minecraft".into(),
            sync_enabled: true,
            base_snapshot: None,
            pending_rules: None,
            marker: "m".into(),
            root_dev: None,
            provider_id: None,
            state: InstanceState::Ok,
            state_detail: None,
            created_ms: now_ms(),
            last_scan_ms: None,
            last_full_verify_ms: None,
            last_synced_ms: None,
            needs_full_scan: true,
        }
    }

    #[test]
    fn migrations_apply_and_reopen_is_idempotent() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("state.db");
        let db = Db::open(&p).unwrap();
        let (dev, _) = db.device().unwrap();
        drop(db);
        let db = Db::open(&p).unwrap();
        assert_eq!(db.device().unwrap().0, dev, "device identity persists");
    }

    #[test]
    fn newer_schema_is_refused_not_clobbered() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("state.db");
        Db::open(&p).unwrap();
        {
            let c = Connection::open(&p).unwrap();
            c.pragma_update(None, "user_version", 99).unwrap();
        }
        let err = Db::open(&p).err().expect("must refuse").to_string();
        assert!(err.contains("newer PackSync"), "{err}");
    }

    #[test]
    fn instance_crud_and_removal_cleans_dependents() {
        let db = Db::open_memory().unwrap();
        let a = inst("a");
        db.instance_insert(&a).unwrap();
        assert_eq!(db.instances().unwrap().len(), 1);
        let u = db.instance_update(a.id, |r| r.state = InstanceState::Conflict).unwrap();
        assert_eq!(u.state, InstanceState::Conflict);
        db.index_replace(
            a.id,
            &[IndexRow {
                path: "x".into(),
                size: 1,
                mtime_ns: 2,
                ctime_ns: Some(3),
                file_id: Some("1:2".into()),
                hash: Hash::of(b"x"),
                recipe: None,
                mixed: None,
                content_size: 1,
                hashed_at_ns: 4,
                verified_at_ns: 5,
            }],
        )
        .unwrap();
        assert_eq!(db.index_load(a.id).unwrap().len(), 1);
        db.instance_remove(a.id).unwrap();
        assert!(db.instance_get(a.id).unwrap().is_none());
        assert!(db.index_load(a.id).unwrap().is_empty());
    }

    #[test]
    fn snapshot_state_machine_records_publish_times_once() {
        let db = Db::open_memory().unwrap();
        let a = inst("a");
        db.instance_insert(&a).unwrap();
        let id = SnapshotId(Hash::of(b"s1"));
        let row = SnapshotRow {
            id,
            instance_id: a.id,
            kind: SnapshotKind::Normal,
            parents: vec![],
            parents_known: true,
            device_id: None,
            device_name: Some("d".into()),
            created: Some("2026-01-01T00:00:00Z".into()),
            origin: Origin::Local,
            state: SnapshotState::LocalComplete,
            files: 3,
            bytes: 30,
            note: None,
            session_outcome: None,
            published_ms: None,
            visible_ms: None,
            has_manifest: true,
        };
        db.snapshot_upsert(&row).unwrap();
        db.snapshot_set_state(a.id, id, SnapshotState::Uploading).unwrap();
        assert!(db.snapshot_get(a.id, id).unwrap().unwrap().published_ms.is_none());
        db.snapshot_set_state(a.id, id, SnapshotState::Published).unwrap();
        let p = db.snapshot_get(a.id, id).unwrap().unwrap();
        assert!(p.published_ms.is_some() && p.visible_ms.is_none());
        db.snapshot_set_state(a.id, id, SnapshotState::Visible).unwrap();
        assert!(db.snapshot_get(a.id, id).unwrap().unwrap().visible_ms.is_some());
        assert!(db.snapshot_set_state(a.id, SnapshotId(Hash::of(b"nope")), SnapshotState::Visible).is_err());
    }

    #[test]
    fn jobs_are_idempotent_by_request_key() {
        let db = Db::open_memory().unwrap();
        let (a, created_a) = db.job_register(crate::ids::JobId::new(), "sync", None, Some("req-1")).unwrap();
        let (b, created_b) = db.job_register(crate::ids::JobId::new(), "sync", None, Some("req-1")).unwrap();
        assert!(created_a && !created_b);
        assert_eq!(a.id, b.id, "a duplicate request returns the existing job");
        let (c, created_c) = db.job_register(crate::ids::JobId::new(), "sync", None, None).unwrap();
        assert!(created_c && c.id != a.id);
        assert_eq!(db.jobs_mark_interrupted().unwrap(), 2, "two distinct jobs were running");
    }

    #[test]
    fn transfer_state_roundtrips_and_events_redact_secrets() {
        let db = Db::open_memory().unwrap();
        db.save("k", "v1");
        assert_eq!(db.load("k").as_deref(), Some("v1"));
        db.save("k", "v2");
        assert_eq!(db.load("k").as_deref(), Some("v2"));
        db.clear("k");
        assert!(db.load("k").is_none());
        db.event(None, "warn", "x", "failed PUT https://x/upload?upload_id=SECRETVALUE123 now");
        let ev = db.events_recent(None, 10).unwrap();
        assert!(!ev[0].message.contains("SECRETVALUE123"), "{}", ev[0].message);
    }

    #[test]
    fn blob_cache_lookup_and_pack_forgetting() {
        let db = Db::open_memory().unwrap();
        let pack = Hash::of(b"pack");
        let e = BlobEntry { hash: Hash::of(b"blob"), offset: 16, stored_len: 10, raw_len: 12, codec: 1, kind: 0 };
        db.blobs_put(pack, std::slice::from_ref(&e)).unwrap();
        assert_eq!(db.blob_lookup(e.hash).unwrap().unwrap(), (pack, e.clone()));
        assert!(db.blob_known(e.hash).unwrap());
        db.blobs_forget_pack(pack).unwrap();
        assert!(!db.blob_known(e.hash).unwrap(), "cache entries for a vanished pack are dropped");
    }
}

//! Resumable uploads, restart-resume, ambiguous creates, session expiry, verification failures
//! and cancellation.

mod common;

use std::sync::Arc;

use common::{digest, pattern, read_all};
use packsync_core::progress::{CancelToken, EventKind, OpCtx, ProgressEvent, ProgressSink};
use packsync_core::retry::RetryPolicy;
use packsync_core::storage::{PutOptions, PutSource, Storage, StorageError, TransferStateStore};
use packsync_drive::DriveSpace;
use packsync_drive::mock::{Endpoint, Fault, FaultRule, MockConfig, When};

const KIB: usize = 1024;
const MIB: usize = 1024 * 1024;

fn objects_named(env: &common::Env, name: &str) -> usize {
    env.mock.objects().iter().filter(|o| o.name == name && !o.is_folder).count()
}

fn chunk_starts(env: &common::Env) -> Vec<u64> {
    env.mock
        .requests()
        .iter()
        .filter(|r| r.endpoint == Endpoint::ResumableChunk)
        .filter_map(|r| r.content_range.as_ref()?.strip_prefix("bytes ")?.split('-').next()?.parse().ok())
        .collect()
}

#[test]
fn interrupted_upload_resumes_in_a_new_instance_from_the_confirmed_offset() {
    let env = common::setup_with(MockConfig::default(), DriveSpace::VisibleFolder, |c| c.retry = RetryPolicy::none());
    let data = pattern(6 * MIB, 1);
    let key = "packs/resume.pack";
    let ctx = OpCtx::detached();
    // The third 1 MiB chunk is cut after 300 KiB; the mock keeps 256 KiB of it.
    env.mock.add_fault(
        FaultRule::new(Endpoint::ResumableChunk, When::Nth(3), Fault::DropAfterRequestBytes(300 * KIB as u64)).max(1),
    );
    let err = env.drive.put(key, PutSource::Bytes(&data), &PutOptions::create_only(Some(digest(&data))), &ctx);
    assert!(matches!(err, Err(StorageError::Offline(_))), "{err:?}");
    assert_eq!(objects_named(&env, "resume.pack"), 0, "nothing is visible before completion");
    let persisted = env.state.load(&format!("{}|up|{key}", env.drive.provider_id()));
    assert!(persisted.is_some_and(|s| s.contains("upload_id")), "the session is persisted for resumption");
    let before = env.mock.requests().len();
    let up_before = env.drive.stats().bytes_up;

    // A brand-new adapter (a restarted process) sharing only the transfer state.
    let second = env.new_instance(|c| c.retry = RetryPolicy::network());
    let info = second.put(key, PutSource::Bytes(&data), &PutOptions::create_only(Some(digest(&data))), &ctx).unwrap();
    assert_eq!(info.size, data.len() as u64);

    let confirmed = (2 * MIB + 256 * KIB) as u64;
    let after: Vec<_> = env.mock.requests()[before..].to_vec();
    assert_eq!(after.iter().filter(|r| r.endpoint == Endpoint::ResumableInit).count(), 0, "no new session");
    assert_eq!(after.iter().filter(|r| r.endpoint == Endpoint::ResumableStatus).count(), 1);
    let first_resumed =
        after.iter().find(|r| r.endpoint == Endpoint::ResumableChunk).and_then(|r| r.content_range.clone()).unwrap();
    assert!(first_resumed.starts_with(&format!("bytes {confirmed}-")), "{first_resumed}");
    let resumed_bytes = env.drive.stats().bytes_up - up_before;
    assert!(resumed_bytes < data.len() as u64 - confirmed + 64 * KIB as u64, "{resumed_bytes}");
    let total_sent = env.drive.stats().bytes_up;
    assert!((total_sent as f64) < 1.5 * data.len() as f64, "sent {total_sent} for {} bytes", data.len());
    assert_eq!(read_all(second.get(key, None, &ctx).unwrap()).unwrap(), data);
    assert_eq!(objects_named(&env, "resume.pack"), 1);
    assert!(env.state.load(&format!("{}|up|{key}", env.drive.provider_id())).is_none(), "state cleared");
    assert_eq!(chunk_starts(&env).first(), Some(&0));
}

#[test]
fn ambiguous_multipart_create_yields_exactly_one_object() {
    let env = common::setup();
    let data = pattern(100 * KIB, 2);
    env.mock.add_fault(FaultRule::new(Endpoint::UploadMultipart, When::Nth(1), Fault::ApplyThenDrop));
    let ctx = OpCtx::detached();
    let info = env
        .drive
        .put("commits/c1.json", PutSource::Bytes(&data), &PutOptions::create_only(Some(digest(&data))), &ctx)
        .unwrap();
    assert_eq!(objects_named(&env, "c1.json"), 1, "the retry must not duplicate");
    assert_eq!(env.sleeps().len(), 1, "one backoff after the lost response");
    assert_eq!(env.mock.count(Endpoint::UploadMultipart), 2, "the second attempt hit 409 on the same ID");
    assert_eq!(
        info.provider_ref.as_deref(),
        env.mock.objects().iter().find(|o| o.name == "c1.json").map(|o| o.id.as_str())
    );
    assert_eq!(read_all(env.drive.get("commits/c1.json", None, &ctx).unwrap()).unwrap(), data);
}

#[test]
fn ambiguous_final_chunk_is_resolved_by_a_status_query() {
    let env = common::setup();
    let data = pattern(6 * MIB + 123, 3);
    env.mock.add_fault(FaultRule::new(Endpoint::ResumableChunk, When::Nth(7), Fault::ApplyThenDrop));
    let ctx = OpCtx::detached();
    env.drive
        .put("packs/a.pack", PutSource::Bytes(&data), &PutOptions::create_only(Some(digest(&data))), &ctx)
        .unwrap();
    assert_eq!(objects_named(&env, "a.pack"), 1);
    assert_eq!(env.mock.count(Endpoint::ResumableStatus), 1);
    assert_eq!(env.mock.count(Endpoint::ResumableInit), 1);
}

#[test]
fn lost_create_across_processes_reuses_the_persisted_id() {
    // New objects are invisible to listings for a while, so the second process cannot find the
    // first object by name; only the persisted pre-generated ID prevents a duplicate.
    let cfg = MockConfig { list_visibility_delay: 100, ..MockConfig::default() };
    let env = common::setup_with(cfg, DriveSpace::VisibleFolder, |c| c.retry = RetryPolicy::none());
    let data = pattern(10 * KIB, 4);
    env.mock.add_fault(FaultRule::new(Endpoint::UploadMultipart, When::Nth(1), Fault::ApplyThenDrop));
    let ctx = OpCtx::detached();
    let opts = PutOptions::create_only(Some(digest(&data)));
    assert!(env.drive.put("heads/h.json", PutSource::Bytes(&data), &opts, &ctx).is_err());
    let second = env.new_instance(|_| {});
    second.put("heads/h.json", PutSource::Bytes(&data), &opts, &ctx).unwrap();
    assert_eq!(objects_named(&env, "h.json"), 1);
}

#[test]
fn expired_session_restarts_cleanly() {
    let env = common::setup();
    let data = pattern(6 * MIB, 5);
    env.mock.add_fault(FaultRule::new(Endpoint::ResumableChunk, When::Nth(3), Fault::ExpireSession));
    let ctx = OpCtx::detached();
    env.drive
        .put("packs/e.pack", PutSource::Bytes(&data), &PutOptions::create_only(Some(digest(&data))), &ctx)
        .unwrap();
    assert_eq!(env.mock.count(Endpoint::ResumableInit), 2, "a fresh session after the 404");
    assert_eq!(objects_named(&env, "e.pack"), 1);
    assert_eq!(read_all(env.drive.get("packs/e.pack", None, &ctx).unwrap()).unwrap(), data);
}

#[test]
fn expired_persisted_session_restarts_in_a_new_instance() {
    let env = common::setup_with(MockConfig::default(), DriveSpace::VisibleFolder, |c| c.retry = RetryPolicy::none());
    let data = pattern(6 * MIB, 6);
    env.mock.add_fault(FaultRule::new(Endpoint::ResumableChunk, When::Nth(2), Fault::DropConnection).max(1));
    let ctx = OpCtx::detached();
    let opts = PutOptions::create_only(Some(digest(&data)));
    assert!(env.drive.put("packs/x.pack", PutSource::Bytes(&data), &opts, &ctx).is_err());
    env.mock.expire_sessions();
    let second = env.new_instance(|_| {});
    second.put("packs/x.pack", PutSource::Bytes(&data), &opts, &ctx).unwrap();
    assert_eq!(env.mock.count(Endpoint::ResumableInit), 2);
    assert_eq!(objects_named(&env, "x.pack"), 1);
    assert_eq!(read_all(second.get("packs/x.pack", None, &ctx).unwrap()).unwrap(), data);
}

#[test]
fn checksum_mismatch_is_corrupt_and_the_bad_object_is_removed() {
    let env = common::setup();
    let ctx = OpCtx::detached();
    let small = pattern(4 * KIB, 7);
    env.mock.add_fault(FaultRule::new(Endpoint::UploadMultipart, When::Nth(1), Fault::CorruptStoredContent));
    let r =
        env.drive.put("packs/s.pack", PutSource::Bytes(&small), &PutOptions::create_only(Some(digest(&small))), &ctx);
    assert!(matches!(r, Err(StorageError::Corrupt(ref m)) if m.contains("removed")), "{r:?}");
    assert_eq!(objects_named(&env, "s.pack"), 0);

    let big = pattern(6 * MIB, 8);
    env.mock.add_fault(FaultRule::new(Endpoint::ResumableChunk, When::Always, Fault::WrongMd5));
    let r = env.drive.put("packs/b.pack", PutSource::Bytes(&big), &PutOptions::create_only(Some(digest(&big))), &ctx);
    assert!(matches!(r, Err(StorageError::Corrupt(_))), "{r:?}");
    assert_eq!(objects_named(&env, "b.pack"), 0);
    // Only the bad objects were deleted: the folders and nothing else were touched.
    assert_eq!(env.mock.count(Endpoint::FilesDelete), 2);
    env.mock.clear_faults();
    env.drive.put("packs/b.pack", PutSource::Bytes(&big), &PutOptions::create_only(Some(digest(&big))), &ctx).unwrap();
    assert_eq!(objects_named(&env, "b.pack"), 1);
}

struct CancelOnProgress(CancelToken);
impl ProgressSink for CancelOnProgress {
    fn emit(&self, ev: ProgressEvent) {
        if matches!(ev.kind, EventKind::Progress) && ev.bytes_done >= 2 * MIB as u64 {
            self.0.cancel();
        }
    }
}

#[test]
fn cancellation_mid_upload_leaves_nothing_and_can_resume() {
    let env = common::setup();
    let data = pattern(6 * MIB, 9);
    let token = CancelToken::new();
    let ctx = OpCtx::new(Arc::new(CancelOnProgress(token.clone())), token);
    let opts = PutOptions::create_only(Some(digest(&data)));
    let r = env.drive.put("packs/c.pack", PutSource::Bytes(&data), &opts, &ctx);
    assert!(matches!(r, Err(StorageError::Cancelled)), "{r:?}");
    assert_eq!(objects_named(&env, "c.pack"), 0);
    let chunks = env.mock.count(Endpoint::ResumableChunk);
    assert!(chunks <= 3, "stopped promptly after cancellation ({chunks} chunks)");
    let ctx = OpCtx::detached();
    env.drive.put("packs/c.pack", PutSource::Bytes(&data), &opts, &ctx).unwrap();
    assert_eq!(env.mock.count(Endpoint::ResumableInit), 1, "the cancelled session was resumed");
    assert_eq!(read_all(env.drive.get("packs/c.pack", None, &ctx).unwrap()).unwrap(), data);
}

#[test]
fn replace_updates_in_place_including_large_objects() {
    let env = common::setup();
    let ctx = OpCtx::detached();
    let key = "instances/i1/heads/dev.json";
    let v1 = pattern(1000, 10);
    let a = env.drive.put(key, PutSource::Bytes(&v1), &PutOptions::replace(), &ctx).unwrap();
    let v2 = pattern(6 * MIB, 11);
    let b = env.drive.put(key, PutSource::Bytes(&v2), &PutOptions::replace(), &ctx).unwrap();
    assert_eq!(a.provider_ref, b.provider_ref, "same file, new content");
    assert_eq!(read_all(env.drive.get(key, None, &ctx).unwrap()).unwrap(), v2);
    assert_eq!(objects_named(&env, "dev.json"), 1);
    // A brand-new instance finds the single-writer key by its persisted ID.
    let second = env.new_instance(|_| {});
    let v3 = b"v3".to_vec();
    let c = second.put(key, PutSource::Bytes(&v3), &PutOptions::replace(), &ctx).unwrap();
    assert_eq!(c.provider_ref, a.provider_ref);
    assert_eq!(objects_named(&env, "dev.json"), 1);
}

#[test]
fn file_sources_stream_in_bounded_chunks_and_detect_size_lies() {
    let env = common::setup();
    let ctx = OpCtx::detached();
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("big.bin");
    let data = pattern(7 * MIB + 5, 12);
    std::fs::write(&p, &data).unwrap();
    env.drive
        .put("packs/f.pack", PutSource::File { path: &p, len: data.len() as u64 }, &PutOptions::create_only(None), &ctx)
        .unwrap();
    assert_eq!(read_all(env.drive.get("packs/f.pack", None, &ctx).unwrap()).unwrap(), data);
    let r = env.drive.put(
        "packs/g.pack",
        PutSource::File { path: &p, len: 6 * MIB as u64 },
        &PutOptions::create_only(None),
        &ctx,
    );
    assert!(matches!(r, Err(StorageError::Corrupt(_))), "{r:?}");
    assert_eq!(objects_named(&env, "g.pack"), 0);
}

#[test]
fn tags_are_stored_and_update_tags_changes_only_properties() {
    let env = common::setup();
    let ctx = OpCtx::detached();
    let opts = PutOptions::replace().with_tag("kind", "lease").with_tag("hb", "1");
    let info = env.drive.put("leases/l.json", PutSource::Bytes(b"{}"), &opts, &ctx).unwrap();
    assert_eq!(info.tags.get("kind").map(String::as_str), Some("lease"));
    let updates_before = env.mock.count(Endpoint::FilesUpdateMedia);
    let mut t = std::collections::BTreeMap::new();
    t.insert("hb".to_string(), "2".to_string());
    let after = env.drive.update_tags("leases/l.json", &t, &ctx).unwrap();
    assert_eq!(after.tags.get("hb").map(String::as_str), Some("2"));
    assert_eq!(after.tags.get("kind").map(String::as_str), Some("lease"));
    assert_eq!(env.mock.count(Endpoint::FilesUpdateMedia), updates_before, "no new content revision");
    let stat = env.drive.stat("leases/l.json", &ctx).unwrap().unwrap();
    assert_eq!(stat.tags.get("hb").map(String::as_str), Some("2"));
    // the PackSync marker is reported (retention relies on it); the adapter's private folder marker is not
    assert_eq!(stat.tags.get("ps").map(String::as_str), Some("1"));
    assert!(!stat.tags.contains_key("psk"), "the private marker is not exposed as a tag");
}

#[test]
fn the_engines_own_ps_tag_is_accepted_and_reported_back() {
    // packsync-core marks everything it stores with ps=1 itself; the adapter always writes the marker,
    // so passing it must be a no-op (not an error) and listings must report it (retention checks it).
    let env = common::setup();
    let ctx = OpCtx::detached();
    let opts = PutOptions::create_only(None).with_tag("ps", "1").with_tag("kind", "pack");
    env.drive.put("packs/tagged.bin", PutSource::Bytes(b"x"), &opts, &ctx).unwrap();
    let info = env.drive.stat("packs/tagged.bin", &ctx).unwrap().unwrap();
    assert_eq!(info.tags.get("ps").map(String::as_str), Some("1"));
    assert_eq!(info.tags.get("kind").map(String::as_str), Some("pack"));
    let listed = env.drive.list("packs/", &ctx).unwrap();
    assert!(listed.iter().all(|o| o.tags.get("ps").map(String::as_str) == Some("1")), "{listed:?}");
    // any other value for the marker is refused
    let bad = PutOptions::create_only(None).with_tag("ps", "0");
    assert!(env.drive.put("packs/bad.bin", PutSource::Bytes(b"x"), &bad, &ctx).is_err());
}

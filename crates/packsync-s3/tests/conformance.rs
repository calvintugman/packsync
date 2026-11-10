//! The storage contract against the S3 provider and the mock server (which checks every signature).

use std::time::Duration;

use packsync_core::progress::OpCtx;
use packsync_core::redact::Secret;
use packsync_core::storage::*;
use packsync_core::testkit::conformance::{ConformanceOptions, assert_conformance};
use packsync_s3::S3Storage;
use packsync_s3::mock::{MockS3, SECRET};

fn opts() -> ConformanceOptions {
    ConformanceOptions { list_count: 60, large_bytes: Some(12 * 1024 * 1024), writers: 8, list_wait: Duration::ZERO }
}

#[test]
fn conforms_against_the_mock() {
    let mock = MockS3::start("packsync-test");
    let st = S3Storage::new(mock.config("packsync"), Secret::new(SECRET)).unwrap();
    assert_conformance(&st, opts());
    // everything it wrote is under its own prefix
    assert!(mock.keys().iter().all(|k| k.starts_with("packsync/")), "{:?}", mock.keys());
}

#[test]
fn conforms_with_small_listing_pages() {
    let mock = MockS3::start_with("packsync-test", 7);
    let st = S3Storage::new(mock.config("packsync"), Secret::new(SECRET)).unwrap();
    assert_conformance(&st, opts());
}

#[test]
fn throttling_is_retried_and_a_wrong_secret_is_an_authentication_error() {
    let mock = MockS3::start("packsync-test");
    let st = S3Storage::new(mock.config("packsync"), Secret::new(SECRET)).unwrap();
    let ctx = OpCtx::detached();
    mock.fail_next(2);
    st.put("a/b.bin", PutSource::Bytes(b"hello"), &PutOptions::create_only(None), &ctx).unwrap();
    assert_eq!(st.stats().snapshot().3, 2, "two throttled attempts were retried");
    assert_eq!(st.get_bytes("a/b.bin", 10, &ctx).unwrap(), b"hello");

    let wrong = S3Storage::new(mock.config("packsync"), Secret::new("not-the-secret")).unwrap();
    match wrong.stat("a/b.bin", &ctx) {
        Err(StorageError::Auth(_)) | Err(StorageError::Permission(_)) => {}
        // HEAD responses have no body, so the error code is unknown: a 403 without a code
        other => panic!("expected an authentication error, got {other:?}"),
    }
    match wrong.list("a/", &ctx) {
        Err(StorageError::Auth(_)) => {}
        other => panic!("expected an authentication error with the S3 code, got {other:?}"),
    }
}

#[test]
fn tags_are_stored_as_metadata_and_listings_belong_to_packsync() {
    let mock = MockS3::start("packsync-test");
    let st = S3Storage::new(mock.config("packsync"), Secret::new(SECRET)).unwrap();
    let ctx = OpCtx::detached();
    let info = st
        .put(
            "packs/x.pack",
            PutSource::Bytes(b"pack"),
            &PutOptions::create_only(None).with_tag("ps", "1").with_tag("kind", "pack"),
            &ctx,
        )
        .unwrap();
    assert!(info.version.is_some());
    let s = st.stat("packs/x.pack", &ctx).unwrap().unwrap();
    assert_eq!(s.tags.get("ps").map(String::as_str), Some("1"));
    assert_eq!(s.tags.get("kind").map(String::as_str), Some("pack"));
    assert!(st.capabilities().owned_namespace && st.capabilities().conditional_write);
}

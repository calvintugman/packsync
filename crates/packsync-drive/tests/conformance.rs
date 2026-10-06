//! The storage conformance suite against `DriveStorage` + the mock server.

mod common;

use std::time::Duration;

use packsync_core::testkit::conformance::{ConformanceOptions, assert_conformance};
use packsync_drive::DriveSpace;
use packsync_drive::mock::MockConfig;

fn opts(list_count: usize) -> ConformanceOptions {
    ConformanceOptions { list_count, large_bytes: Some(12 * 1024 * 1024), writers: 8, list_wait: Duration::ZERO }
}

#[test]
fn conforms_against_the_mock() {
    let env = common::setup();
    assert_conformance(&env.drive, opts(60));
    let s = env.drive.stats();
    assert!(s.chunk >= 12, "the 12 MiB object must use resumable chunks: {s:?}");
    assert!(s.resumable_init >= 1 && s.create > 0 && s.list > 0, "{s:?}");
    assert!(env.sleeps().len() <= 1, "unexpected retries without faults: {:?}", env.drive.recent_retries());
    if !env.sleeps().is_empty() {
        eprintln!("note: retried without injected faults: {:?}", env.drive.recent_retries());
    }
}

#[test]
fn conforms_with_small_pages() {
    let cfg = MockConfig { page_size_cap: Some(25), ..MockConfig::default() };
    let env = common::setup_with(cfg, DriveSpace::VisibleFolder, |c| c.page_size = 25);
    let before = env.mock.count(packsync_drive::mock::Endpoint::FilesList);
    assert_conformance(&env.drive, opts(60));
    assert!(env.mock.requests().iter().any(|r| r.endpoint == packsync_drive::mock::Endpoint::FilesList) && before == 0);
    // 60 objects with pages of 25 need at least three list requests for that one listing.
    let paged = env.mock.requests().iter().filter(|r| r.endpoint == packsync_drive::mock::Endpoint::FilesList).count();
    assert!(paged >= 3, "{paged}");
}

#[test]
fn conforms_in_the_app_data_space() {
    let env = common::setup_with(MockConfig::default(), DriveSpace::AppData, |_| {});
    assert_conformance(&env.drive, ConformanceOptions { large_bytes: Some(6 * 1024 * 1024), ..opts(30) });
    let objs = env.mock.objects();
    let root = objs.iter().find(|o| o.name == "PackSync" && o.is_folder).expect("root folder");
    assert_eq!(root.parents, vec![env.mock.appdata_id().to_string()]);
}

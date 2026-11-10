//! Run the in-process mock S3 server on its own, for trying the CLI end to end without a cloud:
//!
//!     cargo run -p packsync-s3 --features mock --example mock-s3-server -- <bucket> [<data folder>]
//!     PACKSYNC_R2_ENDPOINT=<printed URL> packsync storage use-r2 --account-id mock --bucket <bucket> \
//!         --access-key-id MOCKACCESSKEY --secret-stdin <<< 'mock/secret/key+with/slashes'
//!
//! Objects are kept in memory, or in files under <data folder>; stop it with Ctrl-C.
fn main() {
    let bucket = std::env::args().nth(1).unwrap_or_else(|| "packsync".into());
    let mock = match std::env::args().nth(2) {
        Some(dir) => packsync_s3::mock::MockS3::start_on_disk(&bucket, std::path::Path::new(&dir)),
        None => packsync_s3::mock::MockS3::start(&bucket),
    };
    println!("{}", mock.url());
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

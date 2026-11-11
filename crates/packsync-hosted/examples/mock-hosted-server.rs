//! Run the in-process hosted-service stand-in on a local port (for trying the CLI without Cloudflare):
//! `cargo run -p packsync-hosted --features mock --example mock-hosted-server`, then
//! `PACKSYNC_HOSTED_URL=<printed URL> packsync storage start`.

fn main() {
    let m = packsync_hosted::mock::MockService::start();
    println!("{}", m.url());
    loop {
        std::thread::park();
    }
}

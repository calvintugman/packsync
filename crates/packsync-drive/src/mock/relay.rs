//! A TCP relay in front of the mock's `tiny_http` server. `tiny_http` cannot abort a connection
//! from a request handler (an unanswered request gets a `500`, keep-alive keeps the socket open),
//! so connection-level faults are implemented here: the handler identifies its connection by the
//! peer address `tiny_http` reports (the relay's upstream socket) and asks the relay to cut it,
//! optionally after writing raw bytes (a truncated response) to the client.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use parking_lot::Mutex;

pub(crate) struct Relay {
    addr: SocketAddr,
    /// Upstream-socket local address → (client socket, upstream socket).
    conns: Mutex<HashMap<SocketAddr, (TcpStream, TcpStream)>>,
    stop: AtomicBool,
}

fn pump(mut from: TcpStream, mut to: TcpStream) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
    let _ = to.shutdown(Shutdown::Write);
}

impl Relay {
    pub fn start(upstream: SocketAddr) -> io::Result<Arc<Relay>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let relay = Arc::new(Relay {
            addr: listener.local_addr()?,
            conns: Mutex::new(HashMap::new()),
            stop: AtomicBool::new(false),
        });
        let r = relay.clone();
        thread::Builder::new().name("mock-drive-relay".into()).spawn(move || {
            for stream in listener.incoming() {
                if r.stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(client) = stream else { continue };
                let Ok(up) = TcpStream::connect(upstream) else {
                    let _ = client.shutdown(Shutdown::Both);
                    continue;
                };
                let _ = client.set_nodelay(true);
                let _ = up.set_nodelay(true);
                let (Ok(key), Ok(c1), Ok(c2), Ok(u1), Ok(u2)) =
                    (up.local_addr(), client.try_clone(), client.try_clone(), up.try_clone(), up.try_clone())
                else {
                    continue;
                };
                r.conns.lock().insert(key, (c1, u1));
                let r2 = r.clone();
                let _ = thread::Builder::new().name("mock-relay-up".into()).spawn(move || pump(client, up));
                let _ = thread::Builder::new().name("mock-relay-down".into()).spawn(move || {
                    pump(u2, c2);
                    r2.conns.lock().remove(&key);
                });
            }
        })?;
        Ok(relay)
    }

    /// Address clients connect to.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Abort the connection whose upstream end has local address `key`.
    pub fn kill(&self, key: &SocketAddr) {
        if let Some((c, u)) = self.conns.lock().remove(key) {
            let _ = c.shutdown(Shutdown::Both);
            let _ = u.shutdown(Shutdown::Both);
        }
    }

    /// Write `bytes` straight to the client, then abort the connection.
    pub fn send_raw_and_kill(&self, key: &SocketAddr, bytes: &[u8]) {
        if let Some((mut c, u)) = self.conns.lock().remove(key) {
            let _ = c.write_all(bytes);
            let _ = c.flush();
            let _ = c.shutdown(Shutdown::Both);
            let _ = u.shutdown(Shutdown::Both);
        }
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        let conns: Vec<_> = self.conns.lock().drain().collect();
        for (_, (c, u)) in conns {
            let _ = c.shutdown(Shutdown::Both);
            let _ = u.shutdown(Shutdown::Both);
        }
    }
}

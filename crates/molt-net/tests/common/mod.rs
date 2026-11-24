// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(dead_code, missing_docs)]

//! Test doubles shared by the molt-net integration suites.

/// A cuttable TCP proxy in front of a relay: while enabled it forwards
/// byte-for-byte; "cut" aborts every live forward and refuses new ones —
/// the only way to take a MockRelay down and bring "it" back on the SAME
/// port (the relay itself cannot rebind).
pub mod proxy {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::Mutex;

    pub struct Cuttable {
        pub port: u16,
        enabled: Arc<AtomicBool>,
        forwards: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
        /// Per-connection "swallow everything" flags — one per live forward.
        darks: Arc<Mutex<Vec<Arc<AtomicBool>>>>,
    }

    /// One direction of a forward. While `dark` is set the pump keeps
    /// READING (so the sender's TCP writes keep succeeding) but discards
    /// every byte — the half-dead flow a dropped Tor circuit produces.
    async fn pump(mut from: TcpStream, mut to: TcpStream, dark: Arc<AtomicBool>) {
        let (mut fr, mut fw) = from.split();
        let (mut tr, mut tw) = to.split();
        let mut a = [0u8; 16384];
        let mut b = [0u8; 16384];
        loop {
            tokio::select! {
                r = fr.read(&mut a) => match r {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if !dark.load(Ordering::SeqCst)
                            && tw.write_all(&a[..n]).await.is_err() {
                            break;
                        }
                    }
                },
                r = tr.read(&mut b) => match r {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if !dark.load(Ordering::SeqCst)
                            && fw.write_all(&b[..n]).await.is_err() {
                            break;
                        }
                    }
                },
            }
        }
    }

    impl Cuttable {
        pub async fn run(target: String) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
            let port = listener.local_addr().expect("addr").port();
            let enabled = Arc::new(AtomicBool::new(true));
            let forwards: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> =
                Arc::new(Mutex::new(Vec::new()));
            let darks: Arc<Mutex<Vec<Arc<AtomicBool>>>> = Arc::new(Mutex::new(Vec::new()));
            let on = enabled.clone();
            let fw = forwards.clone();
            let dk = darks.clone();
            tokio::spawn(async move {
                while let Ok((inbound, _)) = listener.accept().await {
                    if !on.load(Ordering::SeqCst) {
                        drop(inbound); // refuse while cut
                        continue;
                    }
                    let target = target.clone();
                    let dark = Arc::new(AtomicBool::new(false));
                    dk.lock().await.push(dark.clone());
                    fw.lock().await.push(tokio::spawn(async move {
                        if let Ok(outbound) = TcpStream::connect(&target).await {
                            pump(inbound, outbound, dark).await;
                        }
                    }));
                }
            });
            Self { port, enabled, forwards, darks }
        }

        pub async fn cut(&self) {
            self.enabled.store(false, Ordering::SeqCst);
            for f in self.forwards.lock().await.drain(..) {
                f.abort();
            }
            self.darks.lock().await.clear();
        }

        pub fn restore(&self) {
            self.enabled.store(true, Ordering::SeqCst);
        }

        /// Go half-dead: every LIVE forward keeps its sockets open but
        /// silently swallows both directions from now on. New connections
        /// forward normally — a redial gets a healthy circuit.
        pub async fn blackhole(&self) {
            for dark in self.darks.lock().await.drain(..) {
                dark.store(true, Ordering::SeqCst);
            }
        }
    }

    /// A forwarding proxy that counts TCP accepts and the inbound sides
    /// that closed, both awaitable: "no accept" is read off the counter,
    /// never off a sleep.
    pub struct Counting {
        pub port: u16,
        accepts: tokio::sync::watch::Receiver<usize>,
        closes: tokio::sync::watch::Receiver<usize>,
    }

    impl Counting {
        pub async fn run(target: String) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
            let port = listener.local_addr().expect("addr").port();
            let (acc_tx, accepts) = tokio::sync::watch::channel(0usize);
            let (close_tx, closes) = tokio::sync::watch::channel(0usize);
            let close_tx = Arc::new(close_tx);
            tokio::spawn(async move {
                while let Ok((mut inbound, _)) = listener.accept().await {
                    acc_tx.send_modify(|n| *n += 1);
                    let (target, close_tx) = (target.clone(), close_tx.clone());
                    tokio::spawn(async move {
                        if let Ok(mut outbound) = TcpStream::connect(&target).await {
                            let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                        }
                        close_tx.send_modify(|n| *n += 1);
                    });
                }
            });
            Self { port, accepts, closes }
        }

        pub fn url(&self) -> String {
            format!("ws://127.0.0.1:{}", self.port)
        }

        pub fn accepts(&self) -> usize {
            *self.accepts.borrow()
        }

        /// A failure bound, never a verdict: green returns as soon as it holds.
        const BOUND: std::time::Duration = std::time::Duration::from_secs(15);

        pub async fn wait_accepts(&self, n: usize) {
            let mut rx = self.accepts.clone();
            tokio::time::timeout(Self::BOUND, rx.wait_for(|c| *c >= n))
                .await
                .expect("the accept never came")
                .expect("proxy alive");
        }

        pub async fn wait_closes(&self, n: usize) {
            let mut rx = self.closes.clone();
            tokio::time::timeout(Self::BOUND, rx.wait_for(|c| *c >= n))
                .await
                .expect("the connection never closed")
                .expect("proxy alive");
        }
    }
}

/// A SOCKS5 proxy that holds each CONNECT until released, then answers
/// success and counts what the client sends after it: the window between
/// a dial's TCP/Tor connect and its handshake, made observable.
pub mod held_socks {
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::{oneshot, Mutex};

    pub struct Held {
        pub port: u16,
        /// Fires once per CONNECT request; the sender releases it.
        pub requests: Mutex<tokio::sync::mpsc::Receiver<(oneshot::Sender<()>, oneshot::Receiver<usize>)>>,
    }

    pub async fn run() -> Held {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind socks");
        let port = listener.local_addr().expect("addr").port();
        let (req_tx, requests) = tokio::sync::mpsc::channel(8);
        let req_tx = Arc::new(req_tx);
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let req_tx = req_tx.clone();
                tokio::spawn(async move {
                    let mut head = [0u8; 2];
                    if s.read_exact(&mut head).await.is_err() {
                        return;
                    }
                    let mut methods = vec![0u8; usize::from(head[1])];
                    if s.read_exact(&mut methods).await.is_err() {
                        return;
                    }
                    let _ = s.write_all(&[0x05, 0x02]).await;
                    let mut ah = [0u8; 2];
                    if s.read_exact(&mut ah).await.is_err() {
                        return;
                    }
                    let mut user = vec![0u8; usize::from(ah[1])];
                    let _ = s.read_exact(&mut user).await;
                    let mut pl = [0u8; 1];
                    let _ = s.read_exact(&mut pl).await;
                    let mut pass = vec![0u8; usize::from(pl[0])];
                    let _ = s.read_exact(&mut pass).await;
                    let _ = s.write_all(&[0x01, 0x00]).await;
                    let mut ch = [0u8; 4];
                    if s.read_exact(&mut ch).await.is_err() {
                        return;
                    }
                    let mut hl = [0u8; 1];
                    let _ = s.read_exact(&mut hl).await;
                    let mut host = vec![0u8; usize::from(hl[0])];
                    let _ = s.read_exact(&mut host).await;
                    let mut port_bytes = [0u8; 2];
                    let _ = s.read_exact(&mut port_bytes).await;
                    let (release_tx, release) = oneshot::channel();
                    let (sent_tx, sent) = oneshot::channel();
                    if req_tx.send((release_tx, sent)).await.is_err() {
                        return;
                    }
                    if release.await.is_err() {
                        return;
                    }
                    let _ = s.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await;
                    // the client's first bytes, or 0 once it hung up silent
                    let mut buf = [0u8; 4096];
                    let after = s.read(&mut buf).await.unwrap_or(0);
                    let _ = sent_tx.send(after);
                });
            }
        });
        Held { port, requests: Mutex::new(requests) }
    }
}


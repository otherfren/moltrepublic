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
}


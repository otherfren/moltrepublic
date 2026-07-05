// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! Empirical probe of the SMP transport handshake: connect to the live
//! server and dump the first transport block it sends, so the handshake
//! parser is written against ground truth, not a spec summary.
//!
//! `cargo test -p molt-net --test smp_handshake_probe -- --ignored --nocapture`

use molt_net::smp::{tls, SmpServer};
use tokio::io::AsyncReadExt;

const KONKIN: &str = "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@smp.konkin.io";

#[tokio::test]
#[ignore = "live network probe"]
async fn dump_server_first_block() {
    let s = SmpServer::parse(KONKIN).expect("parse");
    let mut tls = tls::connect_tls(&s).await.expect("tls");

    // read whatever the server sends first, up to one transport block
    let mut buf = vec![0u8; 16384];
    let n = tokio::time::timeout(std::time::Duration::from_secs(8), tls.read(&mut buf))
        .await
        .expect("read timed out")
        .expect("read");
    println!("server sent {n} bytes as its first block");
    let head = &buf[..n.min(64)];
    println!("first {} bytes (hex): {}", head.len(), hex::encode(head));
    // the spec's paddedBlock = word16 length + content + '#' padding
    if n >= 2 {
        let len = u16::from_be_bytes([buf[0], buf[1]]);
        println!("word16 length prefix = {len}");
        if usize::from(len) + 2 <= n {
            let content = &buf[2..2 + usize::from(len)];
            println!("content ({} bytes) hex: {}", content.len(), hex::encode(&content[..content.len().min(48)]));
            // try to read a version range: minVer(2) maxVer(2)
            if content.len() >= 4 {
                let minv = u16::from_be_bytes([content[0], content[1]]);
                let maxv = u16::from_be_bytes([content[2], content[3]]);
                println!("candidate smpVersionRange: min={minv} max={maxv}");
            }
        }
    }
}

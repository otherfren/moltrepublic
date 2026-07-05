// SPDX-License-Identifier: GPL-3.0-or-later

//! The SMP transport connection: block framing, the handshake, and the
//! transmission layer (transport concept §3.2). Written against the live
//! server's actual bytes (see `tests/smp_handshake_probe.rs`), not a
//! summary.
//!
//! Two nested framings:
//!
//! * **Transport block** — every TLS payload is exactly one 16 384-byte
//!   block: `word16 contentLen | content | '#' padding`.
//! * **Transmission block** (content of a command block) —
//!   `count:1 | (word16 txLen | transmission)…`. A single transmission is
//!   `authorization:shortString | authorized`, where (v7+) `authorized =
//!   corrId | entityId | smpCommand` (the sessionId is no longer on the
//!   wire).
//!
//! Verified incrementally against a live server: the handshake and the
//! unsigned `PING`→`PONG` round-trip need no queue crypto, so they pin the
//! framing before `NEW` (which needs signing) is attempted.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;

use crate::smp::server::SmpServer;
use crate::smp::tls;
use crate::NetError;

/// One SMP transport block.
pub const BLOCK_LEN: usize = 16384;
/// Padding byte SMP fills blocks with.
const PAD: u8 = b'#';
/// corrId marker for a present 24-byte id.
const CORR_PRESENT: u8 = 0x18;
/// The highest SMP version this client implements. The wire transmission
/// format is stable from v7 (sessionId dropped from the wire); we cap at a
/// version we have actually exercised against a live server.
const OUR_MAX_VERSION: u16 = 12;
/// The lowest version we accept.
const OUR_MIN_VERSION: u16 = 7;

/// An open, handshaked SMP connection to one server.
pub struct SmpConn {
    tls: TlsStream<TcpStream>,
    /// The server's pinned CA fingerprint (echoed in the clientHello as
    /// the `keyHash` — the server rejects a mismatch with IDENTITY).
    key_hash: [u8; 32],
    /// Negotiated SMP version.
    pub version: u16,
    /// The 32-byte session identifier from the server handshake.
    pub session_id: Vec<u8>,
    /// Monotonic correlation counter.
    corr: u64,
}

impl SmpConn {
    /// Dial, TLS-pin, and run the SMP handshake with `server`.
    pub async fn connect(server: &SmpServer) -> Result<SmpConn, NetError> {
        let tls = tls::connect_tls(server).await?;
        let mut conn = SmpConn {
            tls,
            key_hash: server.fingerprint_raw(),
            version: 0,
            session_id: Vec::new(),
            corr: 0,
        };
        conn.handshake().await?;
        Ok(conn)
    }

    /// The SMP handshake: read `serverHello`, reply with `clientHello`.
    /// The clientHello echoes the server's identity fingerprint (verified
    /// live: v12 wants `version(2) | keyHash:shortString`).
    async fn handshake(&mut self) -> Result<(), NetError> {
        let block = self.read_block().await?;
        // serverHello = minVer(2) maxVer(2) sessionId(shortString) …
        if block.len() < 5 {
            return Err(NetError::Framing("serverHello too short".into()));
        }
        let server_min = u16::from_be_bytes([block[0], block[1]]);
        let server_max = u16::from_be_bytes([block[2], block[3]]);
        let sid_len = usize::from(block[4]);
        let sid_end = 5 + sid_len;
        if block.len() < sid_end {
            return Err(NetError::Framing("serverHello sessionId truncated".into()));
        }
        self.session_id = block[5..sid_end].to_vec();

        // negotiate: the highest version both sides support
        let version = OUR_MAX_VERSION.min(server_max);
        if version < server_min || version < OUR_MIN_VERSION {
            return Err(NetError::Crypto(format!(
                "no common SMP version (server {server_min}..{server_max}, us {OUR_MIN_VERSION}..{OUR_MAX_VERSION})"
            )));
        }
        self.version = version;

        // clientHello = smpVersion(2) keyHash [authPubKey…]. The keyHash
        // (the server's pinned CA fingerprint) is mandatory — the server
        // closes with IDENTITY otherwise. authPubKey (v7+, Maybe) is
        // omitted; the trailing fields land in the ignored padding.
        let mut hello = version.to_be_bytes().to_vec();
        push_short(&mut hello, &self.key_hash);
        self.write_block(&hello).await?;
        Ok(())
    }

    /// The unsigned keep-alive: `PING` → `PONG`. Proves the framing end to
    /// end without any queue crypto.
    pub async fn ping(&mut self) -> Result<(), NetError> {
        let corr = self.next_corr();
        let tx = encode_transmission(&[], &corr, &[], b"PING");
        self.write_block(&wrap_transmission(&tx)).await?;
        let block = self.read_block().await?;
        let (_corr, _entity, cmd) = parse_first_response(&block)?;
        if cmd == b"PONG" {
            Ok(())
        } else {
            Err(NetError::Framing(format!(
                "expected PONG, got {}",
                String::from_utf8_lossy(&cmd[..cmd.len().min(16)])
            )))
        }
    }

    fn next_corr(&mut self) -> [u8; 24] {
        self.corr += 1;
        let mut c = [0u8; 24];
        c[..8].copy_from_slice(&self.corr.to_be_bytes());
        c
    }

    /// Read one 16 384-byte block and return its `content` slice (unpadded).
    async fn read_block(&mut self) -> Result<Vec<u8>, NetError> {
        let mut buf = vec![0u8; BLOCK_LEN];
        self.tls
            .read_exact(&mut buf)
            .await
            .map_err(|e| NetError::Unreachable(format!("smp read: {e}")))?;
        let len = usize::from(u16::from_be_bytes([buf[0], buf[1]]));
        if 2 + len > BLOCK_LEN {
            return Err(NetError::Framing("block content length overflows block".into()));
        }
        Ok(buf[2..2 + len].to_vec())
    }

    /// Frame `content` into a padded 16 384-byte block and send it.
    async fn write_block(&mut self, content: &[u8]) -> Result<(), NetError> {
        if content.len() + 2 > BLOCK_LEN {
            return Err(NetError::Framing("smp content exceeds one block".into()));
        }
        let mut block = Vec::with_capacity(BLOCK_LEN);
        block.extend_from_slice(
            &u16::try_from(content.len())
                .map_err(|_| NetError::Framing("content over 64 KiB".into()))?
                .to_be_bytes(),
        );
        block.extend_from_slice(content);
        block.resize(BLOCK_LEN, PAD);
        self.tls
            .write_all(&block)
            .await
            .map_err(|e| NetError::Unreachable(format!("smp write: {e}")))?;
        self.tls
            .flush()
            .await
            .map_err(|e| NetError::Unreachable(format!("smp flush: {e}")))?;
        Ok(())
    }
}

/// Encode one transmission (v7+ wire form, sessionId not on the wire):
/// `authorization:shortString | corrId | entityId:shortString | command`.
fn encode_transmission(auth: &[u8], corr: &[u8], entity: &[u8], command: &[u8]) -> Vec<u8> {
    let mut t = Vec::new();
    push_short(&mut t, auth);
    // corrId
    if corr.is_empty() {
        t.push(0x00);
    } else {
        t.push(CORR_PRESENT);
        t.extend_from_slice(corr);
    }
    push_short(&mut t, entity);
    t.extend_from_slice(command);
    t
}

/// Wrap one transmission into a transport-block content:
/// `count:1 | word16 txLen | transmission`.
fn wrap_transmission(tx: &[u8]) -> Vec<u8> {
    let mut c = Vec::with_capacity(3 + tx.len());
    c.push(1); // one transmission
    c.extend_from_slice(&u16::try_from(tx.len()).unwrap_or(0).to_be_bytes());
    c.extend_from_slice(tx);
    c
}

/// A parsed server response transmission: `(corrId, entityId, command)`.
type Response = (Vec<u8>, Vec<u8>, Vec<u8>);

/// Parse the first transmission of a server response block into
/// `(corrId, entityId, command)`.
fn parse_first_response(content: &[u8]) -> Result<Response, NetError> {
    if content.is_empty() {
        return Err(NetError::Framing("empty response block".into()));
    }
    let count = content[0];
    if count == 0 {
        return Err(NetError::Framing("response block has no transmissions".into()));
    }
    let mut p = 1usize;
    let tx_len = usize::from(read_u16(content, &mut p)?);
    let tx = content
        .get(p..p + tx_len)
        .ok_or_else(|| NetError::Framing("response transmission truncated".into()))?;
    // response transmission = authorization(shortString, empty for server)
    //                         corrId entityId smpCommand
    let mut q = 0usize;
    let _auth = read_short(tx, &mut q)?;
    let corr = read_corr(tx, &mut q)?;
    let entity = read_short(tx, &mut q)?;
    let command = tx.get(q..).unwrap_or(&[]).to_vec();
    Ok((corr, entity, command))
}

// --- little-endian-free primitive readers/writers ---

fn push_short(out: &mut Vec<u8>, s: &[u8]) {
    out.push(u8::try_from(s.len()).unwrap_or(0));
    out.extend_from_slice(s);
}

fn read_u16(b: &[u8], p: &mut usize) -> Result<u16, NetError> {
    let v = b
        .get(*p..*p + 2)
        .ok_or_else(|| NetError::Framing("truncated word16".into()))?;
    *p += 2;
    Ok(u16::from_be_bytes([v[0], v[1]]))
}

fn read_short(b: &[u8], p: &mut usize) -> Result<Vec<u8>, NetError> {
    let len = usize::from(*b.get(*p).ok_or_else(|| NetError::Framing("truncated shortString".into()))?);
    *p += 1;
    let s = b
        .get(*p..*p + len)
        .ok_or_else(|| NetError::Framing("shortString overruns".into()))?;
    *p += len;
    Ok(s.to_vec())
}

fn read_corr(b: &[u8], p: &mut usize) -> Result<Vec<u8>, NetError> {
    let marker = *b.get(*p).ok_or_else(|| NetError::Framing("truncated corrId".into()))?;
    *p += 1;
    match marker {
        0x00 => Ok(Vec::new()),
        CORR_PRESENT => {
            let c = b
                .get(*p..*p + 24)
                .ok_or_else(|| NetError::Framing("corrId truncated".into()))?;
            *p += 24;
            Ok(c.to_vec())
        }
        other => Err(NetError::Framing(format!("bad corrId marker {other:#x}"))),
    }
}

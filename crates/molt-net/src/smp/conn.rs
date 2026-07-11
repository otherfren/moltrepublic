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

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;
use tokio_rustls::client::TlsStream;

use crate::smp::server::SmpServer;
use crate::smp::tls;
use crate::NetError;

/// One SMP transport block.
pub const BLOCK_LEN: usize = 16384;
/// Deadline for a *request/response* read or write of one transport block —
/// sized for Tor so a stalled circuit becomes a clean error, never an infinite
/// await (T4 §P5). Deliberately NOT applied to the subscription idle long-poll
/// ([`SmpConn::recv_next`]), which legitimately waits unbounded for the server
/// to push the next `MSG` (a quiet republic pushes nothing) — bounding it would
/// tear a subscription down after 30 s of normal quiet.
const BLOCK_IO_TIMEOUT: Duration = Duration::from_secs(30);
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

/// A queue created via [`SmpConn::new_queue`]: the server-assigned ids
/// and the keys the recipient keeps. `recipient_id` is used by the creator
/// (with `auth_sk`) to `SUB`/`ACK`; `sender_id` is handed to the sender to
/// `SEND` — this is exactly the SMP `QueuePair` the transport concept's
/// invite handover carries.
#[derive(Clone)]
pub struct NewQueue {
    /// The recipient-side queue id (the creator subscribes on this).
    pub recipient_id: Vec<u8>,
    /// The sender-side queue id (handed to the one sender).
    pub sender_id: Vec<u8>,
    /// The recipient's Ed25519 command-auth key.
    pub auth_sk: ed25519_dalek::SigningKey,
    /// The recipient's X25519 DH secret (raw), for decrypting the
    /// server→recipient message layer.
    pub dh_secret: [u8; 32],
    /// The server's X25519 DH public key from `IDS` — the other half of
    /// the server→recipient shared secret.
    pub server_dh: [u8; 32],
}

/// An open, handshaked SMP connection to one server.
pub struct SmpConn {
    tls: TlsStream<tls::DialStream>,
    /// The server's pinned CA fingerprint (echoed in the clientHello as
    /// the `keyHash` — the server rejects a mismatch with IDENTITY).
    key_hash: [u8; 32],
    /// Negotiated SMP version.
    pub version: u16,
    /// The 32-byte session identifier from the server handshake.
    pub session_id: Vec<u8>,
    /// Monotonic correlation counter.
    corr: u64,
    /// A response block already read (the server piggybacks the next `MSG`
    /// on an `ACK` reply), buffered for the next `recv_next`.
    pending_block: Option<Vec<u8>>,
    /// The msg id of a delivered-but-not-yet-acked message; acked lazily on
    /// the next `recv_next` (the SMP ACK/next-MSG ping-pong).
    ack_pending: Option<Vec<u8>>,
}

impl SmpConn {
    /// Dial, TLS-pin, and run the SMP handshake with `server`.
    pub async fn connect(dialer: &tls::Dialer, server: &SmpServer) -> Result<SmpConn, NetError> {
        let tls = tls::connect_tls(dialer, server).await?;
        let mut conn = SmpConn {
            tls,
            key_hash: server.fingerprint_raw(),
            version: 0,
            session_id: Vec::new(),
            corr: 0,
            pending_block: None,
            ack_pending: None,
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

    /// Create a queue on this server (`NEW`). When `subscribe` is set the
    /// server subscribes this connection to the queue immediately (`MSG`s
    /// arrive here). Returns the queue's server-assigned ids, the keys the
    /// recipient keeps, and the server's DH key. Verified against the live
    /// server (returns `IDS`).
    pub async fn new_queue(&mut self, subscribe: bool) -> Result<NewQueue, NetError> {
        let mut sk_bytes = [0u8; 32];
        getrandom::getrandom(&mut sk_bytes).map_err(|e| NetError::Crypto(e.to_string()))?;
        let auth_sk = ed25519_dalek::SigningKey::from_bytes(&sk_bytes);
        let auth_pk = auth_sk.verifying_key().to_bytes();
        let mut dh_bytes = [0u8; 32];
        getrandom::getrandom(&mut dh_bytes).map_err(|e| NetError::Crypto(e.to_string()))?;
        let dh_secret = x25519_dalek::StaticSecret::from(dh_bytes);
        let dh_pk = x25519_dalek::PublicKey::from(&dh_secret).to_bytes();

        // NEW = "NEW " authPk(SPKI,short) dhPk(SPKI,short) basicAuth("0")
        //       subscribeMode("S"|"C") sndSecure("F")
        let mut cmd = Vec::new();
        cmd.extend_from_slice(b"NEW ");
        push_short(&mut cmd, &spki_ed25519(&auth_pk));
        push_short(&mut cmd, &spki_x25519(&dh_pk));
        cmd.push(b'0'); // basicAuth: none
        cmd.push(if subscribe { b'S' } else { b'C' });
        cmd.push(b'T'); // sndSecure: the sender may secure the queue (SKEY)

        let (_corr, _entity, command) = self.send_signed(&auth_sk, &[], &cmd).await?;
        let (recipient_id, sender_id, server_dh) = parse_ids(&command)?;
        Ok(NewQueue {
            recipient_id,
            sender_id,
            auth_sk,
            dh_secret: dh_secret.to_bytes(),
            server_dh,
        })
    }

    /// Retire a queue we created (`DEL`, signed with the recipient key).
    pub async fn delete(
        &mut self,
        recipient_id: &[u8],
        auth_sk: &ed25519_dalek::SigningKey,
    ) -> Result<(), NetError> {
        let (_c, _e, resp) = self.send_signed(auth_sk, recipient_id, b"DEL").await?;
        if resp.starts_with(b"OK") {
            Ok(())
        } else {
            Err(NetError::Crypto(format!(
                "DEL rejected: {}",
                String::from_utf8_lossy(&resp[..resp.len().min(32)])
            )))
        }
    }

    /// Subscribe this connection to a queue we created (`SUB`, signed with
    /// the recipient key). Any following `recv_next` yields its messages.
    /// Call on a connection distinct from the sender's.
    pub async fn sub(
        &mut self,
        recipient_id: &[u8],
        auth_sk: &ed25519_dalek::SigningKey,
    ) -> Result<(), NetError> {
        // the SUB reply is OK (no messages) or the first MSG — buffer it
        let block = self.send_signed_raw(auth_sk, recipient_id, b"SUB").await?;
        self.pending_block = Some(block);
        Ok(())
    }

    /// Secure a queue as the **sender** (`SKEY`, v8+): assert a fresh
    /// Ed25519 sender key so the server will accept our signed `SEND`s.
    /// Returns the key to sign subsequent sends with. Needs the queue's
    /// `NEW` to have set `sndSecure`.
    pub async fn secure_as_sender(
        &mut self,
        sender_id: &[u8],
    ) -> Result<ed25519_dalek::SigningKey, NetError> {
        let mut sk_bytes = [0u8; 32];
        getrandom::getrandom(&mut sk_bytes).map_err(|e| NetError::Crypto(e.to_string()))?;
        let sk = ed25519_dalek::SigningKey::from_bytes(&sk_bytes);
        let mut cmd = Vec::new();
        cmd.extend_from_slice(b"SKEY ");
        push_short(&mut cmd, &spki_ed25519(&sk.verifying_key().to_bytes()));
        let (_c, _e, resp) = self.send_signed(&sk, sender_id, &cmd).await?;
        if resp.starts_with(b"OK") {
            Ok(sk)
        } else {
            Err(NetError::Crypto(format!(
                "SKEY rejected: {}",
                String::from_utf8_lossy(&resp[..resp.len().min(48)])
            )))
        }
    }

    /// Send one message to a queue's **sender** id, signed with the key
    /// from [`Self::secure_as_sender`] — the queue must be secured first.
    /// `body` is the opaque payload the recipient receives (our per-queue
    /// wrap rides inside it); the server wraps it in the server→recipient
    /// layer on delivery.
    pub async fn send_to(
        &mut self,
        sender_id: &[u8],
        sender_key: &ed25519_dalek::SigningKey,
        body: &[u8],
    ) -> Result<(), NetError> {
        // SEND = "SEND" SP msgFlags("F") SP Tail(body)
        let mut cmd = Vec::new();
        cmd.extend_from_slice(b"SEND ");
        cmd.push(b'F');
        cmd.push(b' ');
        cmd.extend_from_slice(body);
        let (_c, _e, resp) = self.send_signed(sender_key, sender_id, &cmd).await?;
        if resp.starts_with(b"OK") {
            Ok(())
        } else {
            Err(NetError::Crypto(format!(
                "SEND rejected: {}",
                String::from_utf8_lossy(&resp[..resp.len().min(48)])
            )))
        }
    }

    /// Receive the next message on a subscribed connection, decrypting the
    /// server→recipient layer. Returns the plaintext body (the bytes the
    /// sender sent, with the server's `timestamp|flags|SP` prefix
    /// stripped). Handles the SMP ping-pong: the previous message is
    /// `ACK`ed here (the server piggybacks the next `MSG` on the ACK
    /// reply), and bare `OK`s / pushed deliveries are followed until a
    /// message arrives.
    pub async fn recv_next(&mut self, q: &NewQueue) -> Result<Vec<u8>, NetError> {
        loop {
            // ack the previously delivered message; its reply may be the
            // next MSG (buffered) or OK
            if let Some(msg_id) = self.ack_pending.take() {
                let mut cmd = Vec::new();
                cmd.extend_from_slice(b"ACK ");
                push_short(&mut cmd, &msg_id);
                let block = self.send_signed_raw(&q.auth_sk, &q.recipient_id, &cmd).await?;
                self.pending_block = Some(block);
            }
            let block = match self.pending_block.take() {
                Some(b) => b,
                // the subscription idle wait: block until the server pushes the
                // next MSG. NO deadline — a quiet queue is normal, and the read
                // (`read_exact`) is cancel-unsafe, so a fired timeout mid-block
                // would corrupt framing. Liveness/reconnect is Stage B.
                None => self.read_block_waiting().await?,
            };
            let (_corr, _entity, command) = parse_first_response(&block)?;
            if command.starts_with(b"MSG ") {
                let mut p = 4;
                let msg_id = read_short(&command, &mut p)?;
                let ciphertext = command.get(p..).unwrap_or(&[]);
                let plaintext = crypto_box_open(&q.server_dh, &q.dh_secret, &msg_id, ciphertext)?;
                self.ack_pending = Some(msg_id);
                // rcvMsgBody = timestamp(8) msgFlags SP body, where msgFlags
                // is "F"/"T" then "take till space" — so strip the 8-byte
                // timestamp and skip to the first space, the body follows
                let after_ts = plaintext.get(8..).unwrap_or(&[]);
                let body = match after_ts.iter().position(|&b| b == b' ') {
                    Some(sp) => after_ts.get(sp + 1..).unwrap_or(&[]).to_vec(),
                    None => Vec::new(),
                };
                return Ok(body);
            }
            // OK (or "END"/"ERR") — keep waiting for a pushed MSG
            if command.starts_with(b"ERR") {
                return Err(NetError::Crypto(format!(
                    "server error during subscribe: {}",
                    String::from_utf8_lossy(&command[..command.len().min(32)])
                )));
            }
        }
    }

    /// Send a signed transmission (`entity` = queue id, empty for `NEW`)
    /// and return the parsed response. The signature covers
    /// `sessionId ++ authorized` — confirmed against the live server (the
    /// v7+ format keeps the sessionId in the *signed* bytes though it is
    /// dropped from the wire).
    async fn send_signed(
        &mut self,
        key: &ed25519_dalek::SigningKey,
        entity: &[u8],
        command: &[u8],
    ) -> Result<Response, NetError> {
        let block = self.send_signed_raw(key, entity, command).await?;
        parse_first_response(&block)
    }

    /// As [`Self::send_signed`] but returns the raw reply block content —
    /// the subscription loop buffers it because an `ACK` reply may carry
    /// the next `MSG`.
    async fn send_signed_raw(
        &mut self,
        key: &ed25519_dalek::SigningKey,
        entity: &[u8],
        command: &[u8],
    ) -> Result<Vec<u8>, NetError> {
        use ed25519_dalek::Signer;
        let corr = self.next_corr();
        // authorized (wire, v7+) = corrId | entityId | command
        let mut authorized = Vec::new();
        authorized.push(CORR_PRESENT);
        authorized.extend_from_slice(&corr);
        push_short(&mut authorized, entity);
        authorized.extend_from_slice(command);
        // signed bytes = short(sessionId) ++ authorized
        let mut signed = Vec::new();
        push_short(&mut signed, &self.session_id);
        signed.extend_from_slice(&authorized);
        let sig = key.sign(&signed).to_bytes();
        // transmission = authorization(short sig) | authorized
        let mut tx = Vec::new();
        push_short(&mut tx, &sig);
        tx.extend_from_slice(&authorized);
        self.write_block(&wrap_transmission(&tx)).await?;
        self.read_block().await
    }

    fn next_corr(&mut self) -> [u8; 24] {
        self.corr += 1;
        let mut c = [0u8; 24];
        c[..8].copy_from_slice(&self.corr.to_be_bytes());
        c
    }

    /// Read one 16 384-byte block (request/response) and return its `content`
    /// slice (unpadded), bounded by [`BLOCK_IO_TIMEOUT`].
    async fn read_block(&mut self) -> Result<Vec<u8>, NetError> {
        let mut buf = vec![0u8; BLOCK_LEN];
        timeout(BLOCK_IO_TIMEOUT, self.tls.read_exact(&mut buf))
            .await
            .map_err(|_| NetError::TorUnavailable("smp read timed out".into()))?
            .map_err(|e| NetError::Unreachable(format!("smp read: {e}")))?;
        parse_block_content(&buf)
    }

    /// Read one block with NO deadline — the subscription idle long-poll
    /// (see [`Self::recv_next`]). A quiet subscribed queue may push nothing for
    /// minutes; a fatal deadline here would silently kill the subscription and
    /// make the node deaf (breaks recovery / runtime delivery / late joins,
    /// clearnet included). This restores the pre-T4 idle-read behaviour.
    async fn read_block_waiting(&mut self) -> Result<Vec<u8>, NetError> {
        let mut buf = vec![0u8; BLOCK_LEN];
        self.tls
            .read_exact(&mut buf)
            .await
            .map_err(|e| NetError::Unreachable(format!("smp read: {e}")))?;
        parse_block_content(&buf)
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
        timeout(BLOCK_IO_TIMEOUT, self.tls.write_all(&block))
            .await
            .map_err(|_| NetError::TorUnavailable("smp write timed out".into()))?
            .map_err(|e| NetError::Unreachable(format!("smp write: {e}")))?;
        timeout(BLOCK_IO_TIMEOUT, self.tls.flush())
            .await
            .map_err(|_| NetError::TorUnavailable("smp flush timed out".into()))?
            .map_err(|e| NetError::Unreachable(format!("smp flush: {e}")))?;
        Ok(())
    }
}

/// Extract the unpadded `content` slice from one raw 16 384-byte block:
/// a big-endian `u16` length prefix followed by that many content bytes.
fn parse_block_content(buf: &[u8]) -> Result<Vec<u8>, NetError> {
    let len = usize::from(u16::from_be_bytes([buf[0], buf[1]]));
    if 2 + len > BLOCK_LEN {
        return Err(NetError::Framing("block content length overflows block".into()));
    }
    Ok(buf[2..2 + len].to_vec())
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

/// A parsed `IDS`: `(recipientId, senderId, serverDhKey)`.
type Ids = (Vec<u8>, Vec<u8>, [u8; 32]);

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

/// SubjectPublicKeyInfo DER for an Ed25519 public key (RFC 8410): the
/// fixed 12-byte prefix (SEQUENCE, AlgorithmIdentifier OID 1.3.101.112,
/// BIT STRING) + the 32-byte key.
fn spki_ed25519(pk: &[u8; 32]) -> Vec<u8> {
    let mut v = vec![
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    v.extend_from_slice(pk);
    v
}

/// SPKI DER for an X25519 public key (OID 1.3.101.110).
fn spki_x25519(pk: &[u8; 32]) -> Vec<u8> {
    let mut v = vec![
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x6e, 0x03, 0x21, 0x00,
    ];
    v.extend_from_slice(pk);
    v
}

/// Parse an `IDS` response into `(recipientId, senderId, serverDhKey)`, or
/// map a server `ERR` into a [`NetError`]. `IDS = "IDS " rid sid
/// srvDhPub(SPKI,short) sndSecure`.
fn parse_ids(command: &[u8]) -> Result<Ids, NetError> {
    if command.starts_with(b"IDS ") {
        let mut p = 4;
        let rid = read_short(command, &mut p)?;
        let sid = read_short(command, &mut p)?;
        let srv_dh_spki = read_short(command, &mut p)?;
        // the X25519 public key is the last 32 bytes of the SPKI DER
        let server_dh = <[u8; 32]>::try_from(
            srv_dh_spki
                .get(srv_dh_spki.len().saturating_sub(32)..)
                .unwrap_or(&[]),
        )
        .map_err(|_| NetError::Framing("IDS server DH key is not 32 bytes".into()))?;
        Ok((rid, sid, server_dh))
    } else {
        Err(NetError::Crypto(format!(
            "NEW not accepted: {}",
            String::from_utf8_lossy(&command[..command.len().min(48)])
        )))
    }
}

/// NaCl crypto_box open (XSalsa20-Poly1305) of the server→recipient
/// message layer: shared secret = DH(recipient_dh, server_dh), nonce =
/// msgId.
fn crypto_box_open(
    server_dh: &[u8; 32],
    recipient_dh: &[u8; 32],
    nonce: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>, NetError> {
    use crypto_box::aead::Aead;
    let nonce = <[u8; 24]>::try_from(nonce)
        .map_err(|_| NetError::Framing("MSG nonce is not 24 bytes".into()))?;
    let their_pk = crypto_box::PublicKey::from(*server_dh);
    let my_sk = crypto_box::SecretKey::from(*recipient_dh);
    crypto_box::SalsaBox::new(&their_pk, &my_sk)
        .decrypt(&nonce.into(), ciphertext)
        .map_err(|_| NetError::Crypto("MSG decryption failed".into()))
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

#[cfg(test)]
mod parse_fuzz {
    use super::*;

    /// Untrusted server input (concept §7): the SMP response + IDS parsers must
    /// never panic on arbitrary bytes — every read is bounds-checked, this is the
    /// regression guard. 30 000 pseudo-random inputs, incl. valid-prefix + garbage.
    #[test]
    fn response_and_ids_parsers_never_panic_on_hostile_bytes() {
        let mut rng: u64 = 0xdead_beef_cafe_1234;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let byte = |v: u64| v.to_le_bytes()[0];
        for _ in 0..30_000 {
            let len = usize::try_from(next() % 320).unwrap_or(0);
            let bytes: Vec<u8> = (0..len).map(|_| byte(next())).collect();
            let _ = parse_first_response(&bytes);
            let _ = parse_ids(&bytes);
            // a well-formed command prefix with a garbage tail exercises the
            // deeper field readers, not just the leading length guard
            for prefix in [b"IDS ".as_slice(), b"MSG ".as_slice(), b"ERR ".as_slice()] {
                let mut framed = prefix.to_vec();
                let tail = next() % 96;
                framed.extend((0..tail).map(|_| byte(next())));
                let _ = parse_ids(&framed);
                let _ = parse_first_response(&framed);
            }
        }
    }
}

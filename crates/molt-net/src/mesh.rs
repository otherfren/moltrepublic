// SPDX-License-Identifier: GPL-3.0-or-later

//! Runtime full-mesh bootstrap (transport concept §3.2/§3.3, increment 2b).
//!
//! After founding, each node opens one **per-pair inbound queue** for every
//! peer (per-pair = unlinkability: a server hosting two of our queues cannot
//! see them as one group) and broadcasts a [`MeshAnnounce`] — for each peer,
//! the queue that peer should send to, plus its wrap key — to the group *in
//! band over MLS*. Once a node holds every peer's announcement it
//! [`assemble_mesh`]s its full-mesh [`PeerLink`]s and hands them to a runtime
//! supervisor. The founding star seeds this exchange (a temporary founder
//! relay) and is dropped once the direct mesh is up.
//!
//! This module is the pure protocol core (announcement wire type + assembly
//! logic); the transport plumbing that carries the announcements and the engine
//! open-path that persists/rebuilds the mesh live above it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use molt_core::MemberId;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::mls::{MlsIncoming, MlsMember};
use crate::supervisor::PeerLink;
use crate::wrap::WrapKey;
use crate::{QueueId, RcvQueue, SndQueueAddr, Transport};

/// One queue handover: a queue's address and its wrap key, as lowercase hex.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueHandover {
    /// The queue's server (`smp://fingerprint@host`; empty for the loopback hub).
    pub server: String,
    /// The queue id, lowercase hex.
    pub queue: String,
    /// The queue's wrap key, lowercase hex.
    pub wrap: String,
}

impl QueueHandover {
    /// Build a handover from a send address + wrap key (an announcer describing
    /// one of its own inbound queues).
    pub fn of(addr: &SndQueueAddr, wrap: &WrapKey) -> QueueHandover {
        QueueHandover {
            server: addr.server.clone(),
            queue: hex::encode(&addr.id.0),
            wrap: hex::encode(wrap.to_bytes()),
        }
    }

    /// The send address this handover points at. `None` on malformed hex.
    pub fn addr(&self) -> Option<SndQueueAddr> {
        Some(SndQueueAddr {
            server: self.server.clone(),
            id: QueueId::from_bytes(hex::decode(&self.queue).ok()?),
        })
    }

    /// The wrap key. `None` on malformed hex / wrong length.
    pub fn wrap_key(&self) -> Option<WrapKey> {
        let b: [u8; 32] = hex::decode(&self.wrap).ok()?.try_into().ok()?;
        Some(WrapKey::from_bytes(b))
    }
}

/// A node's mesh announcement, broadcast to the group in-band over MLS. For each
/// peer, the inbound queue **that peer** should send to (to reach the announcer)
/// and its wrap key. The announcer created one queue per peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeshAnnounce {
    /// peer handle → the queue that peer sends to (to reach the announcer).
    pub queues: BTreeMap<MemberId, QueueHandover>,
}

/// Assemble this node's full-mesh [`PeerLink`]s. `me` is this node's handle;
/// `my_inbound` are the inbound queues it created for each peer (peer → the
/// [`RcvQueue`] it receives on from that peer, and that queue's wrap key);
/// `announces` are the announcements received from peers (peer → their
/// [`MeshAnnounce`]). Returns one link per entry of `my_inbound`, or an error
/// naming the first peer whose handover is missing/malformed.
pub fn assemble_mesh(
    me: &str,
    my_inbound: &BTreeMap<MemberId, (RcvQueue, WrapKey)>,
    announces: &BTreeMap<MemberId, MeshAnnounce>,
) -> Result<Vec<PeerLink>, String> {
    let mut links = Vec::with_capacity(my_inbound.len());
    for (peer, (rcv, rcv_wrap)) in my_inbound {
        // where I send to reach `peer`: the queue `peer` announced for me
        let announce = announces
            .get(peer)
            .ok_or_else(|| format!("no mesh announcement from {peer}"))?;
        let target = announce
            .queues
            .get(me)
            .ok_or_else(|| format!("{peer}'s announcement carries no queue for {me}"))?;
        let snd = target
            .addr()
            .ok_or_else(|| format!("{peer}'s queue for {me} is malformed"))?;
        let wrap_out = target
            .wrap_key()
            .ok_or_else(|| format!("{peer}'s wrap key for {me} is malformed"))?;
        links.push(PeerLink {
            member: peer.clone(),
            snd,
            wrap_out,
            rcv: rcv.clone(),
            wrap_in: rcv_wrap.clone(),
        });
    }
    Ok(links)
}

/// Drive one node's mesh bootstrap over `transport`: open one inbound queue per
/// peer (fresh wrap key each), broadcast our [`MeshAnnounce`] on `announce_out`,
/// collect every peer's on `announce_in`, then [`assemble_mesh`] the full-mesh
/// links. The caller wires `announce_out`/`announce_in` to the group channel
/// (MLS over the founding star, founder-relayed); this function owns only queue
/// creation + assembly.
pub async fn bootstrap_mesh<T: Transport>(
    me: &str,
    peers: &[MemberId],
    transport: &T,
    announce_out: tokio::sync::mpsc::Sender<MeshAnnounce>,
    mut announce_in: tokio::sync::mpsc::Receiver<(MemberId, MeshAnnounce)>,
    timeout: std::time::Duration,
) -> Result<Vec<PeerLink>, String> {
    // one per-pair inbound queue per peer (per-pair = unlinkability)
    let mut my_inbound: BTreeMap<MemberId, (RcvQueue, WrapKey)> = BTreeMap::new();
    let mut queues: BTreeMap<MemberId, QueueHandover> = BTreeMap::new();
    for p in peers {
        let pair = transport.create_queue().await.map_err(|e| e.to_string())?;
        let wrap = WrapKey::fresh().map_err(|e| e.to_string())?;
        // the peer sends to pair.snd (wrapped with `wrap`); I receive on pair.rcv
        queues.insert(p.clone(), QueueHandover::of(&pair.snd, &wrap));
        my_inbound.insert(p.clone(), (pair.rcv, wrap));
    }
    // broadcast my handovers, then wait (up to `timeout` total) until every peer
    // has announced theirs — the bootstrap is best-effort, so a peer that never
    // shows up bounds the wait instead of hanging entry forever
    announce_out
        .send(MeshAnnounce { queues })
        .await
        .map_err(|_| "mesh announce channel closed".to_string())?;
    let deadline = tokio::time::Instant::now() + timeout;
    let mut announces: BTreeMap<MemberId, MeshAnnounce> = BTreeMap::new();
    while announces.len() < peers.len() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, announce_in.recv()).await {
            Ok(Some((from, a))) => {
                announces.insert(from, a);
            }
            Ok(None) => {
                return Err("mesh bootstrap channel closed before every peer announced".to_string())
            }
            Err(_) => {
                return Err(format!(
                    "mesh bootstrap timed out: {}/{} peers announced",
                    announces.len(),
                    peers.len()
                ))
            }
        }
    }
    assemble_mesh(me, &my_inbound, &announces)
}

/// Bootstrap the mesh with the announcements carried as **MLS ciphertext** over
/// a raw-ciphertext channel (the founding star, founder-relayed). Wraps
/// [`bootstrap_mesh`]: our [`MeshAnnounce`] is MLS-encrypted before it leaves
/// on `out_ct`; each ciphertext arriving on `in_ct` is MLS-decrypted to its
/// **authenticated** sender + announcement. Shares the group `mls` with the
/// runtime supervisor (same ratchet, used in sequence). The caller wires
/// `out_ct`/`in_ct` to the actual star queues.
pub async fn bootstrap_over_mls<T: Transport>(
    me: &str,
    peers: &[MemberId],
    transport: &T,
    mls: Arc<Mutex<MlsMember>>,
    out_ct: mpsc::Sender<Vec<u8>>,
    mut in_ct: mpsc::Receiver<Vec<u8>>,
    timeout: std::time::Duration,
) -> Result<Vec<PeerLink>, String> {
    let cap = peers.len().max(1);
    let (ann_out, mut ann_out_rx) = mpsc::channel::<MeshAnnounce>(cap);
    let (ann_in_tx, ann_in_rx) = mpsc::channel::<(MemberId, MeshAnnounce)>(cap);

    // encrypt our outgoing announcement(s)
    let enc_mls = mls.clone();
    let enc = tokio::spawn(async move {
        while let Some(a) = ann_out_rx.recv().await {
            let Ok(bytes) = serde_json::to_vec(&a) else {
                continue;
            };
            let ct = enc_mls.lock().ok().and_then(|mut m| m.encrypt(&bytes).ok());
            if let Some(ct) = ct {
                if out_ct.send(ct).await.is_err() {
                    break;
                }
            }
        }
    });

    // decrypt incoming announcements — the sender is MLS-authenticated. A
    // ciphertext that cannot become an announcement is dropped, but LOUDLY:
    // a silent drop here starves the bootstrap into its timeout with no trace.
    let dec_mls = mls.clone();
    let dec = tokio::spawn(async move {
        while let Some(ct) = in_ct.recv().await {
            let decrypted = match dec_mls.lock() {
                Ok(mut m) => m.decrypt(&ct),
                Err(_) => {
                    tracing::warn!("mesh bootstrap: mls lock poisoned — announcement dropped");
                    continue;
                }
            };
            let got = match decrypted {
                Ok(MlsIncoming::Application { from, plaintext }) => {
                    match serde_json::from_slice::<MeshAnnounce>(&plaintext) {
                        Ok(a) => Some((from, a)),
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "mesh bootstrap: announcement did not parse — dropped"
                            );
                            None
                        }
                    }
                }
                Ok(other) => {
                    let kind = match other {
                        MlsIncoming::Application { .. } => "application",
                        MlsIncoming::Commit => "commit",
                        MlsIncoming::Proposal => "proposal",
                        MlsIncoming::FutureEpoch => "future-epoch",
                    };
                    tracing::warn!(kind, "mesh bootstrap: non-application MLS message — dropped");
                    None
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "mesh bootstrap: announcement failed to decrypt — dropped"
                    );
                    None
                }
            };
            if let Some(pair) = got {
                if ann_in_tx.send(pair).await.is_err() {
                    break;
                }
            }
        }
    });

    let links = bootstrap_mesh(me, peers, transport, ann_out, ann_in_rx, timeout).await;
    // DRAIN, don't abort, the encrypt task: `bootstrap_mesh` returns as soon as
    // every *inbound* announcement is in, but our OWN announcement may still be
    // sitting in `ann_out` un-encrypted. `bootstrap_mesh` has dropped `ann_out`,
    // so the task ends by itself once it flushes that last item into `out_ct` —
    // awaiting it guarantees our announcement reaches the wire before the caller
    // tears down its send task (otherwise a peer waits for it forever).
    let _ = enc.await;
    dec.abort();
    links
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snd(server: &str, id: &[u8]) -> SndQueueAddr {
        SndQueueAddr {
            server: server.to_string(),
            id: QueueId::from_bytes(id.to_vec()),
        }
    }

    #[test]
    fn queue_handover_round_trips() {
        let addr = snd("smp://fp@host", &[1, 2, 3, 4]);
        let wrap = WrapKey::from_bytes([9u8; 32]);
        let h = QueueHandover::of(&addr, &wrap);
        assert_eq!(h.addr().expect("addr").id.0, addr.id.0);
        assert_eq!(h.addr().expect("addr").server, "smp://fp@host");
        assert_eq!(h.wrap_key().expect("wrap").to_bytes(), wrap.to_bytes());
    }

    #[test]
    fn mesh_announce_round_trips_over_json() {
        let mut queues = BTreeMap::new();
        queues.insert(
            "bob".to_string(),
            QueueHandover::of(&snd("", &[7, 7]), &WrapKey::from_bytes([1u8; 32])),
        );
        let a = MeshAnnounce { queues };
        let wire = serde_json::to_vec(&a).expect("encode");
        let back: MeshAnnounce = serde_json::from_slice(&wire).expect("decode");
        assert_eq!(back, a);
    }

    /// The heart of the bootstrap: from my own inbound queues + the peers'
    /// announcements, I get exactly one link per peer, wired the right way —
    /// I SEND to the queue the peer announced for me, and RECEIVE on the queue
    /// I created for that peer.
    #[test]
    fn assemble_builds_one_correctly_wired_link_per_peer() {
        // I am "alice"; my peers are bob and cara.
        let mut my_inbound = BTreeMap::new();
        my_inbound.insert(
            "bob".to_string(),
            (RcvQueue { id: QueueId::from_bytes(vec![0xa, 0xb]) }, WrapKey::from_bytes([10u8; 32])),
        );
        my_inbound.insert(
            "cara".to_string(),
            (RcvQueue { id: QueueId::from_bytes(vec![0xc, 0xd]) }, WrapKey::from_bytes([20u8; 32])),
        );

        // bob's announcement includes the queue alice should send to.
        let mut bob_q = BTreeMap::new();
        bob_q.insert(
            "alice".to_string(),
            QueueHandover::of(&snd("smp://b@srv", &[0xb, 0x1]), &WrapKey::from_bytes([11u8; 32])),
        );
        bob_q.insert(
            "cara".to_string(),
            QueueHandover::of(&snd("smp://b@srv", &[0xb, 0x2]), &WrapKey::from_bytes([12u8; 32])),
        );
        // cara's announcement likewise.
        let mut cara_q = BTreeMap::new();
        cara_q.insert(
            "alice".to_string(),
            QueueHandover::of(&snd("smp://c@srv", &[0xc, 0x1]), &WrapKey::from_bytes([21u8; 32])),
        );

        let mut announces = BTreeMap::new();
        announces.insert("bob".to_string(), MeshAnnounce { queues: bob_q });
        announces.insert("cara".to_string(), MeshAnnounce { queues: cara_q });

        let mut links = assemble_mesh("alice", &my_inbound, &announces).expect("assembles");
        links.sort_by(|a, b| a.member.cmp(&b.member));
        assert_eq!(links.len(), 2);

        let bob = &links[0];
        assert_eq!(bob.member, "bob");
        // send to the queue bob announced FOR alice
        assert_eq!(bob.snd.id.0, vec![0xb, 0x1]);
        assert_eq!(bob.snd.server, "smp://b@srv");
        assert_eq!(bob.wrap_out.to_bytes(), [11u8; 32]);
        // receive on the queue alice created FOR bob
        assert_eq!(bob.rcv.id.0, vec![0xa, 0xb]);
        assert_eq!(bob.wrap_in.to_bytes(), [10u8; 32]);

        let cara = &links[1];
        assert_eq!(cara.member, "cara");
        assert_eq!(cara.snd.id.0, vec![0xc, 0x1]);
        assert_eq!(cara.rcv.id.0, vec![0xc, 0xd]);
    }

    #[test]
    fn assemble_errors_when_a_peer_never_announced() {
        let mut my_inbound = BTreeMap::new();
        my_inbound.insert(
            "bob".to_string(),
            (RcvQueue { id: QueueId::from_bytes(vec![1]) }, WrapKey::from_bytes([1u8; 32])),
        );
        // no announcement from bob at all
        let announces = BTreeMap::new();
        assert!(assemble_mesh("alice", &my_inbound, &announces).is_err());
    }

    #[test]
    fn assemble_errors_when_an_announcement_omits_me() {
        let mut my_inbound = BTreeMap::new();
        my_inbound.insert(
            "bob".to_string(),
            (RcvQueue { id: QueueId::from_bytes(vec![1]) }, WrapKey::from_bytes([1u8; 32])),
        );
        // bob announced, but not a queue for alice
        let mut bob_q = BTreeMap::new();
        bob_q.insert(
            "cara".to_string(),
            QueueHandover::of(&snd("", &[9]), &WrapKey::from_bytes([9u8; 32])),
        );
        let mut announces = BTreeMap::new();
        announces.insert("bob".to_string(), MeshAnnounce { queues: bob_q });
        assert!(assemble_mesh("alice", &my_inbound, &announces).is_err());
    }
}

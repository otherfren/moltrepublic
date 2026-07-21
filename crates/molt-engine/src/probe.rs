// SPDX-License-Identifier: GPL-3.0-or-later
//! **Mesh transport probe — diagnostics only.**
//!
//! A workspace that reopens *deaf* (peers can't find each other, the banner
//! spins "reconnecting") has two very different possible causes, and the fix
//! differs completely:
//!
//! 1. the SMP server deleted our inbound queue (real idle-expiry), or
//! 2. the queue is still alive on the server (with our peers' messages waiting)
//!    and moltrepublic simply fails to *receive* from it on resume — a
//!    moltrepublic bug, not the server.
//!
//! The default SMP server does NOT delete idle queues (only messages expire,
//! after 21 days), which makes (2) the more likely culprit — but that must be
//! *measured*, not assumed. This probe measures it, bypassing the MLS/mesh layer
//! entirely. It runs ONLY when `MOLT_MESH_PROBE` is set, and when it runs it
//! REPLACES the real mesh for that session (SMP allows one subscription per
//! queue), so the workspace is offline-for-diagnostics. For each mesh leg it:
//!
//! - subscribes to THIS node's inbound queue — `SUB → Ok` means the queue is
//!   alive on the server, `Err` means it is gone (expiry for that leg);
//! - sends a marked raw frame to the PEER's inbound queue;
//! - listens: receiving the peer's marker (run the peer in probe mode too, or
//!   catch its real send-retries) proves the queue DELIVERS end-to-end — so the
//!   deafness is ABOVE the transport (resume/mesh wiring), not the server.
//!
//! All output is `tracing` at INFO under target `molt_mesh_probe`. Run e.g.:
//! `MOLT_MESH_PROBE=1 RUST_LOG=molt_mesh_probe=info <moltd …>` and open the deaf
//! workspace on BOTH nodes.

use std::time::Duration;

use molt_core::{MemberId, MeshLink};
use molt_net::supervisor::{self, PeerLink};
use molt_net::{MsgId, Transport};

use crate::founding::RitualTransport;

/// How long each leg listens for an inbound frame after sending its probe.
const PROBE_LISTEN: Duration = Duration::from_secs(20);

/// The marker payload prefix — a received frame starting with this is another
/// node's probe (proof the queue delivers), not real MLS traffic.
const MARKER: &[u8] = b"MOLT-MESH-PROBE:";

/// The measured state of one leg's inbound queue.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LegVerdict {
    /// `SUB → Err`: the queue is gone (server-side expiry/deletion).
    QueueGone,
    /// `SUB → Ok` and a frame was delivered — the queue works end-to-end, so
    /// the deafness is above the transport, not the server.
    AliveDelivering,
    /// `SUB → Ok` but nothing arrived in the window — the peer sent nothing, or
    /// a queue-id split (compare ids across the two nodes' logs).
    AliveButSilent,
}

/// Whether the mesh probe is armed (env `MOLT_MESH_PROBE` set to anything).
pub(crate) fn armed() -> bool {
    std::env::var_os("MOLT_MESH_PROBE").is_some()
}

/// Spawn the diagnostic probe over `mesh` using `transport` (which already
/// carries the reopened queue credentials). Best-effort, off the actor.
pub(crate) fn spawn_mesh_probe(transport: RitualTransport, mesh: Vec<MeshLink>, me: MemberId) {
    let legs: Vec<PeerLink> = mesh.iter().filter_map(PeerLink::from_mesh).collect();
    let server = mesh
        .iter()
        .map(|l| l.snd_server.trim().to_string())
        .find(|s| !s.is_empty())
        .unwrap_or_default();
    tracing::info!(
        target: "molt_mesh_probe",
        me = %me, legs = legs.len(), server = %server,
        "MESH PROBE START — diagnostics only; the real mesh is NOT running this session"
    );
    for peer in legs {
        let transport = transport.clone();
        let me = me.clone();
        tokio::spawn(async move {
            probe_leg(transport, peer, me, PROBE_LISTEN).await;
        });
    }
}

async fn probe_leg(
    transport: RitualTransport,
    peer: PeerLink,
    me: MemberId,
    listen: Duration,
) -> LegVerdict {
    let rcv_id = peer.rcv.id.to_string();
    let snd_id = peer.snd.id.to_string();

    // (1) does OUR inbound queue still exist on the server? SUB → Ok vs Err.
    // A `SUB → Ok` on a deaf leg DISPROVES server-side expiry (an expired/deleted
    // queue answers Err, not Ok).
    let mut rx = match transport.subscribe(&peer.rcv).await {
        Ok(rx) => {
            tracing::info!(
                target: "molt_mesh_probe",
                %me, peer = %peer.member, rcv_id = %rcv_id, snd_id = %snd_id,
                "SUB → OK — our inbound queue for this leg is ALIVE on the server (NOT expired)"
            );
            rx
        }
        Err(e) => {
            tracing::info!(
                target: "molt_mesh_probe",
                %me, peer = %peer.member, rcv_id = %rcv_id, snd_id = %snd_id, error = %e,
                "SUB → ERR — our inbound queue for this leg is GONE (server-side expiry/deletion)"
            );
            return LegVerdict::QueueGone;
        }
    };

    // (2) send a marked raw frame to the PEER's inbound (the address we send to)
    let mut marker = MARKER.to_vec();
    marker.extend_from_slice(me.as_bytes());
    // a single diagnostic frame needs no collision-free id; ignore the
    // (effectively impossible) RNG failure rather than abort the probe
    let mut idb = [0u8; 16];
    let _ = getrandom::getrandom(&mut idb);
    match supervisor::send_framed(&transport, &peer.snd, &peer.wrap_out, MsgId(idb), &marker).await {
        Ok(()) => tracing::info!(
            target: "molt_mesh_probe", %me, peer = %peer.member,
            "SEND → OK — a marked probe frame was accepted for the peer's inbound"
        ),
        Err(e) => tracing::info!(
            target: "molt_mesh_probe", %me, peer = %peer.member, error = %e,
            "SEND → ERR — could not send to the peer's inbound queue"
        ),
    }

    // (3) listen — receiving the peer's marker proves the queue DELIVERS
    let deadline = tokio::time::Instant::now() + listen;
    let mut got_probe = false;
    let mut got_other: u32 = 0;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(d)) => {
                match molt_net::wrap::unwrap_block(&peer.wrap_in, &d.block) {
                    Ok(bytes) if bytes.starts_with(MARKER) => {
                        got_probe = true;
                        tracing::info!(
                            target: "molt_mesh_probe", %me, peer = %peer.member,
                            from = %String::from_utf8_lossy(&bytes),
                            "RECV → the peer's PROBE frame arrived — the queue DELIVERS end-to-end"
                        );
                    }
                    _ => got_other += 1,
                }
                d.ack.ack();
            }
            _ => break,
        }
    }

    // (4) per-leg verdict
    if got_probe {
        tracing::info!(
            target: "molt_mesh_probe", %me, peer = %peer.member,
            "VERDICT: queue ALIVE + DELIVERING — the deafness is ABOVE the transport \
             (MLS/mesh/resume wiring), NOT server expiry"
        );
        LegVerdict::AliveDelivering
    } else if got_other > 0 {
        tracing::info!(
            target: "molt_mesh_probe", %me, peer = %peer.member, frames = got_other,
            "VERDICT: queue ALIVE + delivering real traffic (peer's probe not seen — is the \
             PEER also in probe mode?); NOT server expiry"
        );
        LegVerdict::AliveDelivering
    } else {
        tracing::info!(
            target: "molt_mesh_probe", %me, peer = %peer.member, rcv_id = %rcv_id, snd_id = %snd_id,
            "VERDICT: SUB OK but NOTHING delivered in the window — either the peer sent nothing \
             (run it in probe mode too), or a queue-id SPLIT: compare THIS leg's snd_id against \
             the PEER's rcv_id in its log — they must be equal"
        );
        LegVerdict::AliveButSilent
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use molt_net::loopback::LoopbackHub;
    use molt_net::WrapKey;

    /// Build a self-loop PeerLink over one loopback queue: sending to `snd`
    /// delivers to `rcv`, so the probe hears its own marker — the "alive +
    /// delivering" shape. Returns the hub (to optionally expire the queue) and
    /// the leg.
    async fn self_loop_leg() -> (LoopbackHub, molt_net::loopback::LoopbackTransport, PeerLink) {
        let hub = LoopbackHub::calm();
        let t = hub.transport();
        let pair = t.create_queue().await.expect("create queue");
        let wk = WrapKey::fresh().expect("wrap key");
        let leg = PeerLink {
            member: "bob".to_string(),
            snd: pair.snd,
            wrap_out: wk.clone(),
            rcv: pair.rcv,
            wrap_in: wk,
        };
        (hub, t, leg)
    }

    /// A live queue delivers the probe's own marker → `AliveDelivering`. This is
    /// the shape that DISPROVES server expiry (the queue works end-to-end).
    #[tokio::test]
    async fn a_live_queue_reads_alive_delivering() {
        let (_hub, t, leg) = self_loop_leg().await;
        let verdict = probe_leg(
            RitualTransport::Loopback(t),
            leg,
            "ada".to_string(),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(verdict, LegVerdict::AliveDelivering);
    }

    /// The idle-expiry SHAPE (loopback `expire_queue`: SUB/SEND still Ok but
    /// deliveries are dropped) → `AliveButSilent`: the queue answers SUB but
    /// delivers nothing — exactly the deaf-but-not-gone state to distinguish
    /// from a truly deleted queue.
    #[tokio::test]
    async fn an_expired_queue_reads_alive_but_silent() {
        let (hub, t, leg) = self_loop_leg().await;
        assert!(hub.expire_queue(&leg.rcv.id), "the queue exists to expire");
        let verdict = probe_leg(
            RitualTransport::Loopback(t),
            leg,
            "ada".to_string(),
            Duration::from_millis(300),
        )
        .await;
        assert_eq!(verdict, LegVerdict::AliveButSilent);
    }
}

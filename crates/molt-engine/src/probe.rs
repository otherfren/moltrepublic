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
    /// `SUB → Ok` and a frame arrived that UNWRAPPED (our probe marker, or a
    /// real waiting message) — the queue AND our wrap key both work, so the
    /// deafness is above the transport, not the server.
    AliveDelivering,
    /// `SUB → Ok`, frames DO arrive, but our resumed wrap key cannot open them —
    /// a wrap-key mismatch surviving resume. The transport delivers; we can't
    /// read it. A moltrepublic bug, not the server.
    AliveWrapMismatch,
    /// `SUB → Ok` but nothing arrived in the window — the peer sent nothing.
    /// The queue is alive (not expired), just quiet.
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
    // Stage 2: a leg may have N redundant queues; the diagnostic probes the
    // primary (index 0). (Per-queue probing is a Stage-2b refinement.)
    let rcv0 = peer.rcv0();
    let snd0 = peer.snd0();
    let rcv_id = rcv0.id.to_string();
    let snd_id = snd0.id.to_string();

    // (1) does OUR inbound queue still exist on the server? SUB → Ok vs Err.
    // A `SUB → Ok` on a deaf leg DISPROVES server-side expiry (an expired/deleted
    // queue answers Err, not Ok).
    let mut rx = match transport.subscribe(rcv0).await {
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
    match supervisor::send_framed(&transport, snd0, &peer.wrap_out, MsgId(idb), &marker).await {
        Ok(()) => tracing::info!(
            target: "molt_mesh_probe", %me, peer = %peer.member,
            "SEND → OK — a marked probe frame was accepted for the peer's inbound"
        ),
        Err(e) => tracing::info!(
            target: "molt_mesh_probe", %me, peer = %peer.member, error = %e,
            "SEND → ERR — could not send to the peer's inbound queue"
        ),
    }

    // (3) listen for ONE arriving frame and classify it: our own probe marker,
    // a real frame that UNWRAPS with our key (a waiting message), or a frame our
    // key can't open (a wrap-key mismatch). Any arrival already proves the leg
    // delivers, so we break on the first.
    //
    // CRITICAL (audit finding #2): the probe NEVER acks. Acking deletes a frame
    // from the server, so a diagnostic run on a live workspace would silently
    // destroy a user's waiting messages before the real mesh ever opens. We only
    // OBSERVE — every received frame is left un-acked and redelivers to the real
    // mesh on the next normal open (a real SMP server blocks the next delivery
    // until the current one is acked anyway, so one observed frame is the most a
    // read-only probe can see). The marker is chunk-framed by `send_framed`, so
    // it is detected by CONTAINS, not a prefix match.
    let deadline = tokio::time::Instant::now() + listen;
    let mut got_probe = false;
    let mut backlog = false; // unwrapped OK, not our marker — a real waiting frame
    let mut undecryptable = false; // arrived but our wrap key could not open it
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if let Ok(Some(d)) = tokio::time::timeout(remaining, rx.recv()).await {
        match molt_net::wrap::unwrap_block(&peer.wrap_in, &d.block) {
            Ok(bytes) if bytes.windows(MARKER.len()).any(|w| w == MARKER) => {
                got_probe = true;
                tracing::info!(
                    target: "molt_mesh_probe", %me, peer = %peer.member,
                    "RECV → a PROBE marker arrived and UNWRAPPED — queue delivers AND our wrap key matches"
                );
            }
            Ok(_) => {
                backlog = true;
                tracing::info!(
                    target: "molt_mesh_probe", %me, peer = %peer.member,
                    "RECV → a real frame arrived and UNWRAPPED (not a probe) — a waiting message the transport delivered"
                );
            }
            Err(_) => {
                undecryptable = true;
                tracing::info!(
                    target: "molt_mesh_probe", %me, peer = %peer.member,
                    "RECV → a frame arrived but our wrap key could NOT open it — a WRAP-KEY MISMATCH (the frame is on the queue; our resumed key is wrong)"
                );
            }
        }
        // d dropped here WITHOUT ack — the frame stays on the server.
    }

    // (4) per-leg verdict
    if got_probe || backlog {
        tracing::info!(
            target: "molt_mesh_probe", %me, peer = %peer.member, got_probe, backlog,
            "VERDICT: queue ALIVE + DELIVERING (frames unwrap with our key) — the deafness is \
             ABOVE the transport, NOT server expiry"
        );
        LegVerdict::AliveDelivering
    } else if undecryptable {
        tracing::info!(
            target: "molt_mesh_probe", %me, peer = %peer.member, undecryptable,
            "VERDICT: queue ALIVE, frames ARRIVE but our wrap key can't open them — a WRAP-KEY \
             MISMATCH surviving resume (our bug, above the transport, NOT the server)"
        );
        LegVerdict::AliveWrapMismatch
    } else {
        tracing::info!(
            target: "molt_mesh_probe", %me, peer = %peer.member, rcv_id = %rcv_id, snd_id = %snd_id,
            "VERDICT: SUB OK but NOTHING arrived in the window — the queue is ALIVE (not expired), \
             the peer just sent nothing"
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
            snds: vec![pair.snd],
            wrap_out: wk.clone(),
            rcvs: vec![pair.rcv],
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

    /// Audit finding #2 (data-loss footgun): a diagnostic run must NEVER
    /// consume a real waiting message. A non-marker frame proves the leg
    /// delivers (→ `AliveDelivering`), but acking it would delete the user's
    /// message from the server before the real mesh ever opens. So the probe
    /// leaves every non-marker frame un-acked — it redelivers on the next
    /// (real) subscribe.
    #[tokio::test]
    async fn the_probe_preserves_a_real_waiting_message() {
        use molt_net::Transport;
        let (_hub, t, leg) = self_loop_leg().await;
        // a real (non-marker) application message is already waiting on the leg
        let payload = b"a real waiting application message".to_vec();
        supervisor::send_framed(&t, leg.snd0(), &leg.wrap_out, MsgId([7u8; 16]), &payload)
            .await
            .expect("seed a real waiting frame");

        // probing sees it → alive+delivering, but must not consume it
        let verdict = probe_leg(
            RitualTransport::Loopback(t.clone()),
            leg.clone(),
            "ada".to_string(),
            Duration::from_secs(2),
        )
        .await;
        assert_eq!(
            verdict,
            LegVerdict::AliveDelivering,
            "a real waiting frame proves the leg delivers"
        );

        // the real message SURVIVES the probe: un-acked, it redelivers to a
        // fresh subscribe (the real mesh on the next normal open).
        let mut rx = t.subscribe(leg.rcv0()).await.expect("resubscribe");
        let survived = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let d = match rx.recv().await {
                    Some(d) => d,
                    None => return false,
                };
                // the frame is chunk-framed (MsgId + header + payload + pad), so
                // the payload is CONTAINED, not byte-equal
                let hit = matches!(
                    molt_net::wrap::unwrap_block(&leg.wrap_in, &d.block),
                    Ok(bytes) if bytes.windows(payload.len()).any(|w| w == payload.as_slice())
                );
                d.ack.ack();
                if hit {
                    return true;
                }
            }
        })
        .await
        .expect("a frame redelivered within the window");
        assert!(
            survived,
            "the probe must leave a real waiting message on the server (un-acked)"
        );
    }

    /// The idle-expiry SHAPE (loopback `expire_queue`: SUB/SEND still Ok but
    /// deliveries are dropped) → `AliveButSilent`: the queue answers SUB but
    /// delivers nothing — exactly the deaf-but-not-gone state to distinguish
    /// from a truly deleted queue.
    #[tokio::test]
    async fn an_expired_queue_reads_alive_but_silent() {
        let (hub, t, leg) = self_loop_leg().await;
        assert!(hub.expire_queue(&leg.rcv0().id), "the queue exists to expire");
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

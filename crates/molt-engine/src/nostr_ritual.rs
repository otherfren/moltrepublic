// SPDX-License-Identifier: GPL-3.0-or-later

//! N4a (`docs/transport/nostr_n4_plan.md` §7): the engine-side Nostr ritual
//! tasks — the founder's 1059 inbox loop, the founder's 445 group recv, the
//! shared 445 publish leg, and the whole off-actor **member join task** that
//! finally emits the long-dormant `NetJoin*` commands.
//!
//! Actor rule: nothing here runs ON the actor — these are the spawned tasks
//! feeding results back as engine-internal commands, exactly like the
//! loopback ritual's recv loops. Every command send upgrades a WEAK handle
//! (the ticker rule): a task must never keep a dropped engine alive.
//!
//! Idempotency rule (N2 §3.5: the relay stream is at-least-once): every
//! loop tolerates redelivery — a duplicate `JoinAccepted` is skipped by
//! state, a replayed `Seal`/`Genesis` re-verifies to the same result, and
//! the actor's handlers are spend-once.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use molt_core::Command;
use molt_net::invite::{self, RitualMsg};
use molt_net::ritual_net::{GroupChannel, RitualDelivery, RitualNet};
use tokio::sync::mpsc;

use crate::Envelope;

/// How long the joiner waits for the founder's `JoinAccepted` (mirrors the
/// loopback ritual's 90 s accept deadline; after the ack the waits are
/// unbounded — human deliberation upstream).
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(90);
/// Long-poll slice for the unbounded waits — short enough that an aborted
/// task dies promptly, long enough to stay quiet.
const RECV_SLICE: Duration = Duration::from_secs(30);
/// Best-effort EOSE wait after placing a subscription.
const LIVE_WAIT: Duration = Duration::from_secs(10);

/// What the founder inbox task needs to render one seat's v2 link.
pub(crate) struct SeatInvite {
    pub seat: u32,
    pub ticket: String,
    pub info: molt_core::InviteInfo,
}

/// The member join task's inputs (everything derived from the pasted link +
/// the wizard state at spawn time).
pub(crate) struct JoinCtx {
    pub invite: crate::founding::FoundingInvite,
    /// The relays this node may actually DIAL for the join: the invite's
    /// list narrowed to what the operator confirmed (ADR-0004). Never empty
    /// — `cmd_join_start` refuses the join before spawning us otherwise.
    /// The invite's FULL list stays the group's policy (persisted, and what
    /// the Welcome is checked against).
    pub dial_relays: Vec<String>,
    pub member: String,
    pub phrase: String,
    pub generation: u64,
}

/// Send one engine-internal command through the weak handle. `false` = the
/// engine is gone — the caller returns (its work is moot).
async fn send_cmd(tx: &mpsc::WeakSender<Envelope>, cmd: Command) -> bool {
    let (reply, _rx) = tokio::sync::oneshot::channel();
    let Some(tx) = tx.upgrade() else {
        return false;
    };
    tx.send(Envelope { cmd, reply }).await.is_ok()
}

/// The founder's 1059 inbox: subscribe FIRST, then surface the v2 links
/// (subscribe-before-advertise), then feed every gift-wrapped JoinRequest
/// into the actor's validation ladder — with the §2.1 proof-of-possession
/// gate: the claimed `nostr_pk` must BE the wrap's proven sealer.
pub(crate) fn spawn_founder_inbox(
    net: RitualNet,
    seats: Vec<SeatInvite>,
    generation: u64,
    tx: mpsc::WeakSender<Envelope>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut inbox = match net.inbox().await {
            Ok(i) => i,
            Err(e) => {
                let _ = send_cmd(
                    &tx,
                    Command::NetRitualFailed {
                        error: format!("relay inbox subscribe: {e}"),
                        generation: Some(generation),
                    },
                )
                .await;
                return;
            }
        };
        // subscribe-BEFORE-advertise only means something if the
        // subscription is proven READABLE. A relay that accepts the
        // connection and the REQ but never replays (auth required, rate
        // limited, CLOSED-then-refused) left us publishing seat links over an
        // inbox nothing would ever answer on.
        let st = inbox.live_state(LIVE_WAIT).await;
        if !st.any() {
            let _ = send_cmd(
                &tx,
                Command::NetRitualFailed {
                    error: "the founding inbox is not readable on any relay — no relay \
                            replayed the subscription (auth required, rate limited, or \
                            refused). No invite was published."
                        .to_string(),
                    generation: Some(generation),
                },
            )
            .await;
            return;
        }
        if !st.full() {
            tracing::warn!(
                synced = st.synced,
                connected = st.connected,
                "the founding inbox replayed on only some relays"
            );
        }
        for s in &seats {
            let link = crate::founding::FoundingInvite {
                info: s.info.clone(),
                handover: invite::InviteHandoverV2 {
                    seat: s.seat,
                    ticket: s.ticket.clone(),
                    npub: net.pk_hex(),
                    relays: net.relays().to_vec(),
                },
            }
            .render();
            let cmd = match link {
                Ok(link) => Command::NetRitualLinkReady {
                    seat: s.seat,
                    link,
                    generation: Some(generation),
                },
                Err(e) => Command::NetRitualFailed {
                    error: format!("rendering invite link: {e}"),
                    generation: Some(generation),
                },
            };
            let fatal = matches!(cmd, Command::NetRitualFailed { .. });
            if !send_cmd(&tx, cmd).await || fatal {
                return;
            }
        }
        loop {
            let Some(delivery) = inbox.recv(RECV_SLICE).await else {
                continue; // idle slice; the ritual's Drop aborts us when done
            };
            // only gift-wrapped JoinRequests belong on the founder inbox:
            // Signed/Declined ride the MLS-authenticated 445 channel, and a
            // gift-wrapped copy of them would be authenticated by ADDRESS
            // only — ignore them here, like the loopback recv's ignore arms
            let RitualDelivery::Msg(RitualMsg::Join(j), sender) = delivery else {
                continue;
            };
            // the proof-of-possession comparison itself lives in the ACTOR
            // (`cmd_net_join_requested`), together with every other check —
            // one validation ladder, one place that can explain a refusal in
            // the founding log. This task only carries the proven sender.
            let cmd = Command::NetJoinRequested {
                seat: j.seat,
                member: j.name,
                identity_pk: j.identity_pk,
                nostr_pk: j.nostr_pk,
                proof: j.mac,
                // no queue handover on Nostr — the MAC-bound anchor IS the
                // reply address
                reply: String::new(),
                sender_npub: sender,
                key_package: j.key_package,
                generation: Some(generation),
            };
            if !send_cmd(&tx, cmd).await {
                return;
            }
        }
    })
}

/// The recovery coordinator's 1059 inbox — the recovery twin of
/// [`spawn_founder_inbox`] (N4b §8.8 step 5b).
///
/// Same order, for the same reason: subscribe FIRST, prove the subscription is
/// READABLE, and only then surface the link. A recovery link advertised over an
/// inbox no relay would ever answer on sends the returning member to a dead
/// address, and recovery is the one flow whose user has already lost their
/// device — there is no second channel to notice on.
///
/// The inbox lives until the WORKSPACE CLOSES, which is also exactly how long
/// its ticket stays spendable — `RECOVERY_WELCOME_TIMEOUT` is the rejoiner's
/// wait, not a deadline on this side. It therefore holds only a WEAK sender:
/// the loop must never keep a dropped engine's actor (writer thread,
/// workspace flock) alive.
/// How many times the re-key commit is offered to the relays before the
/// recovery is declared failed. Small on purpose: this is the ONE frame in a
/// recovery that nothing else recovers, and the alternative to a few retries
/// is a republic split nobody is told about.
const REKEY_PUBLISH_ATTEMPTS: u32 = 3;
/// Wait between those attempts.
const REKEY_PUBLISH_BACKOFF: Duration = Duration::from_secs(2);

/// **Deliver a Nostr re-key** (N4b step 6c): the commit as a kind-445 at its
/// PINNED carrier stamp, and then the gift-wrapped kind-444 Welcome to the
/// returning seat's NEW anchor.
///
/// The order is load-bearing and so is the coupling:
///
/// - The **commit first**. It is what moves every survivor to the epoch the
///   Welcome puts the rejoiner at. A frame that beats its commit is held and
///   retried (N5.3c), so the order is a preference rather than a guarantee —
///   but publishing the Welcome first would make that hold the normal path
///   instead of the exception.
/// - The **Welcome only if the commit landed**. The two failures are not
///   symmetric. A Welcome without its commit puts the rejoiner at an epoch no
///   survivor ever reaches — a split, with nothing in the product to heal it.
///   A commit without its Welcome leaves the seat unable to return, which the
///   re-mint failover already covers: the coordinator's next round supplies a
///   fresh link. So the recoverable failure is the one to prefer.
///
/// One landed relay is enough to be durable: relays store the frame, so a
/// survivor that was offline picks the commit up on its catch-up.
pub(crate) fn spawn_rekey_delivery(
    channel: GroupChannel,
    net: RitualNet,
    to_npub: String,
    rekey: crate::chain::NostrRekey,
    payload: molt_net::welcome::WelcomePayload,
    member: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut published = false;
        for attempt in 1..=REKEY_PUBLISH_ATTEMPTS {
            match channel
                .publish_frame_at(&rekey.prev_exporter, &rekey.commit, rekey.stamp)
                .await
            {
                Ok((stamp, report)) => {
                    tracing::info!(
                        %member,
                        stamp,
                        relays = report.accepted.len(),
                        "published the recovery re-key commit"
                    );
                    published = true;
                    break;
                }
                Err(e) => {
                    tracing::warn!(%member, attempt, error = %e, "the re-key commit did not publish");
                    if attempt < REKEY_PUBLISH_ATTEMPTS {
                        tokio::time::sleep(REKEY_PUBLISH_BACKOFF).await;
                    }
                }
            }
        }
        if !published {
            // the ONE thing that matters, and the action that follows from it
            tracing::error!(%member, "the re-key commit reached no relay — re-mint the recovery link");
            return;
        }
        if let Err(e) = net.send_welcome(&to_npub, &payload).await {
            tracing::error!(%member, error = %e, "the recovery welcome did not publish — re-mint the recovery link");
        }
    })
}

pub(crate) fn spawn_recovery_inbox(
    net: RitualNet,
    member: String,
    ticket: String,
    republic: String,
    republic_id: String,
    generation: u64,
    tx: mpsc::WeakSender<Envelope>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // every early exit unregisters the ticket through the SAME failure
        // lane the queue path uses, so a dead mint never leaves a live ticket
        // behind for a replayed request to spend
        let fail = |reason: String| Command::NetRecoverLinkFailed {
            member: member.clone(),
            reason,
            ticket: ticket.clone(),
            generation: Some(generation),
        };
        let mut inbox = match net.inbox().await {
            Ok(i) => i,
            Err(e) => {
                let _ = send_cmd(&tx, fail(format!("relay inbox subscribe: {e}"))).await;
                return;
            }
        };
        let st = inbox.live_state(LIVE_WAIT).await;
        if !st.any() {
            let _ = send_cmd(&tx, fail("no relay replayed the subscription".to_string())).await;
            return;
        }
        if !st.full() {
            tracing::warn!(
                synced = st.synced,
                connected = st.connected,
                "the recovery inbox replayed on only some relays"
            );
        }
        let handover = invite::RecoveryHandoverV2 {
            ticket: ticket.clone(),
            npub: net.pk_hex(),
            relays: net.relays().to_vec(),
            republic_id: republic_id.clone(),
        };
        // encode EXPLICITLY: `RecoveryInvite::render` falls back to the legacy
        // queue shape when the handover cannot encode, and with the empty
        // queue fields below that fallback is `hex("\n\n\n<republic_id>")` — a
        // link that parses as nothing, handed to the operator as a success.
        if let Err(e) = handover.encode() {
            let _ = send_cmd(&tx, fail(format!("rendering recovery link: {e}"))).await;
            return;
        }
        let link = crate::recovery::RecoveryInvite {
            republic,
            member: member.clone(),
            ticket: ticket.clone(),
            server: String::new(),
            queue_id: String::new(),
            wrap: String::new(),
            republic_id,
            handover: Some(handover),
        }
        .render();
        if !send_cmd(
            &tx,
            Command::NetRecoverLinkReady {
                member,
                link,
                generation: Some(generation),
            },
        )
        .await
        {
            return;
        }
        loop {
            let Some(delivery) = inbox.recv(RECV_SLICE).await else {
                continue; // idle slice
            };
            // only gift-wrapped Recover requests belong on this inbox
            let RitualDelivery::Msg(RitualMsg::Recover(r), sender) = delivery else {
                continue;
            };
            // the PoP comparison itself lives in the ACTOR, with every other
            // check — one validation ladder, one place that can refuse. This
            // task only carries the PROVEN sealer across.
            if !send_cmd(&tx, crate::founding::recover_command(r, sender, generation)).await {
                return;
            }
        }
    })
}

/// MLS-encrypt one RitualMsg and publish it as a 445 frame, synchronously.
/// The lock is dropped BEFORE the publish await (never hold a std mutex
/// across await). Returns the carrier `created_at`.
///
/// This is the MEMBER's send path (`Signed`, `Declined`): it propagates its
/// error to the caller, which already fails the join loudly — unlike the
/// founder's fire-and-forget legs, which needed the reporting task below.
async fn publish_frame_now(
    chan: &GroupChannel,
    group: &Arc<Mutex<molt_net::MlsMember>>,
    msg: &RitualMsg,
) -> Result<u64, String> {
    let payload = serde_json::to_vec(msg).map_err(|e| e.to_string())?;
    let (ct, exporter) = {
        let mut g = group.lock().map_err(|_| "mls lock poisoned".to_string())?;
        let ct = g.encrypt(&payload).map_err(|e| e.to_string())?;
        let exporter = g.exporter_secret().map_err(|e| e.to_string())?;
        (ct, exporter)
    };
    chan.publish_frame(&exporter, &ct)
        .await
        .map(|(stamp, _report)| stamp)
        .map_err(|e| e.to_string())
}

/// What a 445 publish task sends. Encrypted ONCE, before any retry.
///
/// Re-encrypting on retry would advance the MLS sender ratchet past the
/// snapshot `finalize_founding` deliberately takes AFTER the genesis encrypt,
/// and every member would meet `SecretReuseError` on reopen. So the retry
/// republishes the SAME ciphertext — for an application frame that is safe:
/// `decrypt_at`'s carrier stamp only feeds the concurrent-COMMIT tiebreak.
pub(crate) enum FramePayload {
    /// Encrypt this message against the group, then publish.
    Encrypt(Arc<Mutex<molt_net::MlsMember>>, RitualMsg),
    /// Already sealed by the caller (the genesis, encrypted before the group
    /// snapshot) — publish these bytes as they are.
    Sealed { ct: Vec<u8>, exporter: [u8; 32] },
}

/// How hard a leg tries before it reports failure.
#[derive(Clone, Copy)]
pub(crate) struct RetryPolicy {
    /// Total publish attempts (1 = no retry).
    pub(crate) attempts: u32,
    /// Delay before the 2nd attempt; doubles each time.
    pub(crate) backoff: Duration,
}

impl RetryPolicy {
    /// Pre-seal legs: the founding is still live and a human is watching, so
    /// fail fast enough to stay well inside a wizard's patience.
    pub(crate) const PRE_SEAL: Self =
        Self { attempts: 3, backoff: Duration::from_millis(700) };
    /// The genesis: the founder has already materialized, so this frame is
    /// the members' ONLY path into the republic. Try harder before giving up.
    pub(crate) const GENESIS: Self =
        Self { attempts: 4, backoff: Duration::from_millis(900) };
}

/// Publish one 445 frame, retrying the PUBLISH only, and ALWAYS report the
/// per-relay outcome back to the actor.
///
/// The sink is not optional. It used to be `Option<...>`, every caller passed
/// `None`, and the reporting path was dead code — so a refused Seal hung both
/// sides in silence. Deleting the Option deletes the seam a future call site
/// can forget to wire.
pub(crate) fn spawn_publish_frame(
    chan: GroupChannel,
    payload: FramePayload,
    what: &'static str,
    retry: RetryPolicy,
    tx: mpsc::WeakSender<Envelope>,
    generation: Option<u64>,
    // which workspace this leg belongs to — carried for legs published after
    // the ritual was taken (the genesis), so a late report cannot be
    // attributed to a founding that started meanwhile
    workspace: String,
) {
    tokio::spawn(async move {
        // encrypt ONCE — see FramePayload
        let sealed: Result<(Vec<u8>, [u8; 32]), String> = match payload {
            FramePayload::Sealed { ct, exporter } => Ok((ct, exporter)),
            FramePayload::Encrypt(group, msg) => (|| {
                let bytes = serde_json::to_vec(&msg).map_err(|e| e.to_string())?;
                let mut g = group.lock().map_err(|_| "mls lock poisoned".to_string())?;
                let ct = g.encrypt(&bytes).map_err(|e| e.to_string())?;
                let exporter = g.exporter_secret().map_err(|e| e.to_string())?;
                Ok((ct, exporter))
            })(),
        };
        let (ct, exporter) = match sealed {
            Ok(v) => v,
            Err(e) => {
                // encryption failed: no publish will ever help
                tracing::error!(what, error = %e, "ritual 445 frame could not be encrypted");
                let _ = send_cmd(
                    &tx,
                    Command::NetRitualPublished {
                        what: what.to_string(),
                        accepted: Vec::new(),
                        failed: vec![format!("encrypt: {e}")],
                        generation,
                        workspace,
                    },
                )
                .await;
                return;
            }
        };

        let mut wait = retry.backoff;
        let mut last: Vec<String> = Vec::new();
        for attempt in 1..=retry.attempts {
            match chan.publish_frame(&exporter, &ct).await {
                Ok((_stamp, report)) => {
                    let failed: Vec<String> = report
                        .failed
                        .iter()
                        .map(|(url, why)| format!("{url}: {why}"))
                        .collect();
                    let _ = send_cmd(
                        &tx,
                        Command::NetRitualPublished {
                            what: what.to_string(),
                            accepted: report.accepted.clone(),
                            failed,
                            generation,
                            workspace,
                        },
                    )
                    .await;
                    return;
                }
                Err(e) => {
                    last = vec![e.to_string()];
                    tracing::warn!(
                        what, attempt, of = retry.attempts, error = %e,
                        "ritual 445 frame did not publish"
                    );
                    if attempt < retry.attempts {
                        tokio::time::sleep(wait).await;
                        wait = wait.saturating_mul(2);
                    }
                }
            }
        }
        // every attempt refused: accepted stays EMPTY, which is what the
        // handler reads as "nobody has this frame"
        let _ = send_cmd(
            &tx,
            Command::NetRitualPublished {
                what: what.to_string(),
                accepted: Vec::new(),
                failed: last,
                generation,
                workspace,
            },
        )
        .await;
    });
}

/// Open one 445 frame against the group: strip the outer exporter layer
/// (current secret first, then the ring), then MLS-decrypt WITH the carrier
/// stamp (`decrypt_at` — the N3 tiebreak's receive side). Undecryptable
/// frames (our own publishes echoed back, replays across a rewind) are a
/// normal part of the stream — skipped, never fatal.
fn open_group_frame(
    group: &Arc<Mutex<molt_net::MlsMember>>,
    content: &str,
    created_at: u64,
) -> Option<(RitualMsg, String)> {
    let mut g = group.lock().ok()?;
    let mut secrets = Vec::with_capacity(1 + molt_net::mls::EXPORTER_RING_K);
    if let Ok(current) = g.exporter_secret() {
        secrets.push(current);
    }
    secrets.extend_from_slice(g.exporter_ring());
    let wire = match molt_net::envelope::open_outer(&secrets, content) {
        Ok(w) => w,
        Err(e) => {
            tracing::debug!(error = %e, "445 outer layer did not open (foreign epoch or noise)");
            return None;
        }
    };
    match g.decrypt_at(&wire, created_at) {
        // `from` is the MLS-authenticated leaf credential — the ONLY
        // authenticator a 445 has (the event itself is signed by a fresh
        // ephemeral key by design). Dropping it is what let any group member
        // impersonate the founder; every caller must use it.
        Ok(molt_net::MlsIncoming::Application { from, plaintext }) => {
            serde_json::from_slice(&plaintext).ok().map(|m| (m, from))
        }
        Ok(other) => {
            // no commit flies during ritual deliberation — anything else is
            // stream noise for this loop
            tracing::debug!(?other, "non-application 445 during the ritual — skipped");
            None
        }
        Err(e) => {
            tracing::debug!(error = %e, "445 frame did not decrypt (own echo or replay)");
            None
        }
    }
}

/// What an inbound 445 ritual frame is, relative to the founder named in
/// the invite link. Three-valued on purpose: "someone else published this"
/// and "the founder published something wrong" must NOT be handled the
/// same way — see [`check_proposal_provenance`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Provenance {
    /// The founder's frame, describing the republic the link promised.
    FromFounder,
    /// Published by another member of the group. IGNORE it — never fail the
    /// join, or any invitee could abort every other invitee's join at will.
    NotTheFounder,
    /// The FOUNDER sent something inconsistent with the invite. A real
    /// failure the joiner must surface rather than sign.
    Refused(String),
}

/// **The 445 proposer binding** (review 2026-08-01, CRITICAL).
///
/// On the loopback path `Seal`/`Genesis` arrived on the member's OWN reply
/// queue, wrapped under a key it minted and handed only to the founder in
/// its MAC-bound JoinRequest — the CHANNEL was the proposer authentication,
/// so no code ever had to check it. The kind-445 group channel is shared by
/// every welcomed seat, so that binding vanished in the fork and has to be
/// re-established explicitly here.
///
/// `from` is the MLS-authenticated leaf credential — the handle the sender
/// passed to `MlsMember::new`, which the founder set to its own member name.
/// Everything else is cross-checked against what the LINK promised, because
/// the link is the joiner's root of trust for "whose founding is this".
/// Is this 445 frame from the FOUNDER?
///
/// `from` is the MLS-authenticated credential of the frame's author, so this
/// is the whole gate. Extracted because BOTH group-channel arms need it: the
/// Seal (which it always had) and the abort (where an ungated arm would let
/// any welcomed seat kill every other seat's join with one frame — exactly
/// the impersonation class fixed as CRITICAL in 63555dc).
pub(crate) fn frame_is_from_founder(from: &str, info: &molt_core::InviteInfo) -> bool {
    from == info.inviter
}

pub(crate) fn check_proposal_provenance(
    from: &str,
    proposal: &molt_core::SealedRoster,
    founder_npub: &str,
    info: &molt_core::InviteInfo,
) -> Provenance {
    if !frame_is_from_founder(from, info) {
        return Provenance::NotTheFounder;
    }
    if proposal.rule_m != info.threshold || proposal.rule_n != info.members {
        return Provenance::Refused(format!(
            "the proposed republic is {}-of-{}, but the invite promised {}-of-{}",
            proposal.rule_m, proposal.rule_n, info.threshold, info.members
        ));
    }
    let Some(founder_seat) = proposal
        .identities
        .iter()
        .find(|i| i.member == info.inviter)
    else {
        return Provenance::Refused(
            "the proposed roster does not seat the founder who invited us".to_string(),
        );
    };
    if founder_seat.nostr_pk != founder_npub {
        return Provenance::Refused(
            "the founder's seat carries a transport key other than the one the \
             invite link named"
                .to_string(),
        );
    }
    Provenance::FromFounder
}

/// The founder's 445 recv: `Signed`/`Declined` come back over the freshly
/// born group and land in the SAME actor handlers as the loopback path.
pub(crate) fn spawn_founder_group_recv(
    chan: GroupChannel,
    group: Arc<Mutex<molt_net::MlsMember>>,
    generation: u64,
    tx: mpsc::WeakSender<Envelope>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut sub = match chan.subscribe().await {
            Ok(s) => s,
            Err(e) => {
                let _ = send_cmd(
                    &tx,
                    Command::NetRitualFailed {
                        error: format!("group subscribe: {e}"),
                        generation: Some(generation),
                    },
                )
                .await;
                return;
            }
        };
        // this gate was MISSING entirely: a founder whose 445 subscription
        // never replays waits forever for Signed frames that can never arrive
        let st = sub.live_state(LIVE_WAIT).await;
        if !st.any() {
            let _ = send_cmd(
                &tx,
                Command::NetRitualFailed {
                    error: "the group channel is not readable on any relay — no relay \
                            replayed the subscription (auth required, rate limited, or \
                            refused)"
                        .to_string(),
                    generation: Some(generation),
                },
            )
            .await;
            return;
        }
        let mut was_deaf = false;
        loop {
            let (content, created_at) = match sub.recv(RECV_SLICE).await {
                molt_net::ritual_net::GroupRecv::Frame { content, created_at } => {
                    if was_deaf {
                        was_deaf = false;
                        let _ = send_cmd(
                            &tx,
                            Command::NetRitualNote {
                                note: "✓ the group channel is back".to_string(),
                                generation: Some(generation),
                            },
                        )
                        .await;
                    }
                    (content, created_at)
                }
                molt_net::ritual_net::GroupRecv::Idle => continue,
                // loud, never fatal: a one-shot founding must survive a blip
                molt_net::ritual_net::GroupRecv::Deaf(why) => {
                    was_deaf = true;
                    let _ = send_cmd(
                        &tx,
                        Command::NetRitualNote {
                            note: format!(
                                "⚠ cannot hear the group channel — {why} · still retrying"
                            ),
                            generation: Some(generation),
                        },
                    )
                    .await;
                    continue;
                }
            };
            let Some((msg, from)) = open_group_frame(&group, &content, created_at) else {
                continue;
            };
            let cmd = match msg {
                // the signature is verified against the seat's ANCHORED key
                // on the actor, so a forged `Signed` cannot be minted — but
                // the authenticated author rides along so the actor can
                // refuse a signature attributed to somebody else's seat
                RitualMsg::Signed(s) => Command::NetSealSigned {
                    seat: s.seat,
                    sig: s.sig,
                    from: from.clone(),
                    generation: Some(generation),
                },
                // a decline carries NO signature, so the MLS author is its
                // only authentication: without it any member could abort the
                // founding and frame another seat for it
                RitualMsg::Declined { seat } => Command::NetJoinDeclined {
                    seat,
                    from: from.clone(),
                    generation: Some(generation),
                },
                // our own Seal/Genesis echoes and anything else: not ours here
                _ => continue,
            };
            if !send_cmd(&tx, cmd).await {
                return;
            }
        }
    })
}

/// What the rejoiner task needs — the `JoinCtx` twin.
pub(crate) struct RecoverCtx {
    /// The link's v2 transport handover: ticket, the coordinator's anchor,
    /// the relays it listens on, and the republic id the seat proof binds.
    pub handover: molt_net::invite::RecoveryHandoverV2,
    /// What THIS node may dial — the handover's relays intersected with this
    /// operator's own confirmed pool (ADR-0004).
    pub dial_relays: Vec<String>,
    pub member: String,
    pub phrase: String,
    pub generation: u64,
}

/// One inbound 445 as an engine envelope. The recovery anchor rides the
/// coordinator's ordinary outbox, so it arrives as an `EventEnvelope` rather
/// than a `RitualMsg` — the two shapes are provably disjoint
/// (`molt-net/tests/frame_disjointness.rs`), so trying one is not ambiguous.
fn open_group_envelope(
    group: &Arc<Mutex<molt_net::MlsMember>>,
    content: &str,
    created_at: u64,
) -> Option<molt_core::EventEnvelope> {
    let mut g = group.lock().ok()?;
    let mut secrets = Vec::with_capacity(1 + molt_net::mls::EXPORTER_RING_K);
    if let Ok(current) = g.exporter_secret() {
        secrets.push(current);
    }
    secrets.extend_from_slice(g.exporter_ring());
    let wire = molt_net::envelope::open_outer(&secrets, content).ok()?;
    match g.decrypt_at(&wire, created_at) {
        Ok(molt_net::MlsIncoming::Application { plaintext, .. }) => {
            serde_json::from_slice(&plaintext).ok()
        }
        _ => None,
    }
}

/// Spawn the whole rejoiner task (the production body of
/// `Command::RecoverStart` on a Nostr republic) — the [`spawn_member_join`]
/// twin. Every exit reports: success ends in `NetRecoverSealed`, every
/// failure in `NetRecoverFailed` with a human reason.
pub(crate) fn spawn_recovery_rejoiner(
    dialer: molt_net::dial::Dialer,
    ctx: RecoverCtx,
    tx: mpsc::WeakSender<Envelope>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let generation = Some(ctx.generation);
        if let Err(error) = recovery_rejoin(dialer, ctx, &tx).await {
            let _ = send_cmd(&tx, Command::NetRecoverFailed { error, generation }).await;
        }
    })
}

/// The rejoiner state machine over Nostr (N4b step 6e).
///
/// Fresh anchor from the RECOVERY ticket → 1059 inbox (readable-gated before
/// anything is advertised) → gift-wrapped `RecoverRequest` carrying the new
/// anchor and a seat proof over it → the 444 Welcome → back inside the MLS
/// group → 445 subscription under the Welcome's rotation seed → assemble the
/// coordinator's served ANCHOR until it verifies standalone → `NetRecoverSealed`.
///
/// **One absolute deadline** ([`crate::recovery::RECOVERY_WELCOME_TIMEOUT`])
/// covers the whole run rather than one per phase: what the returning human
/// is waiting on is "am I back", and a per-phase budget silently multiplies
/// into a wait nobody predicted.
async fn recovery_rejoin(
    dialer: molt_net::dial::Dialer,
    ctx: RecoverCtx,
    tx: &mpsc::WeakSender<Envelope>,
) -> Result<(), String> {
    let h = &ctx.handover;
    let generation = Some(ctx.generation);
    let deadline = tokio::time::Instant::now() + crate::recovery::RECOVERY_WELCOME_TIMEOUT;

    // The seat's identity re-derives from the phrase; its transport anchor is
    // NEW, salted with the RECOVERY ticket — the founding anchor was salted
    // with a ticket that died with the device and cannot be re-derived.
    let (sk, pk) = crate::founding::member_identity(&ctx.phrase)?;
    let entropy = molt_storage::seed_entropy(&ctx.phrase).map_err(|e| e.to_string())?;
    let (mut nostr_raw, new_nostr_pk) = molt_net::nostr_identity(&entropy, &h.ticket);
    let nostr_sk = zeroize::Zeroizing::new(nostr_raw.to_vec());
    zeroize::Zeroize::zeroize(&mut nostr_raw);

    // inbox BEFORE the request (subscribe-before-advertise): a request
    // answered into an unreadable inbox strands a human who has already lost
    // their device, with no second channel to notice on
    let net = RitualNet::new(dialer.clone(), ctx.dial_relays.clone(), &nostr_sk)
        .map_err(|e| format!("transport keys: {e}"))?;
    let mut inbox = net
        .inbox()
        .await
        .map_err(|e| format!("recovery inbox subscribe: {e}"))?;
    if !inbox.live_state(LIVE_WAIT).await.any() {
        return Err(
            "the recovery inbox is not readable on any relay — no relay replayed the \
             subscription (auth required, rate limited, or refused)"
                .to_string(),
        );
    }

    // a FRESH MLS identity + KeyPackage: the coordinator removes the lost
    // leaf and adds this one in a single commit
    let mut mls =
        molt_net::MlsMember::new(&sk, &ctx.member).map_err(|e| format!("mls identity: {e}"))?;
    let kp_hex = hex::encode(mls.key_package().map_err(|e| format!("key package: {e}"))?);
    // the seat proof binds the new anchor AND the relay declaration, so a
    // relay or a hostile coordinator can swap neither on the way
    let declared = ctx.dial_relays.clone();
    let seat_proof = crate::founding::make_seat_proof(
        &sk,
        &h.ticket,
        &kp_hex,
        &h.republic_id,
        &new_nostr_pk,
        &declared,
    );
    net.send_ritual(
        &h.npub,
        &RitualMsg::Recover(invite::RecoverRequest {
            member: ctx.member.clone(),
            identity_pk: pk,
            key_package: kp_hex,
            ticket: h.ticket.clone(),
            seat_proof,
            new_nostr_pk,
            // R5: what this seat can actually dial — its ledger entry
            relays: declared,
            // Nostr replies to the gift-wrap anchor, not to a queue
            reply: None,
        }),
    )
    .await
    .map_err(|e| format!("recovery request: {e}"))?;

    // the rejoiner is not silent while it waits (NetRecoverNote): the wait
    // spans the coordinator's HUMAN approval, so the status line is the
    // difference between "working" and "looks dead until the deadline"
    let _ = send_cmd(
        tx,
        Command::NetRecoverNote {
            note: "request sent - waiting for the coordinator's Welcome".to_string(),
            generation,
        },
    )
    .await;
    // the Welcome, from the COORDINATOR's anchor and nobody else's
    let started = tokio::time::Instant::now();
    let mut last_note = started;
    let payload = loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(
                "no Welcome arrived within 15 minutes — the coordinator must be running \
                 and approve the return"
                    .to_string(),
            );
        }
        // the widening ladder (the join task's cluster-F pattern): a tick a
        // minute, so a long approval wait visibly keeps being a wait
        if now.duration_since(last_note).as_secs() >= 60 {
            last_note = now;
            let mins = now.duration_since(started).as_secs() / 60;
            let _ = send_cmd(
                tx,
                Command::NetRecoverNote {
                    note: format!("waiting for the coordinator's Welcome ({mins} min)"),
                    generation,
                },
            )
            .await;
        }
        match inbox.recv((deadline - now).min(RECV_SLICE)).await {
            Some(RitualDelivery::Welcome(p, sender)) if sender == h.npub => break p,
            _ => continue,
        }
    };
    let _ = send_cmd(
        tx,
        Command::NetRecoverNote {
            note: "welcomed back - fetching the chain anchor".to_string(),
            generation,
        },
    )
    .await;
    mls.join_from_welcome(&payload.welcome)
        .map_err(|e| format!("mls welcome: {e}"))?;
    let group = Arc::new(Mutex::new(mls));

    // 445 under the Welcome's rotation seed, dialing only our own subset
    let chan = GroupChannel::new(dialer, ctx.dial_relays.clone(), payload.rotation_seed);
    let mut sub = chan
        .subscribe()
        .await
        .map_err(|e| format!("group subscribe: {e}"))?;
    if !sub.live_state(LIVE_WAIT).await.any() {
        return Err(
            "the group channel is not readable on any relay — the Welcome arrived but \
             the chain cannot be fetched"
                .to_string(),
        );
    }

    // Assemble the served ANCHOR until it verifies STANDALONE. `verify_served`
    // against the link's republic id is the whole gate: the frames carry no
    // usable author binding for a node that does not yet know the roster, and
    // it needs none — a chain that recomputes to this republic's id is this
    // republic's chain, whoever relayed it.
    let mut blob: Option<molt_core::CheckpointState> = None;
    let mut blocks: Vec<molt_core::ChainBlock> = Vec::new();
    let (head, sealed) = loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(
                "the republic's chain anchor never arrived — the coordinator re-keyed \
                 but served no chain"
                    .to_string(),
            );
        }
        let (content, created_at) = match sub.recv((deadline - now).min(RECV_SLICE)).await {
            molt_net::ritual_net::GroupRecv::Frame { content, created_at } => (content, created_at),
            _ => continue,
        };
        let Some(env) = open_group_envelope(&group, &content, created_at) else {
            continue;
        };
        match env.body {
            molt_core::WorkspaceEvent::CheckpointServed { blob: b } => blob = Some(b),
            molt_core::WorkspaceEvent::Committed(block) => {
                if !blocks.iter().any(|b| b.height == block.height) {
                    blocks.push(block);
                    blocks.sort_by_key(|b| b.height);
                }
            }
            _ => continue,
        }
        // Only a CONSECUTIVE run can verify — and the stream is not one. The
        // coordinator's outbox publishes whatever sits above its cursor, so
        // the rejoiner also sees the head block the recovery just committed:
        // on any republic longer than a couple of blocks that is the anchor
        // plus a block several heights up, with the middle never served.
        // Feeding the gap to `verify_served` fails forever, which reads as
        // "the anchor never arrived" fifteen minutes later.
        let run: Vec<molt_core::ChainBlock> = blocks
            .iter()
            .enumerate()
            .take_while(|(i, b)| {
                u64::try_from(*i).is_ok_and(|i| b.height == blocks[0].height + i)
            })
            .map(|(_, b)| b.clone())
            .collect();
        if run.is_empty() {
            continue;
        }
        if let Ok(pair) = crate::chain::verify_served(blob.as_ref(), &run, Some(&h.republic_id)) {
            blocks = run;
            break pair;
        }
    };

    // **Relay honesty, against the CHAIN.** The join twin compares the
    // Welcome's relays to the invite's, which is founding-only ("the two sets
    // are the same by construction"). Since roster-v4 the pool is governed, so
    // the authority is the verified anchor — and the check can only run HERE,
    // after the chain is verified, which is why it is not up with the Welcome.
    if payload.relays != sealed.relays {
        return Err(
            "the Welcome names a different relay set than the republic ratified — \
             refusing (relay changes are governed by the chain)"
                .to_string(),
        );
    }

    let snapshot = group
        .lock()
        .map_err(|_| "mls lock poisoned".to_string())?
        .snapshot()
        .map_err(|e| format!("mls snapshot: {e}"))?;
    let wire = match blob {
        Some(b) => crate::chain::ServedChainWire::Pruned { checkpoint_blob: b, blocks },
        None => crate::chain::ServedChainWire::Full(blocks),
    };
    tracing::info!(member = %ctx.member, height = head.height, "rejoined and verified the chain anchor");
    send_cmd(
        tx,
        Command::NetRecoverSealed {
            member: ctx.member.clone(),
            chain: serde_json::to_string(&wire).map_err(|e| e.to_string())?,
            mls: hex::encode(snapshot),
            // a Nostr republic has no queue mesh, by design not by omission
            mesh: Vec::new(),
            nostr_sk: hex::encode(&*nostr_sk),
            rotation_seed: hex::encode(payload.rotation_seed),
            generation,
        },
    )
    .await;
    Ok(())
}

/// Spawn the whole member join task (the production body of
/// `Command::JoinStart`). Every exit path reports: success ends in
/// `NetJoinSealed`, every failure in `NetJoinFailed` with a human reason.
pub(crate) fn spawn_member_join(
    dialer: molt_net::dial::Dialer,
    ctx: JoinCtx,
    confirm: mpsc::Receiver<bool>,
    tx: mpsc::WeakSender<Envelope>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let generation = Some(ctx.generation);
        let mut confirm = confirm;
        if let Err(error) = member_join(dialer, ctx, &mut confirm, &tx).await {
            let _ = send_cmd(&tx, Command::NetJoinFailed { error, generation }).await;
        }
    })
}

/// The member state machine over Nostr — the §4.2 restructured order:
/// JoinRequest (gift-wrap) → JoinAccepted → **Welcome before deliberation**
/// (payload v2: MLS Welcome + rotation seed + relays) → group 445s for
/// Seal → ratify → Signed → Genesis, with the SAME verification ladder as
/// the loopback ritual: `verify_seal_proposal` before signing, and the
/// genesis-time byte comparison against the exact ratified table.
async fn member_join(
    dialer: molt_net::dial::Dialer,
    ctx: JoinCtx,
    confirm: &mut mpsc::Receiver<bool>,
    tx: &mpsc::WeakSender<Envelope>,
) -> Result<(), String> {
    let h = ctx.invite.handover;
    let seat = h.seat;
    let generation = Some(ctx.generation);
    // what we DIAL (our confirmed subset) vs what the GROUP uses (the
    // invite's full list) — the two are deliberately different
    let dial_relays = ctx.dial_relays;

    // the joiner's identity — derived exactly as the ritual anchors it
    let (sk, pk) = crate::founding::member_identity(&ctx.phrase)?;
    let entropy = molt_storage::seed_entropy(&ctx.phrase).map_err(|e| e.to_string())?;
    let (mut nostr_raw, nostr_pk) = molt_net::nostr_identity(&entropy, &h.ticket);
    let nostr_sk = zeroize::Zeroizing::new(nostr_raw.to_vec());
    zeroize::Zeroize::zeroize(&mut nostr_raw);

    // endpoint + inbox BEFORE announcing (subscribe-before-advertise)
    let net = RitualNet::new(dialer.clone(), dial_relays.clone(), &nostr_sk)
        .map_err(|e| format!("transport keys: {e}"))?;
    let mut inbox = net
        .inbox()
        .await
        .map_err(|e| format!("inbox subscribe: {e}"))?;
    // announcing a JoinRequest over an unreadable inbox means the founder's
    // reply lands nowhere and the join hangs with no reason
    let st = inbox.live_state(LIVE_WAIT).await;
    if !st.any() {
        return Err(
            "the join inbox is not readable on any relay — no relay replayed the \
             subscription (auth required, rate limited, or refused)"
                .to_string(),
        );
    }

    // MLS identity + KeyPackage, then the MAC-bound JoinRequest to the
    // founder's anchor
    let mut mls =
        molt_net::MlsMember::new(&sk, &ctx.member).map_err(|e| format!("mls identity: {e}"))?;
    let kp = mls.key_package().map_err(|e| format!("key package: {e}"))?;
    let mac = invite::join_mac(&h.ticket, &ctx.member, &pk, &nostr_pk);
    net.send_ritual(
        &h.npub,
        &RitualMsg::Join(invite::JoinRequest {
            seat,
            name: ctx.member.clone(),
            identity_pk: pk.clone(),
            nostr_pk: nostr_pk.clone(),
            mac,
            reply: None,
            key_package: hex::encode(&kp),
        }),
    )
    .await
    .map_err(|e| format!("join request: {e}"))?;

    // accept wait (90 s) — a fast founder's Welcome may overtake the
    // advisory ack; both count as acceptance
    let deadline = tokio::time::Instant::now() + ACCEPT_TIMEOUT;
    let mut welcome: Option<molt_net::welcome::WelcomePayload> = None;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(
                "the founder did not accept within 90 s — check the link, the relays, \
                 and that the founding is still open"
                    .to_string(),
            );
        }
        match inbox.recv(deadline - now).await {
            Some(RitualDelivery::Msg(RitualMsg::JoinAccepted { .. }, sender))
                if sender == h.npub =>
            {
                let _ = send_cmd(tx, Command::NetJoinAccepted { generation }).await;
                break;
            }
            Some(RitualDelivery::Msg(RitualMsg::LinkSpent { reason, .. }, sender))
                if sender == h.npub =>
            {
                // the founder's OWN words: "ask for your own link" is wrong
                // advice when the group already formed around a first attempt
                return Err(if reason.is_empty() {
                    "this invite link was already used by someone else — ask the founder \
                     for a fresh, unused link"
                        .to_string()
                } else {
                    format!("the founder refused this activation: {reason}")
                });
            }
            // the founder gave up: stop waiting instead of sitting here until
            // the accept deadline with no idea why
            Some(RitualDelivery::Msg(RitualMsg::Aborted { reason }, sender))
                if sender == h.npub =>
            {
                return Err(format!("the founder ended this founding: {reason}"));
            }
            Some(RitualDelivery::Welcome(p, sender)) if sender == h.npub => {
                let _ = send_cmd(tx, Command::NetJoinAccepted { generation }).await;
                welcome = Some(p);
                break;
            }
            _ => continue,
        }
    }

    // Welcome wait (unbounded — the founder waits for every seat)
    let payload = match welcome {
        Some(p) => p,
        None => loop {
            match inbox.recv(RECV_SLICE).await {
                Some(RitualDelivery::Welcome(p, sender)) if sender == h.npub => break p,
                Some(RitualDelivery::Msg(RitualMsg::LinkSpent { reason, .. }, sender))
                    if sender == h.npub =>
                {
                    return Err(if reason.is_empty() {
                        "the founder voided this seat — the link was re-used".to_string()
                    } else {
                        format!("the founder voided this seat: {reason}")
                    });
                }
                // …the same, in the wait that really is unbounded
                Some(RitualDelivery::Msg(RitualMsg::Aborted { reason }, sender))
                    if sender == h.npub =>
                {
                    return Err(format!("the founder ended this founding: {reason}"));
                }
                _ => continue,
            }
        },
    };
    // relay honesty: the joiner gated the INVITE relays through its own
    // pool (ADR-0004); a Welcome silently pointing elsewhere would bypass
    // that consent. At founding the two sets are the same by construction;
    // a mismatch is a broken or malicious founder.
    if payload.relays != h.relays {
        return Err(
            "the Welcome names a different relay set than the invite — refusing \
             (relay changes are governed by the chain, not by the ritual)"
                .to_string(),
        );
    }
    mls.join_from_welcome(&payload.welcome)
        .map_err(|e| format!("mls welcome: {e}"))?;
    let group = Arc::new(Mutex::new(mls));
    let chan = GroupChannel::new(dialer, dial_relays.clone(), payload.rotation_seed);
    let mut sub = chan
        .subscribe()
        .await
        .map_err(|e| format!("group subscribe: {e}"))?;
    // …and the same before the Seal wait, which is unbounded: a
    // never-readable 445 subscription would hang the join forever in silence
    let st = sub.live_state(LIVE_WAIT).await;
    if !st.any() {
        return Err(
            "the group channel is not readable on any relay — no relay replayed the \
             subscription (auth required, rate limited, or refused)"
                .to_string(),
        );
    }

    let mut was_deaf = false;
    // Seal wait: the founder's charter proposal as a 445 in the born group.
    // verify_seal_proposal is THE ladder (content-derived id, our 3-anchor
    // seat, every seat's anchor) — sign-what-you-see, unchanged.
    let (proposal, table) = loop {
        let (content, created_at) = match sub.recv(RECV_SLICE).await {
            molt_net::ritual_net::GroupRecv::Frame { content, created_at } => {
                if was_deaf {
                    was_deaf = false;
                    let _ = send_cmd(
                        tx,
                        Command::NetJoinNote {
                            note: "✓ the group channel is back".to_string(),
                            generation,
                        },
                    )
                    .await;
                }
                (content, created_at)
            }
            molt_net::ritual_net::GroupRecv::Idle => continue,
            // loud, never fatal — the join keeps waiting, visibly
            molt_net::ritual_net::GroupRecv::Deaf(why) => {
                was_deaf = true;
                let _ = send_cmd(
                    tx,
                    Command::NetJoinNote {
                        note: format!("⚠ cannot hear the group channel — {why} · still retrying"),
                        generation,
                    },
                )
                .await;
                continue;
            }
        };
        let Some((msg, from)) = open_group_frame(&group, &content, created_at) else {
            continue;
        };
        // the founder gave up after the group was born: end the wait rather
        // than loop forever. Gated on the authenticated author for the same
        // reason the Seal is — an ungated abort is a one-frame kill switch
        // any welcomed seat could pull on every other seat.
        if let RitualMsg::Aborted { reason } = &msg {
            if frame_is_from_founder(&from, &ctx.invite.info) {
                return Err(format!("the founder ended this founding: {reason}"));
            }
            tracing::warn!(%from, "abort frame from a co-member — ignored");
            continue;
        }
        if let RitualMsg::Seal { proposal } = msg {
            // a frame from a co-member is IGNORED, never fatal: parsing or
            // verifying it before the author check would let any invitee
            // abort every other invitee's join with one garbage frame
            let Ok(sealed) = serde_json::from_str::<molt_core::SealedRoster>(&proposal) else {
                tracing::debug!(%from, "unparseable seal on the group channel — ignored");
                continue;
            };
            match check_proposal_provenance(&from, &sealed, &h.npub, &ctx.invite.info) {
                Provenance::NotTheFounder => {
                    tracing::warn!(
                        %from,
                        "a group member other than the founder proposed a charter — ignored"
                    );
                    continue;
                }
                Provenance::Refused(why) => return Err(format!("seal proposal rejected: {why}")),
                Provenance::FromFounder => {}
            }
            let table =
                crate::founding::verify_seal_proposal(&sealed, &ctx.member, &pk, &nostr_pk)
                    .map_err(|e| format!("seal proposal rejected: {e}"))?;
            break (sealed, table);
        }
    };

    // the human gate: surface the charter, block on the wizard's answer
    let _ = send_cmd(
        tx,
        Command::NetJoinCharterProposed {
            name: proposal.name.clone(),
            agenda: proposal.agenda.clone(),
            generation,
        },
    )
    .await;
    let confirmed = confirm.recv().await.unwrap_or(false);
    if !confirmed {
        // tell the group before failing — the founder's recv maps it to the
        // declined seat state
        let _ = publish_frame_now(&chan, &group, &RitualMsg::Declined { seat }).await;
        return Err("charter declined".to_string());
    }
    let sig = molt_storage::identity_sign(&sk, &table);
    publish_frame_now(&chan, &group, &RitualMsg::Signed(invite::SealSigned { seat, sig }))
        .await
        .map_err(|e| format!("seal signature did not publish: {e}"))?;

    let mut was_deaf = false;
    // The wait below is deliberately UNBOUNDED: the genesis lands only after
    // every seat ratified, and a founder finishing a human deliberation is not
    // a failure. But silence and progress looked identical here — a member
    // whose founder died sat on this loop forever with nothing on screen. So
    // the wait says how long it has been waiting (cluster F's deferred
    // elapsed line).
    let waiting_since = tokio::time::Instant::now();
    let mut noted_secs: u64 = 0;
    // Genesis wait: the sealed roster as a 445. Sign-what-you-see closes
    // HERE — the distributed table must byte-equal the ratified one.
    let sealed_final = loop {
        let (content, created_at) = match sub.recv(RECV_SLICE).await {
            molt_net::ritual_net::GroupRecv::Frame { content, created_at } => {
                if was_deaf {
                    was_deaf = false;
                    let _ = send_cmd(
                        tx,
                        Command::NetJoinNote {
                            note: "✓ the group channel is back".to_string(),
                            generation,
                        },
                    )
                    .await;
                }
                (content, created_at)
            }
            molt_net::ritual_net::GroupRecv::Idle => {
                let waited = waiting_since.elapsed().as_secs();
                if let Some(next) = genesis_wait_note(waited, noted_secs) {
                    noted_secs = next;
                    let _ = send_cmd(
                        tx,
                        Command::NetJoinNote {
                            note: format!("⧗ waiting for the genesis · {}", elapsed_label(next)),
                            generation,
                        },
                    )
                    .await;
                }
                continue;
            }
            // loud, never fatal — the join keeps waiting, visibly
            molt_net::ritual_net::GroupRecv::Deaf(why) => {
                was_deaf = true;
                let _ = send_cmd(
                    tx,
                    Command::NetJoinNote {
                        note: format!("⚠ cannot hear the group channel — {why} · still retrying"),
                        generation,
                    },
                )
                .await;
                continue;
            }
        };
        let Some((msg, from)) = open_group_frame(&group, &content, created_at) else {
            continue;
        };
        // …and in the genesis wait too, on the same gate
        if let RitualMsg::Aborted { reason } = &msg {
            if frame_is_from_founder(&from, &ctx.invite.info) {
                return Err(format!("the founder ended this founding: {reason}"));
            }
            tracing::warn!(%from, "abort frame from a co-member — ignored");
            continue;
        }
        if let RitualMsg::Genesis { sealed, .. } = msg {
            // same rule as the Seal: a co-member's frame is ignored, not
            // fatal — otherwise one forged Genesis published first kills
            // every honest joiner while blaming the founder
            let Ok(sealed) = serde_json::from_str::<molt_core::SealedRoster>(&sealed) else {
                tracing::debug!(%from, "unparseable genesis on the group channel — ignored");
                continue;
            };
            match check_proposal_provenance(&from, &sealed, &h.npub, &ctx.invite.info) {
                Provenance::NotTheFounder => {
                    tracing::warn!(%from, "a non-founder published a genesis — ignored");
                    continue;
                }
                Provenance::Refused(why) => {
                    return Err(format!("distributed sealed roster rejected: {why}"))
                }
                Provenance::FromFounder => {}
            }
            let sealed_table =
                crate::founding::verify_seal_proposal(&sealed, &ctx.member, &pk, &nostr_pk)
                    .map_err(|e| format!("distributed sealed roster rejected: {e}"))?;
            if sealed_table != table {
                return Err(
                    "the sealed roster is not the table we ratified — the founder \
                     distributed a different constitution"
                        .to_string(),
                );
            }
            break sealed;
        }
    };

    let snap = {
        let g = group.lock().map_err(|_| "mls lock poisoned".to_string())?;
        g.snapshot().map_err(|e| format!("mls snapshot: {e}"))?
    };
    let sealed_json = serde_json::to_string(&sealed_final).map_err(|e| e.to_string())?;
    let _ = send_cmd(
        tx,
        Command::NetJoinSealed {
            sealed: sealed_json,
            mls: hex::encode(snap),
            mesh: Vec::new(),
            nostr_sk: hex::encode(nostr_sk.as_slice()),
            relays: h.relays,
            rotation_seed: hex::encode(payload.rotation_seed),
            generation,
        },
    )
    .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(inviter: &str, m: u8, n: u8) -> molt_core::InviteInfo {
        molt_core::InviteInfo {
            republic: "R".to_string(),
            threshold: m,
            members: n,
            inviter: inviter.to_string(),
            ticket: "ab".repeat(5),
        }
    }

    fn identity(member: &str, npk: &str) -> molt_core::MemberIdentity {
        molt_core::MemberIdentity {
            member: member.to_string(),
            identity_pk: "aa".repeat(32),
            nostr_pk: npk.to_string(),
        }
    }

    fn proposal(
        inviter: &str,
        founder_npk: &str,
        m: u8,
        n: u8,
    ) -> molt_core::SealedRoster {
        molt_core::SealedRoster {
            name: "R".to_string(),
            republic_id: "rid".to_string(),
            rule_m: m,
            rule_n: n,
            roster: vec![inviter.to_string(), "petra".to_string()],
            identities: vec![
                identity(inviter, founder_npk),
                identity("petra", &"cc".repeat(32)),
            ],
            attestations: Vec::new(),
            agenda: String::new(),
            relays: Vec::new(),
        }
    }

    const FOUNDER_NPK: &str = "dd11dd11dd11dd11dd11dd11dd11dd11dd11dd11dd11dd11dd11dd11dd11dd11";

    /// SECURITY (cluster F2) — the abort arm is a KILL SWITCH, so it is gated
    /// on the MLS-authenticated author exactly like the Seal.
    ///
    /// The 445 channel is shared: every welcomed seat can publish a frame the
    /// others decrypt. An ungated abort would therefore let any invitee end
    /// every other invitee's join with one frame — the same impersonation
    /// class fixed as CRITICAL in 63555dc, re-entering through a new door.
    ///
    /// This pin only secures anything while BOTH group-channel arms call the
    /// helper; if a later refactor inlines the check back, it dies silently.
    #[test]
    fn only_the_founder_can_abort_a_founding() {
        let inv = molt_core::InviteInfo {
            republic: "R".to_string(),
            threshold: 2,
            members: 2,
            inviter: "walter".to_string(),
            ticket: "ab".repeat(5),
        };
        assert!(frame_is_from_founder("walter", &inv), "the founder may end it");
        assert!(
            !frame_is_from_founder("petra", &inv),
            "a welcomed co-member must NOT be able to kill another seat's join"
        );
        assert!(!frame_is_from_founder("", &inv), "an unauthenticated frame may not");
    }

    /// KEYSTONE (review 2026-08-01, CRITICAL) — on the loopback path the
    /// `Seal` arrived on the member's OWN reply queue, which only the founder
    /// held: that private channel WAS the proposer authentication. Over the
    /// SHARED kind-445 group channel every welcomed seat can publish a
    /// decryptable frame, so the binding has to be re-established explicitly.
    ///
    /// Without it a legitimate co-invitee can hand another invitee a roster
    /// it wrote itself — same republic id math, the victim's own three
    /// anchors intact, so `verify_seal_proposal` passes — and the victim
    /// ratifies and materializes a republic the attacker alone governs.
    #[test]
    fn a_proposal_must_come_from_the_founder_named_in_the_link() {
        let inv = info("walter", 2, 2);
        let p = proposal("walter", FOUNDER_NPK, 2, 2);

        assert_eq!(
            check_proposal_provenance("walter", &p, FOUNDER_NPK, &inv),
            Provenance::FromFounder,
            "the founder's own proposal passes"
        );

        // the attack: a co-invitee publishes its own table on the group
        // channel. It must be IGNORED, not fatal — a fatal verdict would let
        // any invitee kill every other invitee's join with one frame.
        assert_eq!(
            check_proposal_provenance("petra", &p, FOUNDER_NPK, &inv),
            Provenance::NotTheFounder,
            "a co-member's proposal is ignored, never signed and never fatal"
        );
    }

    /// The founder's seat in the proposed table must carry the transport
    /// anchor the LINK named. Otherwise an attacker who also controls the
    /// handle (or a founder handle collision) could still swap the seat.
    #[test]
    fn the_proposal_must_anchor_the_founder_from_the_link() {
        let inv = info("walter", 2, 2);
        let mut p = proposal("walter", FOUNDER_NPK, 2, 2);
        p.identities[0].nostr_pk = "ee".repeat(32);
        let Provenance::Refused(err) =
            check_proposal_provenance("walter", &p, FOUNDER_NPK, &inv)
        else {
            panic!("a table anchoring a different founder key must be refused");
        };
        assert!(err.contains("transport key"), "{err}");

        // …and the founder must actually BE in the table
        let mut p = proposal("walter", FOUNDER_NPK, 2, 2);
        p.identities.remove(0);
        let Provenance::Refused(err) =
            check_proposal_provenance("walter", &p, FOUNDER_NPK, &inv)
        else {
            panic!("a table without the founder's seat must be refused");
        };
        assert!(err.contains("does not seat the founder"), "{err}");
    }

    /// The link promised an m-of-n republic; a table that quietly changes
    /// the threshold (the 1-of-2 downgrade that hands one member total
    /// control) must not pass, even when every anchor checks out.
    #[test]
    fn the_proposal_must_match_the_rule_the_link_promised() {
        let inv = info("walter", 2, 2);
        let p = proposal("walter", FOUNDER_NPK, 1, 2);
        let Provenance::Refused(err) =
            check_proposal_provenance("walter", &p, FOUNDER_NPK, &inv)
        else {
            panic!("a downgraded threshold must be refused");
        };
        assert!(err.contains("2-of-2"), "the refusal names both rules: {err}");

        let p = proposal("walter", FOUNDER_NPK, 2, 3);
        assert!(matches!(
            check_proposal_provenance("walter", &p, FOUNDER_NPK, &inv),
            Provenance::Refused(_)
        ));
    }
}

/// Whether the genesis wait is due for an elapsed note, and the mark to
/// record when it is.
///
/// The wait itself is unbounded on purpose — the genesis lands after a HUMAN
/// deliberation, so a slow founder is not a failure. What was wrong is that
/// waiting and being stranded looked identical. Notes go out on a widening
/// ladder rather than a fixed tick: the first minutes are when an operator
/// wonders whether anything is happening, and an hour in a line every 30 s
/// would be noise burying the run log.
fn genesis_wait_note(waited_secs: u64, last_noted: u64) -> Option<u64> {
    const LADDER: [u64; 6] = [30, 120, 300, 900, 1_800, 3_600];
    let due = LADDER
        .iter()
        .rev()
        .find(|mark| waited_secs >= **mark && **mark > last_noted)?;
    Some(*due)
}

/// A wait as few words: `45s`, `2m`, `1h30m`. Never a sentence — this rides
/// inside a run-log line that already says what is being waited for.
fn elapsed_label(secs: u64) -> String {
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    let (h, m) = (mins / 60, mins % 60);
    if m == 0 {
        format!("{h}h")
    } else {
        format!("{h}h{m}m")
    }
}

#[cfg(test)]
mod wait_note_tests {
    use super::*;

    /// A note fires once per ladder rung, never twice, and never before its
    /// rung is reached.
    #[test]
    fn the_elapsed_note_fires_once_per_rung() {
        assert_eq!(genesis_wait_note(0, 0), None, "no note before the first rung");
        assert_eq!(genesis_wait_note(29, 0), None);
        assert_eq!(genesis_wait_note(30, 0), Some(30), "the first rung");
        assert_eq!(genesis_wait_note(31, 30), None, "…and not again on the same rung");
        assert_eq!(genesis_wait_note(120, 30), Some(120), "the next rung");
        // a long stall that skipped rungs reports the HIGHEST reached, not a
        // burst of every rung it slept through
        assert_eq!(genesis_wait_note(4_000, 30), Some(3_600));
        // past the last rung nothing repeats — the log stops growing
        assert_eq!(genesis_wait_note(100_000, 3_600), None);
    }

    /// The label is a few characters, never a sentence.
    #[test]
    fn the_elapsed_label_is_compact() {
        assert_eq!(elapsed_label(45), "45s");
        assert_eq!(elapsed_label(60), "1m");
        assert_eq!(elapsed_label(150), "2m");
        assert_eq!(elapsed_label(3_600), "1h");
        assert_eq!(elapsed_label(5_400), "1h30m");
        for s in [0, 59, 60, 3_599, 3_600, 86_400] {
            assert!(elapsed_label(s).len() <= 6, "{}", elapsed_label(s));
        }
    }
}

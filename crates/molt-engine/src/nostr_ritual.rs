// SPDX-License-Identifier: GPL-3.0-or-later

//! N4a (`docs_archive/transport/nostr_n4_plan.md` §7): the engine-side Nostr ritual
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

use crate::ritual_member::{run_member_ladder, MemberSeat, Phase, Ratify, RitualLeg};
use crate::Envelope;

/// Long-poll slice for the unbounded waits — short enough that an aborted
/// task dies promptly, long enough to stay quiet.
const RECV_SLICE: Duration = Duration::from_secs(30);

/// How many frames after the Welcome a rejoiner logs one by one.
const FRAMES_LOGGED: u64 = 20;

/// The event kind a rejoin frame carries, for its log line.
fn event_kind(body: &molt_core::WorkspaceEvent) -> &'static str {
    match body {
        molt_core::WorkspaceEvent::CheckpointServed { .. } => "checkpoint",
        molt_core::WorkspaceEvent::CheckpointServedPart { .. } => "checkpoint-part",
        molt_core::WorkspaceEvent::Committed(_) => "block",
        molt_core::WorkspaceEvent::MlsCommit { .. } => "mls-commit",
        molt_core::WorkspaceEvent::Chat(_) => "chat",
        _ => "other",
    }
}

/// `hh:mm:ss` UTC of a unix stamp.
fn hms(unix: u64) -> String {
    let d = unix % 86_400;
    format!("{:02}:{:02}:{:02}", d / 3600, (d / 60) % 60, d % 60)
}

/// How far before its recovery request a rejoiner still reads group frames
/// (clock skew between it and the coordinator).
const RECENT_SKEW_SECS: u64 = 600;
/// Best-effort EOSE wait after placing a subscription.
const LIVE_WAIT: Duration = Duration::from_secs(10);
/// A ritual's group-channel replay (founder and joiner): an onion relay over
/// Tor replays a busy channel for longer than `LIVE_WAIT` (field 2026-09-12).
const REPLAY_WAIT: Duration = Duration::from_secs(120);
/// Backoff of a ritual publish or subscription that keeps failing: nothing
/// on the ritual path gives up on its own.
const PUBLISH_RETRY_INITIAL: Duration = Duration::from_secs(2);
const PUBLISH_RETRY_CAP: Duration = Duration::from_secs(60);

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
    /// The recovery phrase — wiped on drop (review R9).
    pub phrase: zeroize::Zeroizing<String>,
    pub generation: u64,
    /// This build runs the chain lock: the JoinRequest declares it.
    pub lock: bool,
}

/// How many `Committed` frames the rejoiner buffers while it waits for a
/// verifiable run, and how far above the lowest seen height a frame may
/// sit (review R7: a flood of distinct heights is bounded, not buffered).
const RECOVER_BLOCK_BUFFER: usize = 256;
const RECOVER_BLOCK_WINDOW: u64 = 256;

/// Send one engine-internal command through the weak handle. `false` = the
/// engine is gone — the caller returns (its work is moot).
pub(crate) async fn send_cmd(tx: &mpsc::WeakSender<Envelope>, cmd: Command) -> bool {
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
        // subscribe-BEFORE-advertise only means something if the
        // subscription is proven READABLE. A relay that accepts the
        // connection and the REQ but never replays (auth required, rate
        // limited, CLOSED-then-refused) left us publishing seat links over an
        // inbox nothing would ever answer on. Not readable yet = keep
        // trying, loudly; no link is published before it is.
        let mut wait = PUBLISH_RETRY_INITIAL;
        let mut inbox = loop {
            let why = match net.inbox().await {
                Ok(mut inbox) => {
                    let st = inbox.read_state(LIVE_WAIT).await;
                    if st.any() {
                        if !st.full() {
                            tracing::warn!(
                                answered = st.answered,
                                connected = st.connected,
                                "the founding inbox answered on only some relays"
                            );
                        }
                        break inbox;
                    }
                    "not readable on any relay".to_string()
                }
                Err(e) => e.to_string(),
            };
            let note = Command::NetRitualNote {
                note: format!("⚠ the founding inbox is not up - {why} · retrying"),
                generation: Some(generation),
            };
            if !send_cmd(&tx, note).await {
                return;
            }
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(PUBLISH_RETRY_CAP);
        };
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
                relays: j.relays,
                vault_pk: j.vault_pk,
                caps: j.caps,
                generation: Some(generation),
            };
            if !send_cmd(&tx, cmd).await {
                return;
            }
        }
    })
}

/// The recovery-side frames an inbox carries to the actor (the PoP
/// comparisons live there, with every other check; this only carries the
/// PROVEN sealer across).
fn recover_inbox_command(delivery: RitualDelivery, generation: u64) -> Option<Command> {
    let RitualDelivery::Msg(msg, sender) = delivery else {
        return None;
    };
    Some(match msg {
        RitualMsg::Recover(r) => crate::founding::recover_command(r, sender, generation),
        RitualMsg::RecoverFailed { member, reason, version, attempt } => {
            Command::NetRecoverPeerFailed { member, reason, version, attempt, sender }
        }
        RitualMsg::RecoverWant { member, ticket, missing, attempt } => {
            Command::NetRecoverWant { member, ticket, missing, attempt, sender, generation: Some(generation) }
        }
        RitualMsg::RecoverReady { member, ticket, attempt } => {
            Command::NetRecoverReady { member, ticket, attempt, sender, generation: Some(generation) }
        }
        _ => return None,
    })
}

/// Subscribe `net`'s 1059 inbox until a relay answers, retrying on a
/// backoff; `waiting` is told why on every failed round. `None` = the
/// engine is gone.
async fn readable_inbox(
    net: &RitualNet,
    what: &'static str,
    tx: &mpsc::WeakSender<Envelope>,
    waiting: impl Fn(&'static str) -> Option<Command>,
) -> Option<molt_net::ritual_net::RitualInbox> {
    let mut wait = PUBLISH_RETRY_INITIAL;
    loop {
        let (why, short) = match net.inbox().await {
            Ok(mut inbox) => {
                let st = inbox.read_state(LIVE_WAIT).await;
                if st.any() {
                    if !st.full() {
                        tracing::warn!(inbox = what, answered = st.answered, connected = st.connected, "inbox_partial");
                    }
                    return Some(inbox);
                }
                ("no relay answered".to_string(), "no relay answers")
            }
            Err(e) => (e.to_string(), "no relay reachable"),
        };
        if let Some(cmd) = waiting(short) {
            if !send_cmd(tx, cmd).await {
                return None;
            }
        }
        tracing::warn!(inbox = what, error = %why, retry_in_s = wait.as_secs(), "inbox_down");
        tokio::time::sleep(wait).await;
        tx.upgrade()?;
        wait = (wait * 2).min(PUBLISH_RETRY_CAP);
    }
}

/// The STANDING seat inbox (`detached_reattach.md` §2.1): an open Nostr
/// workspace listens on its OWN 1059 anchor for the workspace's lifetime.
/// It carries the recovery frames - a link-less request (refused since
/// B3) and the anchor-first rejoiner's Want/Ready, which a restarted
/// coordinator answers `link outdated`. Torn down with the net teardown;
/// commands are scoped by `generation` (net scope).
pub(crate) fn spawn_seat_inbox(
    net: RitualNet,
    generation: u64,
    tx: mpsc::WeakSender<Envelope>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let Some(mut inbox) = readable_inbox(&net, "seat", &tx, |_| None).await else {
            return;
        };
        loop {
            let Some(delivery) = inbox.recv(RECV_SLICE).await else {
                continue; // idle slice
            };
            let Some(cmd) = recover_inbox_command(delivery, generation) else {
                continue;
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
/// device — there is no second channel to notice on. Until a relay answers
/// the mint stays pending, retrying.
///
/// The inbox lives until the WORKSPACE CLOSES, which is also exactly how long
/// its ticket stays spendable. It therefore holds only a WEAK sender: the
/// loop must never keep a dropped engine's actor (writer thread, workspace
/// flock) alive.
#[allow(clippy::too_many_arguments)] // one mint's handover fields, not a bag
pub(crate) fn spawn_recovery_inbox(
    net: RitualNet,
    member: String,
    ticket: String,
    republic: String,
    republic_id: String,
    // the seat's ANCHORED identity pk — rides the link so the rejoiner can
    // resolve the ritual's derivation convention (founder vs joiner salt)
    identity_pk: String,
    // what the rejoiner's v2 consent binds (recovery_auto_approval.md §7e)
    consent_anchor: Option<molt_core::chain::ConsentAnchor>,
    generation: u64,
    tx: mpsc::WeakSender<Envelope>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let waiting = |reason: &str| {
            Some(Command::NetRecoverLinkWaiting {
                member: member.clone(),
                reason: reason.to_string(),
                generation: Some(generation),
            })
        };
        let Some(mut inbox) = readable_inbox(&net, "recovery", &tx, waiting).await else {
            return;
        };
        let handover = invite::RecoveryHandoverV2 {
            ticket: ticket.clone(),
            npub: net.pk_hex(),
            relays: net.relays().to_vec(),
            republic_id: republic_id.clone(),
            identity_pk,
            consent_anchor: consent_anchor.clone(),
        };
        // encode EXPLICITLY: `RecoveryInvite::render` falls back to the legacy
        // queue shape when the handover cannot encode, and with the empty
        // queue fields below that fallback is `hex("\n\n\n<republic_id>")` — a
        // link that parses as nothing, handed to the operator as a success.
        // Every early exit unregisters the ticket through the failure lane,
        // so a dead mint never leaves a live ticket behind.
        if let Err(e) = handover.encode() {
            let fail = Command::NetRecoverLinkFailed {
                member: member.clone(),
                reason: format!("rendering recovery link: {e}"),
                ticket: ticket.clone(),
                generation: Some(generation),
            };
            let _ = send_cmd(&tx, fail).await;
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
            consent_anchor,
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
            let Some(cmd) = recover_inbox_command(delivery, generation) else {
                continue;
            };
            if !send_cmd(&tx, cmd).await {
                return;
            }
        }
    })
}

/// Gift-wrap each of `frames` to `to`, in order, each until a relay takes
/// it (backoff 2 s .. 60 s, no attempt cap). Ends with the engine.
pub(crate) fn spawn_recover_frames(
    net: RitualNet,
    to: String,
    frames: Vec<RitualMsg>,
    what: &'static str,
    counts: Arc<crate::chain::RecoverSendCounts>,
    tx: mpsc::WeakSender<Envelope>,
) -> tokio::task::JoinHandle<()> {
    use std::sync::atomic::Ordering::Relaxed;
    counts.sent.fetch_add(frames.len(), Relaxed);
    tokio::spawn(async move {
        let count = frames.len();
        for msg in frames {
            let mut wait = PUBLISH_RETRY_INITIAL;
            while let Err(e) = net.send_ritual(&to, &msg).await {
                tracing::warn!(what, error = %e, retry_in_s = wait.as_secs(), "recover_frame=unsent");
                tokio::time::sleep(wait).await;
                if tx.upgrade().is_none() {
                    return;
                }
                wait = (wait * 2).min(PUBLISH_RETRY_CAP);
            }
            counts.accepted.fetch_add(1, Relaxed);
        }
        tracing::info!(what, frames = count, "recover_frames=published");
    })
}

/// **Deliver a Nostr re-key** (N4b step 6c): the commit as a kind-445 at its
/// PINNED carrier stamp, and then the gift-wrapped kind-444 Welcome to the
/// returning seat's NEW anchor - each until a relay takes it (backoff
/// 2 s .. 60 s, no attempt cap; B4). Ends with the engine.
///
/// The order is load-bearing and so is the coupling:
///
/// - The **commit first**. It is what moves every survivor to the epoch the
///   Welcome puts the rejoiner at. A frame that beats its commit is held and
///   retried (N5.3c), so the order is a preference rather than a guarantee —
///   but publishing the Welcome first would make that hold the normal path
///   instead of the exception.
/// - The **Welcome only after the commit landed**. A Welcome without its
///   commit puts the rejoiner at an epoch no survivor ever reaches — a
///   split, with nothing in the product to heal it.
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
    tx: mpsc::WeakSender<Envelope>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut wait = PUBLISH_RETRY_INITIAL;
        let mut attempt = 0u32;
        loop {
            attempt = attempt.saturating_add(1);
            match channel.publish_frame_at(&rekey.prev_exporter, &rekey.commit, rekey.stamp).await {
                Ok((stamp, report)) => {
                    tracing::info!(%member, stamp, relays = report.accepted.len(), "rekey_commit=published");
                    break;
                }
                Err(e) => {
                    tracing::warn!(%member, attempt, error = %e, retry_in_s = wait.as_secs(), "rekey_commit=unsent");
                }
            }
            tokio::time::sleep(wait).await;
            if tx.upgrade().is_none() {
                return;
            }
            wait = (wait * 2).min(PUBLISH_RETRY_CAP);
        }
        let mut wait = PUBLISH_RETRY_INITIAL;
        let mut attempt = 0u32;
        loop {
            attempt = attempt.saturating_add(1);
            match net.send_welcome(&to_npub, &payload).await {
                Ok(report) => {
                    tracing::info!(%member, relays = report.accepted.len(), "recover_welcome=published");
                    return;
                }
                Err(e) => {
                    tracing::warn!(%member, attempt, error = %e, retry_in_s = wait.as_secs(), "recover_welcome=unsent");
                }
            }
            tokio::time::sleep(wait).await;
            if tx.upgrade().is_none() {
                return;
            }
            wait = (wait * 2).min(PUBLISH_RETRY_CAP);
        }
    })
}

/// MLS-encrypt one RitualMsg, ONCE: a retry republishes these bytes (see
/// [`FramePayload`]). The lock is released before any await.
fn seal_frame(
    group: &Arc<Mutex<molt_net::MlsMember>>,
    msg: &RitualMsg,
) -> Result<(Vec<u8>, [u8; 32]), String> {
    let payload = serde_json::to_vec(msg).map_err(|e| e.to_string())?;
    let mut g = group.lock().map_err(|_| "mls lock poisoned".to_string())?;
    let ct = g.encrypt(&payload).map_err(|e| e.to_string())?;
    let exporter = g.exporter_secret().map_err(|e| e.to_string())?;
    Ok((ct, exporter))
}

/// What a 445 publish task sends. Encrypted ONCE, before any retry.
///
/// Re-encrypting on retry would advance the MLS sender ratchet past the
/// snapshot `finalize_founding` deliberately takes AFTER the genesis encrypt,
/// and every member would meet `SecretReuseError` on reopen. So the retry
/// republishes the SAME ciphertext — for an application frame that is safe:
/// the carrier stamp enters no decrypt.
pub(crate) enum FramePayload {
    /// Encrypt this message against the group, then publish. Boxed: a
    /// `RitualMsg` (a `RecoverRequest` is its widest arm) dwarfs `Sealed`.
    Encrypt(Arc<Mutex<molt_net::MlsMember>>, Box<RitualMsg>),
    /// Already sealed by the caller (the genesis, encrypted before the group
    /// snapshot) — publish these bytes as they are.
    Sealed { ct: Vec<u8>, exporter: [u8; 32] },
}

/// How hard a leg tries, and whether it keeps re-publishing afterwards.
#[derive(Clone, Copy)]
pub(crate) struct RetryPolicy {
    /// Publish attempts before failure is reported (`u32::MAX` = never
    /// give up; failures are noted instead).
    pub(crate) attempts: u32,
    /// Delay before the 2nd attempt; doubles up to [`PUBLISH_RETRY_CAP`].
    pub(crate) backoff: Duration,
    /// Delays (seconds) of the re-publishes after the first success; the
    /// last one repeats forever when `repeat_forever`.
    pub(crate) repeat: &'static [u64],
    pub(crate) repeat_forever: bool,
}

impl RetryPolicy {
    /// The charter proposal: one-shot for the founder, so it never gives
    /// up, and it is re-published until the ritual ends (the caller aborts
    /// the task) - a member may read its Welcome days later.
    pub(crate) const SEAL: Self = Self {
        attempts: u32::MAX,
        backoff: Duration::from_millis(700),
        repeat: &[900],
        repeat_forever: true,
    };
    /// The farewell of a ritual that is going away: bounded.
    pub(crate) const ABORT: Self = Self {
        attempts: 3,
        backoff: Duration::from_millis(700),
        repeat: &[],
        repeat_forever: false,
    };
    /// The genesis: the founder has already materialized, so this frame is
    /// the members' ONLY path into the republic. A first failure is
    /// reported; either way it is re-published, every three days once the
    /// ladder ends, until the workspace closes (B4: no end).
    pub(crate) const GENESIS: Self = Self {
        attempts: 4,
        backoff: Duration::from_millis(900),
        repeat: &[60, 300, 900, 3_600, 10_800, 21_600, 43_200, 86_400, 172_800, 259_200],
        repeat_forever: true,
    };
}

/// Publish one 445 frame, retrying the PUBLISH only, and ALWAYS report the
/// first per-relay outcome back to the actor; then re-publish the SAME
/// bytes as the policy says.
///
/// The sink is not optional. It used to be `Option<...>`, every caller passed
/// `None`, and the reporting path was dead code — so a refused Seal hung both
/// sides in silence. Deleting the Option deletes the seam a future call site
/// can forget to wire.
#[allow(clippy::too_many_arguments)] // one leg's wiring, not a bag
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
    // set = no further re-publish (the first publish is never cut short)
    stop: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // encrypt ONCE — see FramePayload
        let sealed: Result<(Vec<u8>, [u8; 32]), String> = match payload {
            FramePayload::Sealed { ct, exporter } => Ok((ct, exporter)),
            FramePayload::Encrypt(group, msg) => seal_frame(&group, &msg),
        };
        let report = |accepted: Vec<String>, failed: Vec<String>| Command::NetRitualPublished {
            what: what.to_string(),
            accepted,
            failed,
            generation,
            workspace: workspace.clone(),
        };
        let (ct, exporter) = match sealed {
            Ok(v) => v,
            Err(e) => {
                // encryption failed: no publish will ever help
                tracing::error!(what, error = %e, "ritual 445 frame could not be encrypted");
                let _ = send_cmd(&tx, report(Vec::new(), vec![format!("encrypt: {e}")])).await;
                return;
            }
        };

        let mut wait = retry.backoff;
        let mut attempt = 0u32;
        let published = loop {
            attempt = attempt.saturating_add(1);
            match chan.publish_frame(&exporter, &ct).await {
                Ok((_stamp, report)) => break Ok(report),
                Err(e) => {
                    tracing::warn!(what, attempt, error = %e, "ritual 445 frame did not publish");
                    if attempt >= retry.attempts {
                        break Err(e.to_string());
                    }
                    if attempt >= 3 {
                        let note = Command::NetRitualNote {
                            note: format!("⚠ {what} did not publish - {e} · retrying"),
                            generation,
                        };
                        if !send_cmd(&tx, note).await {
                            return;
                        }
                    }
                    tokio::time::sleep(wait).await;
                    wait = wait.saturating_mul(2).min(PUBLISH_RETRY_CAP);
                }
            }
        };
        let first = match published {
            Ok(r) => report(
                r.accepted.clone(),
                r.failed.iter().map(|(url, why)| format!("{url}: {why}")).collect(),
            ),
            // every attempt refused: accepted stays EMPTY, which is what the
            // handler reads as "nobody has this frame"
            Err(e) => report(Vec::new(), vec![e]),
        };
        if !send_cmd(&tx, first).await {
            return;
        }
        let mut delays = retry.repeat.iter().copied();
        let mut last = None;
        loop {
            let Some(secs) = delays.next().or(last.filter(|_| retry.repeat_forever)) else {
                return;
            };
            last = Some(secs);
            tokio::time::sleep(Duration::from_secs(secs)).await;
            let stopped = stop
                .as_ref()
                .is_some_and(|s| s.load(std::sync::atomic::Ordering::SeqCst));
            if stopped || tx.upgrade().is_none() {
                return;
            }
            if let Err(e) = chan.publish_frame(&exporter, &ct).await {
                tracing::warn!(what, error = %e, "ritual 445 frame did not re-publish");
            }
        }
    })
}

/// Deliver one seat's Welcome, retrying until a relay takes it.
pub(crate) fn spawn_welcome(
    net: RitualNet,
    npub: String,
    payload: molt_net::welcome::WelcomePayload,
    generation: u64,
    tx: mpsc::WeakSender<Envelope>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut wait = PUBLISH_RETRY_INITIAL;
        while let Err(e) = net.send_welcome(&npub, &payload).await {
            tracing::warn!(error = %e, "welcome did not publish");
            let note = Command::NetRitualNote {
                note: format!("⚠ a welcome did not publish - {e} · retrying"),
                generation: Some(generation),
            };
            if !send_cmd(&tx, note).await {
                return;
            }
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(PUBLISH_RETRY_CAP);
        }
    })
}

/// Open one 445 frame against the group: strip the outer exporter layer
/// (current secret first, then the ring), then MLS-decrypt. Undecryptable
/// frames (our own publishes echoed back, replays across a rewind) are a
/// normal part of the stream — skipped, never fatal.
fn open_group_frame(
    group: &Arc<Mutex<molt_net::MlsMember>>,
    content: &str,
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
    match g.decrypt(&wire) {
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
            tracing::debug!(?other, "non-application 445 during the ritual - skipped");
            None
        }
        Err(e) => {
            tracing::debug!(error = %e, "445 frame did not decrypt (own echo or replay)");
            None
        }
    }
}

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

/// **The 445 proposer binding** (review 2026-08-01, CRITICAL), second half.
///
/// On the loopback path `Seal`/`Genesis` arrive on the member's OWN reply
/// queue, wrapped under a key it minted and handed only to the founder in
/// its MAC-bound JoinRequest — the CHANNEL is the proposer authentication,
/// so no code ever has to check it. The kind-445 group channel is shared by
/// every welcomed seat, so that binding has to be re-established explicitly:
/// [`frame_is_from_founder`] gates the AUTHOR (a co-member's frame is
/// ignored, never fatal — or any invitee could abort every other invitee's
/// join at will), and this check pins a founder-authored proposal to what
/// the invite LINK promised — the rule, and the founder's own seat under the
/// anchor the link named — because the link is the joiner's root of trust
/// for "whose founding is this". An `Err` is a real failure the joiner must
/// surface rather than sign.
pub(crate) fn check_proposal_against_link(
    proposal: &molt_core::SealedRoster,
    founder_npub: &str,
    info: &molt_core::InviteInfo,
) -> Result<(), String> {
    if proposal.rule_m != info.threshold || proposal.rule_n != info.members {
        return Err(format!(
            "the proposed republic is {}-of-{}, but the invite promised {}-of-{}",
            proposal.rule_m, proposal.rule_n, info.threshold, info.members
        ));
    }
    let Some(founder_seat) = proposal
        .identities
        .iter()
        .find(|i| i.member == info.inviter)
    else {
        return Err("the proposed roster does not seat the founder who invited us".to_string());
    };
    if founder_seat.nostr_pk != founder_npub {
        return Err(
            "the founder's seat carries a transport key other than the one the \
             invite link named"
                .to_string(),
        );
    }
    Ok(())
}

/// How far back a JOINER's group subscription reaches: it cannot know when
/// the ritual started, and a Seal may be days old when its Welcome is read.
const MEMBER_LOOKBACK_SECS: u64 = 30 * 86_400;

/// A ritual group subscription covering every window since `since`, proven
/// readable on at least one relay.
async fn readable_group_sub(
    chan: &GroupChannel,
    since: u64,
) -> Result<molt_net::ritual_net::GroupSub, String> {
    let mut sub = chan.subscribe_from(since).await.map_err(|e| e.to_string())?;
    if sub.read_state(REPLAY_WAIT).await.any() {
        Ok(sub)
    } else {
        Err("not readable on any relay".to_string())
    }
}

/// The founder's 445 recv: `Signed`/`Declined` come back over the freshly
/// born group and land in the SAME actor handlers as the loopback path.
pub(crate) fn spawn_founder_group_recv(
    chan: GroupChannel,
    group: Arc<Mutex<molt_net::MlsMember>>,
    // the ritual's start (unix secs): frames from every window since then
    since: u64,
    generation: u64,
    tx: mpsc::WeakSender<Envelope>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // a founder whose 445 subscription never replays would wait forever
        // for Signed frames that cannot arrive: retry until it is readable
        let mut wait = PUBLISH_RETRY_INITIAL;
        let mut sub = loop {
            match readable_group_sub(&chan, since).await {
                Ok(sub) => break sub,
                Err(why) => {
                    let note = Command::NetRitualNote {
                        note: format!("⚠ cannot hear the group channel - {why} · still retrying"),
                        generation: Some(generation),
                    };
                    if !send_cmd(&tx, note).await {
                        return;
                    }
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(PUBLISH_RETRY_CAP);
                }
            }
        };
        let mut was_deaf = false;
        loop {
            let content = match sub.recv(RECV_SLICE).await {
                molt_net::ritual_net::GroupRecv::Frame { content, .. } => {
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
                    content
                }
                molt_net::ritual_net::GroupRecv::Idle => continue,
                // loud, never fatal: a one-shot founding must survive a blip
                molt_net::ritual_net::GroupRecv::Deaf(why) => {
                    was_deaf = true;
                    let _ = send_cmd(
                        &tx,
                        Command::NetRitualNote {
                            note: format!(
                                "⚠ cannot hear the group channel - {why} · still retrying"
                            ),
                            generation: Some(generation),
                        },
                    )
                    .await;
                    continue;
                }
            };
            let Some((msg, from)) = open_group_frame(&group, &content) else {
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
                // the backup attestation (❻½): verified against the anchored
                // key on the actor, authenticated author rides along like
                // `Signed`
                RitualMsg::BackupConfirmed { seat, sig } => Command::NetBackupConfirmed {
                    seat,
                    sig,
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
    /// The recovery phrase — wiped on drop (review R9).
    pub phrase: zeroize::Zeroizing<String>,
    pub generation: u64,
    /// Set once the run saw its re-admission seal (a cancel cannot undo it).
    pub readmitted: Arc<std::sync::atomic::AtomicBool>,
    /// Set once the run asked for its card (Ready, or a vote report came).
    pub asked: Arc<std::sync::atomic::AtomicBool>,
    /// This build runs the chain lock: the request declares it.
    pub lock: bool,
}

/// What a quitting rejoiner sends: its run's withdrawal, to the
/// coordinator, over its own endpoint. Filled once the request is out.
type Withdrawal = Arc<Mutex<Option<(RitualNet, String, RitualMsg)>>>;

/// One inbound 445 as an engine envelope. The recovery anchor rides the
/// coordinator's ordinary outbox, so it arrives as an `EventEnvelope` rather
/// than a `RitualMsg` — the two shapes are provably disjoint
/// (`molt-net/tests/frame_disjointness.rs`), so trying one is not ambiguous.
fn open_group_envelope(
    group: &Arc<Mutex<molt_net::MlsMember>>,
    content: &str,
) -> Option<molt_core::EventEnvelope> {
    let mut g = group.lock().ok()?;
    let mut secrets = Vec::with_capacity(1 + molt_net::mls::EXPORTER_RING_K);
    if let Ok(current) = g.exporter_secret() {
        secrets.push(current);
    }
    secrets.extend_from_slice(g.exporter_ring());
    let wire = molt_net::envelope::open_outer(&secrets, content).ok()?;
    match g.decrypt(&wire) {
        Ok(molt_net::MlsIncoming::Application { plaintext, .. }) => {
            serde_json::from_slice(&plaintext).ok()
        }
        _ => None,
    }
}

/// A waiting step says what it waits for at least this often.
const WAIT_NOTE_EVERY: Duration = Duration::from_secs(30);
/// The first ask for missing anchor pieces after the last one arrived;
/// doubles up to the cap.
const WANT_INITIAL: Duration = Duration::from_secs(10);
const WANT_CAP: Duration = Duration::from_secs(120);
/// `RecoverReady` is repeated this often while the Welcome is out: a
/// coordinator that restarted answers `link outdated`.
const READY_REPEAT_INITIAL: Duration = Duration::from_secs(60);
const READY_REPEAT_CAP: Duration = Duration::from_secs(600);
/// No anchor piece and no word from the coordinator: the request is
/// repeated this often, doubling up to the cap. A new coordinator re-sends
/// the anchor; an older one drops the repeat as a relay replay.
const REASK_INITIAL: Duration = Duration::from_secs(30);
const REASK_CAP: Duration = Duration::from_secs(600);

/// The wait line: what, since when, how long.
fn wait_line(what: &str, since_unix: u64, waited: Duration) -> String {
    format!("waiting for {what} since {} ({})", hms(since_unix), elapsed_label(waited.as_secs()))
}

/// Spawn the whole rejoiner task (the production body of
/// `Command::RecoverStart` on a Nostr republic) — the [`spawn_member_join`]
/// twin. Every exit reports: success ends in `NetRecoverSealed`, every
/// failure in `NetRecoverFailed` with a human reason.
///
/// A fired `quit` (a cancel, a new run) ends the run after one attempt to
/// publish its withdrawal (review R2), so the coordinator drops the open
/// attempt or the unsealed card instead of re-keying onto a KeyPackage
/// nobody holds.
pub(crate) fn spawn_recovery_rejoiner(
    dialer: molt_net::dial::Dialer,
    ctx: RecoverCtx,
    quit: tokio::sync::oneshot::Receiver<()>,
    tx: mpsc::WeakSender<Envelope>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let generation = Some(ctx.generation);
        let withdrawal = Withdrawal::default();
        let mut quit = quit;
        let why = tokio::select! {
            r = recovery_rejoin(dialer, ctx, &tx, withdrawal.clone()) => match r {
                Ok(()) => return,
                Err(error) => {
                    let _ = send_cmd(&tx, Command::NetRecoverFailed { error: error.clone(), generation }).await;
                    error
                }
            },
            Ok(()) = &mut quit => "cancelled".to_string(),
        };
        let pending = withdrawal.lock().ok().and_then(|mut w| w.take());
        if let Some((net, to, RitualMsg::RecoverFailed { member, version, attempt, .. })) = pending {
            let msg = RitualMsg::RecoverFailed { member, reason: why, version, attempt };
            match net.send_ritual(&to, &msg).await {
                Ok(_) => tracing::info!("recover_withdraw=sent"),
                Err(e) => tracing::warn!(error = %e, "recover_withdraw=unsent"),
            }
        }
    })
}

/// Join the group from the coordinator's Welcome.
fn join_from_coordinator_welcome(
    mls: &mut molt_net::MlsMember,
    welcome: &[u8],
) -> Result<(), String> {
    match mls.join_from_welcome(welcome) {
        Ok(()) => Ok(()),
        // one anchor per (seat, link): a foreign Welcome here is an earlier,
        // aborted attempt's, and no matching one will ever arrive
        Err(molt_net::MlsError::NotForUs) => {
            Err("link already used by an earlier attempt - mint a new one".to_string())
        }
        Err(e) => Err(format!("mls welcome: {e}")),
    }
}

/// The rejoiner's side of the coordinator conversation: its 1059 inbox,
/// and gift wraps to the coordinator's anchor.
pub(crate) trait RejoinIo {
    /// The next delivery, `None` when nothing arrives within `budget`.
    async fn recv(&mut self, budget: Duration) -> Option<RitualDelivery>;
    /// Gift-wrap `msg` to the coordinator; `Err` = no relay took it.
    async fn send(&mut self, msg: &RitualMsg) -> Result<(), String>;
}

struct RelayIo {
    net: RitualNet,
    inbox: molt_net::ritual_net::RitualInbox,
    to: String,
}

impl RejoinIo for RelayIo {
    async fn recv(&mut self, budget: Duration) -> Option<RitualDelivery> {
        self.inbox.recv(budget).await
    }

    async fn send(&mut self, msg: &RitualMsg) -> Result<(), String> {
        self.net.send_ritual(&self.to, msg).await.map(|_| ()).map_err(|e| e.to_string())
    }
}

/// How the coordinator admitted the seat.
#[allow(clippy::large_enum_variant)] // one value per rejoin
pub(crate) enum Admission {
    /// Anchor-first: the anchor verified before anything sealed.
    AnchorFirst {
        welcome: molt_net::welcome::WelcomePayload,
        blob: Option<molt_core::CheckpointState>,
        blocks: Vec<molt_core::ChainBlock>,
    },
    /// A Welcome with no anchor before it: the coordinator sealed first
    /// (an older build, or an anchor too large for gift wraps).
    SealFirst {
        welcome: molt_net::welcome::WelcomePayload,
        version: Option<String>,
    },
}

/// The anchor pieces of an anchor-first coordinator, as they arrive.
#[derive(Default)]
struct AnchorCollect {
    total: u32,
    got: std::collections::BTreeSet<u32>,
    parts: crate::chain::blob_parts::BlobAssembly,
    blob: Option<molt_core::CheckpointState>,
    block: Option<molt_core::ChainBlock>,
    tried: bool,
}

impl AnchorCollect {
    /// Take one piece; the step line it is worth.
    fn accept(&mut self, index: u32, total: u32, event: &str) -> Option<String> {
        if total == 0 || index >= total {
            return None;
        }
        if total != self.total {
            self.total = total;
            self.got.clear();
        }
        self.got.insert(index);
        match serde_json::from_str::<molt_core::WorkspaceEvent>(event).ok()? {
            molt_core::WorkspaceEvent::CheckpointServedPart { blob_hash, index, total, data } => {
                if let Some(b) = self.parts.accept(&blob_hash, index, total, &data) {
                    self.blob = Some(b);
                    self.tried = false;
                }
                Some(format!("checkpoint part {}/{total}", index.saturating_add(1)))
            }
            molt_core::WorkspaceEvent::CheckpointServed { blob } => {
                self.blob = Some(blob);
                self.tried = false;
                Some("checkpoint received".to_string())
            }
            molt_core::WorkspaceEvent::Committed(block) => {
                let line = format!("block {} received", block.height);
                self.block = Some(block);
                self.tried = false;
                Some(line)
            }
            _ => None,
        }
    }

    fn missing(&self) -> Vec<u32> {
        (0..self.total).filter(|i| !self.got.contains(i)).collect()
    }

    /// Verify what arrived, once per new piece of evidence. `Err` forgets
    /// every piece, so the next ask is for all of them.
    #[allow(clippy::type_complexity)]
    fn verify(
        &mut self,
        republic_id: &str,
    ) -> Option<Result<(Option<molt_core::CheckpointState>, Vec<molt_core::ChainBlock>, u64), String>> {
        if self.tried {
            return None;
        }
        let block = self.block.clone()?;
        if block.height != 0 && self.blob.is_none() {
            return None;
        }
        self.tried = true;
        let blob = if block.height == 0 { None } else { self.blob.clone() };
        let blocks = vec![block];
        let out = crate::chain::verify_served(blob.as_ref(), &blocks, Some(republic_id))
            .map(|(head, _)| (blob, blocks, head.height));
        if out.is_err() {
            let total = self.total;
            *self = Self { total, ..Self::default() };
        }
        Some(out)
    }
}

/// One rejoin run as the coordinator conversation sees it: its id, and
/// whether its re-admission was seen to seal.
pub(crate) struct Run<'a> {
    pub attempt: &'a str,
    pub readmitted: &'a std::sync::atomic::AtomicBool,
    pub asked: &'a std::sync::atomic::AtomicBool,
}

impl Run<'_> {
    /// An answer this run may act on: one naming it, or one from an older
    /// coordinator that names no run.
    fn ours(&self, attempt: &str) -> bool {
        attempt.is_empty() || attempt == self.attempt
    }
}

/// Wait for the coordinator: the anchor first (asking again for missing
/// pieces), `RecoverReady`, then the Welcome. A Welcome that comes first
/// is the seal-first order. Ends only on the Welcome or a refusal of this
/// run; a refusal naming no run (an older coordinator) may be an earlier
/// run's, replayed - it is shown and the wait goes on.
pub(crate) async fn await_admission(
    io: &mut impl RejoinIo,
    h: &molt_net::invite::RecoveryHandoverV2,
    member: &str,
    request: &RitualMsg,
    run: &Run<'_>,
    tx: &mpsc::WeakSender<Envelope>,
    generation: Option<u64>,
) -> Result<Admission, String> {
    use std::sync::atomic::Ordering::SeqCst;
    let note = |n: String| send_cmd(tx, Command::NetRecoverNote { note: n, generation });
    let mut stage = (molt_net::ritual_net::unix_now(), tokio::time::Instant::now());
    let mut last_note = tokio::time::Instant::now();
    let mut version: Option<String> = None;
    let mut collect = AnchorCollect::default();
    let mut verified: Option<(Option<molt_core::CheckpointState>, Vec<molt_core::ChainBlock>)> = None;
    let mut want_wait = WANT_INITIAL;
    let mut want_at: Option<tokio::time::Instant> = None;
    let mut ready_at: Option<tokio::time::Instant> = None;
    let mut ready_fail = PUBLISH_RETRY_INITIAL;
    let mut ready_repeat = READY_REPEAT_INITIAL;
    let ready = RitualMsg::RecoverReady {
        member: member.to_string(),
        ticket: h.ticket.clone(),
        attempt: run.attempt.to_string(),
    };
    let mut advisories: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut reask_wait = REASK_INITIAL;
    let mut reask_at = tokio::time::Instant::now() + reask_wait;
    loop {
        let now = tokio::time::Instant::now();
        // nothing heard yet: the count of pieces is unknown, so ask again
        let silent = verified.is_none() && collect.total == 0 && version.is_none();
        if silent && now >= reask_at {
            match io.send(request).await {
                Ok(()) => {
                    let _ = note("asked the coordinator again".to_string()).await;
                }
                Err(e) => tracing::warn!(error = %e, "recover_reask=unsent"),
            }
            reask_wait = (reask_wait * 2).min(REASK_CAP);
            reask_at = now + reask_wait;
        }
        if now.duration_since(last_note) >= WAIT_NOTE_EVERY {
            last_note = now;
            let what = match (&verified, collect.total) {
                (Some(_), _) => "the Welcome".to_string(),
                (None, 0) => "the coordinator".to_string(),
                (None, total) => format!("the chain anchor ({}/{total})", collect.got.len()),
            };
            let _ = note(wait_line(&what, stage.0, now.duration_since(stage.1))).await;
        }
        if verified.is_none() && want_at.is_some_and(|at| now >= at) {
            let missing = collect.missing();
            if !missing.is_empty() {
                let list: Vec<String> = missing.iter().map(|i| (i + 1).to_string()).collect();
                let want = RitualMsg::RecoverWant {
                    member: member.to_string(),
                    ticket: h.ticket.clone(),
                    missing,
                    attempt: run.attempt.to_string(),
                };
                match io.send(&want).await {
                    Ok(()) => {
                        let _ = note(format!("asked again for anchor parts {}", list.join(" "))).await;
                    }
                    Err(e) => tracing::warn!(error = %e, "recover_want=unsent"),
                }
            }
            want_wait = (want_wait * 2).min(WANT_CAP);
            want_at = Some(now + want_wait);
        }
        if verified.is_some() && ready_at.is_some_and(|at| now >= at) {
            run.asked.store(true, SeqCst);
            match io.send(&ready).await {
                Ok(()) => {
                    ready_at = Some(now + ready_repeat);
                    ready_repeat = (ready_repeat * 2).min(READY_REPEAT_CAP);
                }
                Err(e) => {
                    let _ = note(format!("⚠ the ready answer did not publish - {e} · retrying")).await;
                    ready_at = Some(now + ready_fail);
                    ready_fail = (ready_fail * 2).min(PUBLISH_RETRY_CAP);
                }
            }
        }
        let mut budget = WAIT_NOTE_EVERY.saturating_sub(now.duration_since(last_note));
        let reask = Some(reask_at).filter(|_| silent);
        for at in [want_at.filter(|_| verified.is_none()), ready_at.filter(|_| verified.is_some()), reask].into_iter().flatten() {
            budget = budget.min(at.saturating_duration_since(now));
        }
        let Some(delivery) = io.recv(budget.max(Duration::from_millis(10))).await else {
            continue;
        };
        match delivery {
            RitualDelivery::Welcome(welcome, sender) if sender == h.npub => {
                run.readmitted.store(true, SeqCst);
                return Ok(match verified {
                    Some((blob, blocks)) => Admission::AnchorFirst { welcome, blob, blocks },
                    None => Admission::SealFirst { welcome, version },
                });
            }
            // the coordinator REFUSED this run (WP6): the ticket is not
            // spent, a retry with the right phrase works on the same link
            RitualDelivery::Msg(RitualMsg::RecoverRefused { reason, attempt, .. }, sender) if sender == h.npub => {
                if attempt == run.attempt {
                    return Err(format!("the coordinator refused the request: {reason}"));
                }
                if attempt.is_empty() && advisories.insert(reason.clone()) {
                    tracing::warn!(%reason, "recover_refusal=unattributed");
                    let _ = note(format!("⚠ refused, maybe an earlier run: {reason}")).await;
                }
            }
            RitualDelivery::Msg(
                RitualMsg::RecoverProgress { member, need, roster, approved, version: v, attempt, sealed },
                sender,
            ) if sender == h.npub && run.ours(&attempt) =>
            {
                run.asked.store(true, SeqCst);
                if sealed {
                    run.readmitted.store(true, SeqCst);
                }
                if version.is_none() {
                    let line = if v.is_empty() {
                        "coordinator runs 0.0.18 or older".to_string()
                    } else {
                        format!("coordinator runs {v}")
                    };
                    let _ = note(line).await;
                    version = Some(v);
                }
                let _ = send_cmd(tx, Command::NetRecoverProgress { member, need, roster, approved, generation }).await;
            }
            RitualDelivery::Msg(RitualMsg::RecoverAnchor { index, total, event, attempt, .. }, sender)
                if sender == h.npub && verified.is_none() && run.ours(&attempt) =>
            {
                if let Some(line) = collect.accept(index, total, &event) {
                    let _ = note(line).await;
                }
                // a fresh piece pushes the next ask back
                want_wait = WANT_INITIAL;
                want_at = Some(tokio::time::Instant::now() + want_wait);
                match collect.verify(&h.republic_id) {
                    Some(Ok((blob, blocks, height))) => {
                        let _ = note(format!("chain verified up to block {height}")).await;
                        verified = Some((blob, blocks));
                        ready_at = Some(tokio::time::Instant::now());
                        stage = (molt_net::ritual_net::unix_now(), tokio::time::Instant::now());
                    }
                    Some(Err(e)) => {
                        let _ = note(format!("the anchor does not verify yet: {e}")).await;
                    }
                    None => {}
                }
            }
            _ => {}
        }
    }
}

/// The rejoiner state machine over Nostr (N4b step 6e, B4).
///
/// Fresh anchor from the RECOVERY ticket → 1059 inbox (readable-gated before
/// anything is advertised) → gift-wrapped `RecoverRequest` (anchor-first)
/// → the anchor pieces, verified standalone → `RecoverReady` → the 444
/// Welcome → `NetRecoverSealed`. An older coordinator seals first and
/// pushes the anchor over the group instead; a Welcome with no anchor
/// before it takes that path. No step ends on time: only a refusal, a
/// failed check or `recover_cancel` ends the run.
async fn recovery_rejoin(
    dialer: molt_net::dial::Dialer,
    ctx: RecoverCtx,
    tx: &mpsc::WeakSender<Envelope>,
    withdrawal: Withdrawal,
) -> Result<(), String> {
    let h = &ctx.handover;
    let generation = Some(ctx.generation);
    let note = |n: String| send_cmd(tx, Command::NetRecoverNote { note: n, generation });

    // The seat's identity re-derives from the phrase — resolving WHICH ritual
    // convention anchored it (founder vs joiner salt) via the link's anchored
    // pk; a wrong phrase is refused HERE, before any network round. The
    // transport anchor is NEW, salted with the RECOVERY ticket — the founding
    // anchor was salted with a ticket that died with the device.
    let (sk, pk) = crate::founding::seat_identity(&ctx.phrase, &ctx.member, &h.identity_pk)?;
    let entropy = molt_storage::seed_entropy(&ctx.phrase).map_err(|e| e.to_string())?;
    let (mut nostr_raw, new_nostr_pk) = molt_net::nostr_identity(&entropy, &h.ticket);
    let nostr_sk = zeroize::Zeroizing::new(nostr_raw.to_vec());
    zeroize::Zeroize::zeroize(&mut nostr_raw);

    // inbox BEFORE the request (subscribe-before-advertise): a request
    // answered into an unreadable inbox strands a human who has already lost
    // their device, with no second channel to notice on
    let net = RitualNet::new(dialer.clone(), ctx.dial_relays.clone(), &nostr_sk)
        .map_err(|e| format!("transport keys: {e}"))?;
    let since = (molt_net::ritual_net::unix_now(), tokio::time::Instant::now());
    let mut wait = PUBLISH_RETRY_INITIAL;
    let mut inbox = loop {
        match net.inbox().await {
            Ok(inbox) => break inbox,
            Err(e) => {
                let _ = note(format!("⚠ the recovery inbox is not up - {e} · retrying")).await;
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(PUBLISH_RETRY_CAP);
            }
        }
    };
    while !inbox.first_read(RECV_SLICE).await.any() {
        let _ = note(wait_line("a relay", since.0, since.1.elapsed())).await;
    }

    // a FRESH MLS identity + KeyPackage: the coordinator removes the lost
    // leaf and adds this one in a single commit
    let mut mls =
        molt_net::MlsMember::new(&sk, &ctx.member).map_err(|e| format!("mls identity: {e}"))?;
    let kp_hex = hex::encode(mls.key_package().map_err(|e| format!("key package: {e}"))?);
    // the seat proof binds the new anchor AND the relay declaration for
    // the coordinator; the v2 consent binds both for every survivor
    let declared = ctx.dial_relays.clone();
    let seat_proof = crate::founding::make_seat_proof(
        &sk,
        &h.ticket,
        &kp_hex,
        &h.republic_id,
        &new_nostr_pk,
        &declared,
    );
    // the automatic co-approval of the own re-admission (recovery approval
    // design, 2026-08-08): one distinct signer toward the threshold
    let consent = crate::founding::make_restore_consent(
        &sk,
        &h.republic_id,
        &ctx.member,
        &pk,
        &new_nostr_pk,
        &declared,
        h.consent_anchor.as_ref(),
    );
    // the seal-first order's group frames are read from shortly before the
    // request on (clock skew)
    let asked_at = molt_net::ritual_net::unix_now();
    let attempt = invite::recover_attempt_id(&kp_hex);
    let request = RitualMsg::Recover(invite::RecoverRequest {
        member: ctx.member.clone(),
        identity_pk: pk,
        key_package: kp_hex,
        ticket: h.ticket.clone(),
        seat_proof,
        new_nostr_pk,
        // R5: what this seat can actually dial — its ledger entry
        relays: declared,
        consent,
        // Nostr replies to the gift-wrap anchor, not to a queue
        reply: None,
        anchor_first: true,
        version: env!("CARGO_PKG_VERSION").to_string(),
        started: asked_at,
        caps: if ctx.lock { vec![molt_core::chain::CHAIN_LOCK_CAP.to_string()] } else { Vec::new() },
    });
    if let Ok(mut w) = withdrawal.lock() {
        let msg = RitualMsg::RecoverFailed {
            member: ctx.member.clone(),
            reason: String::new(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            attempt: attempt.clone(),
        };
        *w = Some((net.clone(), h.npub.clone(), msg));
    }
    let mut wait = PUBLISH_RETRY_INITIAL;
    while let Err(e) = net.send_ritual(&h.npub, &request).await {
        let _ = note(format!("⚠ the request did not publish - {e} · retrying")).await;
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(PUBLISH_RETRY_CAP);
    }
    let _ = note("request sent - waiting for the coordinator".to_string()).await;

    let mut io = RelayIo { net, inbox, to: h.npub.clone() };
    let run = Run { attempt: &attempt, readmitted: &ctx.readmitted, asked: &ctx.asked };
    let admission = await_admission(&mut io, h, &ctx.member, &request, &run, tx, generation).await?;
    let (payload, blob, blocks) = match admission {
        Admission::AnchorFirst { welcome, blob, blocks } => {
            join_from_coordinator_welcome(&mut mls, &welcome.welcome)?;
            let _ = note("welcomed back".to_string()).await;
            (welcome, blob, blocks)
        }
        Admission::SealFirst { welcome, version } => {
            join_from_coordinator_welcome(&mut mls, &welcome.welcome)?;
            let _ = note("welcomed back - fetching the chain anchor".to_string()).await;
            let group = Arc::new(Mutex::new(mls));
            if let Some(v) = version {
                tracing::info!(coordinator = %v, "recover_order=seal_first");
            }
            let (blob, blocks) = seal_first_anchor(dialer, &ctx, &group, &welcome, asked_at, tx).await?;
            mls = Arc::try_unwrap(group)
                .map_err(|_| "mls handle still shared".to_string())?
                .into_inner()
                .map_err(|_| "mls lock poisoned".to_string())?;
            (welcome, blob, blocks)
        }
    };

    // **Relay honesty, against the CHAIN.** The join twin compares the
    // Welcome's relays to the invite's, which is founding-only. Here the
    // Welcome carries the GOVERNED pool while this node holds only the
    // anchor; later `set_relays` blocks arrive over the ordinary catch-up.
    // A mismatch is not decidable now (a lie or a later vote) and is safe
    // to carry: the recovered pool materializes from the verified chain
    // alone, so a differing set is logged, never trusted.
    let served_fold = crate::chain::effective_relays_of_served(blob.as_ref(), &blocks);
    if payload.relays != served_fold {
        tracing::warn!(
            welcome = payload.relays.join(" "),
            served = served_fold.join(" "),
            "the Welcome's relay set differs from the served anchor's fold - \
             a later pool vote or a lie; the chain decides on catch-up"
        );
    }

    let snapshot = mls.snapshot().map_err(|e| format!("mls snapshot: {e}"))?;
    let height = blocks.last().map_or(0, |b| b.height);
    let wire = match blob {
        Some(b) => crate::chain::ServedChainWire::Pruned { checkpoint_blob: b, blocks },
        None => crate::chain::ServedChainWire::Full(blocks),
    };
    tracing::info!(member = %ctx.member, height, "rejoined and verified the chain anchor");
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
            founded_ts: payload.founded_ts,
            generation,
        },
    )
    .await;
    Ok(())
}

/// The seal-first order (an older coordinator): the anchor arrives over
/// the group channel the Welcome opened, behind the coordinator's outbox.
/// Reads from `asked_at - RECENT_SKEW_SECS` on - a read window, not a
/// deadline: the coordinator re-pushes the anchor with fresh stamps every
/// ten minutes while this seat stays unheard, so it always lands inside.
async fn seal_first_anchor(
    dialer: molt_net::dial::Dialer,
    ctx: &RecoverCtx,
    group: &Arc<Mutex<molt_net::MlsMember>>,
    payload: &molt_net::welcome::WelcomePayload,
    asked_at: u64,
    tx: &mpsc::WeakSender<Envelope>,
) -> Result<(Option<molt_core::CheckpointState>, Vec<molt_core::ChainBlock>), String> {
    let h = &ctx.handover;
    let generation = Some(ctx.generation);
    let note = |n: String| send_cmd(tx, Command::NetRecoverNote { note: n, generation });
    // 445 under the Welcome's rotation seed, dialing only our own subset
    let chan = GroupChannel::new(dialer, ctx.dial_relays.clone(), payload.rotation_seed);
    let mut wait = PUBLISH_RETRY_INITIAL;
    let mut sub = loop {
        match chan.subscribe_recent(asked_at.saturating_sub(RECENT_SKEW_SECS)).await {
            Ok(sub) => break sub,
            Err(e) => {
                let _ = note(format!("⚠ the group channel is not up - {e} · retrying")).await;
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(PUBLISH_RETRY_CAP);
            }
        }
    };
    // readable = a relay answered; the replay itself is read in the loop
    // below, or a replay longer than the channel never ends (findings R11)
    let since = (molt_net::ritual_net::unix_now(), tokio::time::Instant::now());
    while !sub.first_read(RECV_SLICE).await.any() {
        let _ = note(wait_line("a relay", since.0, since.1.elapsed())).await;
    }

    // Assemble the served ANCHOR until it verifies STANDALONE. `verify_served`
    // against the link's republic id is the whole gate: the frames carry no
    // usable author binding for a node that does not yet know the roster, and
    // it needs none — a chain that recomputes to this republic's id is this
    // republic's chain, whoever relayed it.
    let mut blob: Option<molt_core::CheckpointState> = None;
    let mut verified_run: (u64, usize) = (u64::MAX, 0);
    let mut parts = crate::chain::blob_parts::BlobAssembly::default();
    let mut blocks: Vec<molt_core::ChainBlock> = Vec::new();
    let since = (molt_net::ritual_net::unix_now(), tokio::time::Instant::now());
    // a life sign every 30 s, whatever arrives: frames that cannot be read
    // must not look like silence
    let (mut frames, mut unreadable, mut other) = (0u64, 0u64, 0u64);
    let mut last_beat = tokio::time::Instant::now();
    loop {
        if last_beat.elapsed() >= WAIT_NOTE_EVERY {
            last_beat = tokio::time::Instant::now();
            let line = wait_line("the chain anchor", since.0, since.1.elapsed());
            let _ = note(format!("{line}: frames={frames} unreadable={unreadable} other={other}")).await;
        }
        let budget = WAIT_NOTE_EVERY.saturating_sub(last_beat.elapsed()).max(Duration::from_millis(10));
        let (content, created_at) = match sub.recv(budget).await {
            molt_net::ritual_net::GroupRecv::Frame { content, created_at } => (content, created_at),
            _ => continue,
        };
        frames += 1;
        let opened = open_group_envelope(group, &content);
        // the first frames one by one: size, stamp and what they are
        if frames <= FRAMES_LOGGED {
            let what = opened.as_ref().map_or("unreadable", |e| event_kind(&e.body));
            let line = format!("frame {frames}: {} B at {} {what}", content.len(), hms(created_at));
            tracing::info!(frame = frames, bytes = content.len(), created_at, what, "rejoin_frame");
            let _ = note(line).await;
        }
        let Some(env) = opened else {
            unreadable += 1;
            continue;
        };
        match env.body {
            molt_core::WorkspaceEvent::CheckpointServed { blob: b } => {
                let _ = note("checkpoint received".to_string()).await;
                blob = Some(b);
                // new evidence: the same run may verify now (suffix + blob)
                verified_run = (u64::MAX, 0);
            }
            molt_core::WorkspaceEvent::CheckpointServedPart { blob_hash, index, total, data } => {
                let _ = note(format!("checkpoint part {}/{total}", index.saturating_add(1))).await;
                if let Some(b) = parts.accept(&blob_hash, index, total, &data) {
                    let _ = note("checkpoint received".to_string()).await;
                    blob = Some(b);
                    verified_run = (u64::MAX, 0);
                }
            }
            molt_core::WorkspaceEvent::Committed(block) => {
                // bounded (review R7): any group member can flood distinct
                // heights during the window — keep a window above the lowest
                // seen and at most RECOVER_BLOCK_BUFFER of them
                let lowest = blocks.first().map_or(block.height, |b| b.height.min(block.height));
                if block.height > lowest.saturating_add(RECOVER_BLOCK_WINDOW) {
                    continue;
                }
                if !blocks.iter().any(|b| b.height == block.height) {
                    let _ = note(format!("block {} received", block.height)).await;
                    blocks.push(block);
                    blocks.sort_by_key(|b| b.height);
                    blocks.truncate(RECOVER_BLOCK_BUFFER);
                }
            }
            _ => {
                other += 1;
                continue;
            }
        }
        // Only a CONSECUTIVE run can verify — and the stream is not one: the
        // coordinator's outbox also carries the head block the recovery just
        // committed, several heights above the anchor with the middle never
        // served.
        let run: Vec<molt_core::ChainBlock> = blocks
            .iter()
            .enumerate()
            .take_while(|(i, b)| {
                u64::try_from(*i).is_ok_and(|i| b.height == blocks[0].height + i)
            })
            .map(|(_, b)| b.clone())
            .collect();
        // no re-walk per frame (R7) — but keyed by (start, length), not
        // length alone: `[3]` failing and then `[0]` arriving are two
        // different runs of length one, and the second is the anchor
        if run.is_empty() {
            continue;
        }
        let key = (run[0].height, run.len());
        if key == verified_run {
            continue;
        }
        verified_run = key;
        match crate::chain::verify_served(blob.as_ref(), &run, Some(&h.republic_id)) {
            Ok(_) => {
                let _ = note(format!("chain verified up to block {}", run[run.len() - 1].height)).await;
                return Ok((blob, run));
            }
            Err(e) => {
                let _ = note(format!("blocks {}-{} do not verify yet: {e}", run[0].height, run[run.len() - 1].height)).await;
            }
        }
    }
}

/// Spawn the whole member join task (the production body of
/// `Command::JoinStart`). Every exit path reports: success ends in
/// `NetJoinSealed`, every failure in `NetJoinFailed` with a human reason.
pub(crate) fn spawn_member_join(
    dialer: molt_net::dial::Dialer,
    ctx: JoinCtx,
    confirm: mpsc::Receiver<bool>,
    backup: mpsc::Receiver<bool>,
    // fired by a cancel or a clean quit: tell the founder, then stop
    quit: tokio::sync::oneshot::Receiver<()>,
    tx: mpsc::WeakSender<Envelope>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let generation = Some(ctx.generation);
        if let Err(error) = member_join(dialer, ctx, confirm, backup, quit, &tx).await {
            let _ = send_cmd(&tx, Command::NetJoinFailed { error, generation }).await;
        }
    })
}

/// The Nostr leg of the member ladder ([`crate::ritual_member`]) — the §4.2
/// restructured order: JoinRequest (gift-wrap) → JoinAccepted →
/// **Welcome before deliberation** (payload v2: MLS Welcome, rotation seed,
/// relays) → the group 445s carry Seal → Signed → BackupConfirmed → Genesis.
///
/// The leg authenticates every message before the ladder sees it: the
/// NIP-59 wrap sealer on the 1059 inbox, the MLS leaf credential on the 445
/// channel (`frame_is_from_founder`) — a co-member's frame is dropped, never
/// fatal (one garbage frame must not abort every other seat's join).
struct NostrLeg {
    net: RitualNet,
    inbox: molt_net::ritual_net::RitualInbox,
    dialer: molt_net::dial::Dialer,
    /// What we DIAL (our confirmed subset) — deliberately different from
    /// the invite's full list, which the GROUP uses.
    dial_relays: Vec<String>,
    founder_npub: String,
    invite_relays: Vec<String>,
    info: molt_core::InviteInfo,
    seat: u32,
    tx: mpsc::WeakSender<Envelope>,
    generation: Option<u64>,
    /// The ladder's MLS member, attached before the JoinRequest.
    group: Option<Arc<Mutex<molt_net::MlsMember>>>,
    /// The 445 channel + subscription, from the Welcome on.
    chan: Option<(GroupChannel, molt_net::ritual_net::GroupSub)>,
    rotation_seed: Option<[u8; 32]>,
    was_deaf: bool,
    /// A note that must not be awaited while a frame is in hand (the
    /// ladder selects on `next_msg`): flushed at the next call.
    pending_note: Option<String>,
    /// The clock of the current silent wait: `(phase, since, last noted
    /// rung)`.
    waiting: Option<(Phase, tokio::time::Instant, u64)>,
}

impl NostrLeg {
    async fn note(&self, note: String) {
        let _ = send_cmd(
            &self.tx,
            Command::NetJoinNote {
                note,
                generation: self.generation,
            },
        )
        .await;
    }

    /// The waits on the founder are unbounded - but silence and progress
    /// must not look identical, so a quiet wait says how long it has lasted.
    async fn wait_note(&mut self, phase: Phase) {
        let what = match phase {
            Phase::Accept => "the founder",
            Phase::Genesis => "the genesis",
            Phase::Seal | Phase::Backup => return,
        };
        if self.waiting.map(|w| w.0) != Some(phase) {
            self.waiting = Some((phase, tokio::time::Instant::now(), 0));
        }
        let Some((_, since, noted)) = self.waiting.as_mut() else {
            return;
        };
        if let Some(next) = genesis_wait_note(since.elapsed().as_secs(), *noted) {
            *noted = next;
            self.note(format!("⧗ waiting for {what} · {}", elapsed_label(next))).await;
        }
    }

    /// The Welcome: relay honesty, join the group, stand the 445
    /// subscription up (readable-gated — a never-readable subscription would
    /// hang the Seal wait forever in silence).
    async fn join_group(&mut self, payload: molt_net::welcome::WelcomePayload) -> Result<(), String> {
        // the joiner gated the INVITE relays through its own pool
        // (ADR-0004); a Welcome silently pointing elsewhere would bypass
        // that consent. At founding the two sets are the same by
        // construction; a mismatch is a broken or malicious founder.
        if payload.relays != self.invite_relays {
            return Err(
                "the Welcome names a different relay set than the invite - refused".to_string(),
            );
        }
        let group = self
            .group
            .as_ref()
            .ok_or_else(|| "no group attached".to_string())?;
        group
            .lock()
            .map_err(|_| "mls lock poisoned".to_string())?
            .join_from_welcome(&payload.welcome)
            .map_err(|e| format!("mls welcome: {e}"))?;
        let chan = GroupChannel::new(
            self.dialer.clone(),
            self.dial_relays.clone(),
            payload.rotation_seed,
        );
        let since = molt_net::ritual_net::unix_now().saturating_sub(MEMBER_LOOKBACK_SECS);
        let mut wait = PUBLISH_RETRY_INITIAL;
        let sub = loop {
            match readable_group_sub(&chan, since).await {
                Ok(sub) => break sub,
                Err(why) => {
                    self.note(format!(
                        "⚠ cannot hear the group channel - {why} · still retrying"
                    ))
                    .await;
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(PUBLISH_RETRY_CAP);
                }
            }
        };
        self.rotation_seed = Some(payload.rotation_seed);
        self.chan = Some((chan, sub));
        Ok(())
    }
}

impl RitualLeg for NostrLeg {
    async fn next_msg(
        &mut self,
        phase: Phase,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<Option<RitualMsg>, String> {
        if let Some(note) = self.pending_note.take() {
            self.note(note).await;
        }
        loop {
            if self.chan.is_none() {
                // the 1059 inbox: the founder's ack, refusal, abort — or the
                // Welcome, which moves us onto the group channel
                let budget = match deadline {
                    Some(d) => {
                        let now = tokio::time::Instant::now();
                        if now >= d {
                            return Ok(None);
                        }
                        (d - now).min(RECV_SLICE)
                    }
                    None => RECV_SLICE,
                };
                match self.inbox.recv(budget).await {
                    Some(RitualDelivery::Msg(msg, sender)) if sender == self.founder_npub => {
                        return Ok(Some(msg));
                    }
                    // a fast founder's Welcome may overtake the advisory
                    // ack; it counts as acceptance
                    Some(RitualDelivery::Welcome(p, sender)) if sender == self.founder_npub => {
                        self.join_group(p).await?;
                        return Ok(Some(RitualMsg::JoinAccepted { seat: self.seat }));
                    }
                    Some(_) => continue,
                    None => {
                        self.wait_note(phase).await;
                        continue;
                    }
                }
            }
            let Some((_, sub)) = self.chan.as_mut() else {
                continue;
            };
            let content = match sub.recv(RECV_SLICE).await {
                molt_net::ritual_net::GroupRecv::Frame { content, .. } => {
                    if self.was_deaf {
                        self.was_deaf = false;
                        self.pending_note = Some("✓ the group channel is back".to_string());
                    }
                    content
                }
                molt_net::ritual_net::GroupRecv::Idle => {
                    // the genesis wait is deliberately UNBOUNDED (every seat
                    // must ratify, and a founder finishing a human
                    // deliberation is not a failure) — but silence and
                    // progress must not look identical, so the wait says how
                    // long it has been waiting
                    self.wait_note(phase).await;
                    continue;
                }
                // loud, never fatal — the join keeps waiting, visibly
                molt_net::ritual_net::GroupRecv::Deaf(why) => {
                    self.was_deaf = true;
                    self.note(format!(
                        "⚠ cannot hear the group channel - {why} · still retrying"
                    ))
                    .await;
                    continue;
                }
            };
            let Some(group) = self.group.as_ref() else {
                continue;
            };
            let Some((msg, from)) = open_group_frame(group, &content) else {
                continue;
            };
            // the MLS-authenticated author is the whole gate: an ungated
            // arm would let any welcomed seat abort every other seat's join
            // (or propose a charter) with one frame
            if !frame_is_from_founder(&from, &self.info) {
                tracing::warn!(%from, "group frame from a co-member on the ritual path - ignored");
                continue;
            }
            return Ok(Some(msg));
        }
    }

    async fn send(&mut self, msg: &RitualMsg) -> Result<(), String> {
        match (&self.chan, &self.group) {
            // inside the group: a 445 frame, retried until a relay takes it
            // (a signature lost to a relay blip must not end the join)
            (Some((chan, _)), Some(group)) => {
                let (ct, exporter) = seal_frame(group, msg)?;
                let mut wait = PUBLISH_RETRY_INITIAL;
                loop {
                    match chan.publish_frame(&exporter, &ct).await {
                        Ok(_) => return Ok(()),
                        Err(e) => {
                            self.note(format!("⚠ could not publish - {e} · retrying")).await;
                            tokio::time::sleep(wait).await;
                            wait = (wait * 2).min(PUBLISH_RETRY_CAP);
                        }
                    }
                }
            }
            // before the Welcome: a gift wrap to the founder's anchor; the
            // ladder's resend is the retry
            _ => match self.net.send_ritual(&self.founder_npub, msg).await {
                Ok(_) => Ok(()),
                Err(e) => {
                    self.pending_note =
                        Some(format!("⚠ the join request did not publish - {e} · retrying"));
                    Err(e.to_string())
                }
            },
        }
    }

    fn reply_handover(&self) -> Option<invite::ReplyHandover> {
        // the reply address IS the MAC-bound nostr anchor
        None
    }

    fn declared_relays(&self) -> Vec<String> {
        // what this joiner actually dials — the founder's log names any
        // pool relay missing from it
        self.dial_relays.clone()
    }

    fn attach_group(&mut self, group: Arc<Mutex<molt_net::MlsMember>>) {
        self.group = Some(group);
    }

    fn in_group(&self) -> bool {
        self.chan.is_some()
    }

    fn vet_proposal(&self, proposal: &molt_core::SealedRoster) -> Result<(), String> {
        check_proposal_against_link(proposal, &self.founder_npub, &self.info)
    }

    async fn finish(
        &mut self,
        _name: &str,
        _sealed: &molt_core::SealedRoster,
        _group: &Arc<Mutex<molt_net::MlsMember>>,
        _early: Vec<Vec<u8>>,
    ) -> Option<Vec<molt_core::MeshLink>> {
        // no mesh over relays — the 445 runtime is the group's transport
        None
    }
}

/// The production human gate: the ladder's steps surface as engine
/// commands (the wizard renders them), the answers arrive on the channels
/// `cmd_join_confirm_charter` / `cmd_confirm_seed_backup` release.
struct CommandGate {
    tx: mpsc::WeakSender<Envelope>,
    generation: Option<u64>,
    confirm: mpsc::Receiver<bool>,
    backup: mpsc::Receiver<bool>,
}

impl Ratify for CommandGate {
    async fn accepted(&mut self) {
        let _ = send_cmd(
            &self.tx,
            Command::NetJoinAccepted {
                generation: self.generation,
            },
        )
        .await;
    }

    async fn propose(&mut self, name: &str, agenda: &str, features: Option<&[String]>) {
        let _ = send_cmd(
            &self.tx,
            Command::NetJoinCharterProposed {
                name: name.to_string(),
                agenda: agenda.to_string(),
                features: features.map(<[String]>::to_vec),
                generation: self.generation,
            },
        )
        .await;
    }

    async fn confirm(&mut self) -> Option<bool> {
        self.confirm.recv().await
    }

    async fn backup(&mut self) -> Option<bool> {
        self.backup.recv().await
    }
}

/// The member join over Nostr: derive the seat, stand the endpoint + inbox
/// up (readable-gated, subscribe-before-advertise), then run the ONE member
/// ladder on the [`NostrLeg`] and report the sealed result.
async fn member_join(
    dialer: molt_net::dial::Dialer,
    ctx: JoinCtx,
    confirm: mpsc::Receiver<bool>,
    backup: mpsc::Receiver<bool>,
    mut quit: tokio::sync::oneshot::Receiver<()>,
    tx: &mpsc::WeakSender<Envelope>,
) -> Result<(), String> {
    const QUIT: &str = "the join was cancelled";
    let h = ctx.invite.handover;
    let generation = Some(ctx.generation);
    let dial_relays = ctx.dial_relays;

    // the joiner's identity — derived exactly as the ritual anchors it
    let mut seat = MemberSeat::derive(h.seat, &h.ticket, &ctx.phrase)?;
    if ctx.lock {
        seat.caps.push(molt_core::chain::CHAIN_LOCK_CAP.to_string());
    }

    // endpoint + inbox BEFORE announcing (subscribe-before-advertise)
    let net = RitualNet::new(dialer.clone(), dial_relays.clone(), &seat.nostr_sk)
        .map_err(|e| format!("transport keys: {e}"))?;
    // announcing a JoinRequest over an unreadable inbox means the founder's
    // reply lands nowhere: retry until it is readable
    let readable = async {
        let mut wait = PUBLISH_RETRY_INITIAL;
        loop {
            let why = match net.inbox().await {
                Ok(mut inbox) => {
                    if inbox.read_state(LIVE_WAIT).await.any() {
                        return Ok(inbox);
                    }
                    "not readable on any relay".to_string()
                }
                Err(e) => e.to_string(),
            };
            let note = Command::NetJoinNote {
                note: format!("⚠ the join inbox is not up - {why} · retrying"),
                generation,
            };
            if !send_cmd(tx, note).await {
                return Err("the engine is gone".to_string());
            }
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(PUBLISH_RETRY_CAP);
        }
    };
    let inbox = tokio::select! {
        inbox = readable => inbox?,
        _ = &mut quit => return Err(QUIT.to_string()),
    };

    let mut leg = NostrLeg {
        net,
        inbox,
        dialer,
        dial_relays,
        founder_npub: h.npub,
        invite_relays: h.relays,
        info: ctx.invite.info,
        seat: h.seat,
        tx: tx.clone(),
        generation,
        group: None,
        chan: None,
        rotation_seed: None,
        was_deaf: false,
        pending_note: None,
        waiting: None,
    };
    let gate = CommandGate {
        tx: tx.clone(),
        generation,
        confirm,
        backup,
    };
    let seat_no = seat.seat;
    let out = tokio::select! {
        biased;
        _ = &mut quit => None,
        out = run_member_ladder(&mut leg, &ctx.member, seat, Some(gate), true) => Some(out),
    };
    // a cancel also closes the human gates, which ends the ladder on its
    // own: an error with the quit already fired is still the quit
    let out = match out {
        Some(Err(_)) if quit.try_recv().is_ok() => None,
        other => other,
    };
    let Some(out) = out else {
        // inside the born group this seat can never be filled again: say so
        // once, so the founder is not left waiting on it
        if let (Some((chan, _)), Some(group)) = (&leg.chan, &leg.group) {
            match seal_frame(group, &RitualMsg::Declined { seat: seat_no }) {
                Ok((ct, exporter)) => {
                    if let Err(e) = chan.publish_frame(&exporter, &ct).await {
                        tracing::warn!(error = %e, "leave notice did not publish");
                    }
                }
                Err(e) => tracing::warn!(error = %e, "leave notice could not be sealed"),
            }
        }
        return Err(QUIT.to_string());
    };
    let out = out?;

    let sealed = out.sealed.ok_or_else(|| "no sealed roster".to_string())?;
    let snap = out
        .mls_snapshot
        .ok_or_else(|| "the join ended outside the group".to_string())?;
    let rotation_seed = leg
        .rotation_seed
        .ok_or_else(|| "the join ended without a Welcome".to_string())?;
    let sealed_json = serde_json::to_string(&sealed).map_err(|e| e.to_string())?;
    let _ = send_cmd(
        tx,
        Command::NetJoinSealed {
            sealed: sealed_json,
            mls: hex::encode(snap),
            mesh: Vec::new(),
            nostr_sk: hex::encode(out.nostr_sk.as_slice()),
            relays: leg.invite_relays,
            rotation_seed: hex::encode(rotation_seed),
            generation,
        },
    )
    .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    /// The two production gates composed the way the leg applies them:
    /// "someone else published this" (ignored) and "the founder published
    /// something wrong" (refused) must NOT be handled the same way.
    #[derive(Debug, PartialEq, Eq)]
    enum Provenance {
        FromFounder,
        NotTheFounder,
        Refused(String),
    }

    fn check_proposal_provenance(
        from: &str,
        proposal: &molt_core::SealedRoster,
        founder_npub: &str,
        info: &molt_core::InviteInfo,
    ) -> Provenance {
        if !frame_is_from_founder(from, info) {
            return Provenance::NotTheFounder;
        }
        match check_proposal_against_link(proposal, founder_npub, info) {
            Ok(()) => Provenance::FromFounder,
            Err(why) => Provenance::Refused(why),
        }
    }

    use super::*;

    /// The field failure 2026-09-12: a restarted recovery on the same link
    /// finds the aborted attempt's Welcome on its anchor. Say what to do.
    #[test]
    fn a_welcome_for_an_earlier_attempt_names_the_used_link() {
        let sk = |b: u8| molt_storage::SigningKey::from_bytes(&[b; 32]);
        let mut founder = molt_net::MlsMember::new(&sk(1), "founder").expect("founder");
        founder.create_group().expect("create");
        let stale = {
            let first = molt_net::MlsMember::new(&sk(2), "bob").expect("first attempt");
            founder
                .add_members(&[first.key_package().expect("kp")])
                .expect("add")
                .expect("welcome")
        };
        let mut second = molt_net::MlsMember::new(&sk(2), "bob").expect("second attempt");
        assert_eq!(
            join_from_coordinator_welcome(&mut second, &stale),
            Err("link already used by an earlier attempt - mint a new one".to_string())
        );
    }

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
            vault_pk: String::new(),
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
            features: None,
            founded_ts: 0,
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

#[cfg(test)]
mod admission_tests {
    use super::*;
    use crate::chain::test_support::Builder;
    use molt_core::{ChainChange, Surface, WorkspaceEvent};
    use std::collections::VecDeque;

    const COORDINATOR: &str = "c0c0";

    /// A pruned anchor whose blob (~150 KB of invented notes) needs parts:
    /// the republic id and the frames a coordinator sends.
    fn fat_anchor() -> (String, Vec<String>) {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        let mut blob = crate::chain::checkpoint_state(&b.blocks, 1).expect("state");
        blob.applied = vec![(
            Surface::Memory,
            (0..2_000u64).map(|i| (i, serde_json::json!({ "op": "add_note", "text": "x".repeat(40) }))).collect(),
        )];
        let anchor = b.seal(
            2,
            ChainChange::Checkpoint { upto: 1, state_hash: crate::chain::checkpoint_state_hash(&blob) },
            &["petra", "walter"],
        );
        let mut events = crate::chain::blob_parts::blob_events(&blob, crate::chain::RECOVER_PART_MAX).expect("parts");
        events.push(WorkspaceEvent::Committed(anchor));
        let frames = events.iter().map(|e| serde_json::to_string(e).expect("json")).collect();
        (b.republic_id.clone(), frames)
    }

    fn handover(republic_id: &str) -> molt_net::invite::RecoveryHandoverV2 {
        molt_net::invite::RecoveryHandoverV2 {
            ticket: "ab".repeat(32),
            npub: COORDINATOR.to_string(),
            relays: Vec::new(),
            republic_id: republic_id.to_string(),
            identity_pk: String::new(),
            consent_anchor: None,
        }
    }

    fn welcome() -> molt_net::welcome::WelcomePayload {
        molt_net::welcome::WelcomePayload { welcome: vec![1, 2, 3], rotation_seed: [7; 32], relays: Vec::new(), founded_ts: 0 }
    }

    /// An anchor-first coordinator: the anchor after `after`, one piece
    /// lost on the first send, the Welcome on the first Ready.
    struct Fake {
        frames: Vec<String>,
        lost: Option<u32>,
        after: Duration,
        t0: tokio::time::Instant,
        served: bool,
        queue: VecDeque<RitualDelivery>,
        wants: Vec<Vec<u32>>,
        readies: u32,
        /// Repeated requests seen; answered with the whole anchor when set.
        reasks: u32,
        answer_reask: bool,
        /// The run the pieces name.
        attempt: String,
    }

    impl Fake {
        fn new(frames: Vec<String>, lost: Option<u32>, after: Duration) -> Self {
            Fake {
                frames,
                lost,
                after,
                t0: tokio::time::Instant::now(),
                served: false,
                queue: VecDeque::new(),
                wants: Vec::new(),
                readies: 0,
                reasks: 0,
                answer_reask: false,
                attempt: run_id(),
            }
        }

        fn part(&self, index: u32) -> RitualDelivery {
            let total = u32::try_from(self.frames.len()).expect("total");
            let event = self.frames[usize::try_from(index).expect("index")].clone();
            RitualDelivery::Msg(
                RitualMsg::RecoverAnchor { member: "petra".into(), index, total, event, attempt: self.attempt.clone() },
                COORDINATOR.into(),
            )
        }
    }

    impl RejoinIo for Fake {
        async fn recv(&mut self, budget: Duration) -> Option<RitualDelivery> {
            if !self.served && self.t0.elapsed() >= self.after {
                self.served = true;
                let total = u32::try_from(self.frames.len()).expect("total");
                for i in (0..total).filter(|i| Some(*i) != self.lost) {
                    let d = self.part(i);
                    self.queue.push_back(d);
                }
            }
            if let Some(d) = self.queue.pop_front() {
                return Some(d);
            }
            tokio::time::sleep(budget).await;
            None
        }

        async fn send(&mut self, msg: &RitualMsg) -> Result<(), String> {
            match msg {
                RitualMsg::RecoverWant { missing, .. } => {
                    self.wants.push(missing.clone());
                    for i in missing {
                        let d = self.part(*i);
                        self.queue.push_back(d);
                    }
                }
                RitualMsg::Recover(_) => {
                    self.reasks += 1;
                    if self.answer_reask {
                        let total = u32::try_from(self.frames.len()).expect("total");
                        for i in 0..total {
                            let d = self.part(i);
                            self.queue.push_back(d);
                        }
                    }
                }
                RitualMsg::RecoverReady { .. } => {
                    self.readies += 1;
                    if self.readies == 1 {
                        self.queue.push_back(RitualDelivery::Welcome(welcome(), COORDINATOR.into()));
                    }
                }
                _ => {}
            }
            Ok(())
        }
    }

    fn request() -> RitualMsg {
        RitualMsg::Recover(invite::RecoverRequest {
            member: "petra".into(),
            identity_pk: "aa".into(),
            key_package: "bb".into(),
            ticket: "ab".repeat(32),
            seat_proof: "dd".into(),
            new_nostr_pk: String::new(),
            relays: Vec::new(),
            consent: String::new(),
            reply: None,
            anchor_first: true,
            version: "0.0.21".into(),
            started: 0,
            caps: Vec::new(),
        })
    }

    /// The id of the run [`request`] starts.
    fn run_id() -> String {
        invite::recover_attempt_id("bb")
    }

    /// Await the admission of the run [`request`] starts.
    async fn admit(io: &mut Fake, rid: &str, weak: &mpsc::WeakSender<Envelope>) -> Result<Admission, String> {
        let attempt = run_id();
        let readmitted = std::sync::atomic::AtomicBool::new(false);
        let asked = std::sync::atomic::AtomicBool::new(false);
        let run = Run { attempt: &attempt, readmitted: &readmitted, asked: &asked };
        await_admission(io, &handover(rid), "petra", &request(), &run, weak, Some(1)).await
    }

    /// An engine stand-in that swallows the notes.
    fn sink() -> (mpsc::Sender<Envelope>, mpsc::WeakSender<Envelope>) {
        let (tx, mut rx) = mpsc::channel::<Envelope>(64);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let weak = tx.downgrade();
        (tx, weak)
    }

    /// B4: an anchor that needs twenty minutes still admits the seat - no
    /// step ends on time.
    #[tokio::test(start_paused = true)]
    async fn an_anchor_twenty_minutes_late_still_admits() {
        let (rid, frames) = fat_anchor();
        let (_tx, weak) = sink();
        let mut io = Fake::new(frames, None, Duration::from_secs(20 * 60));
        let started = tokio::time::Instant::now();
        let got = admit(&mut io, &rid, &weak).await.expect("admitted");
        assert!(started.elapsed() >= Duration::from_secs(20 * 60));
        let Admission::AnchorFirst { blob, blocks, .. } = got else {
            panic!("the anchor came first");
        };
        assert!(blob.is_some() && blocks.len() == 1, "the pruned anchor");
        assert_eq!(io.readies, 1);
    }

    /// B4: a piece lost on the way is asked for by index, and the seat is
    /// admitted once it arrives.
    #[tokio::test(start_paused = true)]
    async fn a_lost_part_is_asked_for_again() {
        let (rid, frames) = fat_anchor();
        assert!(frames.len() > 3, "the blob travels in parts: {}", frames.len());
        let (_tx, weak) = sink();
        let mut io = Fake::new(frames, Some(2), Duration::ZERO);
        let got = admit(&mut io, &rid, &weak).await.expect("admitted");
        assert!(matches!(got, Admission::AnchorFirst { .. }));
        assert_eq!(io.wants, vec![vec![2]], "one ask, for the lost piece");
    }

    /// Every piece of the first send is lost: the rejoiner does not know
    /// the count, so it repeats its request until the anchor comes.
    #[tokio::test(start_paused = true)]
    async fn a_rejoiner_that_got_no_piece_asks_again() {
        let (rid, frames) = fat_anchor();
        let (_tx, weak) = sink();
        let mut io = Fake::new(frames, None, Duration::MAX);
        io.answer_reask = true;
        let got = tokio::time::timeout(
            Duration::from_secs(4 * 3600),
            admit(&mut io, &rid, &weak),
        )
        .await
        .expect("the rejoiner asked again")
        .expect("admitted");
        assert!(matches!(got, Admission::AnchorFirst { .. }));
        assert_eq!(io.reasks, 1, "one repeat brought the anchor");
        assert!(io.wants.is_empty());
    }

    /// A seal-first coordinator that reports the vote heard the request:
    /// no repeat.
    #[tokio::test(start_paused = true)]
    async fn a_coordinator_that_reports_progress_gets_no_repeat() {
        let (rid, _) = fat_anchor();
        let (_tx, weak) = sink();
        let mut io = Fake::new(Vec::new(), None, Duration::MAX);
        io.queue.push_back(RitualDelivery::Msg(
            RitualMsg::RecoverProgress {
                member: "petra".into(),
                need: 2,
                roster: vec!["petra".into(), "walter".into()],
                approved: vec!["petra".into()],
                version: "0.0.20".into(),
                attempt: String::new(),
                sealed: false,
            },
            COORDINATOR.into(),
        ));
        let waited = tokio::time::timeout(Duration::from_secs(3600), admit(&mut io, &rid, &weak)).await;
        assert!(waited.is_err(), "still waiting for the Welcome");
        assert_eq!(io.reasks, 0);
    }

    /// A Welcome with no anchor before it is the seal-first order (an
    /// older coordinator); a refusal ends the wait with its reason.
    #[tokio::test(start_paused = true)]
    async fn a_welcome_first_is_seal_first_and_a_refusal_ends_the_wait() {
        let (rid, _) = fat_anchor();
        let (_tx, weak) = sink();
        let mut io = Fake::new(Vec::new(), None, Duration::from_secs(3600));
        io.queue.push_back(RitualDelivery::Welcome(welcome(), COORDINATOR.into()));
        let got = admit(&mut io, &rid, &weak).await.expect("admitted");
        assert!(matches!(got, Admission::SealFirst { .. }));
        // a Welcome from anybody else is not the coordinator's
        let mut io = Fake::new(Vec::new(), None, Duration::from_secs(3600));
        io.queue.push_back(RitualDelivery::Welcome(welcome(), "someone-else".into()));
        io.queue.push_back(RitualDelivery::Msg(
            RitualMsg::RecoverRefused { member: "petra".into(), reason: "link outdated".into(), attempt: run_id() },
            COORDINATOR.into(),
        ));
        let err = admit(&mut io, &rid, &weak).await.err();
        assert_eq!(err.as_deref(), Some("the coordinator refused the request: link outdated"));
    }

    /// Review R1: every run on one link shares its transport key, and relays
    /// replay stored wraps. An earlier run's refusal and anchor pieces must
    /// not end or feed this run.
    #[tokio::test(start_paused = true)]
    async fn an_earlier_runs_refusal_and_pieces_do_not_touch_this_run() {
        let (rid, frames) = fat_anchor();
        let (_tx, weak) = sink();
        let mut io = Fake::new(frames, None, Duration::from_secs(5));
        let stale = invite::recover_attempt_id("an earlier key package");
        io.queue.push_back(RitualDelivery::Msg(
            RitualMsg::RecoverRefused { member: "petra".into(), reason: "seat still active".into(), attempt: stale.clone() },
            COORDINATOR.into(),
        ));
        io.queue.push_back(RitualDelivery::Msg(
            RitualMsg::RecoverAnchor { member: "petra".into(), index: 0, total: 1, event: "{}".into(), attempt: stale },
            COORDINATOR.into(),
        ));
        let got = admit(&mut io, &rid, &weak).await.expect("this run is admitted");
        assert!(matches!(got, Admission::AnchorFirst { .. }));
        assert!(io.wants.is_empty(), "the foreign piece never set the count");
    }

    /// Review R1, an older coordinator: a refusal naming no run may be a
    /// replay of an earlier run's - it does not end the wait.
    #[tokio::test(start_paused = true)]
    async fn a_refusal_naming_no_run_does_not_end_the_wait() {
        let (rid, _) = fat_anchor();
        let (_tx, weak) = sink();
        let mut io = Fake::new(Vec::new(), None, Duration::MAX);
        io.queue.push_back(RitualDelivery::Msg(
            RitualMsg::RecoverRefused { member: "petra".into(), reason: "seat still active".into(), attempt: String::new() },
            COORDINATOR.into(),
        ));
        let waited = tokio::time::timeout(Duration::from_secs(3600), admit(&mut io, &rid, &weak)).await;
        assert!(waited.is_err(), "still waiting");
    }

    /// Review R2: the sealed report marks the run re-admitted, so a cancel
    /// can say a new link is needed.
    #[tokio::test(start_paused = true)]
    async fn the_sealed_report_marks_the_run_readmitted() {
        let (rid, _) = fat_anchor();
        let (_tx, weak) = sink();
        let mut io = Fake::new(Vec::new(), None, Duration::MAX);
        let progress = |sealed| {
            RitualDelivery::Msg(
                RitualMsg::RecoverProgress {
                    member: "petra".into(),
                    need: 2,
                    roster: vec!["petra".into(), "walter".into()],
                    approved: vec!["petra".into(), "walter".into()],
                    version: "0.0.21".into(),
                    attempt: run_id(),
                    sealed,
                },
                COORDINATOR.into(),
            )
        };
        io.queue.push_back(progress(false));
        let attempt = run_id();
        let readmitted = std::sync::atomic::AtomicBool::new(false);
        let asked = std::sync::atomic::AtomicBool::new(false);
        let run = Run { attempt: &attempt, readmitted: &readmitted, asked: &asked };
        let _ = tokio::time::timeout(
            Duration::from_secs(60),
            await_admission(&mut io, &handover(&rid), "petra", &request(), &run, &weak, Some(1)),
        )
        .await;
        assert!(!readmitted.load(std::sync::atomic::Ordering::SeqCst), "a live vote is not the seal");
        io.queue.push_back(progress(true));
        let _ = tokio::time::timeout(
            Duration::from_secs(60),
            await_admission(&mut io, &handover(&rid), "petra", &request(), &run, &weak, Some(1)),
        )
        .await;
        assert!(readmitted.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// Every anchor piece of a ~150 KB blob fits the gift-wrap budget and
    /// the pieces rebuild the blob.
    #[test]
    fn a_large_blob_travels_in_parts_under_the_bound() {
        let (_, frames) = fat_anchor();
        let mut asm = crate::chain::blob_parts::BlobAssembly::default();
        let mut whole = None;
        let mut bytes = 0;
        for f in &frames {
            assert!(f.len() <= crate::chain::RECOVER_PART_MAX, "{}", f.len());
            bytes += f.len();
            if let Ok(WorkspaceEvent::CheckpointServedPart { blob_hash, index, total, data }) = serde_json::from_str(f) {
                whole = asm.accept(&blob_hash, index, total, &data).or(whole);
            }
        }
        assert!(bytes > 150_000, "{bytes}");
        assert!(whole.is_some(), "the parts rebuild the blob");
    }
}

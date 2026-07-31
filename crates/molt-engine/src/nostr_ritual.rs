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
        let _ = inbox.live(LIVE_WAIT).await;
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
            match molt_net::canonical_nostr_pk(&j.nostr_pk) {
                Ok(canonical) if canonical == sender => {}
                _ => {
                    tracing::warn!(
                        %sender,
                        "join request dropped: claimed nostr anchor is not the wrap's proven sealer"
                    );
                    continue;
                }
            }
            let cmd = Command::NetJoinRequested {
                seat: j.seat,
                member: j.name,
                identity_pk: j.identity_pk,
                nostr_pk: j.nostr_pk,
                proof: j.mac,
                // no queue handover on Nostr — the MAC-bound anchor IS the
                // reply address
                reply: String::new(),
                key_package: j.key_package,
                generation: Some(generation),
            };
            if !send_cmd(&tx, cmd).await {
                return;
            }
        }
    })
}

/// MLS-encrypt one RitualMsg and publish it as a 445 frame. The lock is
/// dropped BEFORE the publish await (never hold a std mutex across await).
/// Returns the carrier `created_at`.
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
    chan.publish_frame(&exporter, &ct).await.map_err(|e| e.to_string())
}

/// Fire-and-forget 445 publish for the founder's send legs (`Seal`,
/// `Genesis`). `fail` routes a publish failure into `NetRitualFailed` where
/// the founding must not silently hang (the pre-seal legs); `None` = log
/// loudly only (the genesis leg — the founder has already materialized, and
/// the member's own wait surfaces a relays-down condition).
pub(crate) fn spawn_publish_frame(
    chan: GroupChannel,
    group: Arc<Mutex<molt_net::MlsMember>>,
    msg: RitualMsg,
    what: &'static str,
) {
    spawn_publish_frame_with(chan, group, msg, what, None);
}

pub(crate) fn spawn_publish_frame_with(
    chan: GroupChannel,
    group: Arc<Mutex<molt_net::MlsMember>>,
    msg: RitualMsg,
    what: &'static str,
    fail: Option<(mpsc::WeakSender<Envelope>, u64)>,
) {
    tokio::spawn(async move {
        if let Err(e) = publish_frame_now(&chan, &group, &msg).await {
            tracing::error!(what, error = %e, "ritual 445 frame did not publish");
            if let Some((tx, generation)) = fail {
                let _ = send_cmd(
                    &tx,
                    Command::NetRitualFailed {
                        error: format!("{what} did not publish: {e}"),
                        generation: Some(generation),
                    },
                )
                .await;
            }
        }
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
) -> Option<RitualMsg> {
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
        Ok(molt_net::MlsIncoming::Application { plaintext, .. }) => {
            serde_json::from_slice(&plaintext).ok()
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
        loop {
            let Some((content, created_at)) = sub.recv(RECV_SLICE).await else {
                continue;
            };
            let Some(msg) = open_group_frame(&group, &content, created_at) else {
                continue;
            };
            let cmd = match msg {
                RitualMsg::Signed(s) => Command::NetSealSigned {
                    seat: s.seat,
                    sig: s.sig,
                    generation: Some(generation),
                },
                RitualMsg::Declined { seat } => Command::NetJoinDeclined {
                    seat,
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
    let _ = inbox.live(LIVE_WAIT).await;

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
            Some(RitualDelivery::Msg(RitualMsg::LinkSpent { .. }, sender))
                if sender == h.npub =>
            {
                return Err(
                    "this invite link was already used by someone else — ask the founder \
                     for a fresh, unused link"
                        .to_string(),
                );
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
                Some(RitualDelivery::Msg(RitualMsg::LinkSpent { .. }, sender))
                    if sender == h.npub =>
                {
                    return Err("the founder voided this seat — the link was re-used".to_string());
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
    let _ = sub.live(LIVE_WAIT).await;

    // Seal wait: the founder's charter proposal as a 445 in the born group.
    // verify_seal_proposal is THE ladder (content-derived id, our 3-anchor
    // seat, every seat's anchor) — sign-what-you-see, unchanged.
    let (proposal, table) = loop {
        let Some((content, created_at)) = sub.recv(RECV_SLICE).await else {
            continue;
        };
        let Some(msg) = open_group_frame(&group, &content, created_at) else {
            continue;
        };
        if let RitualMsg::Seal { proposal } = msg {
            let sealed: molt_core::SealedRoster =
                serde_json::from_str(&proposal).map_err(|e| format!("seal proposal: {e}"))?;
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

    // Genesis wait: the sealed roster as a 445. Sign-what-you-see closes
    // HERE — the distributed table must byte-equal the ratified one.
    let sealed_final = loop {
        let Some((content, created_at)) = sub.recv(RECV_SLICE).await else {
            continue;
        };
        let Some(msg) = open_group_frame(&group, &content, created_at) else {
            continue;
        };
        if let RitualMsg::Genesis { sealed, .. } = msg {
            let sealed: molt_core::SealedRoster =
                serde_json::from_str(&sealed).map_err(|e| format!("genesis: {e}"))?;
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

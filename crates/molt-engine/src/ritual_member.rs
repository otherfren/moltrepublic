// SPDX-License-Identifier: GPL-3.0-or-later

//! The **member side of the founding ritual** — ONE ladder for both
//! transports (`docs_archive/ritual/founding_ritual.md` §4 ❷–❽; review R11):
//!
//! ```text
//! activate (JoinRequest, ticket MAC) → accept wait → Seal
//!   → verify_seal_proposal → HUMAN ratifies → sign → ❻½ backup gate
//!   → attest → Genesis → byte-compare against the ratified table
//! ```
//!
//! The loopback leg (`founding.rs`, the test transport) and the Nostr leg
//! (`nostr_ritual.rs`, production) implement [`RitualLeg`]: what differs
//! between them is HOW a founder message is received and authenticated and
//! HOW a message reaches the founder — never what is verified, when, or in
//! which order. Sign-what-you-see lives here exactly once.
//!
//! The human gate ([`Ratify`]) is the second seam: the loopback tests are the
//! human on the far end of a [`Ratifier`]'s channels, the production join
//! surfaces the same steps as engine commands.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use molt_core::SealedRoster;
use molt_net::invite::{self, RitualMsg};
use tokio::sync::mpsc;

use crate::founding::verify_seal_proposal;

/// How long the joiner waits for the founder's `JoinAccepted` (a spent link
/// against a finished ritual, or an offline founder, answers nothing —
/// without the deadline the joiner hangs in "Contacting the inviter…"
/// forever). After the ack every wait is unbounded: the charter deliberation
/// is a human step and takes as long as it takes.
pub(crate) const ACCEPT_TIMEOUT: Duration = Duration::from_secs(90);

/// Which rung of the ladder a [`RitualLeg::next_msg`] call serves — a leg
/// may shape its idle/deaf notes by it (the Nostr leg's elapsed line in the
/// genesis wait); the verification never depends on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Before the founder acknowledged the activation (bounded wait).
    Accept,
    /// Acknowledged; waiting for the charter proposal.
    Seal,
    /// Signed; waiting for the human's phrase-backup proof.
    Backup,
    /// Attested; waiting for the sealed roster.
    Genesis,
}

/// One seat's derived member identity — the ONE derivation both legs share
/// (`founding_ritual.md` §2): the Ed25519 identity from the phrase alone, the
/// Nostr transport anchor salted with THIS seat's ticket.
pub(crate) struct MemberSeat {
    pub(crate) seat: u32,
    pub(crate) ticket: String,
    pub(crate) sk: molt_storage::SigningKey,
    pub(crate) pk: String,
    /// The transport secret — NOT re-derivable once the ticket is gone, so
    /// it must reach the sealed `transport.state` via the [`JoinOutcome`].
    /// Wiped on drop.
    pub(crate) nostr_sk: zeroize::Zeroizing<Vec<u8>>,
    pub(crate) nostr_pk: String,
}

impl MemberSeat {
    /// Derive the seat's identity from the phrase and the invite ticket.
    pub(crate) fn derive(seat: u32, ticket: &str, phrase: &str) -> Result<Self, String> {
        let entropy = molt_storage::seed_entropy(phrase).map_err(|e| e.to_string())?;
        let (sk, pk) = crate::founding::member_identity_from_entropy(&entropy);
        // ONE wiped-on-drop carrier — the stack copy is zeroized immediately
        let (mut nostr_raw, nostr_pk) = molt_net::nostr_identity(&entropy, ticket);
        let nostr_sk = zeroize::Zeroizing::new(nostr_raw.to_vec());
        zeroize::Zeroize::zeroize(&mut nostr_raw);
        Ok(MemberSeat {
            seat,
            ticket: ticket.to_string(),
            sk,
            pk,
            nostr_sk,
            nostr_pk,
        })
    }
}

/// What the member side produced: its anchored identity pk, and — when it
/// waited for it (`collect_genesis`) — the sealed roster the founder
/// distributed at the end, from which the member writes its **own** workspace.
#[doc(hidden)]
pub struct JoinOutcome {
    /// The member's identity public key (what the founder anchored).
    pub pk: String,
    /// The member's derived Nostr transport secret (32-byte secp256k1
    /// scalar — `molt_net::nostr_identity`, salted with this seat's ticket).
    /// The ticket dies with the ritual, so this is NOT re-derivable later:
    /// the caller must seal it into the member's `transport.state.nostr_sk`
    /// beside `identity_sk`. `Zeroizing` — a caller that drops the outcome
    /// (a failed join tail) wipes the scalar with it.
    pub nostr_sk: zeroize::Zeroizing<Vec<u8>>,
    /// The complete sealed roster, present only when `collect_genesis` was
    /// set and the founder finished distributing it.
    pub sealed: Option<SealedRoster>,
    /// The member's own MLS group snapshot after processing the Welcome (and,
    /// if a bootstrap ran, advancing the ratchet through its announcements) —
    /// present only when `collect_genesis` was set and a Welcome arrived. The
    /// caller seals it into the member's `transport.state`.
    pub mls_snapshot: Option<Vec<u8>>,
    /// The member's assembled runtime full-mesh handovers — present only when
    /// the loopback leg's bootstrap ran to completion. The caller seals them
    /// into `transport.state.mesh` and builds the runtime supervisor.
    pub mesh: Option<Vec<molt_core::MeshLink>>,
}

/// The transport under the ladder: one leg per transport family.
///
/// A leg surfaces ONLY founder-authenticated messages — the private reply
/// queue authenticates on loopback, the NIP-59 wrap sealer / the MLS frame
/// author on Nostr — so the ladder never sees a co-member's frame.
pub(crate) trait RitualLeg {
    /// The next ritual message from the founder. `deadline` bounds the wait
    /// (the accept wait); the leg words its own timeout. Must be
    /// cancel-safe: in the backup gate the ladder selects on it against the
    /// human's answer, so a message pulled off the wire must be returned
    /// without a further await.
    fn next_msg(
        &mut self,
        phase: Phase,
        deadline: Option<tokio::time::Instant>,
    ) -> impl Future<Output = Result<RitualMsg, String>> + Send;

    /// Send one message to the founder.
    fn send(&mut self, msg: &RitualMsg) -> impl Future<Output = Result<(), String>> + Send;

    /// The reply-queue handover the JoinRequest advertises (loopback: the
    /// queue the member receives on; Nostr: none — the anchor IS the address).
    fn reply_handover(&self) -> Option<invite::ReplyHandover>;

    /// The relays the JoinRequest declares as dialable (Nostr) — empty on
    /// loopback.
    fn declared_relays(&self) -> Vec<String>;

    /// The member's MLS group, shared with the leg once built — the Nostr leg
    /// joins it from the Welcome and opens 445 frames with it.
    fn attach_group(&mut self, _group: Arc<Mutex<molt_net::MlsMember>>) {}

    /// Whether the leg already joined the group (Nostr: the Welcome arrives
    /// before the deliberation; loopback: it rides the genesis).
    fn in_group(&self) -> bool;

    /// A leg's own check of a Seal / Genesis against what the invite LINK
    /// promised (Nostr: founder seat + rule). Runs after the parse, before
    /// `verify_seal_proposal`; an `Err` is fatal.
    fn vet_proposal(&self, _proposal: &SealedRoster) -> Result<(), String> {
        Ok(())
    }

    /// After the genesis verified and the group is joined: the loopback leg
    /// runs the post-founding mesh bootstrap here (best-effort — `None`
    /// enters without a direct mesh). `early` holds mesh announcements that
    /// raced ahead of the genesis.
    fn finish(
        &mut self,
        name: &str,
        sealed: &SealedRoster,
        group: &Arc<Mutex<molt_net::MlsMember>>,
        early: Vec<Vec<u8>>,
    ) -> impl Future<Output = Option<Vec<molt_core::MeshLink>>> + Send;
}

/// The human on the far end of the ladder: the ratification gate and the
/// phrase-backup gate (`founding_ritual.md` ❺, ❻½). `None` at the ladder
/// means a non-interactive path (sim members, the standalone CLI join),
/// which ratifies and attests as soon as the table verifies.
pub(crate) trait Ratify {
    /// The founder acknowledged the join — early feedback instead of a
    /// silent wait. Best-effort.
    fn accepted(&mut self) -> impl Future<Output = ()> + Send;

    /// Surface the proposed `(final name, agenda, feature selection)` for
    /// review (`None` features = a pre-v5 founder).
    fn propose(
        &mut self,
        name: &str,
        agenda: &str,
        features: Option<&[String]>,
    ) -> impl Future<Output = ()> + Send;

    /// The human's decision: `Some(true)` ratifies (sign), `Some(false)`
    /// declines, `None` = the gate closed without an answer (cancel).
    fn confirm(&mut self) -> impl Future<Output = Option<bool>> + Send;

    /// The human's phrase-backup proof: `Some(true)` releases the
    /// attestation; anything else cancels the join. Cancel-safe (selected
    /// against the wire).
    fn backup(&mut self) -> impl Future<Output = Option<bool>> + Send;
}

/// The joiner's human **ratification gate** over channels: the loopback
/// tests are the human on the other end. Signing *is* the ratification
/// (concept §3.3).
#[doc(hidden)]
pub struct Ratifier {
    /// Fires once when the founder acknowledges the join (`JoinAccepted`) — the
    /// joiner's wizard shows "you're in, waiting for the deliberation" instead of
    /// a silent wait. Best-effort (capacity 1; a resend is dropped).
    pub accepted: mpsc::Sender<()>,
    /// The proposed `(final name, agenda, feature selection)` surfaced for
    /// the human to review (`None` features = a pre-v5 founder).
    pub proposal: mpsc::Sender<(String, String, Option<Vec<String>>)>,
    /// The human's decision: `true` ratifies (sign); `false` declines and
    /// aborts the join; a closed channel cancels it.
    pub confirm: mpsc::Receiver<bool>,
    /// The human's phrase-backup proof (`seed_backup_confirmation.md` ❻½),
    /// AFTER ratifying: `true` releases the signed attestation; a closed
    /// channel cancels the join. Sim/CLI paths (a `None` ratifier) attest
    /// automatically.
    pub backup: mpsc::Receiver<bool>,
}

impl Ratify for Ratifier {
    async fn accepted(&mut self) {
        let _ = self.accepted.try_send(());
    }

    async fn propose(&mut self, name: &str, agenda: &str, features: Option<&[String]>) {
        let _ = self
            .proposal
            .send((name.to_string(), agenda.to_string(), features.map(<[String]>::to_vec)))
            .await;
    }

    async fn confirm(&mut self) -> Option<bool> {
        self.confirm.recv().await
    }

    async fn backup(&mut self) -> Option<bool> {
        self.backup.recv().await
    }
}

/// The founder refused the activation — its reason travels with the frame
/// (review R5: "ask for your own link" is wrong advice for the same person
/// retrying after the group formed), a bare refusal gets the generic line.
fn link_spent_error(accepted: bool, reason: &str) -> String {
    match (accepted, reason.is_empty()) {
        (false, true) => "this invite link was already used by someone else - ask the founder \
                          for a fresh one"
            .to_string(),
        (false, false) => format!("the founder refused this activation: {reason}"),
        (true, true) => "the founder voided this seat - the link was re-used".to_string(),
        (true, false) => format!("the founder voided this seat: {reason}"),
    }
}

fn aborted_error(reason: &str) -> String {
    format!("the founder ended this founding: {reason}")
}

fn poisoned() -> String {
    "mls lock poisoned".to_string()
}

/// Run the member ladder over `leg` for `seat`, as `name`. `collect_genesis`
/// = wait for the sealed roster (a sim member stops at its attestation).
///
/// Every verification of the founding ritual's member side happens HERE,
/// once: `verify_seal_proposal` over the shown proposal before signing (the
/// signed bytes are recomputed from what is shown — never a founder-supplied
/// blob), and the genesis-time byte comparison of the distributed roster
/// against the exact bytes ratified. A leg can only feed messages in and
/// carry them out.
pub(crate) async fn run_member_ladder<L: RitualLeg, R: Ratify>(
    leg: &mut L,
    name: &str,
    seat: MemberSeat,
    mut human: Option<R>,
    collect_genesis: bool,
) -> Result<JoinOutcome, String> {
    let MemberSeat {
        seat: seat_no,
        ticket,
        sk,
        pk,
        nostr_sk,
        nostr_pk,
    } = seat;
    // the MLS member, built from the *same* identity key (concept §3.3: one
    // identity anchors both the genesis table and the MLS credential). Its
    // KeyPackage rides the JoinRequest; the group lives until it is
    // snapshotted into transport.state.
    let mls = molt_net::MlsMember::new(&sk, name).map_err(|e| format!("mls identity: {e}"))?;
    let key_package = mls.key_package().map_err(|e| format!("key package: {e}"))?;
    let group = Arc::new(Mutex::new(mls));
    leg.attach_group(group.clone());

    // ❷ activate: JoinRequest, MAC-bound to the ticket
    let join = RitualMsg::Join(invite::JoinRequest {
        seat: seat_no,
        name: name.to_string(),
        identity_pk: pk.clone(),
        nostr_pk: nostr_pk.clone(),
        mac: invite::join_mac(&ticket, name, &pk, &nostr_pk),
        reply: leg.reply_handover(),
        key_package: hex::encode(&key_package),
        relays: leg.declared_relays(),
    });
    leg.send(&join)
        .await
        .map_err(|e| format!("join request: {e}"))?;

    // ❸/❹ the accept wait is bounded UNTIL the founder acknowledges; the
    // wait for the charter proposal after that is not (human deliberation)
    let accept_deadline = tokio::time::Instant::now() + ACCEPT_TIMEOUT;
    let mut accepted = false;
    let proposal_json = loop {
        let (phase, deadline) = if accepted {
            (Phase::Seal, None)
        } else {
            (Phase::Accept, Some(accept_deadline))
        };
        match leg.next_msg(phase, deadline).await? {
            // idempotent: a redelivered ack (or the Nostr Welcome after the
            // ack) must not stack "accepted" lines
            RitualMsg::JoinAccepted { .. } if !accepted => {
                accepted = true;
                if let Some(h) = human.as_mut() {
                    h.accepted().await;
                }
            }
            // the single-use ticket was already spent (the same link went
            // to two people, or this is a retry after the group formed):
            // fail fast with the founder's reason — a fresh link may or may
            // not be the way out, and only the founder knows which
            RitualMsg::LinkSpent { reason, .. } => {
                return Err(link_spent_error(accepted, &reason));
            }
            RitualMsg::Aborted { reason } => return Err(aborted_error(&reason)),
            RitualMsg::Seal { proposal } => break proposal,
            _ => {}
        }
    };
    let proposal: SealedRoster = serde_json::from_str(&proposal_json)
        .map_err(|e| format!("seal proposal rejected: {e}"))?;
    leg.vet_proposal(&proposal)
        .map_err(|e| format!("seal proposal rejected: {e}"))?;
    // ❺ verify what we are about to ratify BEFORE we sign, and recompute the
    // exact bytes to sign from the shown proposal — so what we sign provably
    // equals the name + agenda + roster we ratify (including OUR derived
    // nostr anchor: a split third anchor is rejected before we sign)
    let table = verify_seal_proposal(&proposal, name, &pk, &nostr_pk)
        .map_err(|e| format!("seal proposal rejected: {e}"))?;
    // the human ratification gate: surface the charter and wait for the
    // confirm before signing
    if let Some(h) = human.as_mut() {
        h.propose(&proposal.name, &proposal.agenda, proposal.features.as_deref())
            .await;
        match h.confirm().await {
            Some(true) => {}
            Some(false) => {
                // an explicit decline is TOLD to the founder (its seat shows
                // declined); a gate that merely closed says nothing
                let _ = leg.send(&RitualMsg::Declined { seat: seat_no }).await;
                return Err("the charter was declined".to_string());
            }
            None => return Err("the ritual was cancelled".to_string()),
        }
    }
    let sig = molt_storage::identity_sign(&sk, &table);
    leg.send(&RitualMsg::Signed(invite::SealSigned { seat: seat_no, sig }))
        .await
        .map_err(|e| format!("seal signature did not publish: {e}"))?;

    // ❻½ (seed_backup_confirmation.md): the phrase-backup round. The
    // interactive path waits for the HUMAN's re-typed proof before the
    // attestation goes out — and keeps listening: a Genesis arriving DURING
    // that wait is a founder that sealed without us (protocol violation: an
    // honest founder cannot reach ❼ before our confirmation), an abort ends
    // the wait instead of leaving the human to confirm into a dead founding.
    // The non-interactive paths attest right away, as they auto-ratify.
    let mut early_mesh: Vec<Vec<u8>> = Vec::new();
    if let Some(h) = human.as_mut() {
        loop {
            tokio::select! {
                confirmed = h.backup() => match confirmed {
                    Some(true) => break,
                    _ => {
                        return Err(
                            "the ritual was cancelled before the backup confirmation".to_string(),
                        )
                    }
                },
                msg = leg.next_msg(Phase::Backup, None) => match msg? {
                    RitualMsg::Genesis { .. } => {
                        return Err(
                            "the founder sealed before our backup confirmation - protocol violation"
                                .to_string(),
                        );
                    }
                    RitualMsg::Aborted { reason } => return Err(aborted_error(&reason)),
                    RitualMsg::MeshAnnounce { ct } => {
                        if let Ok(b) = hex::decode(&ct) {
                            early_mesh.push(b);
                        }
                    }
                    _ => {}
                },
            }
        }
    }
    let att = molt_storage::backup_confirm_bytes(&table);
    let att_sig = molt_storage::identity_sign(&sk, &att);
    leg.send(&RitualMsg::BackupConfirmed {
        seat: seat_no,
        sig: att_sig,
    })
    .await
    .map_err(|e| format!("backup attestation did not publish: {e}"))?;

    if !collect_genesis {
        // sim members stop at their attestation; their KeyPackage still
        // joined the founder's group, they just never process the Welcome
        return Ok(JoinOutcome {
            pk,
            nostr_sk,
            sealed: None,
            mls_snapshot: None,
            mesh: None,
        });
    }

    // ❽ the sealed roster, once every seat attested. A `MeshAnnounce` that
    // races ahead of the genesis (the loopback founder starts its bootstrap
    // right after distributing) is buffered, not dropped.
    let (sealed_json, welcome) = loop {
        match leg.next_msg(Phase::Genesis, None).await? {
            RitualMsg::MeshAnnounce { ct } => {
                if let Ok(b) = hex::decode(&ct) {
                    early_mesh.push(b);
                }
            }
            RitualMsg::Aborted { reason } => return Err(aborted_error(&reason)),
            RitualMsg::Genesis { sealed, welcome } => break (sealed, welcome),
            _ => {}
        }
    };
    let sealed: SealedRoster = serde_json::from_str(&sealed_json)
        .map_err(|e| format!("distributed sealed roster rejected: {e}"))?;
    leg.vet_proposal(&sealed)
        .map_err(|e| format!("distributed sealed roster rejected: {e}"))?;
    // sign-what-you-see closes at the GENESIS: the roster we MATERIALIZE
    // must be byte-identically the table we RATIFIED. verify_seal_proposal
    // re-runs the full checks over the DISTRIBUTED roster (content-derived
    // id, our 3-anchor seat, every seat's anchor format) and returns its
    // canonical bytes, which must equal the exact bytes we signed. Without
    // this, a founder could run the ritual honestly through ratification
    // and then seal a DIFFERENT, fully self-consistent table (e.g. our seat
    // swapped to attacker keys, all n attestations self-signed) —
    // verify_sealed_roster alone cannot catch that, it has no memory of the
    // proposal.
    let sealed_table = verify_seal_proposal(&sealed, name, &pk, &nostr_pk)
        .map_err(|e| format!("distributed sealed roster rejected: {e}"))?;
    if sealed_table != table {
        return Err(
            "the sealed roster is not the table we ratified - the founder distributed a \
             different constitution"
                .to_string(),
        );
    }

    // the group: the Nostr leg joined from its Welcome before the
    // deliberation; on loopback the Welcome rides the genesis. A founding
    // without either (a pre-MLS founder) leaves us groupless.
    if !leg.in_group() {
        if welcome.is_empty() {
            return Ok(JoinOutcome {
                pk,
                nostr_sk,
                sealed: Some(sealed),
                mls_snapshot: None,
                mesh: None,
            });
        }
        let bytes = hex::decode(&welcome).map_err(|e| e.to_string())?;
        group
            .lock()
            .map_err(|_| poisoned())?
            .join_from_welcome(&bytes)
            .map_err(|e| e.to_string())?;
    }
    let mesh = leg.finish(name, &sealed, &group, early_mesh).await;
    // snapshot AFTER the leg finished: a bootstrap advanced the ratchet
    let snap = group
        .lock()
        .map_err(|_| poisoned())?
        .snapshot()
        .map_err(|e| format!("mls snapshot: {e}"))?;
    Ok(JoinOutcome {
        pk,
        nostr_sk,
        sealed: Some(sealed),
        mls_snapshot: Some(snap),
        mesh,
    })
}

#[cfg(test)]
mod tests {
    //! The ladder's own keystones, over a scripted leg — transport-free, so
    //! they pin the member side for BOTH transports at once.

    use super::*;
    use molt_core::MemberIdentity;
    use std::collections::VecDeque;

    /// A leg that plays a founder's script: yields the next scripted message
    /// on every `next_msg` (and waits forever once the script is out),
    /// records everything sent.
    struct ScriptedLeg {
        script: VecDeque<RitualMsg>,
        sent: Vec<RitualMsg>,
    }

    impl RitualLeg for ScriptedLeg {
        async fn next_msg(
            &mut self,
            _phase: Phase,
            deadline: Option<tokio::time::Instant>,
        ) -> Result<RitualMsg, String> {
            match self.script.pop_front() {
                Some(m) => Ok(m),
                None => match deadline {
                    Some(d) => {
                        tokio::time::sleep_until(d).await;
                        Err("scripted founder went silent".to_string())
                    }
                    None => std::future::pending().await,
                },
            }
        }

        async fn send(&mut self, msg: &RitualMsg) -> Result<(), String> {
            self.sent.push(msg.clone());
            Ok(())
        }

        fn reply_handover(&self) -> Option<invite::ReplyHandover> {
            None
        }

        fn declared_relays(&self) -> Vec<String> {
            Vec::new()
        }

        fn in_group(&self) -> bool {
            false
        }

        async fn finish(
            &mut self,
            _name: &str,
            _sealed: &SealedRoster,
            _group: &Arc<Mutex<molt_net::MlsMember>>,
            _early: Vec<Vec<u8>>,
        ) -> Option<Vec<molt_core::MeshLink>> {
            None
        }
    }

    /// The honest 2-of-2 proposal seating the founder and `seat` (bob).
    fn proposal_for(seat: &MemberSeat, agenda: &str) -> SealedRoster {
        let (_f_sk, f_pk) = molt_storage::derive_identity_key(&[7u8; 32], "f");
        let identities = vec![
            MemberIdentity {
                member: "founder".to_string(),
                identity_pk: f_pk,
                nostr_pk: molt_net::nostr_identity(b"founder-entropy", "self").1,
            },
            MemberIdentity {
                member: "bob".to_string(),
                identity_pk: seat.pk.clone(),
                nostr_pk: seat.nostr_pk.clone(),
            },
        ];
        let rid = molt_storage::republic_id("R", 2, 2, &identities);
        SealedRoster {
            name: "R".to_string(),
            republic_id: rid,
            rule_m: 2,
            rule_n: 2,
            roster: vec!["founder".to_string(), "bob".to_string()],
            identities,
            attestations: Vec::new(),
            agenda: agenda.to_string(),
            relays: Vec::new(),
            features: None,
        }
    }

    fn bob() -> MemberSeat {
        let phrase = molt_storage::generate_seed_phrase().expect("phrase");
        MemberSeat::derive(0, &"ab".repeat(32), &phrase).expect("seat")
    }

    /// The ladder's failure — a [`JoinOutcome`] carries a secret and has no
    /// `Debug`, so `expect_err` cannot unwrap it.
    fn failure(res: Result<JoinOutcome, String>) -> String {
        match res {
            Err(e) => e,
            Ok(_) => panic!("the ladder must fail here"),
        }
    }

    fn seal(p: &SealedRoster) -> RitualMsg {
        RitualMsg::Seal {
            proposal: serde_json::to_string(p).expect("json"),
        }
    }

    fn genesis(p: &SealedRoster) -> RitualMsg {
        RitualMsg::Genesis {
            sealed: serde_json::to_string(p).expect("json"),
            welcome: String::new(),
        }
    }

    /// The channel-side of a [`Ratifier`] the test drives by hand.
    struct Human {
        _accepted: mpsc::Receiver<()>,
        proposal: mpsc::Receiver<(String, String, Option<Vec<String>>)>,
        confirm: mpsc::Sender<bool>,
        /// Held open: dropping it would read as a cancel.
        _backup: mpsc::Sender<bool>,
    }

    fn ratifier() -> (Ratifier, Human) {
        let (acc_tx, acc_rx) = mpsc::channel(1);
        let (prop_tx, prop_rx) = mpsc::channel(1);
        let (conf_tx, conf_rx) = mpsc::channel(1);
        let (bak_tx, bak_rx) = mpsc::channel(1);
        (
            Ratifier {
                accepted: acc_tx,
                proposal: prop_tx,
                confirm: conf_rx,
                backup: bak_rx,
            },
            Human {
                _accepted: acc_rx,
                proposal: prop_rx,
                confirm: conf_tx,
                _backup: bak_tx,
            },
        )
    }

    /// The happy path, non-interactive: the ladder sends Join → Signed →
    /// BackupConfirmed in that order, signs exactly the recomputed table, and
    /// hands the sealed roster back.
    #[tokio::test]
    async fn the_ladder_activates_signs_attests_and_collects_the_genesis() {
        let seat = bob();
        let p = proposal_for(&seat, "the pact");
        let table = verify_seal_proposal(&p, "bob", &seat.pk, &seat.nostr_pk).expect("table");
        let pk = seat.pk.clone();
        let mut leg = ScriptedLeg {
            script: VecDeque::from(vec![
                RitualMsg::JoinAccepted { seat: 0 },
                seal(&p),
                genesis(&p),
            ]),
            sent: Vec::new(),
        };
        let out = run_member_ladder(&mut leg, "bob", seat, None::<Ratifier>, true)
            .await
            .expect("the honest ritual completes");
        assert_eq!(out.pk, pk);
        assert_eq!(out.sealed.expect("sealed").agenda, "the pact");
        assert!(out.mls_snapshot.is_none(), "no Welcome, no group");
        let kinds: Vec<&str> = leg
            .sent
            .iter()
            .map(|m| match m {
                RitualMsg::Join(_) => "join",
                RitualMsg::Signed(_) => "signed",
                RitualMsg::BackupConfirmed { .. } => "backup",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["join", "signed", "backup"]);
        let RitualMsg::Signed(s) = &leg.sent[1] else {
            panic!("second send is the signature")
        };
        assert!(
            molt_storage::identity_verify(&pk, &table, &s.sig),
            "the signature covers the recomputed table"
        );
    }

    /// Sign-what-you-see closes at the genesis: a distributed roster that
    /// passes every check but differs from the ratified bytes (a charter
    /// swap) is refused — for every transport, since it is refused HERE.
    #[tokio::test]
    async fn a_sealed_table_differing_from_the_ratified_one_is_rejected() {
        let seat = bob();
        let p = proposal_for(&seat, "the ratified charter");
        let swapped = proposal_for(&seat, "a charter nobody ratified");
        let mut leg = ScriptedLeg {
            script: VecDeque::from(vec![
                RitualMsg::JoinAccepted { seat: 0 },
                seal(&p),
                genesis(&swapped),
            ]),
            sent: Vec::new(),
        };
        let err = failure(run_member_ladder(&mut leg, "bob", seat, None::<Ratifier>, true).await);
        assert!(err.contains("not the table we ratified"), "{err}");
    }

    /// ❻½: a Genesis that arrives WHILE the human is still proving the
    /// backup is a founder that sealed without our attestation — the join
    /// ends as a protocol violation instead of confirming into it. (The
    /// loopback twin always had this check; the Nostr twin did not.)
    #[tokio::test]
    async fn a_genesis_during_the_backup_wait_is_a_protocol_violation() {
        let seat = bob();
        let p = proposal_for(&seat, "the pact");
        let (ratifier, mut human) = ratifier();
        let mut leg = ScriptedLeg {
            script: VecDeque::from(vec![
                RitualMsg::JoinAccepted { seat: 0 },
                seal(&p),
                genesis(&p),
            ]),
            sent: Vec::new(),
        };
        let run = tokio::spawn(async move {
            let r = run_member_ladder(&mut leg, "bob", seat, Some(ratifier), true).await;
            (r, leg.sent)
        });
        human.proposal.recv().await.expect("the charter is surfaced");
        human.confirm.send(true).await.expect("ratify");
        // the human never confirms the backup — the founder's Genesis lands first
        let (res, sent) = run.await.expect("task");
        let err = failure(res);
        assert!(err.contains("protocol violation"), "{err}");
        assert!(
            !sent.iter().any(|m| matches!(m, RitualMsg::BackupConfirmed { .. })),
            "no attestation went out: {}",
            sent.len()
        );
        drop(human);
    }

    /// The founder's abort ends the backup wait too — the human is not left
    /// to confirm into a dead founding.
    #[tokio::test]
    async fn an_abort_during_the_backup_wait_ends_the_join() {
        let seat = bob();
        let p = proposal_for(&seat, "the pact");
        let (ratifier, mut human) = ratifier();
        let mut leg = ScriptedLeg {
            script: VecDeque::from(vec![
                RitualMsg::JoinAccepted { seat: 0 },
                seal(&p),
                RitualMsg::Aborted {
                    reason: "cancelled".to_string(),
                },
            ]),
            sent: Vec::new(),
        };
        let run = tokio::spawn(async move {
            run_member_ladder(&mut leg, "bob", seat, Some(ratifier), true).await
        });
        human.proposal.recv().await.expect("the charter is surfaced");
        human.confirm.send(true).await.expect("ratify");
        let err = failure(run.await.expect("task"));
        assert!(err.contains("the founder ended this founding"), "{err}");
        drop(human);
    }

    /// An explicit decline tells the founder; a closed gate does not.
    #[tokio::test]
    async fn a_decline_is_told_to_the_founder_a_closed_gate_is_not() {
        for (answer, expect_declined, expect_err) in [
            (Some(false), true, "declined"),
            (None, false, "cancelled"),
        ] {
            let seat = bob();
            let p = proposal_for(&seat, "the pact");
            let (ratifier, mut human) = ratifier();
            let mut leg = ScriptedLeg {
                script: VecDeque::from(vec![RitualMsg::JoinAccepted { seat: 0 }, seal(&p)]),
                sent: Vec::new(),
            };
            let run = tokio::spawn(async move {
                let r = run_member_ladder(&mut leg, "bob", seat, Some(ratifier), true).await;
                (r, leg.sent)
            });
            human.proposal.recv().await.expect("the charter is surfaced");
            match answer {
                Some(a) => human.confirm.send(a).await.expect("answer"),
                None => drop(human.confirm),
            }
            let (res, sent) = run.await.expect("task");
            let err = failure(res);
            assert!(err.contains(expect_err), "{err}");
            assert_eq!(
                sent.iter().any(|m| matches!(m, RitualMsg::Declined { .. })),
                expect_declined,
                "answer {answer:?}: sent {}",
                sent.len()
            );
        }
    }

    /// The founder's refusal reason reaches the human verbatim; a bare
    /// refusal gets the generic line.
    #[tokio::test]
    async fn a_spent_link_surfaces_the_founders_reason() {
        for (reason, expect) in [
            ("", "already used by someone else"),
            ("group already formed", "the founder refused this activation: group already formed"),
        ] {
            let seat = bob();
            let mut leg = ScriptedLeg {
                script: VecDeque::from(vec![RitualMsg::LinkSpent {
                    seat: 0,
                    reason: reason.to_string(),
                }]),
                sent: Vec::new(),
            };
            let err = failure(run_member_ladder(&mut leg, "bob", seat, None::<Ratifier>, true).await);
            assert!(err.contains(expect), "{err}");
        }
    }
}

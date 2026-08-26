// SPDX-License-Identifier: GPL-3.0-or-later

//! **Membership blocks — the recovery re-admission**: proposing and
//! registering a `Membership` change, the consent-verified auto-approval
//! (`recovery_auto_approval.md`), the coordinator's seat-proof ladder
//! (`verify_and_propose_restore`) and its progress reports toward the
//! waiting rejoiner, the chain-wide anchor replay register, and the MLS
//! re-key + Welcome once a `Restored` block committed — the mesh arm
//! ([`State::coordinator_rekey`]) and the Nostr arm
//! ([`State::coordinator_rekey_nostr`]). Seats are fixed at founding:
//! `Joined` stays a reserved variant the verifier refuses.

use super::*;

/// The coordinator's snapshot of a pending re-admission vote
/// (`recovery_auto_approval.md` §4) — what [`State::recover_progress_for`]
/// reports toward the waiting rejoiner's checklist.
#[derive(Debug, Clone)]
pub(crate) struct RecoverProgressReport {
    /// The returning seat.
    pub member: String,
    /// Approvals needed (m).
    pub need: u32,
    /// The full roster, in roster order.
    pub roster: Vec<String>,
    /// Members whose voice is counted (verified approvals + the consent).
    pub approved: Vec<String>,
    /// The rejoiner's NEW transport anchor (the gift-wrap address); `None`
    /// on an anchor-less (loopback) recovery.
    pub to: Option<String>,
}

/// The group's rejoin notification — the one thing members see, so it says
/// WHICH door the seat came back through (`detached_reattach.md` §6): a
/// survivor-minted link, or the self-service reattach from a restored
/// backup.
fn rejoin_note(ticketed: bool, member: &str) -> String {
    if ticketed {
        format!("🔑 {member} rejoined the republic after recovery")
    } else {
        format!("🔁 {member} reconnected from a restored backup")
    }
}

/// A recovery in flight on the coordinator: the returning member's fresh MLS
/// KeyPackage + reply-queue handover, kept (in `RecoveryState::pending`,
/// keyed by the returning seat) until its `Restored` block commits — then
/// the coordinator re-keys the group (`restore_member`) and sends the
/// Welcome back to `reply`.
#[derive(Debug, Clone)]
pub(crate) struct PendingRecovery {
    /// Whether the request came over a minted link (`false` = the
    /// self-service reattach) — chooses the group's notification line.
    pub ticketed: bool,
    /// The returning member's fresh MLS KeyPackage, hex.
    pub key_package: String,
    /// The reply queue the Welcome goes back to (mesh arm).
    pub reply: String,
}

/// What a Nostr re-key produced — everything the wire needs, keyed at the
/// stamp the commit was made with.
pub(crate) struct NostrRekey {
    /// The raw MLS commit. It ships RAW inside the 445, never wrapped in an
    /// application ciphertext: a recipient needs it to REACH the new epoch.
    pub commit: Vec<u8>,
    /// The MLS Welcome that puts the returning seat back in the group.
    pub welcome: Vec<u8>,
    /// The exporter secret of the epoch this node just **left** — the one its
    /// recipients are still at.
    ///
    /// A receiver's exporter ring reaches BACKWARD only, so a commit sealed
    /// under the new epoch is opaque to exactly the members it exists to move
    /// forward (`9900f36`). The queue path has no outer layer, which is why
    /// this only bites on 445.
    pub prev_exporter: [u8; 32],
    /// The carrier stamp the commit was keyed with, and the one it MUST be
    /// published at.
    ///
    /// `CommitKey(created_at, sha256(commit))` breaks a concurrent same-epoch
    /// race, and the rule (`molt-net/CLAUDE.md`) is that the stamp comes from
    /// the same source on both sides. The 445 receive side reads the real
    /// `created_at` off the wire, so a sender that let the outbox pick the
    /// publish time would key its own commit at one value while every
    /// receiver keys it at another — the two ends then pick different winners
    /// and diverge permanently under ONE epoch number, silently.
    pub stamp: u64,
}

/// Re-key a Nostr republic's group: replace `member`'s leaf, at a carrier
/// stamp the caller pinned **before** the commit was made.
///
/// The mesh twin is `NetRuntime::restore_member_on_group`, which reaches the
/// group through `real_crypto` — a Nostr republic has no `NetRuntime` at all,
/// its group MLS lives on `GroupNet`.
pub(crate) fn nostr_rekey(
    mls: &std::sync::Mutex<molt_net::MlsMember>,
    member: &str,
    key_package: &[u8],
    stamp: u64,
) -> Result<NostrRekey, String> {
    let mut group = mls
        .lock()
        .map_err(|_| "the group lock is poisoned".to_string())?;
    let (commit, welcome) = group
        .restore_member(member, key_package, stamp)
        .map_err(|e| e.to_string())?;
    // read AFTER the commit: the ring's newest entry is now the epoch the
    // commit was made from, which is where its recipients still are
    let prev_exporter = group.exporter_ring().first().copied().ok_or_else(|| {
        "the re-key left no previous exporter - the commit would seal opaque".to_string()
    })?;
    Ok(NostrRekey { commit, welcome, prev_exporter, stamp })
}

impl State {
    /// Propose a membership change (re-admit a returning member, or add a seat)
    /// and co-sign it — the producer for `Membership` blocks (recovery step ❹).
    /// Further approvals arrive from the other members; a block seals at m-of-n.
    /// Returns the proposal id.
    pub(crate) fn propose_membership(
        &mut self,
        op: MembershipOp,
        member: &str,
        identity_pk: &str,
        nostr_pk: Option<String>,
        relays: Vec<String>,
        consent: Option<String>,
    ) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.chain.proposal_changes.insert(
            id,
            ChainChange::Membership {
                op,
                member: member.to_string(),
                identity_pk: identity_pk.to_string(),
                nostr_pk: nostr_pk.clone(),
                relays: relays.clone(),
                consent: consent.clone(),
            },
        );
        // announce the proposal over the mesh so every member registers + signs
        // the SAME change (the membership twin of a gated `Proposed`)
        let me = self.member();
        let env = self.make_env(
            me,
            WorkspaceEvent::MembershipProposed {
                id: ProposalId(id),
                op,
                member: member.to_string(),
                identity_pk: identity_pk.to_string(),
                nostr_pk: nostr_pk.clone(),
                relays,
                consent,
            },
        );
        self.record(env);
        if self.config.self_cosign {
            self.chain_sign_and_gossip_approval(id);
        }
        id
    }

    /// Register a membership proposal another member put forward, so this node
    /// signs the SAME change (its bytes) when it approves.
    #[allow(clippy::too_many_arguments)] // one gossiped change's fields, not a bag
    pub(crate) fn receive_membership_proposal(
        &mut self,
        id: u64,
        op: MembershipOp,
        member: &str,
        identity_pk: &str,
        nostr_pk: Option<String>,
        relays: Vec<String>,
        consent: Option<String>,
    ) {
        let change = ChainChange::Membership {
            op,
            member: member.to_string(),
            identity_pk: identity_pk.to_string(),
            nostr_pk,
            relays,
            consent,
        };
        if !self.admits_membership_proposal(id, &change) {
            return;
        }
        self.next_id = self.next_id.max(id.saturating_add(1));
        self.chain.proposal_changes.insert(id, change);
        // L2: signatures that OUTRAN this change become displayable now
        self.reverify_pending(id);
        // recovery_auto_approval.md §3: a consent this node can verify itself
        // needs no human voice — sign it now, so a recovery completes as soon
        // as m survivors are online
        self.auto_approve_restore(id);
    }

    /// The three gates a wire membership proposal must pass BEFORE anything
    /// is recorded (the ingest arm calls this first, the registration
    /// re-checks): a plausible id, the pending cap, and an id that names no
    /// different change.
    pub(crate) fn admits_membership_proposal(&self, id: u64, change: &ChainChange) -> bool {
        if !self.plausible_wire_id(id) {
            tracing::warn!(%id, "refusing a membership proposal with an implausible id");
            return false;
        }
        // L3: pending membership changes are bounded by what can ever be
        // open at once — one re-admission per seat plus slack for Joined
        // seats not on the roster yet
        let pending_membership = self
            .chain.proposal_changes
            .values()
            .filter(|c| matches!(c, ChainChange::Membership { .. }))
            .count();
        let cap = self
            .replica
            .as_ref()
            .map(|r| r.roster.len().saturating_add(8))
            .unwrap_or(16);
        if pending_membership >= cap && !self.chain.proposal_changes.contains_key(&id) {
            tracing::warn!(%id, "refusing a membership proposal beyond the pending cap");
            return false;
        }
        // SECURITY: the id is peer-chosen. `proposal_change` resolves an id
        // to `proposal_changes` first, so registering a Membership under an
        // id that already names a SURFACE proposal (or a different pending
        // change) would make honest members' later Approve of THAT proposal
        // sign these membership bytes instead — a threshold-gate bypass that
        // injects a roster member with no human ever approving a membership
        // change. Refuse any occupied id that is not this exact change.
        if !self.id_free_for(id, change) {
            tracing::warn!(%id, "refusing a membership proposal whose id names a different change");
            return false;
        }
        true
    }

    /// Auto-approve a `Membership{Restored}` proposal whose consent THIS node
    /// verified itself (recovery_auto_approval.md §3). The checkpoint
    /// precedent: a correctness attestation, not a product decision — the
    /// human decision was the survivor's mint of the recovery link, and the
    /// consent proves the seat's phrase holder asked for the re-admission.
    /// Everything is re-checked locally; nothing the proposing coordinator
    /// claims is trusted:
    ///
    /// - the change is a consented `Restored` for an ANCHORED seat keeping
    ///   its anchored identity key (the `apply_membership` invariant);
    /// - a claimed transport anchor is canonical and collides with no other
    ///   living seat (the ingest gate's twin — blocks never re-check this);
    /// - the consent verifies over [`molt_core::chain::restore_consent_bytes`]
    ///   against the ANCHORED key;
    /// - replay guard (the checkpoint pattern): at most one signature per
    ///   member per height, so a re-received frame never amplifies.
    ///
    /// A consent-less (legacy) restore and every `Joined` proposal keep the
    /// human card untouched.
    fn auto_approve_restore(&mut self, id: u64) {
        let Some(ChainChange::Membership {
            op: MembershipOp::Restored,
            member,
            identity_pk,
            nostr_pk,
            consent: Some(consent),
            ..
        }) = self.chain.proposal_changes.get(&id)
        else {
            return;
        };
        let (member, identity_pk, nostr_pk, consent) = (
            member.clone(),
            identity_pk.clone(),
            nostr_pk.clone(),
            consent.clone(),
        );
        // a settled record must not re-arm (a re-served proposal after the
        // block applied), and the restored seat itself never counts twice
        if matches!(self.proposals.get(&id), Some(p) if p.state != ProposalState::Proposed) {
            return;
        }
        let me = self.member();
        if me == member {
            return;
        }
        let Some(head) = self.chain.head.as_ref() else {
            return;
        };
        let Some(anchored) = head
            .identities
            .iter()
            .find(|i| i.member == member)
            .map(|i| i.identity_pk.clone())
        else {
            return;
        };
        if identity_pk != anchored {
            tracing::warn!(%id, %member, "restore proposal swaps the anchored identity - not auto-signing");
            return;
        }
        if let Some(npk) = &nostr_pk {
            if molt_net::canonical_nostr_pk(npk).ok().as_deref() != Some(npk.as_str()) {
                tracing::warn!(%id, %member, "restore proposal carries a non-canonical anchor - not auto-signing");
                return;
            }
            // the complete register (review C8): founding anchors, every
            // Restored block's anchor and the blob's working anchors — a
            // restore mints a FRESH anchor, any reuse is a forgery
            if self.anchor_seen_in_chain(npk) {
                tracing::warn!(%id, %member, "restore proposal reuses an anchor the chain knows - not auto-signing");
                return;
            }
        }
        let bytes = molt_core::chain::restore_consent_bytes(
            &self.republic_id(),
            &member,
            &anchored,
            nostr_pk.as_deref().unwrap_or(""),
        );
        if !molt_storage::identity_verify(&anchored, &bytes, &consent) {
            tracing::warn!(%id, %member, "restore consent does not verify - not auto-signing");
            return;
        }
        // replay guard: one signature per member per height
        if self.own_signature_stands(id) {
            return;
        }
        tracing::info!(%id, %member, "consented re-admission verified - auto-approving");
        self.chain_sign_and_gossip_approval(id);
    }

    /// A recovery coordinator's re-admit decision (recovery step ❸): verify a
    /// returning member's seat proof against its ANCHORED identity, then propose
    /// the threshold `Membership{Restored}` block. Recovery re-derives the same
    /// identity, so the requested key must equal the anchored one (it re-keys the
    /// MLS leaf, not the roster). Returns the proposal id, or the refusal reason.
    ///
    /// A verified request also registers the [`PendingRecovery`] (the fresh
    /// KeyPackage + `reply` handover the MLS re-key consumes) — and it must do
    /// so **before** proposing: with a lone coordinator (m=1, self-cosign) the
    /// `Restored` block commits *synchronously inside* `propose_membership`,
    /// and `after_block_applied` keys the re-key on this entry. Registering it
    /// afterwards would silently skip the re-key (the recovery E2E pins this).
    #[allow(clippy::too_many_arguments)] // one verified request's fields, not a bag
    pub(crate) fn verify_and_propose_restore(
        &mut self,
        ticketed: bool,
        member: &str,
        requested_pk: &str,
        key_package_hex: &str,
        ticket: &str,
        seat_proof: &str,
        new_nostr_pk: &str,
        declared_relays: &[String],
        consent: &str,
        reply: &str,
    ) -> Result<u64, String> {
        let anchored = self
            .replica
            .as_ref()
            .and_then(|r| r.identities.iter().find(|i| i.member == member))
            .map(|i| i.identity_pk.clone())
            .ok_or_else(|| format!("no anchored seat for {member}"))?;
        if requested_pk != anchored {
            return Err("recovery must re-derive the seat's own identity key".to_string());
        }
        let rid = self.republic_id();
        // the anchor AND the relay declaration are verified as the rejoiner
        // SIGNED them: tampering either on the wire makes the proof fail
        // rather than silently re-anchoring the seat or re-routing its ledger
        if !crate::founding::verify_seat_proof(
            &anchored,
            ticket,
            key_package_hex,
            &rid,
            new_nostr_pk,
            declared_relays,
            seat_proof,
        ) {
            return Err(format!("seat proof for {member} does not verify"));
        }
        // the rejoiner's consent — its automatic co-approval (recovery
        // approval design, 2026-08-08). Verified HERE, in the one validation
        // ladder, against the ANCHORED key over the exact content the
        // `Restored` change will carry; present-but-invalid is fail-closed
        // (a doctored consent must not ride a block m members then sign)
        if !consent.is_empty() {
            let bytes = molt_core::chain::restore_consent_bytes(
                &rid,
                member,
                &anchored,
                new_nostr_pk,
            );
            if !molt_storage::identity_verify(&anchored, &bytes, consent) {
                return Err(format!("restore consent for {member} does not verify"));
            }
        }
        // R5 — the re-join gate: a declaration that shares no relay with
        // some member would commit the very split R4 exists to detect. The
        // refusal names the relay the others must add — that message IS the
        // feature. Ordered AFTER the proof (only an authentic declaration
        // earns the named answer) and BEFORE the ticket is consumed upstream.
        if !declared_relays.is_empty() {
            for other in self.roster() {
                if other == member {
                    continue;
                }
                let theirs = self.member_relays(&other);
                if theirs.is_empty() || declared_relays.iter().any(|r| theirs.contains(r)) {
                    continue;
                }
                let named = declared_relays.first().cloned().unwrap_or_default();
                return Err(format!("{named} is in nobody else's pool - add it first"));
            }
        }
        self.recovery.pending.insert(
            member.to_string(),
            PendingRecovery {
                ticketed,
                key_package: key_package_hex.to_string(),
                reply: reply.to_string(),
            },
        );
        // the verified new transport anchor rides the block — this is what
        // makes it authoritative for every member that APPLIES it, rather
        // than something each node infers from live traffic
        let anchor = if new_nostr_pk.is_empty() {
            None
        } else {
            Some(new_nostr_pk.to_string())
        };
        // R3b/R5: the seat's OWN declaration when it made one; else the pool
        // it was welcomed over — on the loopback path there is no pool and
        // the declaration stays empty
        let relays = if !declared_relays.is_empty() {
            declared_relays.to_vec()
        } else if anchor.is_some() {
            self.ratified_relays()
        } else {
            Vec::new()
        };
        let consent = if consent.is_empty() {
            None
        } else {
            Some(consent.to_string())
        };
        Ok(self.propose_membership(MembershipOp::Restored, member, &anchored, anchor, relays, consent))
    }

    /// The live re-admission vote for a recovery THIS node coordinates
    /// (`recovery_auto_approval.md` §4): roster, counted voices (verified
    /// signatures ∪ the consenting seat) and the threshold, plus the
    /// rejoiner's new transport anchor as the report's address. `None`
    /// unless `id` is a `Restored` proposal whose member this node holds a
    /// [`PendingRecovery`] for — only the coordinator can reach the waiting
    /// rejoiner. Display data; the Welcome stays the only authority.
    pub(crate) fn recover_progress_for(&self, id: u64) -> Option<RecoverProgressReport> {
        let Some(ChainChange::Membership {
            op: MembershipOp::Restored,
            member,
            nostr_pk,
            consent,
            ..
        }) = self.chain.proposal_changes.get(&id)
        else {
            return None;
        };
        if !self.recovery.pending.contains_key(member) {
            return None;
        }
        let head = self.chain.head.as_ref()?;
        let roster: Vec<String> = head.identities.iter().map(|i| i.member.clone()).collect();
        let mut approved: BTreeSet<String> = self
            .chain.pending_sigs
            .get(&id)
            .map(|p| p.verified.iter().cloned().collect())
            .unwrap_or_default();
        if consent.is_some() {
            approved.insert(member.clone());
        }
        Some(RecoverProgressReport {
            member: member.clone(),
            need: u32::from(head.rule_m),
            roster,
            approved: approved.into_iter().collect(),
            to: nostr_pk.clone(),
        })
    }

    /// The COMPLETED checklist for a sealed `Restored` block this node
    /// coordinates: the block's own signers ∪ the consenting seat — the
    /// proof the threshold passed, reported once at the seal (the live
    /// per-approval frames stop there; see `after_block_applied`).
    pub(crate) fn recover_complete_report(&self, block: &ChainBlock) -> Option<RecoverProgressReport> {
        let ChainChange::Membership {
            op: MembershipOp::Restored,
            member,
            nostr_pk,
            consent,
            ..
        } = &block.change
        else {
            return None;
        };
        if !self.recovery.pending.contains_key(member) {
            return None;
        }
        let head = self.chain.head.as_ref()?;
        let roster: Vec<String> = head.identities.iter().map(|i| i.member.clone()).collect();
        let mut approved: BTreeSet<String> =
            block.sigs.iter().map(|a| a.member.clone()).collect();
        if consent.is_some() {
            approved.insert(member.clone());
        }
        Some(RecoverProgressReport {
            member: member.clone(),
            need: u32::from(head.rule_m),
            roster,
            approved: approved.into_iter().collect(),
            to: nostr_pk.clone(),
        })
    }

    /// Whether `pk` was EVER a seat's transport anchor in this republic —
    /// the genesis anchors, every `Restored` block's re-anchor, and (after
    /// a compaction cut) the checkpoint's summarized working anchors. The
    /// CHAIN is the replay register (`detached_reattach.md` §2.2a): a relay
    /// replaying a months-old accepted reattach request presents an anchor
    /// that is in here, so the request can never re-key the seat onto a
    /// dead incarnation again. (History pruned below a checkpoint forgets
    /// pre-cut anchors — bounded by the cut, and the cooldown covers the
    /// gap for live traffic.)
    pub(crate) fn anchor_seen_in_chain(&self, pk: &str) -> bool {
        if pk.is_empty() {
            return false;
        }
        if self
            .replica
            .as_ref()
            .is_some_and(|r| r.identities.iter().any(|i| i.nostr_pk == pk))
        {
            return true;
        }
        if self.chain.blocks.iter().any(|b| {
            matches!(&b.change, ChainChange::Membership {
                op: MembershipOp::Restored,
                nostr_pk: Some(a),
                ..
            } if a == pk)
        }) {
            return true;
        }
        self.chain.checkpoint_blob
            .as_ref()
            .is_some_and(|blob| blob.anchors.iter().any(|(_, a)| a == pk))
    }

    /// The coordinator's MLS re-key once a `Restored` block committed: run
    /// `restore_member` on the runtime group with the returning member's fresh
    /// KeyPackage → `(commit, welcome)`, then distribute both. The commit is
    /// broadcast to the survivors over the mesh (a recorded `MlsCommit`, sent raw
    /// so each survivor advances to the new epoch); the welcome goes to the
    /// returning member's reply queue. Finally the rejoin is announced in the
    /// group chat. Consumes the pending recovery. A node with no runtime group
    /// logs and does nothing.
    ///
    /// A **Nostr** republic takes the other arm entirely
    /// ([`State::coordinator_rekey_nostr`]): it has no `NetRuntime`, its group
    /// MLS lives on `GroupNet`, its commit rides a 445 at a pinned stamp and
    /// its Welcome is a gift wrap rather than a reply queue.
    pub(super) fn coordinator_rekey(&mut self, member: &str) {
        let Some(pending) = self.recovery.pending.remove(member) else {
            return;
        };
        let ticketed = pending.ticketed;
        let Ok(kp) = hex::decode(&pending.key_package) else {
            tracing::warn!(%member, "recovery KeyPackage is not valid hex");
            return;
        };
        if self.group_net.is_some() {
            self.coordinator_rekey_nostr(ticketed, member, &kp);
            return;
        }
        match self.net.as_ref().and_then(|n| n.restore_member_on_group(member, &kp)) {
            Some(Ok((commit, welcome))) => {
                let me = self.member();
                // 1) broadcast the raw re-key commit to the survivors: recorded as
                // an `MlsCommit`, the outbox fans it out; every survivor merges it
                // and advances to the new epoch (it MUST precede any new-epoch
                // traffic — hence recorded before the announcement below).
                let env =
                    self.make_env(me.clone(), WorkspaceEvent::MlsCommit { commit: hex::encode(&commit), stamp: 0 });
                self.record(env);
                // 2) deliver the welcome + the whole chain to the returning
                // member's reply queue so it rejoins the group AND catches its
                // state up over this same channel (option A). Off the actor.
                if let Some(transport) = self.net.as_ref().and_then(|n| n.runtime_transport()) {
                    let chain_json = match &self.chain.checkpoint_blob {
                        // a pruned coordinator serves blob + suffix — the
                        // rejoiner verifies via the suffix rules (4c)
                        Some(blob) => serde_json::to_string(&ServedChainWire::Pruned {
                            checkpoint_blob: blob.clone(),
                            blocks: self.chain.blocks.clone(),
                        }),
                        None => serde_json::to_string(&self.chain.blocks),
                    }
                    .unwrap_or_default();
                    crate::recovery::spawn_welcome_send(
                        transport,
                        pending.reply.clone(),
                        welcome,
                        chain_json,
                    );
                }
                // 3) announce the rejoin in the group chat — AFTER the commit, so
                // the survivors have advanced to the epoch this notice is
                // encrypted at (ephemeral, best-effort like all chat). A
                // System-kind message: every frontend renders it as a quiet
                // system line, not as the coordinator speaking.
                if let Err(e) = self.post_message_with_kind(
                    me,
                    rejoin_note(ticketed, member),
                    None,
                    molt_core::ChannelRef::Group,
                    molt_core::ChatKind::System,
                ) {
                    // best-effort, like all chat — never blocks the re-key
                    tracing::warn!(error = %e, "could not post the rejoin notice");
                }
                // 4) dynamic mesh membership: the rejoiner's mesh announce
                // follows on this same recovery queue — accept it for exactly
                // this member (docs_archive/transport/dynamic_mesh.md §3)
                self.recovery.mesh_window.insert(member.to_string());
                tracing::info!(%member, "re-keyed the group, broadcast the commit, sent the welcome");
            }
            Some(Err(e)) => tracing::warn!(%member, error = %e, "MLS re-key failed"),
            // this arm is reached in TWO different situations and must not
            // describe them as one: a demo/state-only node has no group at
            // all, while a Nostr workspace HAS one — on `GroupNet`, which
            // `restore_member_on_group` cannot see (it reads
            // `NetRuntime::real_crypto`). Saying "state-only" there sends a
            // debugger looking for a missing group instead of a missing arm.
            None => tracing::warn!(
                %member,
                group = "none",
                "no re-key path for this workspace - the returning seat gets no welcome"
            ),
        }
    }

    /// **The Nostr coordinator's re-key** (N4b step 6c).
    ///
    /// Everything the mesh arm does through a `NetRuntime` and a reply queue,
    /// done through `GroupNet` and the relays instead:
    ///
    /// 1. pin the carrier stamp BEFORE committing, and key the commit with it
    ///    (choosing it afterwards is too late — the commit is already made);
    /// 2. publish the commit as a 445 at exactly that stamp, sealed under the
    ///    epoch this node just left;
    /// 3. gift-wrap the 444 Welcome to the seat's **new** anchor —
    ///    `working_nostr_pk` already returns it, because `project_one` folds
    ///    the `Restored` block before `after_block_applied` runs;
    /// 4. offer the chain ANCHOR (the smallest prefix that verifies), not the
    ///    chain: a pruned holder's whole blob does not fit a gift wrap, and
    ///    pruned is the normal state.
    ///
    /// Steps 2 and 3 are off the actor; 1 and 4 are on it. Every failure is
    /// named — a recovery that quietly does nothing leaves a member locked out
    /// with no way to find out why.
    fn coordinator_rekey_nostr(&mut self, ticketed: bool, member: &str, key_package: &[u8]) {
        // **Everything that can fail is resolved BEFORE the group is touched.**
        //
        // `nostr_rekey` advances the epoch and evicts the old leaf, and there
        // is no undo. A re-key whose delivery then turns out to be impossible
        // leaves this node alone on an epoch no survivor knows about, unable
        // to be read by anyone until some later commit rescues it — the same
        // split the commit-before-welcome rule exists to prevent, reached
        // through an earlier door.
        let relays = self.dialable_group_relays();
        if relays.is_empty() {
            tracing::error!(%member, "no dialable relay for this republic - the re-key cannot be delivered");
            return;
        }
        let Ok(dialer) = self.dialer_for() else {
            tracing::error!(%member, "no usable dial route - the re-key cannot be delivered");
            return;
        };
        // the transport material, copied out so the group borrow below is free
        let Some(nostr) = self.nostr.as_ref() else {
            return;
        };
        let rotation_seed = nostr.rotation_seed;
        // the payload carries what the GROUP ratified, not this node's own
        // intersection: the rejoiner gates that list through its own pool,
        // and handing it a narrowed one would silently shrink the republic
        let ratified: Vec<String> = nostr
            .relays
            .iter()
            .take(molt_net::welcome::MAX_PAYLOAD_RELAYS)
            .cloned()
            .collect();
        let net = match molt_net::ritual_net::RitualNet::new(
            dialer.clone(),
            relays.clone(),
            &nostr.sk,
        ) {
            Ok(n) => n,
            Err(e) => {
                tracing::error!(%member, error = %e, "recovery transport keys - the re-key cannot be delivered");
                return;
            }
        };
        let channel = molt_net::ritual_net::GroupChannel::new(dialer, relays, rotation_seed);
        // the NEW anchor: the `Restored` block that triggered this re-key has
        // already been folded, so this is the key the seat just proved it holds
        let to = self.working_nostr_pk(member);
        if to.is_empty() {
            tracing::error!(%member, "the restored seat carries no transport anchor - nothing to address the welcome to");
            return;
        }

        // --- past here the group really changes -------------------------------
        // the stamp is chosen before anything is committed and travels
        // unchanged into both `restore_member` and `publish_frame_at`
        let stamp = molt_storage::now_secs();
        let Some(group) = self.group_net.as_ref() else {
            return;
        };
        let rekey = match nostr_rekey(&group.mls, member, key_package, stamp) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(%member, error = %e, "the Nostr re-key failed - the returning seat gets no welcome");
                return;
            }
        };
        let payload = molt_net::welcome::WelcomePayload {
            welcome: rekey.welcome.clone(),
            rotation_seed,
            relays: ratified,
        };
        // the commit goes into the LOG too (review M8): the delivery task
        // publishes it a few times now, the outbox re-offers it from here
        // for every laggard the guarantee later finds — at the same stamp,
        // so the resend keys like the original (`detached_reattach.md` §7)
        let commit_env = self.make_env(
            self.member(),
            WorkspaceEvent::MlsCommit {
                commit: hex::encode(&rekey.commit),
                stamp: rekey.stamp,
            },
        );
        crate::nostr_ritual::spawn_rekey_delivery(
            channel,
            net,
            to,
            rekey,
            payload,
            member.to_string(),
        );
        self.record(commit_env);
        // **Forget the seat's OLD accept window.** The returning member is a
        // new incarnation whose log seq space restarts at 1, so the marks from
        // the lost device swallow every fresh envelope as a duplicate — its
        // chat, and the `ChainRequest` that pulls everything above the anchor.
        //
        // The mesh does this at its authenticated recovery-announce
        // (`cmd_net_recover_announced`), which a Nostr republic has no
        // equivalent of. It does have a stronger one: this re-key runs only
        // behind a threshold-committed `Restored` block for exactly this seat.
        self.reset_peer_accept_window(&member.to_string());
        // the rejoiner's trust root, over the same 445 channel it just joined
        self.serve_chain_anchor();
        // …and the same quiet system line the mesh arm posts. It is encrypted
        // at the NEW epoch, so it can outrun the commit that is still being
        // published — a survivor holds it and retries after the merge
        // (N5.3c), which is exactly what that hold exists for.
        let me = self.member();
        if let Err(e) = self.post_message_with_kind(
            me,
            rejoin_note(ticketed, member),
            None,
            molt_core::ChannelRef::Group,
            molt_core::ChatKind::System,
        ) {
            tracing::warn!(error = %e, "could not post the rejoin notice");
        }
        tracing::info!(%member, stamp, "re-keyed the group on Nostr and offered the chain anchor");
    }
}

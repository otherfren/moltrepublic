// SPDX-License-Identifier: GPL-3.0-or-later

//! **Verification of the persistent commit-block chain** (see
//! [`molt_core::chain`]). This is the security layer that makes a handed-over
//! chain *self-authenticating*: a member — or a rejoiner who fetched the chain
//! from an untrusted peer — checks it here with no live mesh and no trust in the
//! deliverer.
//!
//! The split mirrors the founding roster exactly: the canonical bytes live in
//! `molt-core`, the SHA-256 hash in `molt-storage`, and the Ed25519 checks here
//! next to [`crate::founding::verify_sealed_roster`] — of which the genesis
//! check below is a strict generalization (same `republic_id` content-match,
//! same per-member `identity_verify` over [`molt_core::roster_canonical_bytes`]).
//!
//! **Every check is hard-reject.** A bad signature, a broken `prev` link, a gap
//! in the heights, a genesis whose id does not match its content, a double-apply
//! — any of these fails the whole verification. Determinism across every
//! member's converged state demands nothing weaker.

use std::collections::BTreeSet;

use molt_core::{
    approval_bytes, block_link_bytes, ChainBlock, ChainChange, MemberIdentity, MembershipOp,
    RosterAttestation, SealedRoster, Surface, GENESIS_PREV,
};

use crate::State;

/// The verified head of a chain plus the roster it establishes: everything a
/// caller needs to check the *next* block or to trust a synced chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainHead {
    /// Height of the last verified block.
    pub height: u64,
    /// [`block_hash`] of the last verified block — the `prev` the next block
    /// must carry.
    pub hash: String,
    /// The content-derived republic id, fixed by the genesis.
    pub republic_id: String,
    /// Approval threshold (m) for every post-genesis block.
    pub rule_m: u8,
    /// The live roster (name → identity key) after all membership blocks.
    pub identities: Vec<MemberIdentity>,
}

/// SHA-256 (hex) of a block's [`block_link_bytes`] — the value the next block's
/// `prev` points at.
pub(crate) fn block_hash(republic_id: &str, block: &ChainBlock) -> String {
    molt_storage::content_hash(&block_link_bytes(republic_id, block))
}

/// The distinct roster members who validly signed `bytes`. A signature from an
/// unknown member, a bad signature, or the same member signing twice can never
/// inflate the count past the number of real, distinct approvers.
fn valid_signers(
    identities: &[MemberIdentity],
    bytes: &[u8],
    sigs: &[RosterAttestation],
) -> BTreeSet<String> {
    let mut ok = BTreeSet::new();
    for att in sigs {
        let Some(id) = identities.iter().find(|i| i.member == att.member) else {
            continue;
        };
        if molt_storage::identity_verify(&id.identity_pk, bytes, &att.sig) {
            ok.insert(att.member.clone());
        }
    }
    ok
}

/// Evolve the roster for the blocks that *follow* a membership change.
fn apply_membership(
    identities: &mut Vec<MemberIdentity>,
    op: MembershipOp,
    member: &str,
    identity_pk: &str,
) -> Result<(), String> {
    match op {
        MembershipOp::Joined => {
            if identities.iter().any(|i| i.member == member) {
                return Err(format!("member {member} is already in the roster"));
            }
            identities.push(MemberIdentity {
                member: member.to_string(),
                identity_pk: identity_pk.to_string(),
            });
        }
        MembershipOp::Restored => {
            let Some(id) = identities.iter_mut().find(|i| i.member == member) else {
                return Err(format!("cannot restore unknown member {member}"));
            };
            id.identity_pk = identity_pk.to_string();
        }
    }
    Ok(())
}

/// Verify block 0: it must be a genesis whose `republic_id` is the neutral
/// content-derived value, sealed **unanimously** (n-of-n) by every anchored
/// member — the founding attestations. Returns the initial head.
fn verify_genesis(block: &ChainBlock) -> Result<ChainHead, String> {
    if block.height != 0 {
        return Err("genesis is not at height 0".to_string());
    }
    if block.prev != GENESIS_PREV {
        return Err("genesis prev is not the zero root".to_string());
    }
    let ChainChange::Genesis {
        name,
        republic_id,
        rule_m,
        rule_n,
        identities,
        agenda: _,
    } = &block.change
    else {
        return Err("block 0 is not a genesis".to_string());
    };
    if *rule_m == 0 || rule_m > rule_n {
        return Err("genesis threshold is out of range".to_string());
    }
    if usize::from(*rule_n) != identities.len() {
        return Err("genesis roster size does not match n".to_string());
    }
    let rid = molt_storage::republic_id(name, *rule_m, *rule_n, identities);
    if &rid != republic_id {
        return Err("genesis republic id does not match its content".to_string());
    }
    let bytes = approval_bytes(republic_id, 0, &block.change);
    let signers = valid_signers(identities, &bytes, &block.sigs);
    if signers.len() != identities.len() {
        return Err("genesis is not fully signed by every member".to_string());
    }
    Ok(ChainHead {
        height: 0,
        hash: block_hash(republic_id, block),
        republic_id: republic_id.clone(),
        rule_m: *rule_m,
        identities: identities.clone(),
    })
}

/// Verify one post-genesis block against the current head, returning the
/// advanced head. `seen_proposals` accumulates applied proposal ids across the
/// chain so a proposal cannot be committed twice.
fn verify_next(
    head: &ChainHead,
    block: &ChainBlock,
    seen_proposals: &mut BTreeSet<u64>,
) -> Result<ChainHead, String> {
    if block.height != head.height + 1 {
        return Err(format!(
            "block height {} does not follow {}",
            block.height, head.height
        ));
    }
    if block.prev != head.hash {
        return Err(format!("block {} does not link to its predecessor", block.height));
    }
    match &block.change {
        ChainChange::Genesis { .. } => {
            return Err("a genesis cannot appear after height 0".to_string());
        }
        ChainChange::Applied { proposal_id, .. } => {
            if !seen_proposals.insert(*proposal_id) {
                return Err(format!("proposal {proposal_id} is applied twice"));
            }
        }
        ChainChange::Membership { .. } => {}
    }
    let bytes = approval_bytes(&head.republic_id, block.height, &block.change);
    let signers = valid_signers(&head.identities, &bytes, &block.sigs);
    if signers.len() < usize::from(head.rule_m) {
        return Err(format!(
            "block {} has {} valid approvals, threshold is {}",
            block.height,
            signers.len(),
            head.rule_m
        ));
    }
    let mut identities = head.identities.clone();
    if let ChainChange::Membership {
        op,
        member,
        identity_pk,
    } = &block.change
    {
        apply_membership(&mut identities, *op, member, identity_pk)?;
    }
    Ok(ChainHead {
        height: block.height,
        hash: block_hash(&head.republic_id, block),
        republic_id: head.republic_id.clone(),
        rule_m: head.rule_m,
        identities,
    })
}

/// Verify a whole chain from its genesis and return its head. Any failure is
/// hard: the chain is rejected in full (a partially-valid chain is not a thing
/// — a rejoiner that trusted a prefix could fork the republic's state).
pub fn verify_chain(blocks: &[ChainBlock]) -> Result<ChainHead, String> {
    let Some((genesis, rest)) = blocks.split_first() else {
        return Err("empty chain".to_string());
    };
    let mut head = verify_genesis(genesis)?;
    let mut seen = BTreeSet::new();
    for block in rest {
        head = verify_next(&head, block, &mut seen)?;
    }
    Ok(head)
}

impl State {
    /// Build block 0 of the persistent chain from a sealed roster — but only
    /// for a **real** founding (a content-derived republic id and one
    /// attestation per member). A pre-ritual/demo materialize gets no chain
    /// (empty), so the running single-operator simulation is untouched.
    pub(crate) fn genesis_chain(&self, sealed: &SealedRoster) -> Vec<ChainBlock> {
        if sealed.republic_id.is_empty()
            || sealed.identities.is_empty()
            || sealed.attestations.len() != sealed.identities.len()
        {
            return Vec::new();
        }
        vec![ChainBlock {
            height: 0,
            prev: GENESIS_PREV.to_string(),
            change: ChainChange::Genesis {
                name: sealed.name.clone(),
                republic_id: sealed.republic_id.clone(),
                rule_m: sealed.rule_m,
                rule_n: sealed.rule_n,
                identities: sealed.identities.clone(),
                agenda: sealed.agenda.clone(),
            },
            sigs: sealed.attestations.clone(),
        }]
    }

    /// Verify a freshly-loaded or freshly-built chain and adopt it as the open
    /// workspace's chain + head, then re-project the persistent state from it.
    /// A chain that fails verification is **hard-rejected**: the head stays
    /// `None` and nothing is projected (a partially-trusted chain could fork
    /// state — `documents/persistent_chain.md`).
    pub(crate) fn adopt_chain(&mut self, chain: Vec<ChainBlock>) {
        match verify_chain(&chain) {
            Ok(head) => {
                self.chain = chain;
                self.chain_head = Some(head);
                self.apply_chain_to_state();
            }
            Err(e) => {
                tracing::warn!(error = %e, "rejecting an unverifiable chain");
                self.chain.clear();
                self.chain_head = None;
            }
        }
    }

    /// Re-project the persistent state from the whole chain: the gated
    /// surfaces' applied logs (into the chain-owned [`State::chain_applied`], a
    /// full clear-and-refold so a re-base is free) and the roster/identities
    /// (taken from the already-verified head, which evolved them across the
    /// membership blocks). Chat, [`State::applied`] and pending proposals are
    /// left untouched — they are ephemeral or legacy-owned.
    pub(crate) fn apply_chain_to_state(&mut self) {
        let mut projected: std::collections::HashMap<Surface, Vec<serde_json::Value>> =
            std::collections::HashMap::new();
        for block in &self.chain {
            if let ChainChange::Applied {
                surface, payload, ..
            } = &block.change
            {
                projected.entry(*surface).or_default().push(payload.clone());
            }
        }
        self.chain_applied = projected;
        // the verified head carries the roster after every membership block —
        // adopt it so the newcomers/rekeys show up in the roster + approvals
        if let Some(head) = &self.chain_head {
            if let Some(r) = &mut self.replica {
                r.identities = head.identities.clone();
                r.roster = head.identities.iter().map(|i| i.member.clone()).collect();
            }
        }
    }

    /// Surface a chain workspace that opened without its local signing key: it
    /// can still verify and follow the chain, but cannot itself co-sign
    /// governance approvals (a reopen that lost `transport.state`'s
    /// `identity_sk`, or a pre-chain workspace). Cheap invariant check, logged.
    pub(crate) fn note_governance_readiness(&self) {
        if self.chain_head.is_some() && self.identity_sk.is_none() {
            tracing::warn!(
                republic = %self.republic_id(),
                "chain workspace has no local signing key — it can follow governance but not co-sign it"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use molt_core::{ChainChange, MembershipOp, Surface};
    use molt_storage::{derive_identity_key, identity_sign, SigningKey};
    use serde_json::json;

    /// A minimal chain builder: derives each member's identity key from a seed,
    /// seals the genesis with everyone (n-of-n) and appends later blocks signed
    /// by a chosen subset — exactly what the real founding + threshold path
    /// will produce.
    struct Builder {
        republic_id: String,
        keys: Vec<(String, SigningKey)>,
        blocks: Vec<ChainBlock>,
        head_hash: String,
    }

    impl Builder {
        fn new(members: &[&str], rule_m: u8) -> Builder {
            let mut keys: Vec<(String, SigningKey)> = Vec::new();
            let mut identities: Vec<MemberIdentity> = Vec::new();
            for (i, m) in members.iter().enumerate() {
                let seed = [u8::try_from(i + 1).unwrap_or(1); 32];
                let (sk, pk) = derive_identity_key(&seed, m);
                identities.push(MemberIdentity {
                    member: (*m).to_string(),
                    identity_pk: pk,
                });
                keys.push(((*m).to_string(), sk));
            }
            let rule_n = u8::try_from(members.len()).expect("small roster");
            let republic_id = molt_storage::republic_id("Chess Club", rule_m, rule_n, &identities);
            let change = ChainChange::Genesis {
                name: "Chess Club".to_string(),
                republic_id: republic_id.clone(),
                rule_m,
                rule_n,
                identities: identities.clone(),
                agenda: "play chess".to_string(),
            };
            let mut b = Builder {
                republic_id: republic_id.clone(),
                keys,
                blocks: Vec::new(),
                head_hash: GENESIS_PREV.to_string(),
            };
            // genesis is unanimous
            let all: Vec<&str> = members.to_vec();
            let block = b.seal(0, change, &all);
            b.push(block);
            b
        }

        /// Sign `change` at `height` with each named member and return the block.
        fn seal(&self, height: u64, change: ChainChange, signers: &[&str]) -> ChainBlock {
            let bytes = approval_bytes(&self.republic_id, height, &change);
            let sigs = signers
                .iter()
                .map(|name| {
                    let (_, sk) = self
                        .keys
                        .iter()
                        .find(|(m, _)| m == name)
                        .expect("known signer");
                    RosterAttestation {
                        member: (*name).to_string(),
                        sig: identity_sign(sk, &bytes),
                    }
                })
                .collect();
            ChainBlock {
                height,
                prev: self.head_hash.clone(),
                change,
                sigs,
            }
        }

        fn push(&mut self, block: ChainBlock) {
            self.head_hash = block_hash(&self.republic_id, &block);
            self.blocks.push(block);
        }

        /// Commit a gated Applied change signed by `signers` at the next height.
        fn commit_applied(&mut self, proposal_id: u64, signers: &[&str]) {
            let height = u64::try_from(self.blocks.len()).expect("small chain");
            let change = ChainChange::Applied {
                proposal_id,
                surface: Surface::Memory,
                payload: json!({ "op": "add_note", "id": proposal_id }),
            };
            let block = self.seal(height, change, signers);
            self.push(block);
        }
    }

    #[test]
    fn genesis_then_applied_verifies() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        let head = verify_chain(&b.blocks).expect("valid chain verifies");
        assert_eq!(head.height, 1);
        assert_eq!(head.rule_m, 2);
        assert_eq!(head.identities.len(), 3);
    }

    #[test]
    fn a_tampered_payload_is_rejected() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        // rewrite the applied payload without re-signing
        if let ChainChange::Applied { payload, .. } = &mut b.blocks[1].change {
            *payload = json!({ "op": "add_note", "id": 999 });
        }
        assert!(verify_chain(&b.blocks).is_err(), "signatures cover the payload");
    }

    #[test]
    fn a_broken_prev_link_is_rejected() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        b.blocks[1].prev = GENESIS_PREV.to_string();
        assert!(verify_chain(&b.blocks).is_err(), "the chain link is broken");
    }

    #[test]
    fn below_threshold_approvals_are_rejected() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_applied(1, &["petra"]); // only 1 of the required 2
        assert!(verify_chain(&b.blocks).is_err(), "one approval is below m=2");
    }

    #[test]
    fn a_repeated_signature_does_not_reach_threshold() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        // petra signs, then her attestation is duplicated — still one signer
        b.commit_applied(1, &["petra"]);
        let dup = b.blocks[1].sigs[0].clone();
        b.blocks[1].sigs.push(dup);
        assert!(
            verify_chain(&b.blocks).is_err(),
            "one member signing twice is still one approver"
        );
    }

    #[test]
    fn applying_a_proposal_twice_is_rejected() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(7, &["petra", "walter"]);
        b.commit_applied(7, &["petra", "walter"]); // same proposal id again
        assert!(verify_chain(&b.blocks).is_err(), "no double-apply");
    }

    #[test]
    fn a_height_gap_is_rejected() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        b.blocks[1].height = 5; // signatures are height-bound, so this also fails the sig check
        assert!(verify_chain(&b.blocks).is_err(), "heights must be gapless");
    }

    #[test]
    fn a_forged_genesis_id_is_rejected() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        if let ChainChange::Genesis { republic_id, .. } = &mut b.blocks[0].change {
            *republic_id = "deadbeef".to_string();
        }
        assert!(
            verify_chain(&b.blocks).is_err(),
            "the republic id must match the roster content"
        );
    }

    #[test]
    fn a_membership_block_grows_the_roster_and_lets_the_newcomer_approve() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        // add dora with her own derived identity key
        let (dora_sk, dora_pk) = derive_identity_key(&[9u8; 32], "dora");
        let height = u64::try_from(b.blocks.len()).expect("small chain");
        let join = ChainChange::Membership {
            op: MembershipOp::Joined,
            member: "dora".to_string(),
            identity_pk: dora_pk,
        };
        let block = b.seal(height, join, &["petra", "walter"]);
        b.push(block);
        b.keys.push(("dora".to_string(), dora_sk));
        // now an Applied block signed by dora + walter must count dora
        b.commit_applied(1, &["dora", "walter"]);
        let head = verify_chain(&b.blocks).expect("newcomer approval counts");
        assert_eq!(head.identities.len(), 3);
        assert_eq!(head.height, 2);
    }
}

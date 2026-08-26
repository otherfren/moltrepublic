// SPDX-License-Identifier: GPL-3.0-or-later

//! **The persistent commit-block chain on the engine side** — the
//! threshold-signed single-branch state model of
//! `docs_archive/chain/persistent_chain.md`, split by responsibility:
//!
//! - [`verify`] — pure verification, no `State`: the genesis/next-block
//!   checks, the cached [`ChainWalk`], the checkpoint fold and the served /
//!   suffix / wiki-export verifiers;
//! - the holder's projection of a verified chain into `State`, the live
//!   threshold governance, membership (recovery re-admission + re-key),
//!   checkpoints (compaction) and catch-up sync live in the sibling
//!   modules below, each as `impl State` blocks.
//!
//! Everything a caller outside this module needs is re-exported here under
//! `crate::chain::…`; the sibling modules share ONE namespace through
//! `use super::*`, so an item's file is a matter of reading order, never of
//! reachability.

use std::collections::BTreeSet;

use molt_core::{
    approval_bytes, block_link_bytes, ChainBlock, ChainChange, Event, MemberIdentity, MembershipOp,
    ProposalId, ProposalState, RosterAttestation, SealedRoster, Surface, WorkspaceEvent,
    GENESIS_PREV,
};

use crate::State;

mod checkpoint;
mod governance;
mod membership;
mod projection;
mod sync;
mod verify;

pub use verify::{verify_chain, verify_wiki_export, ChainHead, WikiExportReport};
pub(crate) use verify::{
    checkpoint_state, checkpoint_state_hash, effective_relays_of_served, verify_served,
    verify_suffix_chain, working_anchors, ChainWalk, ServedChainWire,
};
#[cfg(test)]
pub(crate) use verify::block_hash;
pub(crate) use governance::PendingApproval;
pub(crate) use membership::{NostrRekey, PendingRecovery, RecoverProgressReport};

use verify::*;

#[cfg(test)]
thread_local! {
    /// Test-only count of per-block verifications. **Thread-local on purpose**:
    /// each test runs on its own thread, so a shared counter would be raced by
    /// every other chain test in the binary.
    ///
    /// This is the only way to state the complexity claim as an assertion
    /// rather than a timing: a holder that re-walks its chain per block shows
    /// up here as `N²`, not `N`.
    pub(crate) static VERIFY_STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };

    /// Test-only count of whole-chain writes. Same reason as `VERIFY_STEPS`:
    /// the write is a BLOCKING round-trip to the storage writer, so "once per
    /// batch" is a claim worth asserting rather than describing.
    pub(crate) static CHAIN_PERSISTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::checkpoint::AUTO_CHECKPOINT_MIN_LEN;
    use super::governance::OPEN_CARDS_PER_PROPOSER_MAX;
    use super::membership::nostr_rekey;
    use super::*;
    use molt_core::{ChainChange, MembershipOp, Surface};
    use molt_storage::{derive_identity_key, identity_sign, SigningKey};
    use serde_json::json;

    /// A minimal chain builder: derives each member's identity key from a seed,
    /// seals the genesis with everyone (n-of-n) and appends later blocks signed
    /// by a chosen subset — exactly what the real founding + threshold path
    /// will produce.
    #[derive(Clone)]
    struct Builder {
        republic_id: String,
        keys: Vec<(String, SigningKey)>,
        blocks: Vec<ChainBlock>,
        head_hash: String,
    }

    impl Builder {
        fn new(members: &[&str], rule_m: u8) -> Builder {
            Builder::new_on_relays(members, rule_m, Vec::new())
        }

        /// A founding whose genesis ratifies `relays` (R3b ledger tests).
        fn new_on_relays(members: &[&str], rule_m: u8, relays: Vec<String>) -> Builder {
            let mut keys: Vec<(String, SigningKey)> = Vec::new();
            let mut identities: Vec<MemberIdentity> = Vec::new();
            for (i, m) in members.iter().enumerate() {
                let seed = [u8::try_from(i + 1).unwrap_or(1); 32];
                let (sk, pk) = derive_identity_key(&seed, m);
                identities.push(MemberIdentity {
                    member: (*m).to_string(),
                    identity_pk: pk,
                    nostr_pk: "cc".repeat(32),
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
                features: None,
                relays,
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

        /// Commit an Organization edit — the surface whose ops occupy
        /// last-write-wins slots (§B.6a), so a checkpoint summarizes them.
        fn commit_org(&mut self, proposal_id: u64, op: &str, value: &str, signers: &[&str]) {
            let height = u64::try_from(self.blocks.len()).expect("small chain");
            let change = ChainChange::Applied {
                proposal_id,
                surface: Surface::Organization,
                payload: json!({ "op": op, "value": value }),
            };
            let block = self.seal(height, change, signers);
            self.push(block);
        }

        /// Commit a `Restored` membership block — the seat keeps its anchored
        /// identity key and re-anchors its transport key.
        fn commit_restored(&mut self, member: &str, nostr_pk: &str, signers: &[&str]) {
            let height = u64::try_from(self.blocks.len()).expect("small chain");
            let change = ChainChange::Membership {
                op: MembershipOp::Restored,
                member: member.to_string(),
                identity_pk: self.pk(member),
                nostr_pk: Some(nostr_pk.to_string()),
                relays: Vec::new(),
                consent: None,
            };
            let block = self.seal(height, change, signers);
            self.push(block);
        }

        /// A member's signing key.
        fn key(&self, member: &str) -> &SigningKey {
            &self
                .keys
                .iter()
                .find(|(m, _)| m == member)
                .expect("known member")
                .1
        }

        /// A member's anchored identity pk (from the genesis roster).
        fn pk(&self, member: &str) -> String {
            let ChainChange::Genesis { identities, .. } = &self.blocks[0].change else {
                panic!("block 0 is not a genesis");
            };
            identities
                .iter()
                .find(|i| i.member == member)
                .expect("anchored member")
                .identity_pk
                .clone()
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
    /// Seats are fixed at founding (product decision 2026-07-11): a
    /// `Joined` block is refused WHOLE, like any unknown change — a joined
    /// seat is not in the founding table and the first checkpoint after it
    /// stranded every pruned holder (review C7).
    fn a_joined_block_is_refused_whole() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        let (_dora_sk, dora_pk) = derive_identity_key(&[9u8; 32], "dora");
        let height = u64::try_from(b.blocks.len()).expect("small chain");
        let join = ChainChange::Membership {
            op: MembershipOp::Joined,
            member: "dora".to_string(),
            identity_pk: dora_pk,
            nostr_pk: None,
            relays: Vec::new(),
            consent: None,
        };
        let block = b.seal(height, join, &["petra", "walter"]);
        b.push(block);
        let err = verify_chain(&b.blocks).expect_err("a joined block does not verify");
        assert!(err.contains("not supported"), "{err}");
    }

    /// C3: one requester is served a catch-up at most once per debounce,
    /// and never for a height above the head.
    #[test]
    fn a_catch_up_request_is_served_once_per_debounce() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_applied(1, &["petra", "dora"]);
        let mut walter = chain_peer("walter", &b, b.blocks.clone());
        walter.clock_override = Some(1_000);
        let before = walter.next_seq;
        wire(&mut walter, "petra", 1, WorkspaceEvent::ChainRequest { from_height: 0 });
        let served = walter.next_seq;
        assert!(served > before, "the first request is served");
        wire(&mut walter, "petra", 2, WorkspaceEvent::ChainRequest { from_height: 0 });
        assert_eq!(walter.next_seq, served, "a repeat inside the debounce serves nothing");
        wire(&mut walter, "petra", 3, WorkspaceEvent::ChainRequest { from_height: 99 });
        assert_eq!(walter.next_seq, served, "nothing above the head is served");
        walter.clock_override = Some(1_000 + crate::net::CHAIN_SERVE_DEBOUNCE_SECS);
        wire(&mut walter, "petra", 4, WorkspaceEvent::ChainRequest { from_height: 0 });
        assert!(walter.next_seq > served, "after the debounce it is served again");
    }

    /// C6: a headless node adopts only ITS republic's genesis — a valid
    /// genesis is trivially forgeable.
    #[test]
    fn a_headless_node_refuses_another_republics_genesis() {
        let b = Builder::new(&["petra", "walter"], 2);
        let other = Builder::new(&["mallory", "walter"], 2);
        let mut walter = chain_peer("walter", &b, b.blocks.clone());
        walter.chain.clear();
        walter.chain_head = None;
        walter.chain_walk = None;
        walter.receive_block(other.blocks[0].clone());
        assert!(walter.chain_head.is_none(), "a foreign genesis is not adopted");
        walter.receive_block(b.blocks[0].clone());
        assert!(walter.chain_head.is_some(), "the own genesis is");
    }

    /// E1 residual: the mint counter clears every id the chain consumed.
    #[test]
    fn the_max_applied_proposal_id_reads_the_chain() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        assert_eq!(crate::State::max_applied_proposal_id(&b.blocks), None);
        b.commit_applied(7, &["petra", "walter"]);
        b.commit_applied(3, &["petra", "walter"]);
        assert_eq!(crate::State::max_applied_proposal_id(&b.blocks), Some(7));
    }

    /// A member that only holds the genesis receives a peer's broadcast commit
    /// block, verifies + adopts it, and its persistent state converges (the
    /// `receive_block` path that a non-committer follows).
    /// WP1: the chain projection feeds the snapshot's parallel id track —
    /// a committed `Applied` block's `proposal_id` reaches the read contract
    /// positionally next to its payload.
    /// N4b step 3 — the WORKING transport anchor is a chain projection.
    ///
    /// A recovered seat's key changes; the roster's genesis anchor does not
    /// (it is the immutable founding record). Every gift-wrap send must
    /// resolve through the projection, because a sender reaching for the
    /// obvious `identities[i].nostr_pk` would address a key the recovered
    /// member no longer holds — SILENTLY, which is exactly why the plan
    /// rejected "infer the anchor from live traffic".
    #[test]
    fn the_working_anchor_follows_a_restored_block_while_the_roster_does_not() {
        let b = Builder::new(&["petra", "walter"], 2);
        let mut st = crate::tests::plain_state();
        st.replica = Some(crate::ReplicaState {
            name: "Chess Club".to_string(),
            member: "walter".to_string(),
            roster: vec!["petra".to_string(), "walter".to_string()],
            rule_m: 2,
            identities: Vec::new(),
            agenda: "play chess".to_string(),
            features: None,
            republic_id: b.republic_id.clone(),
            founded_ts: 0,
        });
        st.adopt_chain(b.blocks.clone());
        let founding = st
            .replica
            .as_ref()
            .and_then(|r| r.identities.iter().find(|i| i.member == "petra"))
            .map(|i| i.nostr_pk.clone())
            .expect("petra is anchored");
        assert_eq!(
            st.working_nostr_pk("petra"),
            founding,
            "before any recovery the projection IS the roster anchor"
        );

        // petra recovers with a FRESH transport key
        let fresh = molt_net::nostr_identity(b"petra-recovered", "new-ticket").1;
        let restored = b.seal(
            1,
            ChainChange::Membership {
                op: MembershipOp::Restored,
                member: "petra".to_string(),
                identity_pk: b.pk("petra"),
                nostr_pk: Some(fresh.clone()),
                relays: Vec::new(),
                consent: None,
            },
            &["petra", "walter"],
        );
        st.adopt_chain({
            let mut c = b.blocks.clone();
            c.push(restored);
            c
        });

        assert_eq!(
            st.working_nostr_pk("petra"),
            fresh,
            "after a Restored block the projection returns the NEW key"
        );
        assert_eq!(
            st.replica
                .as_ref()
                .and_then(|r| r.identities.iter().find(|i| i.member == "petra"))
                .map(|i| i.nostr_pk.clone())
                .expect("still anchored"),
            founding,
            "…while the roster keeps the immutable FOUNDING anchor"
        );
        // an unknown member resolves to nothing, never to somebody else's key
        assert_eq!(st.working_nostr_pk("nobody"), "");
    }

    /// **…and it survives the compaction that drops the block it came from.**
    ///
    /// The `Restored` block is what re-anchors a seat, and a cut drops it.
    /// The roster in the blob keeps the FOUNDING anchor by design
    /// (`apply_membership` refuses to move a seat's identity key), so without
    /// the summary carrying the working anchors explicitly, a compaction
    /// makes every recovered member addressable ONLY at the key it no longer
    /// holds — silently, which is the exact failure `State::chain_anchors`
    /// documents itself as existing to prevent.
    ///
    /// Reachable in the ordinary course: `AUTO_CHECKPOINT_MIN_LEN` is 32.
    #[test]
    fn a_compaction_keeps_the_working_anchor_of_a_recovered_seat() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        let fresh = molt_net::nostr_identity(b"petra-recovered", "new-ticket").1;
        b.commit_restored("petra", &fresh, &["petra", "walter"]);

        // cut ABOVE the Restored block, so the block itself is dropped
        let blob = checkpoint_state(&b.blocks, 1).expect("state@1");
        assert_eq!(
            blob.anchors,
            vec![("petra".to_string(), fresh.clone())],
            "the summary must carry the anchors the dropped blocks established"
        );
        let cut = b.seal(
            2,
            ChainChange::Checkpoint {
                upto: 1,
                state_hash: checkpoint_state_hash(&blob),
            },
            &["petra", "walter"],
        );
        b.push(cut);

        // the pruned holder: blob + the suffix from the anchor block on
        let mut st = crate::tests::plain_state();
        st.replica = Some(crate::ReplicaState {
            name: "Chess Club".to_string(),
            member: "walter".to_string(),
            roster: vec!["petra".to_string(), "walter".to_string()],
            rule_m: 2,
            identities: Vec::new(),
            agenda: "play chess".to_string(),
            features: None,
            republic_id: b.republic_id.clone(),
            founded_ts: 0,
        });
        st.set_checkpoint_blob(Some(blob));
        st.adopt_chain(b.blocks[2..].to_vec());

        assert_eq!(
            st.working_nostr_pk("petra"),
            fresh,
            "after the cut the seat is addressable only at the key it no longer holds"
        );
    }

    /// R3b — the relay LEDGER: every member's chain answers "which relays is
    /// this seat on record as reaching". A founding member is covered by the
    /// ratified genesis pool; a restored seat's threshold-signed declaration
    /// overrides it — for EVERY member reading the same chain; and a
    /// compaction cut must not forget a declaration (checkpoint-v6), because
    /// split detection (R4) runs on exactly this data.
    #[test]
    fn the_ledger_reports_declared_relays_and_survives_a_cut() {
        let pool = vec!["wss://relay.one".to_string()];
        let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool.clone());
        let rid = b.republic_id.clone();
        let replica = move || {
            Some(crate::ReplicaState {
                name: "Chess Club".to_string(),
                member: "walter".to_string(),
                roster: vec!["petra".to_string(), "walter".to_string()],
                rule_m: 2,
                identities: Vec::new(),
                agenda: "play chess".to_string(),
                features: None,
                republic_id: rid.clone(),
                founded_ts: 0,
            })
        };
        let mut st = crate::tests::plain_state();
        st.replica = replica();
        st.adopt_chain(b.blocks.clone());
        assert_eq!(
            st.member_relays("walter"),
            pool,
            "a founding member is covered by the ratified pool"
        );

        // petra re-joins over a DIFFERENT relay and declares it in the block
        let fresh = molt_net::nostr_identity(b"petra-recovered", "new-ticket").1;
        let declared = vec!["wss://relay.two.example".to_string()];
        let restored = b.seal(
            1,
            ChainChange::Membership {
                op: MembershipOp::Restored,
                member: "petra".to_string(),
                identity_pk: b.pk("petra"),
                nostr_pk: Some(fresh),
                relays: declared.clone(),
                consent: None,
            },
            &["petra", "walter"],
        );
        b.push(restored);
        st.adopt_chain(b.blocks.clone());
        assert_eq!(
            st.member_relays("petra"),
            declared,
            "every member's ledger reports the declared pool"
        );
        assert_eq!(st.member_relays("walter"), pool, "the others stay on the ratified pool");

        // cut ABOVE the Restored block: the declaration must ride the summary
        let blob = checkpoint_state(&b.blocks, 1).expect("state@1");
        assert_eq!(
            blob.member_relays,
            vec![("petra".to_string(), declared.clone())],
            "the summary carries the declarations the dropped blocks established"
        );
        let cut = b.seal(
            2,
            ChainChange::Checkpoint { upto: 1, state_hash: checkpoint_state_hash(&blob) },
            &["petra", "walter"],
        );
        b.push(cut);
        let mut pruned = crate::tests::plain_state();
        pruned.replica = replica();
        pruned.set_checkpoint_blob(Some(blob));
        pruned.adopt_chain(b.blocks[2..].to_vec());
        assert_eq!(
            pruned.member_relays("petra"),
            declared,
            "a cut must not forget a declaration"
        );
        assert_eq!(pruned.member_relays("walter"), pool, "…nor the ratified fallback");
    }

    /// R4 — split detection: two members whose effective relay sets are
    /// disjoint produce a verdict naming both, and the members surface says
    /// so per member — compactly, with the relay that would bridge — rather
    /// than staying a silence while the threshold quietly cannot assemble.
    #[test]
    fn disjoint_relay_sets_produce_a_split_verdict_naming_the_bridge() {
        let pool = vec!["wss://relay.one".to_string()];
        let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool.clone());
        let rid = b.republic_id.clone();
        let mut st = crate::tests::plain_state();
        st.replica = Some(crate::ReplicaState {
            name: "Chess Club".to_string(),
            member: "walter".to_string(),
            roster: vec!["petra".to_string(), "walter".to_string()],
            rule_m: 2,
            identities: Vec::new(),
            agenda: "play chess".to_string(),
            features: None,
            republic_id: rid,
            founded_ts: 0,
        });
        st.adopt_chain(b.blocks.clone());
        assert!(st.relay_splits().is_empty(), "one shared pool - no split");

        // petra re-joins over a relay NOBODY else carries
        let fresh = molt_net::nostr_identity(b"petra-recovered", "new-ticket").1;
        let restored = b.seal(
            1,
            ChainChange::Membership {
                op: MembershipOp::Restored,
                member: "petra".to_string(),
                identity_pk: b.pk("petra"),
                nostr_pk: Some(fresh),
                relays: vec!["wss://relay.two.example".to_string()],
                consent: None,
            },
            &["petra", "walter"],
        );
        b.push(restored);
        st.adopt_chain(b.blocks.clone());

        let splits = st.relay_splits();
        assert_eq!(
            splits,
            vec![("petra".to_string(), "walter".to_string())],
            "the verdict names both seats"
        );
        // …and the members surface carries the marker, naming the bridge
        let view = st.members_view();
        let row = |m: &str| {
            view.iter()
                .find(|v| v.member == m)
                .unwrap_or_else(|| panic!("{m} row"))
                .split
                .clone()
        };
        assert!(
            row("petra").contains("walter") && row("petra").contains("wss://relay.two.example"),
            "petra's marker names the counterpart and her odd relay: {:?}",
            row("petra")
        );
        assert!(
            row("walter").contains("petra") && row("walter").contains("wss://relay.one"),
            "walter's marker mirrors it: {:?}",
            row("walter")
        );
    }

    #[test]
    fn chain_applied_entries_carry_their_proposal_id() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_applied(7, &["petra", "walter"]);
        let mut peer = crate::tests::plain_state();
        peer.adopt_chain(b.blocks.clone());
        let snap = peer.snapshot(Surface::Memory, None, None);
        assert_eq!(snap.applied.len(), 1, "one committed Applied block");
        assert_eq!(
            snap.applied_ids,
            vec![Some(7)],
            "the block's proposal id rides the id track"
        );
    }

    /// The deliberation gossip is ephemeral RAM on every RECEIVER — only the
    /// proposer's own log records `Proposed`. A holder that adopts an
    /// Applied block without the card (reopen replay, catch-up past lost
    /// gossip) must materialize the record FROM the block: the chain
    /// carries payload and signers. Without it the Accepted view degraded
    /// to an id-less row — no voters, and the raw multi-line patch dumped
    /// into the value cell (field report 2026-08-16, the dev republic).
    #[test]
    fn an_adopted_applied_block_materializes_its_accepted_card() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        let block = b.seal(
            1,
            ChainChange::Applied {
                proposal_id: 4,
                surface: Surface::Memory,
                payload: json!({
                    "op": "wiki_patch",
                    "value": "diff --git a/a.md b/a.md\nnew file mode 100644\n--- /dev/null\n+++ b/a.md\n@@ -0,0 +1,1 @@\n+hi\n",
                    "summary": "+1 -0 →0 ~1",
                }),
            },
            &["petra", "walter"],
        );
        b.push(block);

        // a reopen: the chain comes from disk, the ephemeral proposal map
        // is empty (this peer never was the proposer)
        let mut peer = crate::tests::plain_state();
        peer.replica = Some(crate::ReplicaState {
            name: "Chess Club".to_string(),
            member: "dora".to_string(),
            roster: vec!["petra".to_string(), "walter".to_string(), "dora".to_string()],
            rule_m: 2,
            identities: Vec::new(),
            agenda: String::new(),
            features: None,
            republic_id: b.republic_id.clone(),
            founded_ts: 0,
        });
        peer.adopt_chain(b.blocks.clone());

        let snap = peer.snapshot(Surface::Memory, None, None);
        assert_eq!(snap.applied_ids, vec![Some(4)]);
        assert_eq!(snap.accepted.len(), 1, "the accepted card exists again");
        let card = &snap.accepted[0];
        assert_eq!(card.id.0, 4);
        assert_eq!(card.state, molt_core::ProposalState::Applied);
        assert_eq!(card.approvals, 2, "the sealed block's signer count");
        let approved: Vec<&str> = card
            .votes
            .iter()
            .filter(|v| v.vote == molt_core::VoteState::Approved)
            .map(|v| v.member.as_str())
            .collect();
        assert_eq!(approved, vec!["petra", "walter"], "who voted is chain-proven");
        assert_eq!(
            card.payload["op"],
            json!("wiki_patch"),
            "the payload keeps its shape (the GUI's patch rendering keys on it)"
        );

        // the LIVE twin: a broadcast block for a proposal this node never
        // heard of (its gossip was lost) materializes the card the same way
        let late = b.seal(
            2,
            ChainChange::Applied {
                proposal_id: 9,
                surface: Surface::Memory,
                payload: json!({ "op": "add_note", "title": "minutes" }),
            },
            &["walter", "dora"],
        );
        peer.receive_block(late);
        let snap = peer.snapshot(Surface::Memory, None, None);
        assert_eq!(snap.accepted.len(), 2, "both cards stand");
        let card = snap
            .accepted
            .iter()
            .find(|c| c.id.0 == 9)
            .expect("the late block's card");
        assert_eq!(card.approvals, 2);
        assert!(
            card.votes
                .iter()
                .any(|v| v.member == "dora" && v.vote == molt_core::VoteState::Approved),
            "the live-adopted card names its signers too"
        );
    }

    /// KEYSTONE for `tie_break` (previously untested): two members seal
    /// competing blocks at the same height; the lower hash wins the tip.
    /// A record MATERIALIZED from the displaced block must VANISH with it
    /// (review 2026-08-16: flipping it to Proposed minted a permanent,
    /// unowned open card — unwithdrawable, re-gossiped forever, and it
    /// blocked auto-checkpoints on that holder).
    #[test]
    fn tie_break_drops_a_materialized_card_with_its_displaced_block() {
        let b = Builder::new(&["petra", "walter"], 2);
        let genesis = b.blocks.clone();
        let block_a = b.seal(
            1,
            ChainChange::Applied {
                proposal_id: 7,
                surface: Surface::Memory,
                payload: json!({ "op": "add_note", "title": "a" }),
            },
            &["petra", "walter"],
        );
        let block_b = b.seal(
            1,
            ChainChange::Applied {
                proposal_id: 9,
                surface: Surface::Memory,
                payload: json!({ "op": "add_note", "title": "b" }),
            },
            &["petra", "walter"],
        );
        let rid = b.republic_id.clone();
        let hash = |blk: &ChainBlock| molt_storage::content_hash(&block_link_bytes(&rid, blk));
        let (winner, loser) = if hash(&block_a) < hash(&block_b) {
            (block_a, block_b)
        } else {
            (block_b, block_a)
        };
        let (loser_id, winner_id) = match (&loser.change, &winner.change) {
            (
                ChainChange::Applied { proposal_id: l, .. },
                ChainChange::Applied { proposal_id: w, .. },
            ) => (*l, *w),
            _ => unreachable!("both are Applied"),
        };

        // the peer adopts the LOSER first (its card is materialized from
        // the block — this holder never saw the gossip), then the winner
        // arrives and takes the tip
        let mut peer = crate::tests::plain_state();
        peer.replica = Some(crate::ReplicaState {
            name: "Chess Club".to_string(),
            member: "walter".to_string(),
            roster: vec!["petra".to_string(), "walter".to_string()],
            rule_m: 2,
            identities: Vec::new(),
            agenda: String::new(),
            features: None,
            republic_id: rid.clone(),
            founded_ts: 0,
        });
        peer.adopt_chain(genesis);
        peer.receive_block(loser);
        assert_eq!(
            peer.proposals.get(&loser_id).map(|p| p.state),
            Some(ProposalState::Applied),
            "the loser's card is materialized while its block stands"
        );

        peer.receive_block(winner);
        assert_eq!(peer.chain.len(), 2);
        let tip = peer.chain.last().expect("tip");
        assert!(
            matches!(&tip.change, ChainChange::Applied { proposal_id, .. } if *proposal_id == winner_id),
            "the lower hash holds the tip"
        );
        assert!(
            !peer.proposals.contains_key(&loser_id),
            "the materialized card vanished with its displaced block - no phantom open card"
        );
        assert_eq!(
            peer.proposals.get(&winner_id).map(|p| p.state),
            Some(ProposalState::Applied),
            "the winner's card stands, chain-proven"
        );
    }

    #[test]
    fn a_peer_adopts_a_broadcast_block_and_converges() {
        let b = Builder::new(&["petra", "walter"], 2);
        let genesis = b.blocks.clone();
        // a block committed elsewhere: an Applied change signed by both members
        let change = ChainChange::Applied {
            proposal_id: 1,
            surface: Surface::Memory,
            payload: json!({ "op": "add_note", "title": "minutes" }),
        };
        let block = b.seal(1, change, &["petra", "walter"]);

        // walter holds only the genesis, then the block arrives over the mesh
        let mut peer = crate::tests::plain_state();
        peer.replica = Some(crate::ReplicaState {
            name: "Chess Club".to_string(),
            member: "walter".to_string(),
            roster: vec!["petra".to_string(), "walter".to_string()],
            rule_m: 2,
            identities: Vec::new(), // adopt_chain fills these from the verified head
            agenda: "play chess".to_string(),
            features: None,
            republic_id: b.republic_id.clone(),
            founded_ts: 0,
        });
        peer.adopt_chain(genesis);
        assert!(peer.is_chain_governed());
        assert_eq!(peer.chain_head.as_ref().expect("head").height, 0);

        peer.receive_block(block);
        assert_eq!(peer.chain.len(), 2, "the peer adopted the broadcast block");
        assert_eq!(peer.chain_head.as_ref().expect("head").height, 1);
        let applied = peer
            .chain_applied
            .get(&Surface::Memory)
            .expect("memory projection");
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].0, Some(1), "the projection keeps the proposal id");
        assert_eq!(applied[0].1["title"], json!("minutes"));

        // an invalid block (tampered payload, sigs no longer match) is rejected
        let mut forged = b.seal(
            2,
            ChainChange::Applied {
                proposal_id: 2,
                surface: Surface::Memory,
                payload: json!({ "op": "add_note", "title": "real" }),
            },
            &["petra", "walter"],
        );
        forged.prev = peer.chain_head.as_ref().expect("head").hash.clone();
        if let ChainChange::Applied { payload, .. } = &mut forged.change {
            *payload = json!({ "op": "add_note", "title": "forged" });
        }
        peer.receive_block(forged);
        assert_eq!(peer.chain.len(), 2, "a tampered block is hard-rejected");
    }

    /// Attach real (temp-dir) storage to a test peer; `dead_writer` closes
    /// the writer first, so every blocking persist honestly reports `false`.
    fn attach_storage(peer: &mut crate::State, dead_writer: bool) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("tmp");
        let seed =
            molt_storage::seed_entropy(&molt_storage::generate_seed_phrase().expect("phrase"))
                .expect("entropy");
        let genesis = molt_core::EventEnvelope {
            prev_seq: 0,
            seq: 1,
            ts: 10,
            by: "walter".to_string(),
            body: molt_core::WorkspaceEvent::Founded {
                name: "Chess Club".to_string(),
                rule_m: 2,
                rule_n: 2,
                member: "walter".to_string(),
                roster: vec!["petra".to_string(), "walter".to_string()],
                identities: Vec::new(),
                attestations: Vec::new(),
                republic_id: String::new(),
                agenda: String::new(),
                relays: Vec::new(),
                features: None,
            },
        };
        let ws = molt_storage::create_workspace(tmp.path(), &seed, &genesis).expect("create");
        let dir = ws.dir().to_path_buf();
        let handle = molt_storage::start_writer(ws);
        if dead_writer {
            handle.clone().close(None);
        }
        peer.active = Some(crate::ActiveStorage {
            id: "w-h3".to_string(),
            dir,
            prefs: molt_core::WorkspacePrefs::default(),
            handle,
        });
        tmp
    }

    /// **Known-debt refinement (2026-08-16 list): only a buffered block
    /// ADJACENT to head pins the auto-checkpoint.** The buffer accepts
    /// claims up to head+4096, so an insider posting one plausible
    /// near-future height used to freeze compaction until a drain or
    /// re-serve cleared it. A gap block cannot apply next — only head+1
    /// says the head is about to move and the cut would be stale.
    #[test]
    fn a_far_future_buffered_block_does_not_pin_the_auto_checkpoint() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        for i in 1..=32 {
            b.commit_applied(i, &["petra", "walter"]);
        }
        let head_h = b.blocks.last().expect("blocks").height;
        let mut dummy = b.blocks[1].clone();

        // a far-future claim in the buffer must NOT hold the compaction
        let mut peer = chain_peer("petra", &b, b.blocks.clone());
        dummy.height = head_h + 2;
        peer.pending_blocks.insert(head_h + 2, dummy.clone());
        peer.maybe_auto_checkpoint();
        assert!(
            peer.proposal_changes
                .values()
                .any(|c| matches!(c, ChainChange::Checkpoint { .. })),
            "a gap block cannot apply next - the cut must still be proposed"
        );

        // …but a block adjacent to head still pins it: the head is about
        // to move and the cut would be stale on arrival
        let mut peer = chain_peer("petra", &b, b.blocks.clone());
        dummy.height = head_h + 1;
        peer.pending_blocks.insert(head_h + 1, dummy);
        peer.maybe_auto_checkpoint();
        assert!(
            !peer
                .proposal_changes
                .values()
                .any(|c| matches!(c, ChainChange::Checkpoint { .. })),
            "an adjacent buffered block keeps pinning the auto-checkpoint"
        );
    }

    /// **H3 second half (total_review.md): the governance broadcast waits
    /// for the durable persist.** A threshold-sealed block whose write did
    /// NOT reach the disk is still appended and projected locally — the
    /// signatures are real — but it is NOT broadcast: no `Committed`
    /// envelope, no decision summary. The peers seal the byte-identical
    /// block from the approval gossip themselves; this node must not
    /// spread history it does not durably hold. A durable seal broadcasts
    /// exactly as before.
    #[test]
    fn a_block_that_missed_the_disk_is_not_broadcast() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        let genesis = b.blocks.clone();
        b.commit_applied(1, &["petra", "walter"]);
        let block = b.blocks[1].clone();

        // (1) the writer is gone — sealed, appended, NOT broadcast
        let mut peer = chain_peer("walter", &b, genesis.clone());
        let _tmp = attach_storage(&mut peer, true);
        let seq_before = peer.next_seq;
        let chat_before = peer.chat.len();
        peer.adopt_committed_block(block.clone(), 1);
        assert_eq!(peer.chain.len(), 2, "the sealed block is appended locally");
        assert_eq!(
            peer.next_seq, seq_before,
            "no envelope may be minted for a block the disk never took"
        );
        assert_eq!(peer.chat.len(), chat_before, "no decision summary either");

        // (2) the writer lives — durable, broadcast as before
        let mut peer = chain_peer("petra", &b, genesis);
        let _tmp = attach_storage(&mut peer, false);
        let seq_before = peer.next_seq;
        peer.adopt_committed_block(block, 1);
        assert_eq!(peer.chain.len(), 2);
        assert!(
            peer.next_seq > seq_before,
            "a durable seal broadcasts its Committed envelope"
        );
        peer.active.take().expect("active").handle.close(None);
    }

    /// A 2-member chain-governed peer holding only the genesis `b` roots.
    fn chain_peer(member: &str, b: &Builder, chain: Vec<ChainBlock>) -> crate::State {
        let mut peer = crate::tests::plain_state();
        peer.replica = Some(crate::ReplicaState {
            name: "Chess Club".to_string(),
            member: member.to_string(),
            roster: vec!["petra".to_string(), "walter".to_string()],
            rule_m: 2,
            identities: Vec::new(),
            agenda: "play chess".to_string(),
            features: None,
            republic_id: b.republic_id.clone(),
            founded_ts: 0,
        });
        peer.adopt_chain(chain);
        peer
    }

    /// A block arriving ahead of our head is buffered, then applied once the
    /// gap fills — catch-up converges regardless of arrival order.
    #[test]
    fn out_of_order_blocks_buffer_and_converge() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        let genesis = b.blocks.clone();
        b.commit_applied(1, &["petra", "walter"]);
        b.commit_applied(2, &["petra", "walter"]);
        let block1 = b.blocks[1].clone();
        let block2 = b.blocks[2].clone();

        let mut peer = chain_peer("walter", &b, genesis);
        // the height-2 block arrives first — a gap, so it is buffered
        peer.receive_block(block2);
        assert_eq!(
            peer.chain_head.as_ref().expect("head").height,
            0,
            "a gap block is buffered, not applied"
        );
        assert_eq!(peer.pending_blocks.len(), 1);
        // the height-1 block fills the gap; the buffered height-2 drains behind it
        peer.receive_block(block1);
        assert_eq!(peer.chain_head.as_ref().expect("head").height, 2);
        assert!(peer.pending_blocks.is_empty(), "the buffer drained");
    }

    /// One survivor holding the full chain re-serves a lagging member the whole
    /// missing suffix — the resilience property (any survivor suffices), and the
    /// suffix applies even delivered out of order.
    #[test]
    fn a_survivor_serves_a_lagging_member_the_full_suffix() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        let genesis = b.blocks.clone();
        b.commit_applied(1, &["petra", "walter"]);
        b.commit_applied(2, &["petra", "walter"]);
        let full = b.blocks.clone();

        let mut peer = chain_peer("walter", &b, genesis);
        assert_eq!(peer.chain_head.as_ref().expect("head").height, 0);

        // the survivor serves every block from the peer's head+1 (=1) onward,
        // straight out of its own chain — exactly what serve_chain_from does
        let served: Vec<ChainBlock> = full.iter().filter(|bl| bl.height >= 1).cloned().collect();
        assert_eq!(served.len(), 2, "survivor serves b1 + b2 from its chain");
        for bl in served.into_iter().rev() {
            peer.receive_block(bl); // delivered newest-first to exercise buffering
        }
        assert_eq!(
            peer.chain_head.as_ref().expect("head").height,
            2,
            "the lagging member caught up to the survivor"
        );
        assert!(peer.pending_blocks.is_empty());
    }

    /// Split a bootstrap offer back into the shape `verify_served` takes.
    fn split_bootstrap(
        events: &[WorkspaceEvent],
    ) -> (Option<molt_core::CheckpointState>, Vec<ChainBlock>) {
        let mut blob = None;
        let mut blocks = Vec::new();
        for ev in events {
            match ev {
                WorkspaceEvent::CheckpointServed { blob: b } => blob = Some(b.clone()),
                WorkspaceEvent::Committed(bl) => blocks.push(bl.clone()),
                other => panic!("a bootstrap offer carries nothing else: {other:?}"),
            }
        }
        (blob, blocks)
    }

    /// **The anchor is the smallest prefix that verifies standalone.**
    ///
    /// A rejoiner cannot be handed the whole chain — one `set_image` block
    /// exceeds the gift-wrap cap — and cannot be handed a bare head, because
    /// `verify_chain` is all-or-nothing from the anchor and a headless node
    /// drops every block served to it. So it is handed the ANCHOR, and asks
    /// for the rest over the ordinary catch-up once it has a workspace.
    ///
    /// The COUNT is asserted, not merely the verification: an implementation
    /// that served the whole chain would verify perfectly well and quietly
    /// reintroduce the size cliff this exists to avoid.
    #[test]
    fn the_served_anchor_is_the_smallest_prefix_that_verifies() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        b.commit_applied(2, &["petra", "walter"]);

        // a FULL holder offers the genesis, alone
        let full = chain_peer("walter", &b, b.blocks.clone());
        let offer = full.anchor_bootstrap();
        let (blob, blocks) = split_bootstrap(&offer);
        assert!(blob.is_none(), "a full holder has no blob to offer");
        assert_eq!(blocks.len(), 1, "the genesis and nothing else - not the chain");
        assert_eq!(blocks[0].height, 0);
        let (head, sealed) = verify_served(blob.as_ref(), &blocks, Some(&b.republic_id))
            .expect("the genesis verifies standalone");
        assert_eq!(head.height, 0, "a length-1 chain is a valid chain");
        assert_eq!(sealed.republic_id, b.republic_id);

        // …and a PRUNED holder offers its blob plus its anchor block, because
        // by then no node anywhere still holds a genesis
        let blob_at_2 = checkpoint_state(&b.blocks, 2).expect("state@2");
        let anchor = b.seal(
            3,
            ChainChange::Checkpoint {
                upto: 2,
                state_hash: checkpoint_state_hash(&blob_at_2),
            },
            &["petra", "walter"],
        );
        b.push(anchor.clone());
        let mut pruned = chain_peer("walter", &b, b.blocks[..3].to_vec());
        pruned.receive_block(anchor);
        assert!(pruned.checkpoint_blob.is_some(), "the holder pruned");

        let offer = pruned.anchor_bootstrap();
        let (blob, blocks) = split_bootstrap(&offer);
        assert!(blob.is_some(), "a pruned holder's blob IS its trust root");
        assert_eq!(blocks.len(), 1, "the anchor block alone, not the suffix");
        assert_eq!(blocks[0].height, 3);
        let (head, _) = verify_served(blob.as_ref(), &blocks, Some(&b.republic_id))
            .expect("blob + anchor verify standalone under the suffix rules");
        assert_eq!(head.height, 3, "the rejoiner starts at the cut, not at zero");

        // …and broadcasting it must not disturb the SERVER: a 445 reaches
        // every member, so the offer travels back through this node's own
        // apply path (and through every survivor's) as a duplicate
        let before = (pruned.chain.clone(), pruned.chain_head.clone());
        pruned.serve_chain_anchor();
        assert_eq!(pruned.chain, before.0, "serving must not move the server's chain");
        assert_eq!(
            pruned.chain_head.as_ref().map(|h| h.height),
            before.1.as_ref().map(|h| h.height),
            "nor its head"
        );
    }

    /// WP3, the wire side of the decodability gate: a peer's `set_image`
    /// gossip with undecodable bytes is dropped with a warning, never
    /// recorded as a pending proposal (convergence before enforcement —
    /// the same posture as the byte-cap guard it extends).
    #[test]
    fn an_undecodable_peer_set_image_is_dropped_not_recorded() {
        use base64::Engine as _;
        let b = Builder::new(&["petra", "walter"], 2);
        let mut peer = chain_peer("walter", &b, b.blocks.clone());
        let deliver = |peer: &mut crate::State, id: u64, b64: String| {
            let env = molt_core::EventEnvelope { prev_seq: 0,
                seq: 90 + id,
                ts: 1_751_000_000,
                by: "petra".to_string(),
                body: WorkspaceEvent::Proposed {
                    id: ProposalId(id),
                    surface: Surface::Organization,
                    payload: json!({ "op": "set_image", "value": "x.png", "bytes_b64": b64 }),
                },
            };
            peer.cmd_net_delivered("petra".to_string(), env, None)
                .expect("a wire drop acks, never errors");
        };
        // garbage within the byte cap: dropped, not recorded
        let garbage = base64::engine::general_purpose::STANDARD.encode(b"not an image");
        deliver(&mut peer, 9, garbage);
        assert!(
            !peer.proposals.contains_key(&9),
            "undecodable peer bytes must never become a pending proposal"
        );
        // a real 2x2 png is recorded
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAFklEQVR4nGM8ISfHwMDAxMDAwMDAAAANBAEIfXHKZgAAAABJRU5ErkJggg==";
        deliver(&mut peer, 10, png.to_string());
        assert!(
            peer.proposals.contains_key(&10),
            "a decodable peer set_image is recorded as pending"
        );
    }

    /// **Self-edit, on the wire** (`member_profiles_plan.md` §2): a member
    /// profile belongs to its member, and the link identity is the only
    /// proof of authorship — so a profile proposal claiming ANOTHER seat is
    /// dropped, never recorded. Node-independent like the drops beside it,
    /// so every honest holder drops the same frame. The picture ops carry
    /// the decodable+square verdict onto the wire too.
    #[test]
    fn a_profile_proposal_claiming_another_member_is_dropped() {
        use base64::Engine as _;
        let b = Builder::new(&["petra", "walter"], 2);
        let mut peer = chain_peer("walter", &b, b.blocks.clone());
        let deliver = |peer: &mut crate::State, id: u64, from: &str, payload: serde_json::Value| {
            let env = molt_core::EventEnvelope {
                prev_seq: 0,
                seq: 300 + id,
                ts: 1_751_000_000,
                by: from.to_string(),
                body: WorkspaceEvent::Proposed {
                    id: ProposalId(id),
                    surface: Surface::Organization,
                    payload,
                },
            };
            peer.cmd_net_delivered(from.to_string(), env, None)
                .expect("a wire drop acks, never errors");
        };
        // A profile op arriving under ANOTHER member's link is the normal
        // WP2 shape, not a forgery: `serve_open_governance` re-serves every
        // open card under the SERVING peer's identity (make_env(me, body)),
        // so a catching-up holder meets walter's edit with from = petra.
        // Dropping on `payload.member != from` blinded exactly that holder -
        // it could never see, let alone vote on, another seat's profile
        // proposal. Authorship is unauthenticated by design here
        // (`ProposalRecord.by` is a DISPLAY hint); the self-edit rule is
        // enforced where it IS decidable, at the propose gate.
        deliver(&mut peer, 20, "petra", json!({ "op": "set_member_desc", "member": "walter", "value": "hi" }));
        assert!(
            peer.proposals.contains_key(&20),
            "a re-served profile card must reach a catching-up holder"
        );
        // petra editing her own: recorded
        deliver(&mut peer, 21, "petra", json!({ "op": "set_member_desc", "member": "petra", "value": "hi" }));
        assert!(peer.proposals.contains_key(&21), "a member's own profile edit is recorded");
        // what IS node-independently decidable: the seat must exist. A
        // profile op for a stranger could never fold onto anything
        deliver(&mut peer, 26, "petra", json!({ "op": "set_member_desc", "member": "ghost", "value": "hi" }));
        assert!(
            !peer.proposals.contains_key(&26),
            "a profile op for a seat that is not in the roster is dropped"
        );
        deliver(&mut peer, 27, "petra", json!({ "op": "set_member_desc", "value": "hi" }));
        assert!(
            !peer.proposals.contains_key(&27),
            "a profile op naming no seat is dropped"
        );
        // the picture ops carry the square rule onto the wire
        let square = base64::engine::general_purpose::STANDARD
            .encode(crate::tests::tiny_bmp_header(2, 2));
        let wide = base64::engine::general_purpose::STANDARD
            .encode(crate::tests::tiny_bmp_header(4, 2));
        deliver(&mut peer, 22, "petra", json!({ "op": "set_member_image", "member": "petra", "value": "f.bmp", "bytes_b64": wide }));
        assert!(!peer.proposals.contains_key(&22), "a non-square peer avatar is dropped");
        deliver(&mut peer, 23, "petra", json!({ "op": "set_member_image", "member": "petra", "value": "f.bmp", "bytes_b64": square }));
        assert!(peer.proposals.contains_key(&23), "a square, decodable peer avatar is recorded");
        // the length cap is a contract, not a local preference: a description
        // the propose gate refuses must not walk in through the wire door
        let long = "x".repeat(crate::proposals::DESC_MAX + 1);
        deliver(&mut peer, 24, "petra", json!({ "op": "set_member_desc", "member": "petra", "value": long }));
        assert!(
            !peer.proposals.contains_key(&24),
            "an over-long description is dropped at the wire too"
        );
        let edge = "x".repeat(crate::proposals::DESC_MAX);
        deliver(&mut peer, 25, "petra", json!({ "op": "set_member_desc", "member": "petra", "value": edge }));
        assert!(peer.proposals.contains_key(&25), "a description at the limit is recorded");
    }

    /// A 2-of-3 chain peer holding the FULL three-member roster (the shared
    /// `chain_peer` pins the founding pair).
    fn chain_peer_3(member: &str, b: &Builder) -> crate::State {
        let mut peer = crate::tests::plain_state();
        peer.replica = Some(crate::ReplicaState {
            name: "Chess Club".to_string(),
            member: member.to_string(),
            roster: vec!["petra".to_string(), "walter".to_string(), "dora".to_string()],
            rule_m: 2,
            identities: Vec::new(),
            agenda: "play chess".to_string(),
            features: None,
            republic_id: b.republic_id.clone(),
            founded_ts: 0,
        });
        peer.adopt_chain(b.blocks.clone());
        peer
    }

    /// One wire envelope from `from` (per-sender seq; prev_seq 0 = unordered).
    fn wire(peer: &mut crate::State, from: &str, seq: u64, body: WorkspaceEvent) {
        let env = molt_core::EventEnvelope {
            prev_seq: 0,
            seq,
            ts: 1_751_000_000 + seq,
            by: from.to_string(),
            body,
        };
        peer.cmd_net_delivered(from.to_string(), env, None)
            .expect("a wire delivery acks, never errors");
    }

    /// **The proposer pulls a proposal back** (the ProposalCard's "pull
    /// back"): terminal like a rejection, but no vote is forged — the
    /// verdict is `withdrawn`, never "declined by".
    #[test]
    fn a_withdraw_turns_the_card_terminal_without_forging_a_vote() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut peer = chain_peer_3("walter", &b);
        let id = match peer
            .cmd_propose(
                Surface::Organization,
                json!({ "op": "set_name", "value": "Mine" }),
            )
            .expect("propose")
        {
            molt_core::Reply::Proposed { id } => id,
            other => panic!("unexpected reply {other:?}"),
        };
        peer.cmd_withdraw(id).expect("withdraw");
        let p = peer.proposals.get(&id.0).expect("card");
        assert_eq!(p.state, ProposalState::Rejected);
        assert!(p.withdrawn, "the verdict is its own, not a decline");
        assert!(p.decliners.is_empty(), "no vote forged");
        assert_eq!(p.declined_by, "", "no decliner named");
        assert!(
            !peer.pending_sigs.contains_key(&id.0),
            "collected signatures are cleared"
        );
        // terminal: a second withdraw refuses
        assert!(peer.cmd_withdraw(id).is_err());
    }

    /// Only the proposer withdraws: the local command refuses a foreign
    /// card, and the wire arm counts a withdraw only when the link
    /// identity IS the recorded proposer (no signature — same posture as
    /// declines, plus the proposer check).
    #[test]
    fn only_the_proposer_may_withdraw() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut peer = chain_peer_3("walter", &b);
        wire(
            &mut peer,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(9),
                surface: Surface::Organization,
                payload: json!({ "op": "set_name", "value": "Petras" }),
            },
        );
        // walter is not the proposer — the command refuses
        assert!(matches!(
            peer.cmd_withdraw(ProposalId(9)),
            Err(molt_core::MoltError::NotTheProposer(_))
        ));
        // forgery: dora's link carries petra's withdraw — dropped
        wire(
            &mut peer,
            "dora",
            1,
            WorkspaceEvent::Withdrawn { id: ProposalId(9), by: "petra".to_string() },
        );
        assert_eq!(
            peer.proposals.get(&9).expect("card").state,
            ProposalState::Proposed
        );
        // dora withdrawing petra's card as herself — not the proposer, dropped
        wire(
            &mut peer,
            "dora",
            2,
            WorkspaceEvent::Withdrawn { id: ProposalId(9), by: "dora".to_string() },
        );
        assert_eq!(
            peer.proposals.get(&9).expect("card").state,
            ProposalState::Proposed
        );
        // the real proposer pulls it back
        wire(
            &mut peer,
            "petra",
            2,
            WorkspaceEvent::Withdrawn { id: ProposalId(9), by: "petra".to_string() },
        );
        let p = peer.proposals.get(&9).expect("card");
        assert_eq!(p.state, ProposalState::Rejected);
        assert!(p.withdrawn);
    }

    /// A withdraw ahead of its proposal parks (G7 orders per sender only)
    /// and lands the moment the card arrives — the verdict must never be
    /// lost to arrival order, exactly like a parked decline.
    #[test]
    fn a_withdraw_ahead_of_its_proposal_parks_and_registers() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut peer = chain_peer_3("walter", &b);
        wire(
            &mut peer,
            "petra",
            1,
            WorkspaceEvent::Withdrawn { id: ProposalId(9), by: "petra".to_string() },
        );
        wire(
            &mut peer,
            "petra",
            2,
            WorkspaceEvent::Proposed {
                id: ProposalId(9),
                surface: Surface::Organization,
                payload: json!({ "op": "set_name", "value": "Gone" }),
            },
        );
        let p = peer.proposals.get(&9).expect("card");
        assert_eq!(p.state, ProposalState::Rejected);
        assert!(p.withdrawn, "the parked withdraw registered on arrival");
    }

    /// The own withdraw re-serves with the open governance — a peer that
    /// was closed while the card died must still learn the verdict.
    #[test]
    fn open_governance_reserves_the_own_withdraw() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut peer = chain_peer_3("walter", &b);
        let id = match peer
            .cmd_propose(
                Surface::Organization,
                json!({ "op": "set_name", "value": "Short-lived" }),
            )
            .expect("propose")
        {
            molt_core::Reply::Proposed { id } => id,
            other => panic!("unexpected reply {other:?}"),
        };
        peer.cmd_withdraw(id).expect("withdraw");
        // keep the card inside the display retention (fixture ts is historic)
        peer.proposals.get_mut(&id.0).expect("card").declined_at = crate::now_secs();
        let events = peer.open_governance_events();
        assert!(
            events.iter().any(|e| matches!(
                e,
                WorkspaceEvent::Withdrawn { id: wid, by } if *wid == id && by == "walter"
            )),
            "the own withdraw re-serves"
        );
    }

    /// Live incident 2026-08-09 (defect 6): a decline is a VOTE — it must
    /// converge like an approval. Two wire declines in a 2-of-3 kill the
    /// proposal on every node; before the receive arm existed they were
    /// acked and DROPPED, so a majority-declined vote stayed pending forever
    /// on every node but the decliner's own.
    #[test]
    fn wire_declines_converge_and_reject_at_the_veto_threshold() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut peer = chain_peer_3("walter", &b);
        wire(
            &mut peer,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(9),
                surface: Surface::Organization,
                payload: json!({ "op": "set_name", "value": "New Name" }),
            },
        );
        wire(
            &mut peer,
            "dora",
            1,
            WorkspaceEvent::Declined { id: ProposalId(9), by: "dora".to_string(), hash: String::new() },
        );
        let p = peer.proposals.get(&9).expect("registered");
        assert_eq!(p.decliners, vec!["dora".to_string()], "the wire decline counts");
        assert_eq!(p.state, ProposalState::Proposed, "one decline in 2-of-3 leaves room");
        // forgery: the body claims dora again, but the link says petra — dropped,
        // a peer can only ever decline as itself
        wire(
            &mut peer,
            "petra",
            2,
            WorkspaceEvent::Declined { id: ProposalId(9), by: "dora".to_string(), hash: String::new() },
        );
        let p = peer.proposals.get(&9).expect("still there");
        assert_eq!(p.decliners.len(), 1, "a decline must carry its link identity");
        // a duplicate of dora's decline (resend) stays ONE voice
        wire(
            &mut peer,
            "dora",
            2,
            WorkspaceEvent::Declined { id: ProposalId(9), by: "dora".to_string(), hash: String::new() },
        );
        assert_eq!(peer.proposals.get(&9).expect("still there").decliners.len(), 1);
        // petra's real decline tips it: 2 > n − m = 1 → Rejected
        wire(
            &mut peer,
            "petra",
            3,
            WorkspaceEvent::Declined { id: ProposalId(9), by: "petra".to_string(), hash: String::new() },
        );
        let p = peer.proposals.get(&9).expect("still there");
        assert_eq!(p.state, ProposalState::Rejected, "a majority decline is terminal");
        assert_eq!(p.declined_by, "petra", "the tipping decliner is named");
        assert!(p.declined_at > 0, "the decline timestamp is the envelope's");
    }

    /// D7: a decline the FULL park would shed must stay UNACKED — the
    /// accept point ran before the park admission, so a shed voice was
    /// ACKed and the at-least-once guarantee was already spent on it: the
    /// sender trims it and the voice is gone for good. Left unacked, the
    /// resend machinery re-earns it once the park has room (or the
    /// proposal lands). The implausible-id garbage case deliberately stays
    /// accept-and-drop — a u64::MAX decline must not ride resend forever.
    #[test]
    fn a_shed_decline_stays_unacked_for_the_resend() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let base = walter.next_id;
        let per_member = u64::try_from(crate::proposals::PARKED_DECLINES_PER_MEMBER_MAX)
            .expect("cap fits");
        // fill petra's whole per-member allowance with plausible unknown ids
        for i in 0..per_member {
            wire(
                &mut walter,
                "petra",
                i + 1,
                WorkspaceEvent::Declined { id: ProposalId(base + i), by: "petra".to_string(), hash: String::new() },
            );
        }
        let accepted = |st: &crate::State, seq: u64| {
            st.accepted.get("petra").is_some_and(|w| w.is_accepted(seq))
        };
        assert!(accepted(&walter, per_member), "parked voices are accepted and acked");
        // the voice the park sheds must NOT be marked accepted
        wire(
            &mut walter,
            "petra",
            per_member + 1,
            WorkspaceEvent::Declined {
                id: ProposalId(base + per_member),
                by: "petra".to_string(),
                hash: String::new(),
            },
        );
        assert!(
            !accepted(&walter, per_member + 1),
            "a shed voice stays unacked so the resend re-earns it"
        );
        // …while garbage far past the mint window stays accept-and-drop
        wire(
            &mut walter,
            "petra",
            per_member + 2,
            WorkspaceEvent::Declined { id: ProposalId(u64::MAX), by: "petra".to_string(), hash: String::new() },
        );
        assert!(
            accepted(&walter, per_member + 2),
            "implausible-id garbage is accepted and dropped, never resent"
        );
    }

    /// D4: a park drain speaks with EVERY drained voice — one
    /// `Event::Declined` per registered member (never one event naming
    /// `decliners.last()`), and a drain that tips emits the voices AND the
    /// `Rejected`. An event-stream consumer must not undercount votes.
    #[test]
    fn a_park_drain_emits_one_declined_event_per_voice() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        // veto_room = 4 - 2 = 2: two parked voices stay a Voice drain
        let b = Builder::new(&["petra", "walter", "dora", "erika"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        wire(
            &mut walter,
            "dora",
            1,
            WorkspaceEvent::Declined { id: ProposalId(4), by: "dora".to_string(), hash: String::new() },
        );
        wire(
            &mut walter,
            "erika",
            1,
            WorkspaceEvent::Declined { id: ProposalId(4), by: "erika".to_string(), hash: String::new() },
        );
        let mut ev = walter.subscribe_events();
        wire(
            &mut walter,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(4),
                surface: Surface::Organization,
                payload: json!({ "op": "set_name", "value": "Late" }),
            },
        );
        let mut declined: Vec<String> = Vec::new();
        let mut rejected = 0;
        while let Ok(e) = ev.try_recv() {
            match e {
                crate::Event::Declined { id, by } if id.0 == 4 => declined.push(by),
                crate::Event::Rejected { id } if id.0 == 4 => rejected += 1,
                _ => {}
            }
        }
        declined.sort_unstable();
        assert_eq!(
            declined,
            vec!["dora".to_string(), "erika".to_string()],
            "one event per drained voice"
        );
        assert_eq!(rejected, 0, "two voices in veto room 2 do not tip");

        // …and a drain that TIPS still speaks every voice, then the verdict
        let mut peer = chain_signer("walter", &b, b.blocks.clone());
        for (i, who) in ["dora", "erika", "petra"].iter().enumerate() {
            wire(
                &mut peer,
                who,
                u64::try_from(i).expect("i") + 1,
                WorkspaceEvent::Declined {
                    id: ProposalId(4),
                    by: (*who).to_string(),
                    hash: String::new(),
                },
            );
        }
        let mut ev = peer.subscribe_events();
        wire(
            &mut peer,
            "petra",
            2,
            WorkspaceEvent::Proposed {
                id: ProposalId(4),
                surface: Surface::Organization,
                payload: json!({ "op": "set_name", "value": "Late" }),
            },
        );
        let (mut declined, mut rejected) = (0, 0);
        while let Ok(e) = ev.try_recv() {
            match e {
                crate::Event::Declined { id, .. } if id.0 == 4 => declined += 1,
                crate::Event::Rejected { id } if id.0 == 4 => rejected += 1,
                _ => {}
            }
        }
        assert_eq!((declined, rejected), (3, 1), "3 voices + the verdict");
    }

    /// D5: the decision line of a DECLINED vote is minted under a
    /// DETERMINISTIC message id — whoever tips posts it, concurrent
    /// posters collapse via the ordinary duplicate-id drop, and a wire tip
    /// posts too (it used to stay silent, so a vote tipped by a received
    /// decline had no decision line anywhere).
    #[test]
    fn a_wire_tipped_decline_posts_its_summary_exactly_once() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        // veto_room = 3 - 2 = 1: the SECOND decline tips
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        wire(
            &mut walter,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(4),
                surface: Surface::Organization,
                payload: json!({ "op": "set_name", "value": "X" }),
            },
        );
        let summaries = |st: &crate::State| {
            st.chat_visible()
                .filter(|m| {
                    m.kind == molt_core::ChatKind::System
                        && matches!(&m.channel, molt_core::ChannelRef::Patch { id } if id.0 == 4)
                })
                .count()
        };
        walter.cmd_decline(ProposalId(4)).expect("own voice, no tip");
        assert_eq!(summaries(&walter), 0, "one voice does not decide");
        wire(
            &mut walter,
            "dora",
            1,
            WorkspaceEvent::Declined { id: ProposalId(4), by: "dora".to_string(), hash: String::new() },
        );
        assert_eq!(
            summaries(&walter),
            1,
            "the wire tip posts the decision line"
        );
        // the OTHER tipper's copy arrives under the SAME deterministic id —
        // the ordinary duplicate-id drop collapses it
        let sid = crate::chat::decision_summary_id(&b.republic_id, 4, true);
        let copy = molt_core::ChatMessage::text(sid, "dora".to_string(), "⚖ #4 ⊘ …".to_string(), crate::now_secs())
            .with_channel(molt_core::ChannelRef::Patch { id: ProposalId(4) })
            .with_kind(molt_core::ChatKind::System);
        wire(&mut walter, "dora", 2, WorkspaceEvent::Chat(copy));
        assert_eq!(summaries(&walter), 1, "concurrent posters collapse to one line");
    }

    /// D1: a decline binds the payload the decliner SAW, not a bare id —
    /// two proposers minting the same id in one gossip round-trip must
    /// not let a voice register against a proposal the decliner never
    /// judged. An empty hash (older sender) keeps id-only semantics, and
    /// the park stores the hash so a drained voice is checked too.
    #[test]
    fn a_decline_carrying_a_foreign_payload_hash_does_not_register() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        wire(
            &mut walter,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(4),
                surface: Surface::Organization,
                payload: json!({ "op": "set_name", "value": "X" }),
            },
        );
        let h = |v: &serde_json::Value| crate::State::decline_payload_hash(v);
        wire(
            &mut walter,
            "dora",
            1,
            WorkspaceEvent::Declined {
                id: ProposalId(4),
                by: "dora".to_string(),
                hash: h(&json!({ "op": "set_name", "value": "Y" })),
            },
        );
        let p = walter.proposals.get(&4).expect("card");
        assert!(p.decliners.is_empty(), "a mismatching hash must not register");
        wire(
            &mut walter,
            "dora",
            2,
            WorkspaceEvent::Declined {
                id: ProposalId(4),
                by: "dora".to_string(),
                hash: h(&json!({ "op": "set_name", "value": "X" })),
            },
        );
        assert_eq!(
            walter.proposals.get(&4).expect("card").decliners,
            vec!["dora".to_string()],
            "the matching hash registers"
        );
        wire(
            &mut walter,
            "petra",
            2,
            WorkspaceEvent::Declined {
                id: ProposalId(4),
                by: "petra".to_string(),
                hash: String::new(),
            },
        );
        assert_eq!(
            walter.proposals.get(&4).expect("card").decliners.len(),
            2,
            "an empty hash (older sender) keeps id-only semantics"
        );
        // the PARK stores the hash: a parked mismatch never registers either
        wire(
            &mut walter,
            "dora",
            3,
            WorkspaceEvent::Declined {
                id: ProposalId(9),
                by: "dora".to_string(),
                hash: h(&json!({ "op": "set_name", "value": "Z" })),
            },
        );
        wire(
            &mut walter,
            "petra",
            3,
            WorkspaceEvent::Proposed {
                id: ProposalId(9),
                surface: Surface::Organization,
                payload: json!({ "op": "set_name", "value": "W" }),
            },
        );
        assert!(
            walter.proposals.get(&9).expect("card").decliners.is_empty(),
            "a drained parked voice is hash-checked too"
        );
    }

    /// A decline can outrun its proposal on the wire (G7 orders per sender
    /// only) and it replays from the own log before a re-served proposal
    /// returns: either way it PARKS and registers the moment the proposal
    /// is known — a vote must never be lost to arrival order.
    #[test]
    fn a_decline_ahead_of_its_proposal_parks_and_registers() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut peer = chain_peer_3("walter", &b);
        wire(
            &mut peer,
            "dora",
            1,
            WorkspaceEvent::Declined { id: ProposalId(4), by: "dora".to_string(), hash: String::new() },
        );
        wire(
            &mut peer,
            "petra",
            1,
            WorkspaceEvent::Declined { id: ProposalId(4), by: "petra".to_string(), hash: String::new() },
        );
        assert!(peer.proposals.is_empty(), "no card yet - the declines wait");
        wire(
            &mut peer,
            "petra",
            2,
            WorkspaceEvent::Proposed {
                id: ProposalId(4),
                surface: Surface::Organization,
                payload: json!({ "op": "set_name", "value": "Late" }),
            },
        );
        let p = peer.proposals.get(&4).expect("registered");
        assert_eq!(p.decliners.len(), 2, "both parked declines registered");
        assert_eq!(p.state, ProposalState::Rejected, "and they tip it immediately");
    }

    /// WP2 re-serve carries the OWN decline: a vote against survives RAM
    /// loss like a collected signature does, so a rejoiner (or a node whose
    /// pre-fix engine dropped the gossip) can still converge. Foreign
    /// declines are NOT re-attested — only the link identity vouches a
    /// decline — and a REJECTED card still serves the own voice.
    #[test]
    fn open_governance_reserves_the_own_decline() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut peer = chain_peer_3("walter", &b);
        // an open card walter declined (1 ≤ veto room → still Proposed)
        wire(
            &mut peer,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(7),
                surface: Surface::Organization,
                payload: json!({ "op": "set_name", "value": "Open" }),
            },
        );
        peer.cmd_decline(ProposalId(7)).expect("decline");
        // a card that went Rejected (petra's wire decline tips it)
        wire(
            &mut peer,
            "petra",
            2,
            WorkspaceEvent::Proposed {
                id: ProposalId(8),
                surface: Surface::Organization,
                payload: json!({ "op": "set_name", "value": "Dead" }),
            },
        );
        peer.cmd_decline(ProposalId(8)).expect("decline");
        wire(
            &mut peer,
            "petra",
            3,
            WorkspaceEvent::Declined { id: ProposalId(8), by: "petra".to_string(), hash: String::new() },
        );
        assert_eq!(
            peer.proposals.get(&8).expect("card").state,
            ProposalState::Rejected
        );
        // the fixture's wire ts is historic — stamp the decline fresh, or
        // the retention gate below would age the card out immediately
        peer.proposals.get_mut(&8).expect("card").declined_at = crate::now_secs();
        // a parked own decline (own-log replay raced a re-served proposal)
        peer.register_decline(11, "walter", 1_751_000_000, "");
        let events = peer.open_governance_events();
        let own_declines: Vec<u64> = events
            .iter()
            .filter_map(|e| match e {
                WorkspaceEvent::Declined { id, by, .. } if by == "walter" => Some(id.0),
                _ => None,
            })
            .collect();
        assert!(own_declines.contains(&7), "the open card's own decline re-serves");
        assert!(own_declines.contains(&8), "the rejected card's own decline re-serves");
        assert!(own_declines.contains(&11), "the parked own decline re-serves");
        assert!(
            !events.iter().any(
                |e| matches!(e, WorkspaceEvent::Declined { by, .. } if by == "petra")
            ),
            "a foreign decline is never re-attested"
        );
        // a rejected card past the display retention has no convergence
        // audience — its voice leaves the batch (review 2026-08-09,
        // finding 12), so the re-serve stays bounded
        peer.proposals.get_mut(&8).expect("card").declined_at = 1;
        let aged: Vec<u64> = peer
            .open_governance_events()
            .iter()
            .filter_map(|e| match e {
                WorkspaceEvent::Declined { id, by, .. } if by == "walter" => Some(id.0),
                _ => None,
            })
            .collect();
        assert!(!aged.contains(&8), "an aged-out rejected voice stops re-serving");
        assert!(aged.contains(&7), "the open card's voice stays");
    }

    /// Answering a ChainRequest re-records the served Proposed envelopes
    /// through the applier — that must not clobber the live card: an
    /// unconditional insert wiped every collected foreign decline on the
    /// SERVING node (review 2026-08-09, finding 1).
    #[test]
    fn serving_open_governance_keeps_the_own_cards_decliners() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut peer = chain_peer_3("walter", &b);
        wire(
            &mut peer,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(9),
                surface: Surface::Organization,
                payload: json!({ "op": "set_name", "value": "Kept" }),
            },
        );
        wire(
            &mut peer,
            "dora",
            1,
            WorkspaceEvent::Declined { id: ProposalId(9), by: "dora".to_string(), hash: String::new() },
        );
        peer.serve_open_governance();
        assert_eq!(
            peer.proposals.get(&9).expect("card").decliners,
            vec!["dora".to_string()],
            "serving must not wipe the collected voices"
        );
    }

    /// A decline referencing an id far past the mint counter is garbage —
    /// parking it would poison `next_id` (one u64::MAX frame froze every
    /// later local mint; review 2026-08-09, finding 2).
    #[test]
    fn a_decline_for_an_implausible_id_is_dropped() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut peer = chain_peer_3("walter", &b);
        let before = peer.next_id;
        wire(
            &mut peer,
            "petra",
            1,
            WorkspaceEvent::Declined { id: ProposalId(u64::MAX), by: "petra".to_string(), hash: String::new() },
        );
        assert!(peer.pending_declines.is_empty(), "garbage never parks");
        assert_eq!(peer.next_id, before, "and never moves the mint counter");
    }

    /// Decline-after-approve stays ALLOWED — it is how a proposer
    /// withdraws (the auto-cosign would otherwise lock every proposal
    /// open); the summary test pins the terminal effect. The view still
    /// reports the own stance so frontends can gray per what the engine
    /// actually refuses (approve-after-decline, re-decline).
    #[test]
    fn a_decline_after_the_own_approval_still_works() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut peer = chain_peer_3("walter", &b);
        peer.identity_sk = b
            .keys
            .iter()
            .find(|(m, _)| m == "walter")
            .map(|(_, sk)| sk.clone());
        wire(
            &mut peer,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(9),
                surface: Surface::Organization,
                payload: json!({ "op": "set_name", "value": "Erst ja" }),
            },
        );
        peer.cmd_approve(ProposalId(9)).expect("approve signs");
        let p = peer.proposals.get(&9).cloned().expect("card");
        assert!(peer.view(9, &p).approved_by_me, "the signature is collected");
        peer.cmd_decline(ProposalId(9)).expect("the withdrawal path stays open");
        let p = peer.proposals.get(&9).cloned().expect("card");
        let v = peer.view(9, &p);
        assert!(v.declined_by_me, "the stance the frontend grays on");
        // D2 (last vote counts): the decline RETRACTED the collected
        // signature — one member holds one stance, never both
        assert!(
            !peer
                .pending_sigs
                .get(&9)
                .is_some_and(|s| s.sigs.iter().any(|a| a.member == "walter")),
            "the own signature is retracted by the decline"
        );
        assert!(!v.approved_by_me, "…and the view says so");
    }

    /// L3 headline: `receive_checkpoint_proposal` ran `id + 1` BEFORE any
    /// guard — with overflow-checks + panic=abort in release, one hostile
    /// frame from any roster peer ABORTED the process. And every wire
    /// receive fn bumped the mint counter before its guards, so an
    /// in-window absurd id poisoned every later local mint.
    #[test]
    fn implausible_wire_ids_neither_abort_nor_poison_the_mint() {
        let b = Builder::new(&["petra", "walter"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let before = walter.next_id;
        // the former one-frame remote abort
        walter.receive_checkpoint_proposal(u64::MAX, 0, "00");
        assert_eq!(walter.next_id, before, "no mint poison from the cut");
        // the surface twin
        assert!(!walter.receive_proposed(
            u64::MAX,
            Surface::Memory,
            json!({ "op": "add_note" }),
            "petra"
        ));
        assert_eq!(walter.next_id, before, "no mint poison from a proposal");
        // …and the membership twin
        walter.receive_membership_proposal(
            u64::MAX,
            MembershipOp::Restored,
            "petra",
            &b.pk("petra"),
            None,
            Vec::new(),
            None,
        );
        assert_eq!(walter.next_id, before, "no mint poison from membership");
    }

    /// L3: signatures collect only for ROSTER members — dedup is by the
    /// free-form member string, so distinct fake names grew one Vec
    /// without bound (~96 KiB of wire per entry).
    #[test]
    fn approvals_from_non_members_never_enter_the_pending_set() {
        let b = Builder::new(&["petra", "walter"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        wire(
            &mut walter,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(1),
                surface: Surface::Memory,
                payload: json!({ "op": "add_note", "id": 1 }),
            },
        );
        for i in 0..100u64 {
            walter.receive_approval(1, &format!("ghost{i}"), 1, "ff");
        }
        assert!(
            walter.pending_sigs.get(&1).map_or(0, |p| p.sigs.len()) <= 2,
            "ghost names must not grow the set"
        );
    }

    /// L3: ONE cut per head is registered and co-signed — the identical
    /// (upto, state_hash) under fresh ids minted one registry entry plus
    /// one signed Approved per frame (a 1:1 outbound amplifier).
    #[test]
    fn only_one_cut_per_head_registers() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let b = Builder::new(&["petra", "walter"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let ours = checkpoint_state_hash(
            &walter.own_checkpoint_state(0).expect("own projection"),
        );
        for id in 50..60u64 {
            walter.receive_checkpoint_proposal(id, 0, &ours);
        }
        let cuts = walter
            .proposal_changes
            .values()
            .filter(|c| matches!(c, ChainChange::Checkpoint { .. }))
            .count();
        assert_eq!(cuts, 1, "the first id IS the cut for this head");
    }

    /// L3: the future-block buffer holds only heights the drain could ever
    /// reach and stays size-capped — an unverified far-future block was
    /// buffered forever (and one such block froze auto-compaction).
    #[test]
    fn a_far_future_block_is_refused_not_buffered() {
        let b = Builder::new(&["petra", "walter"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let junk = |height: u64| ChainBlock {
            height,
            prev: "00".to_string(),
            change: ChainChange::Applied {
                proposal_id: height,
                surface: Surface::Memory,
                payload: json!({ "op": "add_note" }),
            },
            sigs: Vec::new(),
        };
        walter.receive_block(junk(u64::MAX / 2));
        assert!(
            walter.pending_blocks.is_empty(),
            "a block far past the head never buffers"
        );
        walter.receive_block(junk(3));
        assert_eq!(walter.pending_blocks.len(), 1, "a near gap buffers for the drain");
    }

    /// L3: a flooding proposer crowds only ITSELF — the newest own card is
    /// refused at the cap, another member's card still lands.
    #[test]
    fn a_wire_proposal_flood_is_bounded_per_proposer() {
        let b = Builder::new(&["petra", "walter"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        for i in 0..200u64 {
            walter.receive_proposed(
                100 + i,
                Surface::Memory,
                json!({ "op": "add_note", "i": i }),
                "petra",
            );
        }
        let open_petra = walter
            .proposals
            .values()
            .filter(|p| p.state == ProposalState::Proposed && p.by == "petra")
            .count();
        assert_eq!(open_petra, OPEN_CARDS_PER_PROPOSER_MAX, "the cap holds");
        assert!(
            walter.receive_proposed(
                900,
                Surface::Memory,
                json!({ "op": "add_note" }),
                "walter"
            ),
            "another member's honest card still lands"
        );
    }

    /// L2: the DISPLAYED approval count and pills read only signatures
    /// that VERIFY — a peer gossiping junk must not inflate progress or
    /// paint a forged stance onto a named seat. Sealing was always safe
    /// (`try_commit` filters); this pins the display.
    #[test]
    fn an_unverifiable_approval_is_not_displayed_as_consent() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let b = Builder::new(&["petra", "walter"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let payload = json!({ "op": "add_note", "id": 1 });
        wire(
            &mut walter,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(1),
                surface: Surface::Memory,
                payload: payload.clone(),
            },
        );
        // junk: parses as no valid signature over the approval bytes
        walter.receive_approval(1, "petra", 1, "deadbeef");
        assert_eq!(walter.chain_approval_count(1), 0, "junk shows no progress");
        let p = walter.proposals.get(&1).cloned().expect("card");
        let v = walter.view(1, &p);
        assert_eq!(v.approvals, 0);
        let petra_row = v
            .votes
            .iter()
            .find(|mv| mv.member == "petra")
            .map(|mv| mv.vote)
            .expect("row");
        assert_eq!(petra_row, molt_core::VoteState::Open, "no forged pill");
        // …the genuine signature counts, and the vote still seals (liveness)
        let change = ChainChange::Applied {
            proposal_id: 1,
            surface: Surface::Memory,
            payload: payload.clone(),
        };
        let bytes = approval_bytes(&b.republic_id, 1, &change);
        walter.receive_approval(1, "petra", 1, &identity_sign(b.key("petra"), &bytes));
        assert_eq!(walter.chain_approval_count(1), 1, "the genuine one displays");
        walter.chain_sign_and_gossip_approval(1);
        assert_eq!(
            walter.chain_head.as_ref().expect("head").height,
            1,
            "verification costs no liveness - the block seals"
        );
    }

    /// **A wire membership proposal passes its gates BEFORE it is recorded.**
    /// Recording first persisted a phantom card per frame and let one
    /// `id = u64::MAX - 1` set `next_id = u64::MAX` on every node — after
    /// which every further proposal in the republic silently vanished
    /// (review 2026-08-25, HIGH).
    #[test]
    fn a_membership_proposal_with_an_implausible_id_is_not_recorded() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let next_before = walter.next_id;
        let hostile = u64::MAX - 1;
        wire(
            &mut walter,
            "petra",
            1,
            WorkspaceEvent::MembershipProposed {
                id: ProposalId(hostile),
                op: MembershipOp::Restored,
                member: "dora".to_string(),
                identity_pk: b.pk("dora"),
                nostr_pk: None,
                relays: Vec::new(),
                consent: None,
            },
        );
        assert_eq!(walter.next_id, next_before, "next_id is not poisoned");
        assert!(!walter.proposals.contains_key(&hostile), "no phantom card");
        assert!(!walter.proposal_changes.contains_key(&hostile), "nothing registered");
    }

    /// **A forged approval under THIS node's name is never re-signed.**
    ///
    /// Review 2026-08-25 (CRITICAL): "this node approved X" was inferred
    /// from the wire-collected set, which any member fills with junk under
    /// any roster name. At the next re-base the node then signed X with its
    /// REAL key — a threshold bypass by one insider, no human decision.
    /// The decision register is local: only `cmd_approve`'s own signing
    /// path writes it.
    #[test]
    fn a_forged_own_approval_is_not_re_signed_at_the_rebase() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let hostile = json!({ "op": "add_note", "id": 1 });
        wire(
            &mut walter,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(1),
                surface: Surface::Memory,
                payload: hostile.clone(),
            },
        );
        // petra gossips a junk approval UNDER WALTER'S NAME
        walter.receive_approval(1, "walter", 1, "deadbeef");
        // an unrelated block seals at height 1 (petra + dora) — the re-base
        // sweeps every pending set at the old height
        wire(
            &mut walter,
            "petra",
            2,
            WorkspaceEvent::Proposed {
                id: ProposalId(2),
                surface: Surface::Memory,
                payload: json!({ "op": "add_note", "id": 2 }),
            },
        );
        b.commit_applied(2, &["petra", "dora"]);
        walter.receive_block(b.blocks[1].clone());
        assert_eq!(walter.chain_head.as_ref().expect("head").height, 1);
        let mine = walter
            .pending_sigs
            .get(&1)
            .is_some_and(|p| p.sigs.iter().any(|a| a.member == "walter"));
        assert!(!mine, "walter never decided on #1 - the re-base must not sign it");
        assert_eq!(walter.chain_approval_count(1), 0, "no forged progress");
    }

    /// **A retracted approval is not re-signed at the re-base.** D2: a
    /// decline retracts this member's signature; the decision register
    /// must forget it too, or the next block puts the signature straight
    /// back while the member is listed as a decliner.
    #[test]
    fn a_declined_own_approval_is_not_re_signed_at_the_rebase() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let mut b = Builder::new(&["petra", "walter", "dora", "eve", "finn"], 3);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        wire(
            &mut walter,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(1),
                surface: Surface::Memory,
                payload: json!({ "op": "add_note", "id": 1 }),
            },
        );
        walter.cmd_approve(ProposalId(1)).expect("walter approves");
        assert!(walter.own_approvals.contains(&1));
        walter.cmd_decline(ProposalId(1)).expect("…then retracts");
        assert!(!walter.own_approvals.contains(&1), "the register forgets");
        wire(
            &mut walter,
            "petra",
            2,
            WorkspaceEvent::Proposed {
                id: ProposalId(2),
                surface: Surface::Memory,
                payload: json!({ "op": "add_note", "id": 2 }),
            },
        );
        b.commit_applied(2, &["petra", "dora", "eve"]);
        walter.receive_block(b.blocks[1].clone());
        let mine = walter
            .pending_sigs
            .get(&1)
            .is_some_and(|p| p.sigs.iter().any(|a| a.member == "walter"));
        assert!(!mine, "a retracted approval must not come back at the re-base");
        assert_eq!(walter.chain_approval_count(1), 0);
    }

    /// The decision register is ephemeral; an own `Approved` replayed from
    /// the log (a restart) rebuilds it, an own `Declined` clears it.
    #[test]
    fn the_own_log_rebuilds_the_decision_register() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut walter = chain_peer("walter", &b, b.blocks.clone());
        walter.apply(&molt_core::EventEnvelope {
            prev_seq: 0,
            seq: 1,
            ts: 1,
            by: "petra".to_string(),
            body: WorkspaceEvent::Proposed {
                id: ProposalId(1),
                surface: Surface::Memory,
                payload: json!({ "op": "add_note", "id": 1 }),
            },
        });
        walter.apply(&molt_core::EventEnvelope {
            prev_seq: 0,
            seq: 2,
            ts: 2,
            by: "walter".to_string(),
            body: WorkspaceEvent::Approved {
                id: ProposalId(1),
                by: "walter".to_string(),
                height: 1,
                sig: "irrelevant-for-the-register".to_string(),
            },
        });
        assert!(walter.own_approvals.contains(&1), "an own Approved rebuilds it");
        walter.apply(&molt_core::EventEnvelope {
            prev_seq: 0,
            seq: 3,
            ts: 3,
            by: "walter".to_string(),
            body: WorkspaceEvent::Declined {
                id: ProposalId(1),
                by: "walter".to_string(),
                hash: String::new(),
            },
        });
        assert!(!walter.own_approvals.contains(&1), "an own Declined clears it");
    }

    /// **Junk never evicts a verified signature.** "Latest wins" let one
    /// insider replace every member's genuine approval with garbage at the
    /// same height — a vote that never reaches m anywhere (review
    /// 2026-08-25, HIGH).
    #[test]
    fn junk_does_not_evict_a_verified_signature() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let payload = json!({ "op": "add_note", "id": 1 });
        wire(
            &mut walter,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(1),
                surface: Surface::Memory,
                payload: payload.clone(),
            },
        );
        let change = ChainChange::Applied {
            proposal_id: 1,
            surface: Surface::Memory,
            payload,
        };
        let bytes = approval_bytes(&b.republic_id, 1, &change);
        let genuine = identity_sign(b.key("petra"), &bytes);
        walter.receive_approval(1, "petra", 1, &genuine);
        assert_eq!(walter.chain_approval_count(1), 1);
        // dora (or anyone) gossips junk under petra's name at the same height
        walter.receive_approval(1, "petra", 1, "deadbeef");
        assert_eq!(walter.chain_approval_count(1), 1, "the genuine one stands");
        let kept = walter
            .pending_sigs
            .get(&1)
            .and_then(|p| p.sigs.iter().find(|a| a.member == "petra"))
            .map(|a| a.sig.clone());
        assert_eq!(kept.as_deref(), Some(genuine.as_str()));
    }

    /// L2 liveness twin: an approval that OUTRAN its card is collected but
    /// not displayed, and becomes displayable the moment the card lands —
    /// the naive drop-on-unverifiable fix would wedge gossip ordering.
    #[test]
    fn an_approval_that_outran_its_card_counts_once_the_card_lands() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let payload = json!({ "op": "set_name", "value": "Early" });
        let change = ChainChange::Applied {
            proposal_id: 1,
            surface: Surface::Organization,
            payload: payload.clone(),
        };
        let bytes = approval_bytes(&b.republic_id, 1, &change);
        walter.receive_approval(1, "petra", 1, &identity_sign(b.key("petra"), &bytes));
        assert_eq!(
            walter.chain_approval_count(1),
            0,
            "not verifiable yet - the card has not landed"
        );
        wire(
            &mut walter,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(1),
                surface: Surface::Organization,
                payload,
            },
        );
        assert_eq!(
            walter.chain_approval_count(1),
            1,
            "the card landed - the collected signature displays"
        );
    }

    /// D2: `try_commit` excludes CURRENT decliners — a stale re-served
    /// signature of a member whose standing decline this node holds must
    /// not count toward m, or a majority-declined proposal seals on
    /// whichever node collected the leftovers.
    #[test]
    fn a_current_decliners_stale_signature_does_not_seal() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let payload = json!({ "op": "set_name", "value": "Contested" });
        wire(
            &mut walter,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(1),
                surface: Surface::Organization,
                payload: payload.clone(),
            },
        );
        // dora's decline stands…
        wire(
            &mut walter,
            "dora",
            1,
            WorkspaceEvent::Declined { id: ProposalId(1), by: "dora".to_string(), hash: String::new() },
        );
        // …then her STALE signature arrives (re-served by a peer that
        // missed the decline) and collects
        let change = ChainChange::Applied {
            proposal_id: 1,
            surface: Surface::Organization,
            payload: payload.clone(),
        };
        let bytes = approval_bytes(&b.republic_id, 1, &change);
        let dora_sig = identity_sign(b.key("dora"), &bytes);
        walter.receive_approval(1, "dora", 1, &dora_sig);
        // walter co-signs: 2 collected — but dora is a CURRENT decliner
        walter.chain_sign_and_gossip_approval(1);
        assert_eq!(
            walter.chain_head.as_ref().expect("head").height,
            0,
            "no block seals while a counted signer's decline stands"
        );
        assert!(
            matches!(walter.proposals.get(&1), Some(p) if p.state == ProposalState::Proposed),
            "the card stays open"
        );
    }

    /// D2 (last vote counts, decided 2026-08-16): approving over the own
    /// standing decline RETRACTS the decline — the newest stance wins,
    /// mirroring the decline's signature retraction. One member, one
    /// stance, changeable until the vote seals.
    #[test]
    fn an_approve_retracts_the_standing_own_decline() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut peer = chain_peer_3("walter", &b);
        wire(
            &mut peer,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(9),
                surface: Surface::Organization,
                payload: json!({ "op": "set_name", "value": "Beides?" }),
            },
        );
        peer.identity_sk = b
            .keys
            .iter()
            .find(|(m, _)| m == "walter")
            .map(|(_, sk)| sk.clone());
        peer.cmd_decline(ProposalId(9)).expect("decline");
        peer.cmd_approve(ProposalId(9)).expect("the newest stance wins");
        let p = peer.proposals.get(&9).cloned().expect("card");
        let v = peer.view(9, &p);
        assert!(!v.declined_by_me, "the decline is retracted");
        assert!(v.approved_by_me, "…and the approval stands");
        assert!(
            !p.decliners.iter().any(|d| d == "walter"),
            "the decliner list no longer names the member"
        );
    }

    /// An applied MEMBERSHIP card reads its voters from the sealed block
    /// too — matched by content (op + member), since a Membership block
    /// carries no proposal id (review 2026-08-09, finding 7).
    #[test]
    fn an_applied_membership_card_reports_the_block_signers() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        let block = b.seal(
            1,
            ChainChange::Membership {
                op: MembershipOp::Restored,
                member: "dora".to_string(),
                identity_pk: b
                    .keys
                    .iter()
                    .find(|(m, _)| m == "dora")
                    .map(|(_, sk)| hex::encode(sk.verifying_key().to_bytes()))
                    .expect("dora's key"),
                nostr_pk: None,
                relays: Vec::new(),
                consent: None,
            },
            &["petra", "walter"],
        );
        b.push(block);
        let mut peer = chain_peer_3("walter", &b);
        assert_eq!(
            peer.chain_head.as_ref().map(|h| h.height),
            Some(1),
            "the membership block adopted"
        );
        peer.proposals.insert(
            4,
            molt_core::ProposalRecord {
                surface: Surface::Organization,
                payload: json!({ "op": "restore_member", "member": "dora" }),
                approvals: 0,
                state: ProposalState::Applied,
                declined_at: 0,
                declined_by: String::new(),
                decliners: Vec::new(),
                voted: Vec::new(),
                by: String::new(),
                superseded: false,
                withdrawn: false,
            },
        );
        let p = peer.proposals.get(&4).cloned().expect("card");
        let v = peer.view(4, &p);
        assert_eq!(v.approvals, 2, "the membership block's signature count");
    }

    /// An APPLIED card keeps naming its voters: the sealed block carries
    /// the signatures (the ephemeral collection is cleared at commit), so
    /// the view reads them from the chain — live incident 2026-08-09,
    /// defect 7: the applied history showed "0 approvals, every pill open".
    #[test]
    fn an_over_subscribed_voter_still_reads_approved_on_the_applied_card() {
        // D6: try_commit seals only the m lowest-named signatures (chain
        // truth), but a voter whose signature fell off the block must not
        // read Open on a vote they cast — the collected set survives as
        // record-side DISPLAY data at the seal.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut peer = chain_signer("walter", &b, b.blocks.clone());
        wire(
            &mut peer,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(1),
                surface: Surface::Memory,
                payload: json!({ "op": "add_note", "id": 1 }),
            },
        );
        peer.cmd_approve(ProposalId(1)).expect("walter signs - 1 of 2 locally");
        // the block seals from the other side, signed by petra and dora
        b.commit_applied(1, &["petra", "dora"]);
        peer.receive_block(b.blocks[1].clone());
        let p = peer.proposals.get(&1).cloned().expect("card");
        assert_eq!(p.state, ProposalState::Applied);
        let v = peer.view(1, &p);
        assert_eq!(v.approvals, 2, "the chain-proven count stays the block's");
        assert!(v.approved_by_me, "walter cast a vote and must see it");
        let walter_row = v
            .votes
            .iter()
            .find(|mv| mv.member == "walter")
            .map(|mv| mv.vote)
            .expect("roster row");
        assert_eq!(
            walter_row,
            molt_core::VoteState::Approved,
            "the over-subscribed voter's pill"
        );
        // a post-seal approval must feed the display, never resurrect the
        // ephemeral collection on a terminal card
        wire(
            &mut peer,
            "dora",
            1,
            WorkspaceEvent::Approved {
                id: ProposalId(1),
                by: "dora".to_string(),
                height: 1,
                sig: "ff".to_string(),
            },
        );
        assert!(
            !peer.pending_sigs.contains_key(&1),
            "a terminal card collects no pending signatures"
        );
    }

    #[test]
    fn an_applied_card_reports_the_block_signers() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut peer = chain_peer_3("walter", &b);
        // the card arrives as gossip; the sealed block (signed by petra and
        // dora) commits it — walter himself never collected a signature
        wire(
            &mut peer,
            "petra",
            1,
            WorkspaceEvent::Proposed {
                id: ProposalId(1),
                surface: Surface::Memory,
                payload: json!({ "op": "add_note", "id": 1 }),
            },
        );
        b.commit_applied(1, &["petra", "dora"]);
        peer.receive_block(b.blocks[1].clone());
        let p = peer.proposals.get(&1).cloned().expect("card");
        assert_eq!(p.state, ProposalState::Applied);
        let v = peer.view(1, &p);
        assert_eq!(v.approvals, 2, "the block's signature count");
        let vote_of = |m: &str| {
            v.votes
                .iter()
                .find(|mv| mv.member == m)
                .map(|mv| mv.vote)
                .expect("roster row")
        };
        assert_eq!(vote_of("petra"), molt_core::VoteState::Approved);
        assert_eq!(vote_of("dora"), molt_core::VoteState::Approved);
        assert_eq!(vote_of("walter"), molt_core::VoteState::Open);
        assert!(!v.approved_by_me, "walter did not sign");
        // the read contract serves the applied proposals too (the Accepted
        // table renders from the snapshot, co-equal for every frontend)
        let snap = peer.snapshot(Surface::Memory, None, None);
        assert_eq!(snap.accepted.len(), 1, "the applied card is in the snapshot");
        assert_eq!(snap.accepted[0].id, ProposalId(1));
        assert_eq!(snap.accepted[0].approvals, 2, "with its block-sourced voters");
    }

    /// WP4b stage 2, full holders: a committed checkpoint block verifies
    /// only when its `state_hash` matches THIS chain's own recomputed
    /// projection — a forged or drifted summary is hard-rejected with the
    /// whole chain (all-or-nothing, like every other violation).
    #[test]
    fn a_checkpoint_block_verifies_against_the_own_projection() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        b.commit_applied(2, &["walter", "dora"]);
        let state = checkpoint_state(&b.blocks, 2).expect("state@2");
        let good = b.seal(
            3,
            ChainChange::Checkpoint {
                upto: 2,
                state_hash: checkpoint_state_hash(&state),
            },
            &["petra", "walter"],
        );
        let mut chain = b.blocks.clone();
        chain.push(good.clone());
        let head = verify_chain(&chain).expect("a truthful checkpoint verifies");
        assert_eq!(head.height, 3);
        // a forged state hash is rejected with the whole chain
        let forged = b.seal(
            3,
            ChainChange::Checkpoint {
                upto: 2,
                state_hash: molt_storage::content_hash(b"not the projection"),
            },
            &["petra", "walter"],
        );
        let mut bad = b.blocks.clone();
        bad.push(forged);
        assert!(verify_chain(&bad).is_err(), "a forged checkpoint kills the chain");
        // upto must precede the block height
        let self_ref = b.seal(
            3,
            ChainChange::Checkpoint {
                upto: 3,
                state_hash: checkpoint_state_hash(&state),
            },
            &["petra", "walter"],
        );
        let mut bad = b.blocks.clone();
        bad.push(self_ref);
        assert!(verify_chain(&bad).is_err(), "upto >= height is structural nonsense");
    }

    /// WP4b stage 2, suffix holders: a chain that BEGINS with a checkpoint
    /// verifies from the blob as trust anchor — blob hash, founding
    /// recomputation (forgery check without the genesis), current-roster
    /// threshold on the anchor block, double-apply seeded across the cut.
    #[test]
    fn a_suffix_chain_bootstraps_from_a_checkpoint() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        b.commit_applied(2, &["walter", "dora"]);
        let blob = checkpoint_state(&b.blocks, 2).expect("state@2");
        let anchor = b.seal(
            3,
            ChainChange::Checkpoint {
                upto: 2,
                state_hash: checkpoint_state_hash(&blob),
            },
            &["petra", "walter"],
        );
        b.push(anchor.clone());
        // one more applied block on top — the suffix a dropped-history
        // holder keeps
        b.commit_applied(7, &["petra", "dora"]);
        let suffix: Vec<ChainBlock> = b.blocks[3..].to_vec();
        assert_eq!(suffix.len(), 2, "anchor + one applied block");

        let head = verify_suffix_chain(&blob, &suffix, &b.republic_id)
            .expect("the suffix verifies from the checkpoint anchor");
        assert_eq!(head.height, 4);
        assert_eq!(head.identities.len(), 3, "roster comes from the blob");

        // a doctored blob (foreign roster key) fails the hash check
        let mut forged = blob.clone();
        forged.roster[0].identity_pk = "00".repeat(32);
        assert!(
            verify_suffix_chain(&forged, &suffix, &b.republic_id).is_err(),
            "a doctored roster no longer hashes to the signed state"
        );
        // …and its nostr_pk twin: under checkpoint-v2 the third anchor is
        // inside the hashed bytes, so a swapped roster transport anchor is
        // caught exactly like a swapped identity key (under v1 it was NOT
        // hashed — a served blob's roster anchor was silently mutable)
        let mut forged_npk = blob.clone();
        forged_npk.roster[0].nostr_pk = "ee".repeat(32);
        assert!(
            verify_suffix_chain(&forged_npk, &suffix, &b.republic_id).is_err(),
            "a doctored roster nostr anchor no longer hashes to the signed state"
        );
        // a wholly self-consistent forged blob still fails the founding
        // recomputation against the expected republic id
        let mut alien = blob.clone();
        alien.founding_name = "Fake Club".to_string();
        let alien_anchor_hash = checkpoint_state_hash(&alien);
        let mut alien_suffix = suffix.clone();
        if let ChainChange::Checkpoint { state_hash, .. } = &mut alien_suffix[0].change {
            *state_hash = alien_anchor_hash;
        }
        assert!(
            verify_suffix_chain(&alien, &alien_suffix, &b.republic_id).is_err(),
            "a forged founding does not recompute to the real republic id"
        );
        // double-apply across the cut: proposal 1 was consumed below upto
        let mut replay = b.clone();
        replay.commit_applied(1, &["petra", "walter"]);
        let replay_suffix: Vec<ChainBlock> = replay.blocks[3..].to_vec();
        assert!(
            verify_suffix_chain(&blob, &replay_suffix, &b.republic_id).is_err(),
            "an id consumed below the cut cannot re-apply in the suffix"
        );
        // below-threshold anchor signatures are refused
        let weak_anchor = b.seal(
            3,
            ChainChange::Checkpoint {
                upto: 2,
                state_hash: checkpoint_state_hash(&blob),
            },
            &["petra"],
        );
        assert!(
            verify_suffix_chain(&blob, &[weak_anchor], &b.republic_id).is_err(),
            "one signature is not a threshold"
        );
    }

    /// WP4b stage 3: the propose flow end to end at the state level.
    /// Petra proposes the cut (self-cosign = 1 of 2); Walter receives the
    /// gossip, RECOMPUTES the hash from his own chain, auto-co-signs on
    /// the match, and the checkpoint block seals at 2-of-2 — on both
    /// nodes, byte-identically. A mismatched hash is never signed; a
    /// stale cut dies on re-base instead of sealing an invalid block.
    #[test]
    fn a_checkpoint_proposal_seals_via_verify_before_sign() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        let mut walter = chain_signer("walter", &b, b.blocks.clone());

        let id = match petra.cmd_propose_checkpoint().expect("propose") {
            molt_core::Reply::Proposed { id } => id.0,
            other => panic!("unexpected: {other:?}"),
        };
        assert_eq!(
            petra.pending_sigs.get(&id).map(|p| p.sigs.len()),
            Some(1),
            "the proposer co-signed its own cut"
        );
        let (upto, state_hash) = match petra.proposal_changes.get(&id) {
            Some(ChainChange::Checkpoint { upto, state_hash }) => {
                (*upto, state_hash.clone())
            }
            other => panic!("unexpected change: {other:?}"),
        };
        assert_eq!(upto, 1, "the cut is the current head (B-F1)");

        // a WRONG hash is refused: nothing registered, nothing signed
        walter.receive_checkpoint_proposal(id, upto, "00");
        assert!(!walter.proposal_changes.contains_key(&id));
        assert!(!walter.pending_sigs.contains_key(&id));

        // the truthful gossip: walter recomputes, matches, auto-co-signs
        walter.receive_checkpoint_proposal(id, upto, &state_hash);
        let petra_sig = petra
            .pending_sigs
            .get(&id)
            .expect("petra's set")
            .sigs
            .first()
            .expect("petra signed")
            .sig
            .clone();
        walter.receive_approval(id, "petra", 2, &petra_sig);
        assert_eq!(
            walter.chain_head.as_ref().expect("head").height,
            2,
            "the checkpoint sealed at 2-of-2 on walter"
        );
        assert!(matches!(
            walter.chain.last().expect("block").change,
            ChainChange::Checkpoint { .. }
        ));
        // petra converges from walter's signature the same way
        let walter_sig = walter
            .chain
            .last()
            .expect("block")
            .sigs
            .iter()
            .find(|a| a.member == "walter")
            .expect("walter signed")
            .sig
            .clone();
        petra.receive_approval(id, "walter", 2, &walter_sig);
        assert_eq!(petra.chain_head.as_ref().expect("head").height, 2);
        assert_eq!(
            block_hash(&b.republic_id, petra.chain.last().expect("b")),
            block_hash(&b.republic_id, walter.chain.last().expect("b")),
            "both nodes sealed the byte-identical checkpoint block"
        );
        // the sealed proposal's bookkeeping is gone on both
        assert!(!petra.proposal_changes.contains_key(&id));
        assert!(!walter.proposal_changes.contains_key(&id));
    }

    /// WP4b stage 4: sealing a checkpoint DROPS the summarized history
    /// locally (B-F2), the blob becomes the trust anchor, pre-cut applied
    /// entries stay readable, and the pruned holder keeps verifying and
    /// extending its suffix chain — including a reopen-style re-adopt.
    #[test]
    fn a_sealed_checkpoint_drops_history_and_the_holder_keeps_governing() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        // the propose flow seals the cut at 2-of-2 (stage-3 mechanics)
        let hash = checkpoint_state_hash(&checkpoint_state(&b.blocks, 1).expect("state"));
        walter.receive_checkpoint_proposal(40, 1, &hash);
        let change = ChainChange::Checkpoint { upto: 1, state_hash: hash };
        let bytes = approval_bytes(&b.republic_id, 2, &change);
        let petra_sig = identity_sign(b.key("petra"), &bytes);
        walter.receive_approval(40, "petra", 2, &petra_sig);
        // sealed AND pruned: only the anchor remains, the blob anchors
        assert_eq!(walter.chain_head.as_ref().expect("head").height, 2);
        assert_eq!(walter.chain.len(), 1, "history below the cut is dropped");
        assert!(matches!(
            walter.chain.first().expect("anchor").change,
            ChainChange::Checkpoint { .. }
        ));
        let blob = walter.checkpoint_blob.clone().expect("blob anchors the holder");
        assert_eq!(blob.upto, 1);
        // pre-cut applied entries survive in the read projection
        let mem = walter.chain_applied.get(&Surface::Memory).expect("projection");
        assert_eq!(mem.len(), 1, "the pre-cut applied entry stays readable");
        // the pruned holder keeps governing: a fresh applied change seals
        // on top of the suffix (verify runs the suffix rules)
        let payload = json!({"op": "add_note", "title": "post-cut"});
        walter.receive_proposed(41, Surface::Memory, payload.clone(), "peer");
        let post = ChainChange::Applied {
            proposal_id: 41,
            surface: Surface::Memory,
            payload,
        };
        let bytes = approval_bytes(&b.republic_id, 3, &post);
        let petra_sig = identity_sign(b.key("petra"), &bytes);
        walter.receive_approval(41, "petra", 3, &petra_sig);
        walter.chain_sign_and_gossip_approval(41);
        assert_eq!(
            walter.chain_head.as_ref().expect("head").height,
            3,
            "the pruned holder extends its suffix"
        );
        assert_eq!(
            walter.chain_applied.get(&Surface::Memory).map(|v| v.len()),
            Some(2),
            "pre- and post-cut entries read together"
        );
        // reopen-style: a fresh holder re-anchors on blob + suffix
        let mut reopened = chain_peer("walter", &b, b.blocks.clone());
        reopened.checkpoint_blob = Some(blob);
        reopened.adopt_chain(walter.chain.clone());
        assert_eq!(
            reopened.chain_head.as_ref().expect("head").height,
            3,
            "a pruned chain re-adopts from the persisted blob"
        );
        assert_eq!(
            reopened.chain_applied.get(&Surface::Memory).map(|v| v.len()),
            Some(2)
        );
        // …and the Accepted cards match an unpruned holder's (review
        // 2026-08-16): the pre-cut card materializes from the blob's
        // summarized payloads — voter pills open, the sigs went with the
        // cut — the post-cut card from its live block, voters proven
        let snap = reopened.snapshot(Surface::Memory, None, None);
        let card = |pid: u64| {
            snap.accepted
                .iter()
                .find(|c| c.id.0 == pid)
                .unwrap_or_else(|| panic!("card {pid}"))
        };
        assert_eq!(card(1).approvals, 0, "pre-cut: only chain-provable votes show");
        assert!(
            card(41)
                .votes
                .iter()
                .any(|v| v.vote == molt_core::VoteState::Approved),
            "post-cut: the live block's signers show"
        );
    }

    /// A reopen replays the proposal CARDS from the persisted gossip first
    /// and adopts the chain second (`open_stored_workspace`) — adoption must
    /// settle every card the chain already consumed, or each restart
    /// resurrects decided votes as open cards (live incident 2026-08-09: a
    /// sealed `set_relays` vote came back 'proposed' on every launch of the
    /// proposer's node, its `restore_member` twins with it).
    #[test]
    fn adopting_a_chain_settles_replayed_proposal_cards() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        let genesis = b.blocks.clone();
        b.commit_applied(7, &["petra", "walter"]);
        b.commit_restored("petra", &"ab".repeat(32), &["petra", "walter"]);

        // the reopen shape: gossip-replayed cards, THEN the chain
        let mut reopened = chain_peer("walter", &b, genesis);
        reopened.receive_proposed(7, Surface::Memory, json!({ "op": "add_note", "id": 7 }), "peer");
        reopened.receive_proposed(
            8,
            Surface::Organization,
            json!({ "op": "restore_member", "member": "petra" }),
            "peer",
        );
        reopened.adopt_chain(b.blocks.clone());

        let card = reopened.proposals.get(&7).expect("card survives");
        assert_eq!(card.state, ProposalState::Applied, "the chain consumed id 7");
        let restore = reopened.proposals.get(&8).expect("restore card survives");
        assert_eq!(
            restore.state,
            ProposalState::Applied,
            "the Restored block settles the membership card"
        );

        // the LIVE twin: a late (resent) Proposed for a consumed id must not
        // re-open a card. Adoption already materialized the APPLIED record
        // from the block (ensure_applied_record) — the resend must neither
        // create a second one nor flip it back to open.
        let mut live = chain_peer("walter", &b, b.blocks.clone());
        assert!(
            !live.receive_proposed(7, Surface::Memory, json!({ "op": "add_note", "id": 7 }), "peer"),
            "a consumed id must not open a fresh card"
        );
        assert_eq!(
            live.proposals.get(&7).map(|p| p.state),
            Some(ProposalState::Applied),
            "the consumed id stays a settled, chain-proven card"
        );
    }

    /// **The `seen` trap.** Once a checkpoint drops the history below the cut,
    /// the double-apply guard can no longer be read off `self.chain`: the
    /// blocks carrying those proposal ids are gone. It has to come from the
    /// blob's `consumed_ids` — which is exactly what a walk state carried
    /// across the prune, or rebuilt from the surviving blocks, gets wrong.
    ///
    /// `verify_suffix_chain` seeds it correctly today
    /// (`a_suffix_chain_bootstraps_from_a_checkpoint`), and this is the
    /// ENGINE-level twin: the guard must survive the prune on a live holder,
    /// which is the property an incremental verifier has to preserve.
    #[test]
    fn an_id_consumed_below_the_cut_cannot_replay_on_a_pruned_holder() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        b.commit_applied(2, &["petra", "walter"]);
        let pre_cut = b.blocks.clone();
        let blob = checkpoint_state(&b.blocks, 2).expect("state@2");
        let anchor = b.seal(
            3,
            ChainChange::Checkpoint {
                upto: 2,
                state_hash: checkpoint_state_hash(&blob),
            },
            &["petra", "walter"],
        );
        b.push(anchor.clone());

        let mut peer = chain_peer("walter", &b, pre_cut);
        peer.receive_block(anchor);
        assert!(peer.checkpoint_blob.is_some(), "the cut sealed and anchored");
        assert_eq!(peer.chain.len(), 1, "history below the cut is dropped");
        assert_eq!(peer.chain_head.as_ref().expect("head").height, 3);

        // proposal 1 was consumed at height 1 — a block this holder no longer
        // has. Re-offering it must still be refused.
        b.commit_applied(1, &["petra", "walter"]);
        let replay = b.blocks.last().expect("the replay block").clone();
        assert_eq!(replay.height, 4, "the replay sits on top of the anchor");
        peer.receive_block(replay);
        assert_eq!(
            peer.chain_head.as_ref().expect("head").height,
            3,
            "an id consumed below the cut cannot re-apply after the prune"
        );
        assert_eq!(peer.chain.len(), 1, "the refused block is not retained");

        // …while a FRESH id on the same suffix still extends it, so the test
        // cannot pass by the holder having stopped accepting anything
        b.blocks.pop();
        b.head_hash = block_hash(&b.republic_id, &peer.chain[0]);
        b.commit_applied(9, &["petra", "walter"]);
        peer.receive_block(b.blocks.last().expect("fresh block").clone());
        assert_eq!(
            peer.chain_head.as_ref().expect("head").height,
            4,
            "a fresh id extends the pruned holder"
        );
    }

    /// **The catch-up is linear.** Draining a buffered suffix used to verify
    /// the whole chain from the anchor for every block, and TWICE per block
    /// (a probe clone, then the append) — `2nN + m·N(N+1)` signature checks,
    /// all inside one uninterruptible actor turn. A node catching up then
    /// looked exactly like a dead one to its peers, which is what the
    /// delivery guarantee escalates on.
    ///
    /// Counted, not timed: the assertion is the point of the whole change.
    #[test]
    fn catching_up_verifies_each_block_once() {
        const N: usize = 40;
        let b = grown_chain(N + 1);
        let mut peer = chain_peer("walter", &b, b.blocks[..1].to_vec());

        // reverse order, so every block buffers and the whole suffix drains
        // in ONE turn — the shape a real catch-up has
        VERIFY_STEPS.with(|c| c.set(0));
        CHAIN_PERSISTS.with(|c| c.set(0));
        for block in b.blocks[1..].iter().rev() {
            peer.receive_block(block.clone());
        }
        let steps = VERIFY_STEPS.with(std::cell::Cell::get);
        let writes = CHAIN_PERSISTS.with(std::cell::Cell::get);

        assert_eq!(
            peer.chain_head.as_ref().expect("head").height,
            u64::try_from(N).expect("small chain"),
            "the whole suffix drained"
        );
        assert_eq!(
            steps, N,
            "each block is verified exactly once - a re-walk per block would \
             cost {} here, and 7M at N=1000",
            N * (N + 1)
        );
        assert_eq!(
            writes, 1,
            "the drained batch is written ONCE - the write blocks on the \
             storage writer's ack, so {N} of them would sit inside one turn"
        );
    }

    /// Batching the write must not turn "once per block" into "never".
    ///
    /// Every path that accepts a block ends in exactly one
    /// `persist_chain_now`; this is the guard for a future third caller that
    /// forgets, which would leave accepted blocks unwritten and silently
    /// re-fetched on every restart.
    #[test]
    fn an_accepted_block_is_written_once_and_a_refused_one_not_at_all() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        let genesis = b.blocks.clone();
        b.commit_applied(1, &["petra", "walter"]);
        let mut peer = chain_peer("walter", &b, genesis);

        CHAIN_PERSISTS.with(|c| c.set(0));
        peer.receive_block(b.blocks[1].clone());
        assert_eq!(peer.chain_head.as_ref().expect("head").height, 1);
        assert_eq!(
            CHAIN_PERSISTS.with(std::cell::Cell::get),
            1,
            "an accepted block is written"
        );

        let refused = b.seal(
            2,
            ChainChange::Applied {
                proposal_id: 2,
                surface: Surface::Memory,
                payload: json!({ "op": "add_note", "id": 2 }),
            },
            &["petra"],
        );
        CHAIN_PERSISTS.with(|c| c.set(0));
        peer.receive_block(refused);
        assert_eq!(peer.chain_head.as_ref().expect("head").height, 1);
        assert_eq!(
            CHAIN_PERSISTS.with(std::cell::Cell::get),
            0,
            "a refused block writes nothing"
        );
    }

    /// A **refused** block must leave the walk byte-identical, because the
    /// walk is now cached across calls.
    ///
    /// The order inside `verify_next` is what makes this sharp: the
    /// double-apply guard is consulted BEFORE the signatures are checked. An
    /// implementation that recorded the id while checking would let one
    /// unsigned block burn a proposal id forever — this holder would then
    /// refuse a block every other node accepts, which is a fork produced by
    /// bookkeeping alone.
    #[test]
    fn a_refused_block_does_not_poison_the_walk() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        let genesis = b.blocks.clone();
        let mut peer = chain_peer("walter", &b, genesis);

        // a well-formed block for proposal 7, signed by ONE of two — refused
        // at the threshold, but only after the guard has seen the id
        let change = ChainChange::Applied {
            proposal_id: 7,
            surface: Surface::Memory,
            payload: json!({ "op": "add_note", "id": 7 }),
        };
        peer.receive_block(b.seal(1, change, &["petra"]));
        assert_eq!(
            peer.chain_head.as_ref().expect("head").height,
            0,
            "a below-threshold block is refused"
        );

        // …and the legitimate block for the SAME proposal still lands
        b.commit_applied(7, &["petra", "walter"]);
        peer.receive_block(b.blocks[1].clone());
        assert_eq!(
            peer.chain_head.as_ref().expect("head").height,
            1,
            "a refused block must not burn its proposal id"
        );
    }

    /// The incrementally-folded projection must equal the whole-chain
    /// rebuild — including across a Membership block, where the anchors map
    /// and the roster move too.
    ///
    /// This is the property `project_one` trades a refold for; a full rebuild
    /// re-clones every payload in the chain per block, which made a drain of
    /// N blocks clone the applied log N²/2 times.
    #[test]
    fn the_appended_projection_equals_the_whole_chain_rebuild() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        let fresh = molt_net::nostr_identity(b"walter-recovered", "new-ticket").1;
        b.commit_restored("walter", &fresh, &["petra", "walter"]);
        b.commit_applied(2, &["petra", "walter"]);
        let mut peer = chain_peer("walter", &b, b.blocks[..1].to_vec());
        for block in b.blocks[1..].iter().rev() {
            peer.receive_block(block.clone());
        }
        assert_eq!(peer.chain.len(), 4, "the whole suffix drained");

        let incremental = (peer.chain_applied.clone(), peer.chain_anchors.clone());
        peer.apply_chain_to_state();
        assert_eq!(
            incremental,
            (peer.chain_applied.clone(), peer.chain_anchors.clone()),
            "the appended projection must equal the whole-chain rebuild"
        );
    }

    /// Extending incrementally must equal verifying from the anchor — at
    /// EVERY prefix, not just the end. The cached walk is only sound while
    /// that holds, and it is the property a second implementation would
    /// silently drift from.
    #[test]
    fn incremental_extension_equals_full_verification_at_every_prefix() {
        let b = grown_chain(12);
        let mut peer = chain_peer("walter", &b, b.blocks[..1].to_vec());
        for (i, block) in b.blocks[1..].iter().enumerate() {
            peer.receive_block(block.clone());
            let full = verify_chain(&b.blocks[..=i + 1]).expect("the prefix verifies in full");
            let cached = peer.chain_head.as_ref().expect("head");
            assert_eq!(cached.height, full.height);
            assert_eq!(cached.hash, full.hash, "prefix {} diverged", i + 1);
            assert_eq!(cached.identities, full.identities);
            assert!(
                peer.chain_walk
                    .as_ref()
                    .expect("the walk is kept")
                    .describes(&peer.chain, peer.checkpoint_blob.as_ref()),
                "the cached walk must describe the chain it was built on"
            );
        }
    }

    /// Grow a builder chain to exactly `len` blocks (genesis included).
    fn grown_chain(len: usize) -> Builder {
        let mut b = Builder::new(&["petra", "walter"], 2);
        for id in 0..len.saturating_sub(1) {
            b.commit_applied(u64::try_from(id + 100).expect("small id"), &["petra", "walter"]);
        }
        assert_eq!(b.blocks.len(), len);
        b
    }

    /// Drive one gated proposal through `s` to a sealed block: peer
    /// approval first, then the local co-sign seals at 2-of-2.
    fn seal_one(s: &mut crate::State, b: &Builder, peer: &str, id: u64) {
        let target = s.chain_head.as_ref().expect("head").height + 1;
        let payload = json!({"op": "add_note", "id": id});
        s.receive_proposed(id, Surface::Memory, payload.clone(), "peer");
        let change = ChainChange::Applied {
            proposal_id: id,
            surface: Surface::Memory,
            payload,
        };
        let bytes = approval_bytes(&b.republic_id, target, &change);
        let sig = identity_sign(b.key(peer), &bytes);
        s.receive_approval(id, peer, target, &sig);
        s.chain_sign_and_gossip_approval(id);
        assert_eq!(
            s.chain_head.as_ref().expect("head").height,
            target,
            "the driven proposal seals"
        );
    }

    /// The pending checkpoint cut registered in `s`, if any.
    fn pending_cut(s: &crate::State) -> Option<u64> {
        s.proposal_changes.values().find_map(|c| match c {
            ChainChange::Checkpoint { upto, .. } => Some(*upto),
            _ => None,
        })
    }

    /// WP4b automation: once the chain reaches the trigger length, the
    /// alphabetically LOWEST-named roster member auto-proposes the
    /// compaction cut right after a block commit (every co-signer is at
    /// the same head then) — and co-signs it like a manual propose. A
    /// non-lowest member never auto-proposes: one deterministic
    /// proposer, no node-local id collisions.
    #[test]
    fn the_lowest_member_auto_proposes_a_checkpoint_at_the_trigger_length() {
        let b = grown_chain(AUTO_CHECKPOINT_MIN_LEN - 2);
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        // one below the trigger: a commit runs the hook, but no cut yet —
        // pins the length lower bound THROUGH the hook, not just at init
        seal_one(&mut petra, &b, "walter", 90);
        assert_eq!(pending_cut(&petra), None, "below the trigger: no cut");
        seal_one(&mut petra, &b, "walter", 300);
        let head = petra.chain_head.as_ref().expect("head").height;
        assert_eq!(
            pending_cut(&petra),
            Some(head),
            "the lowest member proposes the cut at the fresh head"
        );

        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        seal_one(&mut walter, &b, "petra", 90);
        seal_one(&mut walter, &b, "petra", 300);
        assert_eq!(
            pending_cut(&walter),
            None,
            "a non-lowest member never auto-proposes"
        );
    }

    /// The trigger is bound to SEALING at the live head: a passively
    /// applied block (catch-up serve, another sealer's broadcast) never
    /// auto-proposes — a catching-up node would cut at a stale
    /// intermediate head, and a lockstep-catching-up quorum could even
    /// co-sign that cut and fork a holder after it dropped history.
    #[test]
    fn a_passively_applied_block_never_auto_proposes() {
        let mut b = grown_chain(AUTO_CHECKPOINT_MIN_LEN - 1);
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        // the trigger length arrives via the PASSIVE path — no cut
        b.commit_applied(400, &["petra", "walter"]);
        petra.receive_block(b.blocks.last().expect("built block").clone());
        assert_eq!(petra.chain.len(), AUTO_CHECKPOINT_MIN_LEN, "passively at length");
        assert_eq!(
            pending_cut(&petra),
            None,
            "a passively applied block never triggers the cut"
        );
        // the next SELF-sealed block fires it
        seal_one(&mut petra, &b, "walter", 90);
        let head = petra.chain_head.as_ref().expect("head").height;
        assert_eq!(
            pending_cut(&petra),
            Some(head),
            "the node's own seal at the live head triggers the cut"
        );
    }

    /// The automation never cuts while a vote is open: an interfering
    /// seal would only stale the cut. The trigger re-fires on the commit
    /// that resolves the last open vote.
    #[test]
    fn no_auto_checkpoint_while_a_vote_is_open() {
        let b = grown_chain(AUTO_CHECKPOINT_MIN_LEN - 1);
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        // a second, still-open vote holds the cut back
        petra.receive_proposed(91, Surface::Memory, json!({"op": "add_note", "id": 91}), "peer");
        seal_one(&mut petra, &b, "walter", 90);
        assert_eq!(
            pending_cut(&petra),
            None,
            "no cut while another vote is open"
        );
        // resolving the open vote triggers the cut on ITS commit
        let target = petra.chain_head.as_ref().expect("head").height + 1;
        let change = ChainChange::Applied {
            proposal_id: 91,
            surface: Surface::Memory,
            payload: json!({"op": "add_note", "id": 91}),
        };
        let bytes = approval_bytes(&b.republic_id, target, &change);
        let sig = identity_sign(b.key("walter"), &bytes);
        petra.receive_approval(91, "walter", target, &sig);
        petra.chain_sign_and_gossip_approval(91);
        assert_eq!(
            pending_cut(&petra),
            Some(target),
            "the commit that clears the last open vote fires the cut"
        );
    }

    /// A staled cut needs no timer: the very block that staled it re-runs
    /// the trigger and re-proposes at the new head.
    #[test]
    fn a_staled_auto_cut_re_proposes_on_the_next_commit() {
        let b = grown_chain(AUTO_CHECKPOINT_MIN_LEN - 1);
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        seal_one(&mut petra, &b, "walter", 90);
        let first_cut = pending_cut(&petra).expect("auto cut pending");
        // an interfering surface vote seals first — the cut goes stale
        // (id 300: well clear of the auto-cut's freshly minted next_id)
        seal_one(&mut petra, &b, "walter", 300);
        let head = petra.chain_head.as_ref().expect("head").height;
        assert_eq!(
            pending_cut(&petra),
            Some(head),
            "the staled cut is re-proposed at the new head"
        );
        assert!(first_cut < head, "the old cut was swept, not resurrected");
    }

    /// WP4b stage 4b: a holder that is BEHIND a served cut re-anchors on
    /// blob + anchor + suffix (hard-verified), and a forged blob is
    /// dropped at the cheap rid check.
    #[test]
    fn a_lagging_holder_re_anchors_on_a_served_blob() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        let blob = checkpoint_state(&b.blocks, 1).expect("state@1");
        let anchor = b.seal(
            2,
            ChainChange::Checkpoint {
                upto: 1,
                state_hash: checkpoint_state_hash(&blob),
            },
            &["petra", "walter"],
        );
        b.push(anchor.clone());
        b.commit_applied(7, &["petra", "walter"]);
        let suffix_tail = b.blocks.last().expect("tail").clone();

        // the laggard holds only the genesis
        let mut lag = chain_peer("walter", &b, b.blocks[..1].to_vec());
        assert_eq!(lag.chain_head.as_ref().expect("head").height, 0);
        // a forged blob (wrong founding) dies at the rid check
        let mut forged = blob.clone();
        forged.founding_name = "Fake".to_string();
        lag.receive_checkpoint_blob(forged);
        assert!(lag.pending_served_blob.is_none());
        // the served pieces arrive in any order: blob, tail, anchor
        lag.receive_checkpoint_blob(blob.clone());
        lag.receive_block(suffix_tail);
        assert_eq!(lag.chain_head.as_ref().expect("head").height, 0, "waits for the anchor");
        lag.receive_block(anchor);
        assert_eq!(
            lag.chain_head.as_ref().expect("head").height,
            3,
            "re-anchored on blob + anchor + suffix"
        );
        assert!(lag.checkpoint_blob.is_some());
        assert_eq!(
            lag.chain_applied.get(&Surface::Memory).map(|v| v.len()),
            Some(2),
            "pre-cut and post-cut entries both readable"
        );
    }

    /// SECURITY (total-review 2026-07-18): a peer-chosen id must never let
    /// a MEMBERSHIP proposal hijack a surface proposal's approvals — the
    /// same forge the checkpoint arm was hardened against, on the older
    /// membership arm. And symmetrically a surface proposal must not shadow
    /// a pending chain change.
    #[test]
    fn a_membership_proposal_cannot_hijack_a_colliding_surface_id() {
        let b = Builder::new(&["petra", "walter"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        // honest surface proposal id 5, awaiting approvals
        walter.receive_proposed(5, Surface::Memory, json!({"op": "add_note"}), "peer");
        // attacker gossips a membership change under the SAME id
        walter.receive_membership_proposal(5, MembershipOp::Joined, "mallory", &"ab".repeat(32), None, Vec::new(), None);
        // the id still resolves to the SURFACE proposal — approving it can
        // never sign membership bytes
        assert!(matches!(
            walter.proposal_change(5),
            Some(ChainChange::Applied { .. })
        ));
        // the reverse: a surface proposal cannot shadow a pending membership
        let mut walter2 = chain_signer("walter", &b, b.blocks.clone());
        walter2.receive_membership_proposal(6, MembershipOp::Joined, "dora", &"cd".repeat(32), None, Vec::new(), None);
        walter2.receive_proposed(6, Surface::Memory, json!({"op": "add_note"}), "peer");
        assert!(matches!(
            walter2.proposal_change(6),
            Some(ChainChange::Membership { .. })
        ));
        assert!(!walter2.proposals.contains_key(&6), "surface proposal refused");
    }

    /// The supersede walk (shared_memory_real.md §4): sealing one wiki
    /// patch deterministically retires every OVERLAPPING pending patch —
    /// terminal and unattributed (no vote forged: `declined_by` stays
    /// empty) — keeps the DISJOINT one approvable and applying, and a
    /// stale patch learned late (catch-up) registers superseded right
    /// away. Approving a superseded card is refused honestly.
    #[test]
    fn a_sealed_wiki_patch_supersedes_overlapping_pending_patches() {
        const ADD_A: &str = "diff --git a/a.md b/a.md\nnew file mode 100644\n--- /dev/null\n+++ b/a.md\n@@ -0,0 +1,2 @@\n+hello\n+world\n";
        const EDIT_A_1: &str = "diff --git a/a.md b/a.md\n--- a/a.md\n+++ b/a.md\n@@ -1,2 +1,2 @@\n-hello\n+hallo\n world\n";
        const EDIT_A_2: &str = "diff --git a/a.md b/a.md\n--- a/a.md\n+++ b/a.md\n@@ -1,2 +1,2 @@\n-hello\n+servus\n world\n";
        const ADD_B: &str = "diff --git a/b.md b/b.md\nnew file mode 100644\n--- /dev/null\n+++ b/b.md\n@@ -0,0 +1,1 @@\n+disjoint\n";
        let wp = |p: &str| json!({"op": "wiki_patch", "summary": "x", "value": p});

        let b = Builder::new(&["petra", "walter"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        seal_wiki(&mut walter, &b, "petra", 10, wp(ADD_A));
        assert_eq!(
            walter.wiki_tree().get("a.md").map(String::as_str),
            Some("hello\nworld\n"),
            "the fold serves the sealed base"
        );

        // two pending edits of the SAME region, one disjoint add
        walter.receive_proposed(11, Surface::Memory, wp(EDIT_A_1), "petra");
        walter.receive_proposed(12, Surface::Memory, wp(EDIT_A_2), "petra");
        walter.receive_proposed(13, Surface::Memory, wp(ADD_B), "petra");

        seal_wiki(&mut walter, &b, "petra", 11, wp(EDIT_A_1));
        let p12 = walter.proposals.get(&12).cloned().expect("card 12");
        assert_eq!(p12.state, ProposalState::Rejected, "overlap retires");
        assert!(p12.superseded, "…as SUPERSEDED, not declined");
        assert!(p12.declined_by.is_empty(), "no vote is forged");
        assert!(walter.view(12, &p12).superseded);
        assert!(
            matches!(
                walter.cmd_approve(molt_core::ProposalId(12)),
                Err(molt_core::MoltError::AlreadyTerminal(_, _))
            ),
            "approving a superseded card is refused"
        );
        let p13 = walter.proposals.get(&13).cloned().expect("card 13");
        assert_eq!(p13.state, ProposalState::Proposed, "disjoint stays open");
        assert!(!p13.superseded);

        // …and the disjoint one still seals and folds
        seal_wiki(&mut walter, &b, "petra", 13, wp(ADD_B));
        assert_eq!(
            walter.wiki_tree().get("b.md").map(String::as_str),
            Some("disjoint\n")
        );
        assert_eq!(
            walter.wiki_tree().get("a.md").map(String::as_str),
            Some("hallo\nworld\n")
        );

        // a stale patch learned LATE registers superseded immediately
        walter.receive_proposed(14, Surface::Memory, wp(EDIT_A_2), "petra");
        let p14 = walter.proposals.get(&14).cloned().expect("card 14");
        assert_eq!(p14.state, ProposalState::Rejected);
        assert!(p14.superseded);

        // …and the READ serves the same base to GUI and MCP alike
        // (co-equality: one projection, shared_memory_real.md WP-B)
        let snap = walter.snapshot(Surface::Memory, None, None);
        assert_eq!(snap.wiki_rev, 3, "ADD_A + EDIT_A_1 + ADD_B applied");
        assert_eq!(
            snap.wiki_tree,
            vec![
                molt_core::WikiDoc {
                    path: "a.md".to_string(),
                    content: "hallo\nworld\n".to_string()
                },
                molt_core::WikiDoc {
                    path: "b.md".to_string(),
                    content: "disjoint\n".to_string()
                },
            ]
        );
    }

    /// `seal_one`'s wiki twin: drive `payload` through the real chain
    /// machinery to a sealed Applied block.
    fn seal_wiki(
        s: &mut crate::State,
        b: &Builder,
        peer: &str,
        id: u64,
        payload: serde_json::Value,
    ) {
        let target = s.chain_head.as_ref().expect("head").height + 1;
        s.receive_proposed(id, Surface::Memory, payload.clone(), "peer");
        let change = ChainChange::Applied {
            proposal_id: id,
            surface: Surface::Memory,
            payload,
        };
        let bytes = approval_bytes(&b.republic_id, target, &change);
        let sig = identity_sign(b.key(peer), &bytes);
        s.receive_approval(id, peer, target, &sig);
        s.chain_sign_and_gossip_approval(id);
        assert_eq!(s.chain_head.as_ref().expect("head").height, target, "sealed");
    }

    /// The pull-back visibility gate: a record remembers who proposed it,
    /// and `mine` is reader-relative — true only when the reader IS that
    /// member ("" matches nobody).
    #[test]
    fn proposal_views_know_their_proposer_and_mine_is_reader_relative() {
        let b = Builder::new(&["petra", "walter"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        walter.receive_proposed(5, Surface::Memory, json!({"op": "add_note"}), "petra");
        let p = walter.proposals.get(&5).cloned().expect("record");
        assert_eq!(p.by, "petra");
        assert!(!walter.view(5, &p).mine, "petra's proposal is not walter's");
        // walter's own: the record carries his name, the view says mine
        walter.receive_proposed(6, Surface::Memory, json!({"op": "add_note"}), "walter");
        let own = walter.proposals.get(&6).cloned().expect("record");
        assert!(walter.view(6, &own).mine);
        // a pre-field record ("" proposer) is nobody's
        let mut blank = walter.proposals.get(&5).cloned().expect("record");
        blank.by = String::new();
        assert!(!walter.view(5, &blank).mine);
    }

    /// SECURITY: attacker-served checkpoint data with a height-0 anchor or
    /// upto = u64::MAX must be REFUSED, never underflow/overflow into a
    /// process abort (overflow-checks=true).
    #[test]
    fn malicious_checkpoint_heights_are_refused_not_panics() {
        let b = Builder::new(&["petra", "walter"], 2);
        let blob = checkpoint_state(&b.blocks, 0).expect("state@0");
        // a height-0 "checkpoint anchor" (anchor.height - 1 would underflow)
        let anchor0 = ChainBlock {
            height: 0,
            prev: GENESIS_PREV.to_string(),
            change: ChainChange::Checkpoint {
                upto: u64::MAX,
                state_hash: checkpoint_state_hash(&blob),
            },
            sigs: Vec::new(),
        };
        assert!(
            verify_suffix_chain(&blob, &[anchor0], &b.republic_id).is_err(),
            "a height-0 anchor is refused, not an underflow abort"
        );
        // a served blob with upto = u64::MAX (blob.upto + 1 would overflow)
        let mut peer = chain_peer("walter", &b, b.blocks.clone());
        let mut bomb = blob.clone();
        bomb.upto = u64::MAX;
        peer.pending_served_blob = Some(bomb);
        peer.try_adopt_from_blob(); // must not panic
        assert!(peer.pending_served_blob.is_none(), "the overflow blob is dropped");
    }

    /// Review pins: an id collision must never turn the auto-cosign into
    /// an unattended approval of a DIFFERENT change, and the gossip frame
    /// crosses the wire.
    #[test]
    fn a_checkpoint_proposal_never_signs_a_colliding_id() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let hash = checkpoint_state_hash(&checkpoint_state(&b.blocks, 1).expect("state"));
        // id already names a pending MEMBERSHIP change → refused, unsigned
        walter.receive_membership_proposal(5, MembershipOp::Restored, "petra", &b.pk("petra"), None, Vec::new(), None);
        walter.receive_checkpoint_proposal(5, 1, &hash);
        assert!(
            !walter.pending_sigs.contains_key(&5),
            "an occupied id must never be auto-signed"
        );
        assert!(matches!(
            walter.proposal_changes.get(&5),
            Some(ChainChange::Membership { .. })
        ));
        // id already names a SURFACE proposal → refused too
        walter.receive_proposed(6, Surface::Memory, json!({"op": "add_note"}), "peer");
        walter.receive_checkpoint_proposal(6, 1, &hash);
        assert!(!walter.pending_sigs.contains_key(&6));
        // a replayed valid frame does not amplify into more signatures
        walter.receive_checkpoint_proposal(9, 1, &hash);
        let sigs = walter.pending_sigs.get(&9).map(|p| p.sigs.len());
        walter.receive_checkpoint_proposal(9, 1, &hash);
        assert_eq!(walter.pending_sigs.get(&9).map(|p| p.sigs.len()), sigs);
        // the gossip frame is wire-scoped
        assert!(crate::net::crosses_wire(&WorkspaceEvent::CheckpointProposed {
            id: ProposalId(1),
            upto: 1,
            state_hash: hash,
        }));
    }

    /// A checkpoint cut pinned at the old head dies when another block
    /// commits first — dropped on re-base (re-cut needed), never re-signed
    /// into an invalid block.
    #[test]
    fn a_stale_checkpoint_proposal_dies_on_rebase() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        let id = match petra.cmd_propose_checkpoint().expect("propose") {
            molt_core::Reply::Proposed { id } => id.0,
            other => panic!("unexpected: {other:?}"),
        };
        // another applied block races the checkpoint to height 2
        b.commit_applied(7, &["petra", "walter"]);
        petra.receive_block(b.blocks.last().expect("block").clone());
        assert_eq!(petra.chain_head.as_ref().expect("head").height, 2);
        assert!(
            !petra.proposal_changes.contains_key(&id)
                && !petra.pending_sigs.contains_key(&id),
            "the stale cut is dropped, not re-signed"
        );
    }

    /// Review findings, pinned: (1) the anchor must not be circularly
    /// trusted — a blob whose roster is m sock-puppet keys (with the
    /// GENUINE public founding table, so the republic id recomputes!) is
    /// rejected even though its hash and "signatures" are self-consistent;
    /// (2) a checkpoint whose `upto` leaves a gap below its height is
    /// refused (gap blocks would escape blob AND suffix); (3) a SECOND
    /// checkpoint inside a suffix recomputes from the blob base and
    /// verifies.
    #[test]
    fn a_forged_roster_anchor_and_a_gap_upto_are_rejected() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        b.commit_applied(2, &["walter", "dora"]);
        let blob = checkpoint_state(&b.blocks, 2).expect("state@2");

        // sock-puppet forge: genuine founding fields, attacker-owned roster
        let mut forged = blob.clone();
        let (evil_sk1, evil_pk1) = derive_identity_key(&[9u8; 32], "petra");
        let (evil_sk2, evil_pk2) = derive_identity_key(&[8u8; 32], "walter");
        forged.roster = vec![
            MemberIdentity {
                member: "petra".to_string(),
                identity_pk: evil_pk1,
                nostr_pk: "ee".repeat(32),
            },
            MemberIdentity {
                member: "walter".to_string(),
                identity_pk: evil_pk2,
                nostr_pk: "ff".repeat(32),
            },
        ];
        let change = ChainChange::Checkpoint {
            upto: 2,
            state_hash: checkpoint_state_hash(&forged),
        };
        let bytes = approval_bytes(&b.republic_id, 3, &change);
        let anchor = ChainBlock {
            height: 3,
            prev: "00".repeat(32),
            change,
            sigs: vec![
                RosterAttestation { member: "petra".to_string(), sig: identity_sign(&evil_sk1, &bytes) },
                RosterAttestation { member: "walter".to_string(), sig: identity_sign(&evil_sk2, &bytes) },
            ],
        };
        assert!(
            verify_suffix_chain(&forged, &[anchor], &b.republic_id).is_err(),
            "a sock-puppet roster must never bootstrap a rejoiner"
        );

        // a gap upto (blocks between cut and block height) is refused on
        // both verify paths
        let gap = b.seal(
            3,
            ChainChange::Checkpoint {
                upto: 1,
                state_hash: checkpoint_state_hash(
                    &checkpoint_state(&b.blocks, 1).expect("state@1"),
                ),
            },
            &["petra", "walter"],
        );
        let mut chain = b.blocks.clone();
        chain.push(gap.clone());
        assert!(verify_chain(&chain).is_err(), "full holders refuse a gap upto");
        assert!(
            verify_suffix_chain(&blob, &[gap], &b.republic_id).is_err(),
            "suffix holders refuse a gap upto"
        );
    }

    /// N1 PIN — the suffix path must run the same structural size check as
    /// `verify_genesis` (`founding_identities.len() == rule_n`): a served
    /// blob whose founding table carries MORE entries than `rule_n` grafts
    /// attacker-owned "founding" keys into the signer set. The forged blob
    /// here is fully self-consistent (id and state hash recomputed over the
    /// 4-entry table, anchor signed by m REAL founding keys) and is checked
    /// against its own id — the trust-the-file restore posture — so only
    /// the size check can reject it. (Under the injective republic-id-v2
    /// layout a grafted table can no longer COLLIDE with the real id, so
    /// this is defense in depth for the paths that pin no external id.)
    #[test]
    fn a_suffix_blob_with_an_oversized_founding_table_is_rejected() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        let blob = checkpoint_state(&b.blocks, 1).expect("state@1");
        let mut forged = blob.clone();
        let (_evil_sk, evil_pk) = derive_identity_key(&[9u8; 32], "evil");
        forged.founding_identities.push(MemberIdentity {
            member: "evil".to_string(),
            identity_pk: evil_pk,
            nostr_pk: "dd".repeat(32),
        });
        forged.republic_id = molt_storage::republic_id(
            &forged.founding_name,
            forged.rule_m,
            forged.rule_n,
            &forged.founding_identities,
        );
        let change = ChainChange::Checkpoint {
            upto: 1,
            state_hash: checkpoint_state_hash(&forged),
        };
        let bytes = approval_bytes(&forged.republic_id, 2, &change);
        let sigs = ["petra", "walter"]
            .iter()
            .map(|name| {
                let (_, sk) = b.keys.iter().find(|(m, _)| m == name).expect("key");
                RosterAttestation {
                    member: (*name).to_string(),
                    sig: identity_sign(sk, &bytes),
                }
            })
            .collect();
        let anchor = ChainBlock {
            height: 2,
            prev: "00".repeat(32),
            change,
            sigs,
        };
        assert!(
            verify_suffix_chain(&forged, &[anchor], &forged.republic_id).is_err(),
            "a founding table larger than rule_n must be rejected"
        );
    }

    /// N1 PIN — the roster⊆founding comparison covers the THIRD anchor: a
    /// blob whose roster entry keeps its member+identity_pk but swaps the
    /// nostr anchor, with the state hash recomputed and the anchor block
    /// re-signed by m real founding keys (insider collusion — the state-hash
    /// check cannot catch a self-consistent re-signature), must still be
    /// rejected: seats are fixed at founding, so every roster entry must be
    /// a LITERAL founding-table entry, transport anchor included.
    #[test]
    fn a_resigned_roster_nostr_anchor_swap_is_rejected() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        let blob = checkpoint_state(&b.blocks, 1).expect("state@1");
        let mut forged = blob.clone();
        forged.roster[0].nostr_pk = "ee".repeat(32); // not petra's founding anchor
        let change = ChainChange::Checkpoint {
            upto: 1,
            state_hash: checkpoint_state_hash(&forged),
        };
        let bytes = approval_bytes(&forged.republic_id, 2, &change);
        let sigs = ["petra", "walter"]
            .iter()
            .map(|name| {
                let (_, sk) = b.keys.iter().find(|(m, _)| m == name).expect("key");
                RosterAttestation {
                    member: (*name).to_string(),
                    sig: identity_sign(sk, &bytes),
                }
            })
            .collect();
        let anchor = ChainBlock {
            height: 2,
            prev: "00".repeat(32),
            change,
            sigs,
        };
        assert!(
            verify_suffix_chain(&forged, &[anchor], &b.republic_id).is_err(),
            "a roster entry whose nostr anchor is not its founding-table anchor must be rejected"
        );
    }

    /// A second checkpoint INSIDE a suffix recomputes from the blob base
    /// and verifies — the chained-compaction path both holder types must
    /// agree on.
    #[test]
    fn a_second_checkpoint_inside_a_suffix_verifies_from_the_blob() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        let blob = checkpoint_state(&b.blocks, 1).expect("state@1");
        let anchor = b.seal(
            2,
            ChainChange::Checkpoint {
                upto: 1,
                state_hash: checkpoint_state_hash(&blob),
            },
            &["petra", "walter"],
        );
        b.push(anchor);
        b.commit_applied(9, &["petra", "walter"]);
        // the second cut, at the new head
        let state4 = checkpoint_state(&b.blocks, 3).expect("state@3");
        let second = b.seal(
            4,
            ChainChange::Checkpoint {
                upto: 3,
                state_hash: checkpoint_state_hash(&state4),
            },
            &["petra", "walter"],
        );
        b.push(second);
        // full holders accept the chained compaction…
        verify_chain(&b.blocks).expect("full holders verify the chained checkpoints");
        // …and so do suffix holders recomputing the second cut from the blob
        let suffix: Vec<ChainBlock> = b.blocks[2..].to_vec();
        let head = verify_suffix_chain(&blob, &suffix, &b.republic_id)
            .expect("suffix holders verify the second checkpoint from the blob base");
        assert_eq!(head.height, 4);
    }

    /// **A checkpoint SUMMARIZES — it does not archive** (§B.6a, decided
    /// 2026-08-03). The republic's logo changed three times; the blob carries
    /// the CURRENT one, and only that one.
    ///
    /// Asserted by CONTENT, not by count: a summary that kept the FIRST entry
    /// would satisfy a count check just as well, and would be silently,
    /// permanently wrong about what the republic looks like.
    #[test]
    fn a_checkpoint_keeps_the_current_value_of_a_slot_not_its_history() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_org(1, "set_image", "first.png", &["petra", "walter"]);
        b.commit_org(2, "set_image", "second.png", &["petra", "walter"]);
        b.commit_org(3, "set_image", "third.png", &["petra", "walter"]);

        let state = checkpoint_state(&b.blocks, 3).expect("state");
        let org: Vec<&(u64, serde_json::Value)> = state
            .applied
            .iter()
            .find(|(s, _)| *s == Surface::Organization)
            .map(|(_, list)| list.iter().collect())
            .unwrap_or_default();

        assert_eq!(org.len(), 1, "three logos survived the cut: {org:?}");
        assert_eq!(
            org[0].1.get("value").and_then(serde_json::Value::as_str),
            Some("third.png"),
            "the summary kept the wrong logo - a republic would show a superseded image forever"
        );
        // …and every consumed id survives, including the two whose payload
        // was dropped. This is the guard most likely to be lost by accident.
        assert_eq!(
            state.consumed_ids,
            vec![1, 2, 3],
            "a summarized-away payload must still be an un-re-appliable proposal id"
        );
    }

    /// Distinct slots do NOT collide, and a removal supersedes the image it
    /// removes — the two halves of "slot", both of which a naive
    /// keep-the-last-Organization-entry rule would get wrong.
    #[test]
    fn slots_are_independent_and_a_removal_supersedes_its_image() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_org(1, "set_image", "logo.png", &["petra", "walter"]);
        b.commit_org(2, "set_name", "Chess Club Reloaded", &["petra", "walter"]);
        b.commit_org(3, "remove_image", "", &["petra", "walter"]);

        let state = checkpoint_state(&b.blocks, 3).expect("state");
        let (_, org) = state
            .applied
            .iter()
            .find(|(s, _)| *s == Surface::Organization)
            .expect("organization entries");
        let ops: Vec<&str> = org
            .iter()
            .filter_map(|(_, p)| p.get("op").and_then(serde_json::Value::as_str))
            .collect();
        assert_eq!(
            ops,
            vec!["set_name", "remove_image"],
            "the name and image slots must survive independently, and the removal must \
             supersede the set_image it removes: {org:?}"
        );
    }

    /// **A checkpoint is a summary, not a delete.** Memory's notes are
    /// distinct objects, not superseded state, so every one of them survives
    /// the cut — the rule cannot be read as "keep only the last entry".
    #[test]
    fn accumulating_entries_all_survive_the_cut() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        b.commit_applied(2, &["petra", "walter"]);
        b.commit_applied(3, &["petra", "walter"]);

        let state = checkpoint_state(&b.blocks, 3).expect("state");
        let (_, notes) = state
            .applied
            .iter()
            .find(|(s, _)| *s == Surface::Memory)
            .expect("memory entries");
        assert_eq!(
            notes.len(),
            3,
            "notes are distinct objects - summarizing them away deletes the shared brain: {notes:?}"
        );
    }

    /// An op no build declares takes the CONSERVATIVE direction: it
    /// accumulates. Dropping something that was not superseded loses data,
    /// and an older node meeting a newer op must not guess otherwise.
    #[test]
    fn an_undeclared_op_accumulates() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_org(1, "set_mascot", "otter", &["petra", "walter"]);
        b.commit_org(2, "set_mascot", "heron", &["petra", "walter"]);

        let state = checkpoint_state(&b.blocks, 2).expect("state");
        let (_, org) = state
            .applied
            .iter()
            .find(|(s, _)| *s == Surface::Organization)
            .expect("organization entries");
        assert_eq!(org.len(), 2, "an undeclared op must not be summarized away: {org:?}");
    }

    /// **The incremental walk and the batch fold must agree on the summary.**
    ///
    /// A proposer computes a cut's `state_hash` with the batch fold; every
    /// verifier re-checks it with the incremental walk inside `verify_chain`.
    /// A rule that reached one and not the other would leave a republic
    /// unable to gather signatures for ANY cut, and nothing would say why —
    /// which is why `fold_state` delegates to `fold_one` rather than
    /// repeating the match. This test is what keeps that true.
    ///
    /// The chain deliberately mixes both kinds: a superseded slot, an
    /// accumulating note, and a second slot.
    #[test]
    fn the_walk_and_the_fold_summarize_identically() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_org(1, "set_image", "first.png", &["petra", "walter"]);
        b.commit_applied(2, &["petra", "walter"]);
        b.commit_org(3, "set_image", "second.png", &["petra", "walter"]);
        b.commit_org(4, "set_charter", "play more chess", &["petra", "walter"]);

        let folded = checkpoint_state(&b.blocks, 4).expect("batch fold");
        let cut = b.seal(
            5,
            ChainChange::Checkpoint {
                upto: 4,
                state_hash: checkpoint_state_hash(&folded),
            },
            &["petra", "walter"],
        );
        let mut chain = b.blocks.clone();
        chain.push(cut);
        let head = verify_chain(&chain).expect(
            "the walk must reach the same summary the fold did - otherwise no cut is signable",
        );
        assert_eq!(head.height, 5);
    }

    /// **A payload the summary dropped is still an un-re-appliable id.**
    ///
    /// The single most likely thing to be lost by accident here: `applied`
    /// shrinks, so it is tempting to let `consumed_ids` shrink with it. That
    /// would turn every superseded logo back into a proposal a suffix holder
    /// would happily apply again — the double-apply guard, silently repealed
    /// for exactly the entries a cut just summarized away.
    #[test]
    fn a_summarized_away_payload_can_never_re_apply() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_org(1, "set_image", "first.png", &["petra", "walter"]);
        b.commit_org(2, "set_image", "second.png", &["petra", "walter"]);
        let blob = checkpoint_state(&b.blocks, 2).expect("state@2");

        // proposal 1's payload is GONE from the summary…
        let (_, org) = blob
            .applied
            .iter()
            .find(|(s, _)| *s == Surface::Organization)
            .expect("organization entries");
        assert_eq!(org.len(), 1, "precondition: the first logo was summarized away");
        assert!(!org.iter().any(|(id, _)| *id == 1), "precondition: id 1's payload is dropped");
        // …and it is still consumed
        assert!(blob.consumed_ids.contains(&1), "the dropped payload's id must survive");

        let cut = b.seal(
            3,
            ChainChange::Checkpoint {
                upto: 2,
                state_hash: checkpoint_state_hash(&blob),
            },
            &["petra", "walter"],
        );
        b.push(cut);
        b.commit_org(1, "set_image", "resurrected.png", &["petra", "walter"]);
        let suffix: Vec<ChainBlock> = b.blocks[3..].to_vec();
        assert!(
            verify_suffix_chain(&blob, &suffix, &b.republic_id).is_err(),
            "a summarized-away proposal id re-applied in the suffix - the double-apply \
             guard was repealed for exactly the entries the cut dropped"
        );
    }

    /// **A suffix holder folding onto a summarized blob lands where a full
    /// holder folding from the genesis does.** Without it, the first cut
    /// after a prune would disagree across the republic — the pruned nodes
    /// against the ones that kept their history.
    #[test]
    fn a_suffix_holder_summarizes_onto_the_blob_the_same_way() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_org(1, "set_image", "first.png", &["petra", "walter"]);
        b.commit_applied(2, &["petra", "walter"]);
        // cut at 2, then keep changing the SAME slot past the cut
        let blob = checkpoint_state(&b.blocks, 2).expect("state@2");
        let cut = b.seal(
            3,
            ChainChange::Checkpoint {
                upto: 2,
                state_hash: checkpoint_state_hash(&blob),
            },
            &["petra", "walter"],
        );
        b.push(cut);
        b.commit_org(4, "set_image", "second.png", &["petra", "walter"]);

        // the full holder's answer…
        let full = checkpoint_state(&b.blocks, 4).expect("full fold@4");
        // …and the pruned holder's, folding the suffix onto the blob
        let suffix: Vec<ChainBlock> = b.blocks[3..].to_vec();
        let from_blob = fold_state(blob, &suffix, 4).expect("suffix fold@4");

        assert_eq!(
            checkpoint_state_hash(&full),
            checkpoint_state_hash(&from_blob),
            "a pruned holder and a full holder disagree about the summarized state"
        );
        let (_, org) = from_blob
            .applied
            .iter()
            .find(|(s, _)| *s == Surface::Organization)
            .expect("organization entries");
        assert_eq!(org.len(), 1, "the blob's superseded image survived the second fold: {org:?}");
        assert_eq!(
            org[0].1.get("value").and_then(serde_json::Value::as_str),
            Some("second.png")
        );
    }

    /// WP4b stage 1: two nodes that hold the SAME chain compute the SAME
    /// checkpoint state, canonical bytes and hash — the property that
    /// makes an m-of-n signature over the hash meaningful. Different
    /// content ⇒ different hash; the founding table inside the state
    /// recomputes to the real republic id (the genesis forgery check
    /// survives the genesis block being dropped later); consumed ids ride
    /// sorted.
    ///
    /// The chains deliberately carry BOTH kinds of applied entry: without a
    /// summarized slot in here, the determinism keystone would say nothing
    /// about the one rule most able to break it — every node must drop the
    /// same superseded entries, or a republic silently loses the ability to
    /// compact at all.
    #[test]
    fn checkpoint_state_is_deterministic_and_binds_the_founding() {
        let mut b1 = Builder::new(&["petra", "walter", "dora"], 2);
        b1.commit_applied(2, &["petra", "walter"]);
        b1.commit_applied(1, &["walter", "dora"]);
        b1.commit_org(3, "set_image", "old.png", &["petra", "walter"]);
        b1.commit_org(4, "set_image", "new.png", &["walter", "dora"]);
        let mut b2 = Builder::new(&["petra", "walter", "dora"], 2);
        b2.commit_applied(2, &["petra", "walter"]);
        b2.commit_applied(1, &["walter", "dora"]);
        b2.commit_org(3, "set_image", "old.png", &["petra", "walter"]);
        b2.commit_org(4, "set_image", "new.png", &["walter", "dora"]);

        let s1 = checkpoint_state(&b1.blocks, 4).expect("state 1");
        let s2 = checkpoint_state(&b2.blocks, 4).expect("state 2");
        assert_eq!(
            checkpoint_state_hash(&s1),
            checkpoint_state_hash(&s2),
            "equal chains yield the identical checkpoint hash"
        );
        // the canonical bytes carry the versioned tag (v6 since the relay
        // ledger joined; v5 the working anchors, v4 the summary rule, v3 the
        // ratified pool — each a change in WHAT the same chain hashes to,
        // which is exactly what the tag exists to announce)
        let bytes = molt_core::checkpoint_canonical_bytes(&s1);
        assert!(bytes.starts_with(b"molt-chain-checkpoint-v6\0"));
        // …and the pool is really covered: a summary whose relays were swapped
        // must not hash the same. Without this the tamper-evidence roster-v4
        // gives the genesis would vanish the moment a republic pruned.
        let mut swapped = s1.clone();
        swapped.relays = vec!["wss://not-what-was-ratified.example".to_string()];
        assert_ne!(
            checkpoint_state_hash(&s1),
            checkpoint_state_hash(&swapped),
            "the checkpoint must bind the ratified pool"
        );
        // consumed ids are sorted regardless of commit order, and the
        // summarized-away logo (3) is still among them
        assert_eq!(s1.consumed_ids, vec![1, 2, 3, 4]);
        // the founding table recomputes to the real republic id — the
        // forgery check a suffix bootstrapper will rely on
        assert_eq!(
            molt_storage::republic_id(
                &s1.founding_name,
                s1.rule_m,
                s1.rule_n,
                &s1.founding_identities
            ),
            s1.republic_id
        );
        // a different cut or different content changes the hash
        let shorter = checkpoint_state(&b1.blocks, 3).expect("shorter cut");
        assert_ne!(checkpoint_state_hash(&s1), checkpoint_state_hash(&shorter));

        // acceptance of checkpoint BLOCKS is pinned by the stage-2 tests
        // (a_checkpoint_block_verifies_against_the_own_projection,
        // a_suffix_chain_bootstraps_from_a_checkpoint)
    }

    /// WP2 pin: the catch-up re-gossip relies on the receive side being
    /// idempotent — a duplicated `Proposed` stays ONE pending entry, a
    /// duplicated `Approved` stays ONE signature per member, and neither
    /// resurrects a proposal whose block already committed.
    #[test]
    fn regossiped_proposals_and_approvals_are_idempotent() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let payload = json!({ "op": "add_note", "title": "minutes" });

        // a re-gossiped Proposed lands once
        walter.receive_proposed(1, Surface::Memory, payload.clone(), "peer");
        walter.receive_proposed(1, Surface::Memory, payload.clone(), "peer");
        let pending: Vec<_> = walter
            .proposals
            .iter()
            .filter(|(_, p)| p.state == ProposalState::Proposed)
            .collect();
        assert_eq!(pending.len(), 1, "one entry, not two");

        // a re-gossiped Approved lands as ONE signature for that member
        let change = ChainChange::Applied {
            proposal_id: 1,
            surface: Surface::Memory,
            payload: payload.clone(),
        };
        let bytes = approval_bytes(&b.republic_id, 1, &change);
        let petra_sig = identity_sign(b.key("petra"), &bytes);
        walter.receive_approval(1, "petra", 1, &petra_sig);
        walter.receive_approval(1, "petra", 1, &petra_sig);
        let sigs = &walter.pending_sigs.get(&1).expect("pending set").sigs;
        assert_eq!(sigs.len(), 1, "one signature per member: {sigs:?}");

        // walter co-signs — the block seals at 2-of-3
        walter.chain_sign_and_gossip_approval(1);
        assert_eq!(walter.chain_head.as_ref().expect("head").height, 1);
        assert!(
            matches!(walter.proposals.get(&1), Some(p) if p.state == ProposalState::Applied),
            "the proposal committed"
        );

        // LATE re-gossip (another answering peer) must not resurrect it
        walter.receive_proposed(1, Surface::Memory, payload, "peer");
        walter.receive_approval(1, "petra", 1, &petra_sig);
        assert!(
            matches!(walter.proposals.get(&1), Some(p) if p.state == ProposalState::Applied),
            "a committed proposal stays committed"
        );
        assert_eq!(
            walter.chain_head.as_ref().expect("head").height,
            1,
            "no second block for the same proposal"
        );
    }

    /// WP2: whoever answers a `ChainRequest` also re-serves the OPEN
    /// governance state — per open proposal a regular `Proposed` plus the
    /// already-collected `Approved` signatures (verbatim, position-bound —
    /// nothing is re-signed). A reopened member replays those through its
    /// normal receive arms and can then co-sign; the block seals at m.
    #[test]
    fn a_catchup_answer_reserves_open_governance() {
        let b = Builder::new(&["petra", "walter"], 2);
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        let payload = json!({ "op": "add_note", "title": "minutes" });
        petra
            .cmd_propose(Surface::Memory, payload.clone())
            .expect("petra proposes");

        // what petra's catch-up answer re-gossips: the open proposal and
        // her own collected co-signature
        let bodies = petra.open_governance_events();
        let (mut saw_proposed, mut relayed_sig) = (false, None);
        for body in &bodies {
            match body {
                WorkspaceEvent::Proposed { id, surface, payload: p } => {
                    assert_eq!((id.0, *surface), (1, Surface::Memory));
                    assert_eq!(p, &payload, "the payload rides unchanged");
                    saw_proposed = true;
                }
                WorkspaceEvent::Approved { id, by, height, sig } => {
                    assert_eq!((id.0, by.as_str(), *height), (1, "petra", 1));
                    relayed_sig = Some(sig.clone());
                }
                other => panic!("unexpected re-gossip event: {other:?}"),
            }
        }
        assert!(saw_proposed, "the open proposal is re-served");
        let relayed_sig = relayed_sig.expect("petra's collected signature is re-served");

        // walter — the reopened member: RAM lost the gossip, the chain has
        // only the genesis. The re-gossip restores proposal + count, then
        // his own co-signature seals the block (2-of-2).
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        walter.receive_proposed(1, Surface::Memory, payload, "peer");
        walter.receive_approval(1, "petra", 1, &relayed_sig);
        assert_eq!(
            walter.pending_sigs.get(&1).map(|s| s.sigs.len()),
            Some(1),
            "the reopened member sees the collected approval count"
        );
        walter.chain_sign_and_gossip_approval(1);
        assert_eq!(
            walter.chain_head.as_ref().expect("head").height,
            1,
            "the recovered proposal is fully approvable - the block seals"
        );
    }

    /// A chain-governed member that can also SIGN (holds its identity key).
    fn chain_signer(member: &str, b: &Builder, chain: Vec<ChainBlock>) -> crate::State {
        let mut s = chain_peer(member, b, chain);
        s.identity_sk = Some(b.key(member).clone());
        s
    }

    /// Re-admission (recovery step ❹): a survivor proposes a `Membership{Restored}`
    /// change and, once the threshold of members has signed it (here + "over the
    /// mesh"), a Restored block seals — the group's threshold-gated authorization
    /// of a returning member. Recovery keeps the same anchored identity key.
    #[test]
    fn a_threshold_restored_block_re_admits_a_member() {
        let b = Builder::new(&["petra", "walter"], 2);
        let walter_pk = b.pk("walter");
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        let mut walter = chain_signer("walter", &b, b.blocks.clone());

        // petra proposes re-admitting walter and co-signs (1 of 2 — pending)
        let id = petra.propose_membership(MembershipOp::Restored, "walter", &walter_pk, None, Vec::new(), None);
        assert_eq!(
            petra.chain_head.as_ref().expect("head").height,
            0,
            "one signature does not re-admit"
        );

        // walter learns the proposal + petra's signature, then co-signs
        walter.receive_membership_proposal(id, MembershipOp::Restored, "walter", &walter_pk, None, Vec::new(), None);
        let petra_sig = petra
            .pending_sigs
            .get(&id)
            .expect("petra's pending set")
            .sigs
            .iter()
            .find(|a| a.member == "petra")
            .expect("petra signed")
            .sig
            .clone();
        walter.receive_approval(id, "petra", 1, &petra_sig);
        walter.chain_sign_and_gossip_approval(id);

        // the Restored block seals at 2-of-2
        let head = walter.chain_head.as_ref().expect("head");
        assert_eq!(head.height, 1);
        assert!(
            matches!(
                walter.chain.last().expect("block").change,
                ChainChange::Membership {
                    op: MembershipOp::Restored,
                    ..
                }
            ),
            "the sealed block re-admits the member"
        );
    }

    /// Recovery step ❸: a coordinator re-admits a returning member ONLY on a
    /// valid seat proof against the anchored identity — a forged proof, or a
    /// request that would re-key to a different identity, is refused. A pass
    /// proposes the threshold Restored block.
    #[test]
    fn a_coordinator_re_admits_only_a_valid_seat_proof() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut coord = chain_signer("petra", &b, b.blocks.clone());
        let rid = b.republic_id.clone();
        let ticket = "recovery-ticket-xyz";
        let kp_hex = "beef";

        // the returning member (dora) signs the seat proof with its OWN key
        let good = crate::make_seat_proof(b.key("dora"), ticket, kp_hex, &rid, "", &[]);
        let id = coord
            .verify_and_propose_restore(true, "dora", &b.pk("dora"), kp_hex, ticket, &good, "", &[], "", "")
            .expect("a valid seat proof re-admits");
        assert!(matches!(
            coord.proposal_changes.get(&id),
            Some(ChainChange::Membership {
                op: MembershipOp::Restored,
                ..
            })
        ));
        // a verified request registers the pending recovery (the MLS re-key
        // consumes it the moment the block commits — even synchronously)
        assert!(coord.pending_recovery.contains_key("dora"));

        // a proof signed by the WRONG key (petra forging dora's) is rejected
        let forged = crate::make_seat_proof(b.key("petra"), ticket, kp_hex, &rid, "", &[]);
        assert!(coord
            .verify_and_propose_restore(true, "dora", &b.pk("dora"), kp_hex, ticket, &forged, "", &[], "", "")
            .is_err());

        // a request that re-keys the seat to a DIFFERENT identity is rejected —
        // recovery re-derives the SAME key
        assert!(coord
            .verify_and_propose_restore(true, "dora", &b.pk("walter"), kp_hex, ticket, &good, "", &[], "", "")
            .is_err());
    }

    /// The restored member's consent bytes, signed with its own roster key.
    fn consent_for(b: &Builder, member: &str, nostr_pk: &str) -> String {
        molt_storage::identity_sign(
            b.key(member),
            &molt_core::chain::restore_consent_bytes(
                &b.republic_id,
                member,
                &b.pk(member),
                nostr_pk,
            ),
        )
    }

    /// The rejoiner's consent counts as ONE distinct signer (recovery
    /// approval design, 2026-08-08): at m = n the coordinator's single
    /// surviving signature plus a valid consent seals the Restored block —
    /// the case that was a structural dead end before — and the sealed
    /// chain verifies from zero on an adopting reader.
    #[test]
    fn a_consented_restore_seals_at_m_equals_n() {
        let b = Builder::new(&["petra", "walter"], 2);
        let walter_pk = b.pk("walter");
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        let consent = consent_for(&b, "walter", "");
        petra.propose_membership(
            MembershipOp::Restored,
            "walter",
            &walter_pk,
            None,
            Vec::new(),
            Some(consent),
        );
        let head = petra.chain_head.as_ref().expect("head");
        assert_eq!(head.height, 1, "petra's signature + walter's consent reach 2-of-2");
        verify_chain(&petra.chain).expect("an adopting reader accepts the consented block");
    }

    /// Fail-closed on every consent abuse — the whole chain rejects
    /// (verify_chain is all-or-nothing): a forged consent, a consent on a
    /// non-restore change, a double-counted member, and a consent that has
    /// to stand in for EVERY missing signature.
    #[test]
    fn consent_abuse_rejects_the_chain() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let restored = |consent: Option<String>| ChainChange::Membership {
            op: MembershipOp::Restored,
            member: "dora".to_string(),
            identity_pk: b.pk("dora"),
            nostr_pk: None,
            relays: Vec::new(),
            consent,
        };

        // the honest shape: one survivor signature + dora's consent = 2-of-3
        let good = consent_for(&b, "dora", "");
        let mut chain = b.blocks.clone();
        chain.push(b.seal(1, restored(Some(good.clone())), &["petra"]));
        verify_chain(&chain).expect("one survivor + consent reaches m");

        // (a) forged: walter's key cannot consent for dora
        let forged = molt_storage::identity_sign(
            b.key("walter"),
            &molt_core::chain::restore_consent_bytes(
                &b.republic_id,
                "dora",
                &b.pk("dora"),
                "",
            ),
        );
        let mut chain = b.blocks.clone();
        chain.push(b.seal(1, restored(Some(forged)), &["petra"]));
        let err = verify_chain(&chain).expect_err("a forged consent must reject");
        assert!(err.contains("consent"), "the error names the consent: {err}");

        // (b) a consent on a non-restore membership change
        let mut chain = b.blocks.clone();
        chain.push(b.seal(
            1,
            ChainChange::Membership {
                op: MembershipOp::Joined,
                member: "erika".to_string(),
                identity_pk: "aa".repeat(32),
                nostr_pk: None,
                relays: Vec::new(),
                consent: Some(good.clone()),
            },
            &["petra", "walter"],
        ));
        let err = verify_chain(&chain).expect_err("consent on a join must reject");
        assert!(err.contains("non-restore"), "{err}");

        // (c) the restored member must not count twice (consent + signature)
        let mut chain = b.blocks.clone();
        chain.push(b.seal(1, restored(Some(good.clone())), &["dora"]));
        let err = verify_chain(&chain).expect_err("double-counting must reject");
        assert!(err.contains("twice"), "{err}");

        // (d) consent alone is ONE voice — it never reaches m = 2 by itself
        let mut chain = b.blocks.clone();
        chain.push(b.seal(1, restored(Some(good)), &[]));
        let err = verify_chain(&chain).expect_err("consent alone is below threshold");
        assert!(err.contains("threshold"), "{err}");
    }

    /// The approval surface (recovery approval design, 2026-08-08): a
    /// verified request creates a HUMAN-visible proposal record, a survivor
    /// approves it through the PUBLIC `cmd_approve`, and the commit settles
    /// the record to `Applied` with the vote bookkeeping dropped.
    #[test]
    fn a_wire_membership_proposal_is_votable_without_hand_applying() {
        // D3: the applier runs only for the proposer's OWN log, so the wire
        // arm must create the human-facing record itself — without it a
        // receiver held no card, cmd_approve said UnknownProposal, and an
        // m>=3 recovery stalled (coordinator co-sign + rejoiner consent are
        // only 2 distinct signers).
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let b = Builder::new(&["petra", "walter", "dora", "erika"], 3);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let consent = consent_for(&b, "dora", "");
        wire(
            &mut walter,
            "petra",
            1,
            WorkspaceEvent::MembershipProposed {
                id: ProposalId(5),
                op: MembershipOp::Restored,
                member: "dora".to_string(),
                identity_pk: b.pk("dora"),
                nostr_pk: None,
                relays: Vec::new(),
                consent: Some(consent),
            },
        );
        assert!(
            walter.proposals.contains_key(&5),
            "the receiver holds the votable card"
        );
        walter.cmd_approve(ProposalId(5)).expect("the survivor can approve");
    }

    #[test]
    fn a_membership_proposal_is_a_visible_approvable_record() {
        // CONSENT-LESS (a legacy rejoiner): the one restore shape that still
        // needs the human vote — auto-approval only ever signs a consent this
        // node verified itself (recovery_auto_approval.md §2).
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut coord = chain_signer("petra", &b, b.blocks.clone());
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let rid = b.republic_id.clone();
        let ticket = "recovery-ticket-xyz";
        let kp_hex = "beef";
        let proof = crate::make_seat_proof(b.key("dora"), ticket, kp_hex, &rid, "", &[]);
        let id = coord
            .verify_and_propose_restore(
                true,
                "dora",
                &b.pk("dora"),
                kp_hex,
                ticket,
                &proof,
                "",
                &[],
                "",
                "",
            )
            .expect("a valid request proposes");

        // visible on the proposer: a real record with the reserved op
        let rec = coord.proposals.get(&id).expect("the proposer holds a record");
        assert_eq!(rec.payload["op"], "restore_member");
        assert_eq!(rec.payload["member"], "dora");
        assert_eq!(rec.state, ProposalState::Proposed, "1 of 2 voices - still open");

        // …and on a receiver: the gossip's log event creates the SAME record
        let env = walter.make_env(
            "petra".to_string(),
            WorkspaceEvent::MembershipProposed {
                id: ProposalId(id),
                op: MembershipOp::Restored,
                member: "dora".to_string(),
                identity_pk: b.pk("dora"),
                nostr_pk: None,
                relays: Vec::new(),
                consent: None,
            },
        );
        walter.apply(&env);
        walter.receive_membership_proposal(
            id,
            MembershipOp::Restored,
            "dora",
            &b.pk("dora"),
            None,
            Vec::new(),
            None,
        );
        assert_eq!(
            walter.proposals.get(&id).map(|p| p.state),
            Some(ProposalState::Proposed),
            "the receiver sees an open, votable record"
        );
        assert!(
            !walter
                .pending_sigs
                .get(&id)
                .is_some_and(|p| p.sigs.iter().any(|a| a.member == "walter")),
            "a consent-less restore never auto-signs - the human vote is the content"
        );
        let petra_sig = coord
            .pending_sigs
            .get(&id)
            .expect("petra's pending set")
            .sigs
            .iter()
            .find(|a| a.member == "petra")
            .expect("petra co-signed")
            .sig
            .clone();
        walter.receive_approval(id, "petra", 1, &petra_sig);
        assert_eq!(
            walter.chain_head.as_ref().expect("head").height,
            0,
            "1 signature + no consent stays open"
        );

        // the PUBLIC approve — the exact call that answered UnknownProposal
        // before the record existed
        walter.cmd_approve(ProposalId(id)).expect("approve accepts the id");

        // petra + walter = 2-of-3: sealed, settled
        assert_eq!(walter.chain_head.as_ref().expect("head").height, 1);
        assert_eq!(
            walter.proposals.get(&id).map(|p| p.state),
            Some(ProposalState::Applied),
            "the commit settles the record"
        );
        assert!(
            !walter.pending_sigs.contains_key(&id) && !walter.proposal_changes.contains_key(&id),
            "the vote bookkeeping is dropped"
        );
        verify_chain(&walter.chain).expect("the sealed chain verifies from zero");
    }

    /// Auto-approval (recovery_auto_approval.md §3): a survivor that RECEIVES
    /// a `Restored` proposal carrying a consent it can verify itself signs it
    /// without a human — the recovery completes as soon as m survivors are
    /// online, no card-clicking required. The seal needs no `cmd_approve`.
    #[test]
    fn a_consented_restore_is_approved_without_a_human() {
        let b = Builder::new(&["petra", "walter", "dora", "erika"], 3);
        let mut coord = chain_signer("petra", &b, b.blocks.clone());
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let rid = b.republic_id.clone();
        let ticket = "recovery-ticket-xyz";
        let kp_hex = "beef";
        let proof = crate::make_seat_proof(b.key("dora"), ticket, kp_hex, &rid, "", &[]);
        let consent = consent_for(&b, "dora", "");
        let id = coord
            .verify_and_propose_restore(
                true,
                "dora",
                &b.pk("dora"),
                kp_hex,
                ticket,
                &proof,
                "",
                &[],
                &consent,
                "",
            )
            .expect("a valid request proposes");

        let env = walter.make_env(
            "petra".to_string(),
            WorkspaceEvent::MembershipProposed {
                id: ProposalId(id),
                op: MembershipOp::Restored,
                member: "dora".to_string(),
                identity_pk: b.pk("dora"),
                nostr_pk: None,
                relays: Vec::new(),
                consent: Some(consent.clone()),
            },
        );
        walter.apply(&env);
        walter.receive_membership_proposal(
            id,
            MembershipOp::Restored,
            "dora",
            &b.pk("dora"),
            None,
            Vec::new(),
            Some(consent),
        );
        // the receipt alone put walter's REAL signature into the pending set
        assert!(
            walter
                .pending_sigs
                .get(&id)
                .is_some_and(|p| p.sigs.iter().any(|a| a.member == "walter")),
            "a verified consent auto-signs on receipt"
        );
        // …and petra's gossiped signature completes the threshold: petra +
        // walter + dora's consent = 3-of-4, sealed with no cmd_approve call
        let petra_sig = coord
            .pending_sigs
            .get(&id)
            .expect("petra's pending set")
            .sigs
            .iter()
            .find(|a| a.member == "petra")
            .expect("petra co-signed")
            .sig
            .clone();
        walter.receive_approval(id, "petra", 1, &petra_sig);
        assert_eq!(walter.chain_head.as_ref().expect("head").height, 1);
        assert_eq!(
            walter.proposals.get(&id).map(|p| p.state),
            Some(ProposalState::Applied),
            "the commit settles the record without a human approve"
        );
        verify_chain(&walter.chain).expect("the sealed chain verifies from zero");
    }

    /// The chain IS the replay register (field storm 2026-08-24): every
    /// anchor that was ever anchored — genesis or a Restored block — refuses
    /// a replayed self-service request; a fresh salt passes.
    #[test]
    fn a_chain_known_anchor_is_a_replay() {
        let b = Builder::new(&["petra", "walter"], 2);
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        let genesis_anchor = "cc".repeat(32);
        assert!(petra.anchor_seen_in_chain(&genesis_anchor), "genesis anchors count");
        let consent = consent_for(&b, "walter", "ab");
        petra.propose_membership(
            MembershipOp::Restored,
            "walter",
            &b.pk("walter"),
            Some("ab".to_string()),
            Vec::new(),
            Some(consent),
        );
        assert_eq!(petra.chain_head.as_ref().expect("head").height, 1, "sealed");
        assert!(petra.anchor_seen_in_chain("ab"), "a Restored block's anchor counts");
        assert!(!petra.anchor_seen_in_chain(&"99".repeat(32)), "a fresh salt passes");
        assert!(!petra.anchor_seen_in_chain(""), "empty is never a hit");
    }

    /// The coordinator's vote report toward the waiting rejoiner
    /// (recovery_auto_approval.md §4): roster in roster order, the counted
    /// voices (its own co-signature + the consent), the threshold — and
    /// nothing for a proposal it does not coordinate.
    #[test]
    fn the_coordinator_reports_the_vote_progress_for_a_pending_recovery() {
        let b = Builder::new(&["petra", "walter", "dora"], 3);
        let mut coord = chain_signer("petra", &b, b.blocks.clone());
        let rid = b.republic_id.clone();
        let ticket = "recovery-ticket-xyz";
        let kp_hex = "beef";
        let proof = crate::make_seat_proof(b.key("dora"), ticket, kp_hex, &rid, "", &[]);
        let consent = consent_for(&b, "dora", "");
        let id = coord
            .verify_and_propose_restore(
                true,
                "dora",
                &b.pk("dora"),
                kp_hex,
                ticket,
                &proof,
                "",
                &[],
                &consent,
                "",
            )
            .expect("a valid request proposes");
        let report = coord.recover_progress_for(id).expect("a coordinated recovery reports");
        assert_eq!(report.member, "dora");
        assert_eq!(report.need, 3);
        assert_eq!(report.roster, vec!["petra", "walter", "dora"], "roster order");
        assert_eq!(
            report.approved,
            vec!["dora", "petra"],
            "the coordinator's co-signature and the consent are counted; walter is not"
        );
        // a proposal this node does not coordinate reports nothing
        assert!(coord.recover_progress_for(id + 1).is_none());
    }

    /// The auto-approval trusts NOTHING the coordinator claims: a consent
    /// that does not verify against the seat's anchored key never auto-signs
    /// (a malicious coordinator would otherwise harvest m unattended
    /// signatures for a block the verifier then rejects — or worse, for a
    /// change nobody consented to).
    #[test]
    fn a_forged_consent_never_auto_signs() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        // signed by the WRONG seat's key (petra's), claiming dora's consent
        let forged = molt_storage::identity_sign(
            b.key("petra"),
            &molt_core::chain::restore_consent_bytes(&b.republic_id, "dora", &b.pk("dora"), ""),
        );
        walter.receive_membership_proposal(
            7,
            MembershipOp::Restored,
            "dora",
            &b.pk("dora"),
            None,
            Vec::new(),
            Some(forged),
        );
        assert!(
            !walter
                .pending_sigs
                .get(&7)
                .is_some_and(|p| p.sigs.iter().any(|a| a.member == "walter")),
            "a forged consent must wait for a human, never auto-sign"
        );
    }

    /// A restore claiming a transport anchor another living seat already
    /// holds (or one that is not even canonical) never auto-signs — the
    /// coordinator's ingest checks this, but auto-approval re-checks it
    /// because it must not trust the coordinator.
    #[test]
    fn a_restore_claiming_a_foreign_anchor_never_auto_signs() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        // every Builder seat carries this anchor — dora claiming it collides
        // with petra's and walter's living seats (and a non-canonical string
        // refuses on the same guard ladder)
        let taken = "cc".repeat(32);
        let consent = consent_for(&b, "dora", &taken);
        walter.receive_membership_proposal(
            7,
            MembershipOp::Restored,
            "dora",
            &b.pk("dora"),
            Some(taken),
            Vec::new(),
            Some(consent),
        );
        assert!(
            !walter
                .pending_sigs
                .get(&7)
                .is_some_and(|p| p.sigs.iter().any(|a| a.member == "walter")),
            "an anchor collision must wait for a human, never auto-sign"
        );
    }

    /// R6 — the pool is group state any member can move and no member can
    /// move alone: a `set_relays` edit is an ordinary gated Organization
    /// proposal; below threshold the effective pool does not move, at m it
    /// does — for every member folding the same chain.
    #[test]
    fn a_pool_edit_commits_under_threshold_and_moves_the_effective_pool() {
        let pool = vec!["wss://relay.one".to_string()];
        let b = Builder::new_on_relays(&["petra", "walter"], 2, pool.clone());
        // WALTER — not the founder — raises the edit
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        walter
            .cmd_propose(
                Surface::Organization,
                serde_json::json!({
                    "op": "set_relays",
                    "value": "wss://relay.one wss://relay.three.example",
                }),
            )
            .expect("a member may propose a pool edit");
        let (id, surface, payload) = {
            let (id, rec) = walter.proposals.iter().next().expect("open proposal");
            (*id, rec.surface, rec.payload.clone())
        };
        assert_eq!(
            walter.effective_relays(),
            pool,
            "below threshold the pool must not move"
        );
        // petra learns the proposal + walter's signature, then co-signs
        petra.receive_proposed(id, surface, payload, "peer");
        let walter_sig = walter
            .pending_sigs
            .get(&id)
            .expect("walter's pending set")
            .sigs
            .iter()
            .find(|a| a.member == "walter")
            .expect("walter signed")
            .sig
            .clone();
        petra.receive_approval(id, "walter", 1, &walter_sig);
        petra.chain_sign_and_gossip_approval(id);
        assert_eq!(petra.chain_head.as_ref().expect("head").height, 1, "sealed at m");
        assert_eq!(
            petra.effective_relays(),
            vec!["wss://relay.one".to_string(), "wss://relay.three.example".to_string()],
            "at m the pool moves"
        );
    }

    /// R6 make-before-break (found LIVE 2026-08-09): a pool edit sharing NO
    /// relay with the effective pool is refused outright. The commit that
    /// moves the pool travels over the OLD pool; a member that has not
    /// applied it yet keeps listening there while the members that have
    /// rebuild onto the new pool only — with zero overlap the two sides can
    /// never meet again (a throwaway republic split exactly this way).
    /// A full migration is two votes: add the new relay, then drop the old.
    #[test]
    fn a_pool_edit_sharing_no_relay_with_the_current_pool_is_refused() {
        let pool = vec!["wss://relay.one".to_string()];
        let b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let err = walter
            .cmd_propose(
                Surface::Organization,
                serde_json::json!({
                    "op": "set_relays",
                    "value": "wss://relay.two.example",
                }),
            )
            .expect_err("a zero-overlap pool edit must be refused");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("wss://relay.one"),
            "the refusal names a current relay to keep: {msg}"
        );
        // …and the same target relay passes as make-before-break step one
        walter
            .cmd_propose(
                Surface::Organization,
                serde_json::json!({
                    "op": "set_relays",
                    "value": "wss://relay.one wss://relay.two.example",
                }),
            )
            .expect("keeping one shared relay is the legal migration step");
    }

    /// A DECIDED vote appends its summary to its discussion (story
    /// 2026-08-09): the SEALER posts one System message into the patch
    /// channel — so "Discussion" on an accepted vote says what exactly was
    /// decided, and the notice replicates like any chat message instead of
    /// being minted once per node.
    #[test]
    fn a_sealed_vote_appends_its_summary_to_the_discussion() {
        let pool = vec!["wss://relay.one".to_string()];
        let b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        walter
            .cmd_propose(
                Surface::Organization,
                serde_json::json!({
                    "op": "set_relays",
                    "value": "wss://relay.one wss://relay.three.example",
                }),
            )
            .expect("proposes");
        let (id, surface, payload) = {
            let (id, rec) = walter.proposals.iter().next().expect("open proposal");
            (*id, rec.surface, rec.payload.clone())
        };
        petra.receive_proposed(id, surface, payload, "peer");
        let walter_sig = walter
            .pending_sigs
            .get(&id)
            .expect("walter's pending set")
            .sigs
            .iter()
            .find(|a| a.member == "walter")
            .expect("walter signed")
            .sig
            .clone();
        petra.receive_approval(id, "walter", 1, &walter_sig);
        petra.chain_sign_and_gossip_approval(id);
        assert_eq!(petra.chain_head.as_ref().expect("head").height, 1, "sealed at m");
        // the SEALER's log carries the summary, in the vote's own channel
        let sum = petra
            .chat_visible()
            .find(|m| {
                m.kind == molt_core::ChatKind::System
                    && matches!(&m.channel, molt_core::ChannelRef::Patch { id: p } if p.0 == id)
            })
            .expect("the sealed vote posts its summary into the discussion")
            .clone();
        assert!(
            sum.body.contains('✓') && sum.body.contains("relay.three.example"),
            "the summary names the outcome and the decided content: {}",
            sum.body
        );
        // …and the proposer does NOT mint its own copy (it receives the
        // sealer's message over the wire like any chat)
        assert!(
            walter.chat_visible().all(|m| m.kind != molt_core::ChatKind::System),
            "only the sealer appends"
        );
    }

    /// The negative outcome gets the same treatment: the decline that makes
    /// approval unreachable posts the summary, naming the decliner.
    #[test]
    fn a_terminal_decline_appends_its_summary_to_the_discussion() {
        let pool = vec!["wss://relay.one".to_string()];
        let b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        walter
            .cmd_propose(
                Surface::Organization,
                serde_json::json!({ "op": "set_name", "value": "NewName" }),
            )
            .expect("proposes");
        // n = 2, m = 2: one decline makes the threshold unreachable
        walter.cmd_decline(ProposalId(1)).expect("declines");
        let sum = walter
            .chat_visible()
            .find(|m| {
                m.kind == molt_core::ChatKind::System
                    && matches!(&m.channel, molt_core::ChannelRef::Patch { id: p } if p.0 == 1)
            })
            .expect("the terminal decline posts its summary")
            .clone();
        assert!(
            sum.body.contains('⊘')
                && sum.body.contains("walter")
                && sum.body.contains("NewName"),
            "the summary names the outcome, the decliner and the content: {}",
            sum.body
        );
    }

    /// Make-before-break holds at the FOLD, not only at propose (review
    /// 2026-08-09): the propose gate is local courtesy — a peer on another
    /// build can gossip a zero-overlap edit, and two individually-legal
    /// pending edits can compose into one. The fold is the only place every
    /// node passes deterministically, so an applied `set_relays` sharing no
    /// relay with the pool accumulated SO FAR is a no-op — a pure function
    /// of chain content, identical on every holder.
    #[test]
    fn a_zero_overlap_pool_block_folds_as_a_no_op() {
        let r_a = "wss://relay.one".to_string();
        let r_b = "wss://relay.two.example".to_string();
        let mut b = Builder::new_on_relays(&["petra", "walter"], 2, vec![r_a.clone()]);
        let block = |b: &Builder, h, value: &str| {
            b.seal(
                h,
                ChainChange::Applied {
                    proposal_id: h,
                    surface: Surface::Organization,
                    payload: serde_json::json!({ "op": "set_relays", "value": value }),
                },
                &["petra", "walter"],
            )
        };
        // height 1: zero overlap with [A] — must keep [A]
        let zero = block(&b, 1, &r_b);
        b.push(zero);
        let walter = chain_signer("walter", &b, b.blocks.clone());
        assert_eq!(walter.effective_relays(), vec![r_a.clone()], "zero overlap folds as no-op");
        // height 2: [A B] overlaps via A — applies; height 3: [B] overlaps
        // via B — applies. The legal two-vote migration lands on [B].
        let step = block(&b, 2, &format!("{r_a} {r_b}"));
        b.push(step);
        let done = block(&b, 3, &r_b);
        b.push(done);
        let walter = chain_signer("walter", &b, b.blocks.clone());
        assert_eq!(walter.effective_relays(), vec![r_b], "the two-vote migration applies");
    }

    /// Charter features D5: a `set_features` edit is an ordinary gated
    /// Organization proposal — below threshold the effective set does not
    /// move, at m it does, for every member folding the same chain. The
    /// legacy baseline (D6) is Shared Memory: this republic was founded
    /// pre-v5 (`features: None`), so `memory` is on and everything else off
    /// until voted in.
    #[test]
    fn a_feature_edit_commits_under_threshold_and_moves_the_effective_set() {
        let pool = vec!["wss://relay.one".to_string()];
        let b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        assert_eq!(
            walter.effective_features(),
            vec!["memory".to_string()],
            "the legacy baseline is Shared Memory"
        );
        // WALTER — not the founder — raises the edit, deliberately unsorted
        // with a duplicate: the proposal is stored canonicalized
        walter
            .cmd_propose(
                Surface::Organization,
                serde_json::json!({
                    "op": "set_features",
                    "value": "quests memory quests",
                }),
            )
            .expect("a member may propose a feature edit");
        let (id, surface, payload) = {
            let (id, rec) = walter.proposals.iter().next().expect("open proposal");
            (*id, rec.surface, rec.payload.clone())
        };
        assert_eq!(
            payload.get("value").and_then(serde_json::Value::as_str),
            Some("memory quests"),
            "the proposal carries the canonical set"
        );
        assert_eq!(
            walter.effective_features(),
            vec!["memory".to_string()],
            "below threshold the set must not move"
        );
        petra.receive_proposed(id, surface, payload, "peer");
        let walter_sig = walter
            .pending_sigs
            .get(&id)
            .expect("walter's pending set")
            .sigs
            .iter()
            .find(|a| a.member == "walter")
            .expect("walter signed")
            .sig
            .clone();
        petra.receive_approval(id, "walter", 1, &walter_sig);
        petra.chain_sign_and_gossip_approval(id);
        assert_eq!(petra.chain_head.as_ref().expect("head").height, 1, "sealed at m");
        assert_eq!(
            petra.effective_features(),
            vec!["memory".to_string(), "quests".to_string()],
            "at m the set moves"
        );
    }

    /// Enable-only at propose time: dropping an enabled feature, re-enabling
    /// the current set unchanged, and an unknown key are all refused before
    /// anything reaches the members.
    #[test]
    fn a_feature_edit_that_shrinks_repeats_or_invents_is_refused() {
        let pool = vec!["wss://relay.one".to_string()];
        let b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let propose = |st: &mut crate::State, value: &str| {
            st.cmd_propose(
                Surface::Organization,
                serde_json::json!({ "op": "set_features", "value": value }),
            )
        };
        let err = propose(&mut walter, "quests").expect_err("dropping memory must be refused");
        assert!(format!("{err:?}").contains("memory: cannot be disabled"), "{err:?}");
        let err = propose(&mut walter, "memory").expect_err("a no-op must be refused");
        assert!(format!("{err:?}").contains("already enabled"), "{err:?}");
        let err = propose(&mut walter, "memory kanban").expect_err("an unknown key must be refused");
        assert!(format!("{err:?}").contains("unknown feature: kanban"), "{err:?}");
        let err = propose(&mut walter, "").expect_err("an empty edit must be refused");
        assert!(format!("{err:?}").contains("nothing to enable"), "{err:?}");
    }

    /// Enable-only holds at the FOLD, not only at propose: the fold is a
    /// UNION, so a hand-built block that "drops" a feature (bypassing every
    /// propose-time gate) folds as pure addition on every holder. This is
    /// the deterministic twin — without it, "features can never be switched
    /// off" would be local courtesy.
    #[test]
    fn a_feature_dropping_block_folds_as_a_union() {
        let pool = vec!["wss://relay.one".to_string()];
        let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
        // height 1: "quests" alone — as a REPLACEMENT it would drop memory
        let drop = b.seal(
            1,
            ChainChange::Applied {
                proposal_id: 1,
                surface: Surface::Organization,
                payload: serde_json::json!({ "op": "set_features", "value": "quests" }),
            },
            &["petra", "walter"],
        );
        b.push(drop);
        let walter = chain_signer("walter", &b, b.blocks.clone());
        assert_eq!(
            walter.effective_features(),
            vec!["memory".to_string(), "quests".to_string()],
            "a dropping block folds as a union - nothing is ever disabled"
        );
    }

    /// shared_memory_real.md WP-B keystone: memory's applied entries are
    /// ACCUMULATING at a checkpoint cut (`applied_lww_slot` = None), so
    /// the fold over the summarized state is byte-identical to the fold
    /// over the full chain — a cut can never fork the wiki.
    #[test]
    fn a_checkpoint_cut_keeps_the_wiki_fold_identical() {
        const ADD_A: &str = "diff --git a/a.md b/a.md\nnew file mode 100644\n--- /dev/null\n+++ b/a.md\n@@ -0,0 +1,2 @@\n+hello\n+world\n";
        const EDIT_A: &str = "diff --git a/a.md b/a.md\n--- a/a.md\n+++ b/a.md\n@@ -1,2 +1,2 @@\n-hello\n+hallo\n world\n";
        let pool = vec!["wss://relay.one".to_string()];
        let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
        for (h, id, patch) in [(1u64, 10u64, ADD_A), (2, 11, EDIT_A)] {
            let block = b.seal(
                h,
                ChainChange::Applied {
                    proposal_id: id,
                    surface: Surface::Memory,
                    payload: serde_json::json!({ "op": "wiki_patch", "value": patch }),
                },
                &["petra", "walter"],
            );
            b.push(block);
        }
        let full = chain_signer("walter", &b, b.blocks.clone());
        let full_tree = full.wiki_tree();
        assert_eq!(
            full_tree.get("a.md").map(String::as_str),
            Some("hallo\nworld\n")
        );
        let state = checkpoint_state(&b.blocks, 2).expect("summary");
        let mem: Vec<serde_json::Value> = state
            .applied
            .iter()
            .find(|(s, _)| *s == Surface::Memory)
            .map(|(_, entries)| entries.iter().map(|(_, p)| p.clone()).collect())
            .expect("memory summary");
        assert_eq!(
            molt_core::wiki_fold::wiki_fold(&mem),
            full_tree,
            "a cut keeps the fold byte-identical"
        );
    }

    /// Two racing enables both survive a compaction cut: `set_features`
    /// entries ACCUMULATE in the checkpoint summary (deliberately no
    /// `applied_lww_slot` — an LWW summary would keep only the later value
    /// and silently lose the other vote's addition across the cut).
    #[test]
    fn racing_feature_enables_both_survive_a_checkpoint_cut() {
        let pool = vec!["wss://relay.one".to_string()];
        let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
        // two independently-proposed enables, each a superset of the
        // baseline but not of each other (the race)
        for (h, value) in [(1, "memory quests"), (2, "memory vault")] {
            let block = b.seal(
                h,
                ChainChange::Applied {
                    proposal_id: h,
                    surface: Surface::Organization,
                    payload: serde_json::json!({ "op": "set_features", "value": value }),
                },
                &["petra", "walter"],
            );
            b.push(block);
        }
        let state = checkpoint_state(&b.blocks, 2).expect("summary");
        let org = state
            .applied
            .iter()
            .find(|(s, _)| *s == Surface::Organization)
            .map(|(_, entries)| entries)
            .expect("organization summary");
        let kept: Vec<&str> = org
            .iter()
            .filter_map(|(_, p)| p.get("value").and_then(serde_json::Value::as_str))
            .collect();
        assert_eq!(
            kept,
            vec!["memory quests", "memory vault"],
            "both racing enables survive the summary"
        );
        // …and the union over the summarized entries is the full set
        let walter = chain_signer("walter", &b, b.blocks.clone());
        assert_eq!(
            walter.effective_features(),
            vec!["memory".to_string(), "quests".to_string(), "vault".to_string()],
        );
    }

    /// **A cut must not carry every superseded avatar forever**
    /// (`member_profiles_plan.md` §3): the profile ops hold per-member LWW
    /// slots, so the summary keeps the LATEST picture and description per
    /// seat — one seat's edit never drops another's — and the fold over the
    /// summarized entries equals the fold over the full chain.
    #[test]
    fn a_checkpoint_cut_keeps_only_the_latest_avatar_per_member() {
        let pool = vec!["wss://relay.one".to_string()];
        let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
        let entries = [
            ("set_member_image", "petra", "old.png", "b2xk"),
            ("set_member_desc", "petra", "typo", ""),
            ("set_member_image", "walter", "walter.png", "d2FsdGVy"),
            ("set_member_image", "petra", "new.png", "bmV3"),
            ("set_member_desc", "petra", "keeps the bees", ""),
        ];
        for (h, (op, member, value, bytes)) in entries.iter().enumerate() {
            let height = u64::try_from(h + 1).expect("small height");
            let mut payload = serde_json::json!({ "op": op, "member": member, "value": value });
            if !bytes.is_empty() {
                payload["bytes_b64"] = serde_json::Value::String((*bytes).to_string());
            }
            let block = b.seal(
                height,
                ChainChange::Applied {
                    proposal_id: height,
                    surface: Surface::Organization,
                    payload,
                },
                &["petra", "walter"],
            );
            b.push(block);
        }
        let state = checkpoint_state(&b.blocks, 5).expect("summary");
        let org: Vec<(Option<u64>, serde_json::Value)> = state
            .applied
            .iter()
            .find(|(s, _)| *s == Surface::Organization)
            .map(|(_, e)| e.iter().map(|(id, p)| (Some(*id), p.clone())).collect())
            .expect("organization summary");
        let kept: Vec<&str> = org
            .iter()
            .filter_map(|(_, p)| p.get("value").and_then(serde_json::Value::as_str))
            .collect();
        assert_eq!(
            kept,
            vec!["walter.png", "new.png", "keeps the bees"],
            "the cut must keep exactly the latest entry per member and field"
        );

        // the post-cut fold is the live fold
        let full = chain_signer("walter", &b, b.blocks.clone());
        let live: Vec<(String, String, String)> = full
            .member_profiles()
            .iter()
            .map(|(m, p)| ((*m).to_string(), p.image.clone(), p.desc.to_string()))
            .collect();
        let mut cut = chain_signer("walter", &b, vec![b.blocks[0].clone()]);
        cut.chain_applied.insert(Surface::Organization, org);
        let after: Vec<(String, String, String)> = cut
            .member_profiles()
            .iter()
            .map(|(m, p)| ((*m).to_string(), p.image.clone(), p.desc.to_string()))
            .collect();
        assert_eq!(after, live, "a cut must not change what the profiles fold to");
        assert_eq!(
            live,
            vec![
                ("petra".to_string(), "new.png".to_string(), "keeps the bees".to_string()),
                ("walter".to_string(), "walter.png".to_string(), String::new()),
            ]
        );
    }

    /// D7: the engine refuses selecting and proposing on a surface the
    /// charter has not enabled — the co-equal twin of the nav hiding it —
    /// and an enabling block opens the same gate for every holder.
    #[test]
    fn a_disabled_surface_refuses_select_and_propose_until_enabled() {
        let pool = vec!["wss://relay.one".to_string()];
        let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        // legacy baseline {memory}: memory passes, quests is refused
        walter.cmd_select_surface(Surface::Memory).expect("memory is enabled");
        let err = walter
            .cmd_select_surface(Surface::Quests)
            .expect_err("selecting a disabled surface must be refused");
        assert_eq!(format!("{err}"), "quests: not enabled");
        let err = walter
            .cmd_select_view(Surface::Quests, "board".to_string())
            .expect_err("selecting a disabled surface's view must be refused");
        assert_eq!(format!("{err}"), "quests: not enabled");
        let err = walter
            .cmd_propose(Surface::Quests, serde_json::json!({ "op": "x", "value": "y" }))
            .expect_err("proposing on a disabled surface must be refused");
        assert_eq!(format!("{err}"), "quests: not enabled");
        // the enabling block opens the gate
        let enable = b.seal(
            1,
            ChainChange::Applied {
                proposal_id: 1,
                surface: Surface::Organization,
                payload: serde_json::json!({ "op": "set_features", "value": "memory quests" }),
            },
            &["petra", "walter"],
        );
        b.push(enable);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        walter.cmd_select_surface(Surface::Quests).expect("an enabled surface passes");
        // …and the effective set is on the status surface (co-equal read)
        assert_eq!(
            walter.status().features,
            vec!["memory".to_string(), "quests".to_string()],
        );
    }

    /// D7's approve half (review 2026-08-12): a peer's proposal on a
    /// disabled surface lands in the pool (ingest is tolerant — the
    /// enabling block may simply not have applied here yet), but no
    /// signature leaves this node for it, so it can never reach m honest
    /// seats. Once the feature is enabled the same approval passes.
    #[test]
    fn an_approval_on_a_disabled_surface_is_refused_until_enabled() {
        let pool = vec!["wss://relay.one".to_string()];
        let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        // a peer proposal on quests (disabled: legacy baseline is {memory})
        walter.receive_proposed(
            9,
            Surface::Quests,
            serde_json::json!({ "op": "add_quest", "title": "t" }),
            "peer",
        );
        let err = walter
            .cmd_approve(molt_core::ProposalId(9))
            .expect_err("no signature may leave for a disabled surface");
        assert_eq!(format!("{err}"), "quests: not enabled");
        // the enabling block opens the gate for the SAME proposal
        let enable = b.seal(
            1,
            ChainChange::Applied {
                proposal_id: 1,
                surface: Surface::Organization,
                payload: serde_json::json!({ "op": "set_features", "value": "memory quests" }),
            },
            &["petra", "walter"],
        );
        b.push(enable);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        walter.receive_proposed(
            9,
            Surface::Quests,
            serde_json::json!({ "op": "add_quest", "title": "t" }),
            "peer",
        );
        walter
            .cmd_approve(molt_core::ProposalId(9))
            .expect("an enabled surface accepts the approval");
    }

    /// Review 2026-08-12 (mixed versions): an unknown key can become
    /// effective here via a NEWER build's applied block (wire ingest never
    /// runs this build's validate). The enable-only gate must not demand a
    /// key this build cannot name — validate would refuse it — and the
    /// union fold keeps it regardless, so feature governance keeps working.
    #[test]
    fn an_unknown_effective_key_does_not_brick_feature_governance() {
        let pool = vec!["wss://relay.one".to_string()];
        let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
        let newer = b.seal(
            1,
            ChainChange::Applied {
                proposal_id: 1,
                surface: Surface::Organization,
                payload: serde_json::json!({ "op": "set_features", "value": "memory zzz" }),
            },
            &["petra", "walter"],
        );
        b.push(newer);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        assert!(
            walter.effective_features().iter().any(|f| f == "zzz"),
            "the unknown key is effective (union fold keeps it)"
        );
        // this build proposes WITHOUT the key it cannot name — accepted
        walter
            .cmd_propose(
                Surface::Organization,
                serde_json::json!({ "op": "set_features", "value": "memory quests" }),
            )
            .expect("an unknown effective key must not brick the gates");
        // …and the fold still keeps zzz alongside the new enable. Select
        // the OPEN card: adoption materialized the applied block's card too
        let (id, surface, payload) = {
            let (id, rec) = walter
                .proposals
                .iter()
                .find(|(_, p)| p.state == ProposalState::Proposed)
                .expect("open proposal");
            (*id, rec.surface, rec.payload.clone())
        };
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        petra.receive_proposed(id, surface, payload, "peer");
        let walter_sig = walter
            .pending_sigs
            .get(&id)
            .expect("walter's pending set")
            .sigs
            .iter()
            .find(|a| a.member == "walter")
            .expect("walter signed")
            .sig
            .clone();
        petra.receive_approval(id, "walter", 2, &walter_sig);
        petra.chain_sign_and_gossip_approval(id);
        assert_eq!(
            petra.effective_features(),
            vec![
                "memory".to_string(),
                "quests".to_string(),
                "zzz".to_string()
            ],
            "the union keeps what this build cannot name"
        );
    }

    /// The mint counter stays ahead of chain-consumed proposal ids (review
    /// 2026-08-12): a holder that adopted its chain WITHOUT the ephemeral
    /// event log (a blob-seeded rejoiner after total loss) would otherwise
    /// mint an id the chain already decided — every peer's ingest refuses
    /// that as a stale resend, so the proposal could never seal: a silent
    /// governance-liveness hole.
    #[test]
    fn a_fresh_adopter_never_mints_a_chain_consumed_proposal_id() {
        let pool = vec!["wss://relay.one".to_string()];
        let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
        let enable = b.seal(
            1,
            ChainChange::Applied {
                proposal_id: 1,
                surface: Surface::Organization,
                payload: serde_json::json!({ "op": "set_features", "value": "memory quests" }),
            },
            &["petra", "walter"],
        );
        b.push(enable);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        walter
            .cmd_propose(
                Surface::Organization,
                serde_json::json!({ "op": "set_features", "value": "memory quests vault" }),
            )
            .expect("propose");
        // the OPEN card (adoption materialized the applied block's card too)
        let (id, rec) = walter
            .proposals
            .iter()
            .find(|(_, p)| p.state == ProposalState::Proposed)
            .expect("open proposal");
        assert!(*id > 1, "the consumed id 1 must be skipped, got {id}");
        // and a peer registers it instead of refusing a "stale resend"
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        assert!(
            petra.receive_proposed(*id, rec.surface, rec.payload.clone(), "peer"),
            "the peer registers the freshly minted id"
        );
    }

    /// The baseline rule (D6): a v5 founding's ratified selection IS the
    /// baseline — `Some([])` means "nothing optional", never the legacy
    /// Shared-Memory grandfather, and an explicit selection replaces it.
    #[test]
    fn the_feature_baseline_follows_the_ratified_selection() {
        let b = Builder::new_on_relays(&["petra", "walter"], 2, Vec::new());
        let mut st = chain_signer("walter", &b, b.blocks.clone());
        if let Some(r) = st.replica.as_mut() {
            r.features = Some(Vec::new());
        }
        assert_eq!(
            st.effective_features(),
            Vec::<String>::new(),
            "an explicitly empty selection enables nothing"
        );
        if let Some(r) = st.replica.as_mut() {
            r.features = Some(vec!["wallet".to_string()]);
        }
        assert_eq!(st.effective_features(), vec!["wallet".to_string()]);
    }

    /// R6: an edit that would strand a member — a new pool sharing no relay
    /// with what that member is on record as reaching — is refused at
    /// propose time, naming the member and its relay (the R4 split it would
    /// otherwise commit).
    #[test]
    fn a_pool_edit_that_would_strand_a_member_is_refused() {
        let pool = vec!["wss://relay.one".to_string()];
        let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
        // petra is on record as reaching ONLY relay.two
        let fresh = molt_net::nostr_identity(b"petra-recovered", "new-ticket").1;
        let restored = b.seal(
            1,
            ChainChange::Membership {
                op: MembershipOp::Restored,
                member: "petra".to_string(),
                identity_pk: b.pk("petra"),
                nostr_pk: Some(fresh),
                relays: vec!["wss://relay.two.example".to_string()],
                consent: None,
            },
            &["petra", "walter"],
        );
        b.push(restored);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let err = walter
            .cmd_propose(
                Surface::Organization,
                serde_json::json!({
                    "op": "set_relays",
                    "value": "wss://relay.three.example",
                }),
            )
            .expect_err("a pool that strands a member must be refused");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("petra") && msg.contains("wss://relay.two.example"),
            "the refusal names the stranded member and its relay: {msg}"
        );
    }

    /// R5 — the re-join gate: a declaration that shares no relay with some
    /// member is refused, NAMING the relay the others must add — that
    /// message is the whole feature. The same declaration passes once the
    /// pool carries the relay.
    #[test]
    fn a_rejoin_over_a_foreign_relay_is_refused_naming_it() {
        let ticket = "recovery-ticket-r5";
        let kp_hex = "beef";
        let declared = vec!["wss://relay.two.example".to_string()];

        // republic pool: relay.one only — dora's declared relay bridges nobody
        let b = Builder::new_on_relays(
            &["petra", "walter", "dora"],
            2,
            vec!["wss://relay.one".to_string()],
        );
        let mut coord = chain_signer("petra", &b, b.blocks.clone());
        let proof = crate::make_seat_proof(
            b.key("dora"),
            ticket,
            kp_hex,
            &b.republic_id,
            "",
            &declared,
        );
        let err = coord
            .verify_and_propose_restore(
                true,
                "dora",
                &b.pk("dora"),
                kp_hex,
                ticket,
                &proof,
                "",
                &declared,
                "",
                "",
            )
            .expect_err("a declaration bridging nobody must be refused");
        assert!(
            err.contains("wss://relay.two.example") && err.contains("add"),
            "the refusal names the relay the others must add: {err}"
        );

        // the SAME declaration passes once the pool carries the relay
        let b2 = Builder::new_on_relays(
            &["petra", "walter", "dora"],
            2,
            vec!["wss://relay.one".to_string(), "wss://relay.two.example".to_string()],
        );
        let mut coord2 = chain_signer("petra", &b2, b2.blocks.clone());
        let proof2 = crate::make_seat_proof(
            b2.key("dora"),
            ticket,
            kp_hex,
            &b2.republic_id,
            "",
            &declared,
        );
        let id = coord2
            .verify_and_propose_restore(
                true,
                "dora",
                &b2.pk("dora"),
                kp_hex,
                ticket,
                &proof2,
                "",
                &declared,
                "",
                "",
            )
            .expect("the same declaration passes once the pool carries it");
        // …and the block carries the seat's OWN declaration (its ledger entry)
        assert!(matches!(
            coord2.proposal_changes.get(&id),
            Some(ChainChange::Membership { relays, .. }) if *relays == declared
        ));
    }

    /// When a `Restored` block commits, the coordinator (the node holding the
    /// pending recovery for that member) consumes it to drive the MLS re-key;
    /// a node without a pending recovery for that member does nothing. Here
    /// there is no runtime group, so the re-key is a logged no-op — but the
    /// trigger CONDITION (consume the pending recovery on commit) is exercised.
    #[test]
    fn a_restored_commit_triggers_the_coordinators_rekey() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let walter_pk = b.pk("walter");
        let mut coord = chain_signer("petra", &b, b.blocks.clone());
        coord.pending_recovery.insert(
            "walter".to_string(),
            PendingRecovery {
                ticketed: true,
                member: "walter".to_string(),
                key_package: "beef".to_string(),
                reply: String::new(),
            },
        );

        // build a Restored block for walter and hand it to the coordinator
        let change = ChainChange::Membership {
            op: MembershipOp::Restored,
            member: "walter".to_string(),
            identity_pk: walter_pk,
            nostr_pk: None,
            relays: Vec::new(),
            consent: None,
        };
        let block = b.seal(1, change, &["petra", "walter"]);
        coord.receive_block(block);

        assert_eq!(coord.chain_head.as_ref().expect("head").height, 1);
        assert!(
            !coord.pending_recovery.contains_key("walter"),
            "the coordinator consumed the pending recovery on the Restored commit"
        );
    }

    /// **Re-mint failover (decision A1, 2026-07-11), chain level.** When the
    /// recovery coordinator dies, any survivor mints a NEW recovery link and a
    /// complete second recovery round runs — producing a SECOND `Restored`
    /// block for the SAME seat. The chain must accept it: same anchored
    /// `identity_pk` at two consecutive heights (only the MLS leaf re-keys
    /// again; the roster identity never moves). Counter-assertion: a `Restored`
    /// block that re-keys the roster identity to a DIFFERENT key is rejected
    /// (`recovery_ritual.md` §6 — rotation is out of scope; the coordinator's
    /// refusal to *propose* such a change is pinned separately in
    /// `a_coordinator_re_admits_only_a_valid_seat_proof`).
    #[test]
    fn a_second_restored_block_for_the_same_seat_verifies() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        let walter_pk = b.pk("walter");
        let restored = ChainChange::Membership {
            op: MembershipOp::Restored,
            member: "walter".to_string(),
            identity_pk: walter_pk.clone(),
            nostr_pk: None,
            relays: Vec::new(),
            consent: None,
        };
        // round 1: the first recovery attempt's Restored block commits …
        let block = b.seal(1, restored.clone(), &["petra", "walter"]);
        b.push(block);
        // … then the coordinator dies; the re-mint failover runs a COMPLETE
        // second round: a second Restored block for the same seat, same key
        let block = b.seal(2, restored, &["petra", "walter"]);
        b.push(block);
        let head = verify_chain(&b.blocks).expect("two Restored blocks for one seat verify");
        assert_eq!(head.height, 2);
        assert_eq!(
            head.identities
                .iter()
                .find(|i| i.member == "walter")
                .expect("walter stays anchored")
                .identity_pk,
            walter_pk,
            "recovery re-keys the MLS leaf, never the roster identity"
        );

        // counter: a threshold of survivors must NOT be able to swap the seat
        // to a different identity key via a Restored block — hard-reject
        let (_, other_pk) = derive_identity_key(&[42u8; 32], "walter");
        let hijack = ChainChange::Membership {
            op: MembershipOp::Restored,
            member: "walter".to_string(),
            identity_pk: other_pk,
            nostr_pk: None,
            relays: Vec::new(),
            consent: None,
        };
        let block = b.seal(3, hijack, &["petra", "walter"]);
        b.push(block);
        assert!(
            verify_chain(&b.blocks).is_err(),
            "a Restored block with a non-anchored identity key must be rejected"
        );
    }

    /// A three-member MLS group (`coord`, `walter`, `dora`) — the shape a
    /// coordinator re-keys from.
    fn mls_trio() -> (molt_net::MlsMember, molt_net::MlsMember, molt_net::MlsMember) {
        let key = |n: u8| SigningKey::from_bytes(&[n; 32]);
        let mut coord = molt_net::MlsMember::new(&key(1), "coord").expect("coord");
        let walter = molt_net::MlsMember::new(&key(2), "walter").expect("walter");
        let dora = molt_net::MlsMember::new(&key(3), "dora").expect("dora");
        coord.create_group().expect("create");
        let welcome = coord
            .add_members(&[
                walter.key_package().expect("walter kp"),
                dora.key_package().expect("dora kp"),
            ])
            .expect("add")
            .expect("welcome");
        let (mut walter, mut dora) = (walter, dora);
        walter.join_from_welcome(&welcome).expect("walter joins");
        dora.join_from_welcome(&welcome).expect("dora joins");
        (coord, walter, dora)
    }

    /// **The Nostr re-key seals under the epoch its recipients are still at**
    /// (N4b step 6c, the `9900f36` lesson re-pinned at the production entry
    /// point).
    ///
    /// A receiver's exporter ring reaches BACKWARD only. So a commit whose
    /// outer layer is sealed at the epoch the coordinator just moved TO is
    /// opaque to exactly the members it exists to move forward — and the
    /// whole recovery is undeliverable, silently, because an opaque frame
    /// looks like relay spam. The negative half is the test: the NEW epoch's
    /// exporter must NOT open it.
    #[test]
    fn a_nostr_rekey_commit_opens_for_the_survivors_it_is_meant_for() {
        // dora is the SURVIVOR here — walter is the seat being restored, and
        // its old leaf is evicted by the very commit under test
        let (coord, _walter, survivor) = mls_trio();
        // walter lost everything and re-derives the SAME identity
        let returning =
            molt_net::MlsMember::new(&SigningKey::from_bytes(&[2u8; 32]), "walter").expect("kp");
        let kp = returning.key_package().expect("key package");

        let survivor_secrets = {
            let mut v = vec![survivor.exporter_secret().expect("survivor exporter")];
            v.extend_from_slice(survivor.exporter_ring());
            v
        };
        let mls = std::sync::Mutex::new(coord);
        let rekey = nostr_rekey(&mls, "walter", &kp, 1_759_000_000).expect("the re-key runs");

        // the commit, sealed the way the delivery task seals it…
        let sealed = molt_net::envelope::seal_outer(&rekey.prev_exporter, &rekey.commit)
            .expect("seal the commit");
        assert!(
            molt_net::envelope::open_outer(&survivor_secrets, &sealed).is_ok(),
            "a survivor that has NOT yet merged the commit cannot open it - the whole \
             re-key is undeliverable to exactly the members it is for"
        );
        // …and the counter-case: the epoch the coordinator moved TO must not
        // be what it sealed under, or the assertion above passes by accident
        let new_epoch = mls.lock().expect("lock").exporter_secret().expect("new exporter");
        assert_ne!(
            new_epoch, rekey.prev_exporter,
            "the commit was sealed at the coordinator's NEW epoch - backward-only \
             exporter rings make that opaque to every survivor"
        );
    }

    /// The stamp the commit is KEYED with is the stamp it is carried at.
    ///
    /// `CommitKey(created_at, digest)` breaks a concurrent same-epoch race,
    /// and both ends must derive it from the same value — the 445 receive side
    /// reads the real `created_at` off the wire. A coordinator that let the
    /// outbox pick the publish time would key its own commit at one value
    /// while every receiver keys it at another, and the two would pick
    /// different winners under ONE epoch number, silently.
    #[test]
    fn the_rekey_carries_the_stamp_it_was_keyed_with() {
        let (coord, _survivor, _dora) = mls_trio();
        let returning =
            molt_net::MlsMember::new(&SigningKey::from_bytes(&[2u8; 32]), "walter").expect("kp");
        let kp = returning.key_package().expect("key package");

        let pinned = 1_759_123_456;
        let mls = std::sync::Mutex::new(coord);
        let rekey = nostr_rekey(&mls, "walter", &kp, pinned).expect("the re-key runs");
        assert_eq!(
            rekey.stamp, pinned,
            "the re-key must carry its own pinned stamp - the delivery has no other \
             source for it, and re-reading a clock is exactly the divergence"
        );
    }

    /// The Welcome really admits the returning seat: it is the whole point of
    /// the re-key, and a commit that produced an unusable Welcome would still
    /// satisfy both tests above.
    #[test]
    fn the_rekey_welcome_puts_the_returning_seat_back_in_the_group() {
        let (coord, _walter, mut survivor) = mls_trio();
        let mut returning =
            molt_net::MlsMember::new(&SigningKey::from_bytes(&[2u8; 32]), "walter").expect("kp");
        let kp = returning.key_package().expect("key package");

        let mls = std::sync::Mutex::new(coord);
        let rekey = nostr_rekey(&mls, "walter", &kp, 1_759_000_000).expect("the re-key runs");

        // the survivor merges the commit and reaches the new epoch
        match survivor.decrypt(&rekey.commit).expect("survivor processes the commit") {
            molt_net::mls::MlsIncoming::Commit { .. } => {}
            other => panic!("expected a commit, got {other:?}"),
        }
        returning.join_from_welcome(&rekey.welcome).expect("the seat rejoins");
        // …and the two can now talk, which is what "recovered" means
        let ct = returning.encrypt(b"back").expect("encrypt");
        match survivor.decrypt(&ct).expect("survivor reads the rejoiner") {
            molt_net::mls::MlsIncoming::Application { from, plaintext } => {
                assert_eq!(from, "walter");
                assert_eq!(plaintext, b"back");
            }
            other => panic!("expected an application message, got {other:?}"),
        }
    }

    /// **Re-mint failover, engine level: a survivor (or a restarted, amnesiac
    /// coordinator) adopting a committed `Restored` block it holds NO pending
    /// recovery for is inert.** The chain extends normally, but
    /// `coordinator_rekey` never runs: nothing is recorded (no
    /// `WorkspaceEvent::MlsCommit` broadcast), the mesh window is not armed,
    /// and a pending recovery for a DIFFERENT member is left untouched. This
    /// is the crash-before-re-key case: the block committed, the coordinator
    /// died, and the re-mint failover's second round supplies the re-key.
    #[test]
    fn a_restored_commit_without_a_pending_recovery_is_inert() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let walter_pk = b.pk("walter");
        let mut node = chain_signer("petra", &b, b.blocks.clone());
        // a pending recovery for ANOTHER member must survive walter's commit
        node.pending_recovery.insert(
            "dora".to_string(),
            PendingRecovery {
                ticketed: true,
                member: "dora".to_string(),
                key_package: "beef".to_string(),
                reply: String::new(),
            },
        );
        let seq_before = node.next_seq;

        // a Restored block for walter — committed elsewhere — arrives; this
        // node holds no pending recovery for walter
        let change = ChainChange::Membership {
            op: MembershipOp::Restored,
            member: "walter".to_string(),
            identity_pk: walter_pk,
            nostr_pk: None,
            relays: Vec::new(),
            consent: None,
        };
        let block = b.seal(1, change, &["petra", "walter"]);
        node.receive_block(block);

        // the chain extends …
        assert_eq!(node.chain_head.as_ref().expect("head").height, 1);
        // … but the re-key trigger stayed inert: no envelope of any kind was
        // recorded (make_env is the only seq stamp, so an MlsCommit broadcast
        // or a chat notice would have advanced next_seq) …
        assert_eq!(node.next_seq, seq_before, "no MlsCommit/notice was recorded");
        // … the recovery mesh window was never armed …
        assert!(node.recovery_mesh_window.is_empty());
        // … and only walter's (absent) entry was consulted — dora's pending
        // recovery is untouched
        assert!(node.pending_recovery.contains_key("dora"));
    }

    /// A rejoiner that lost everything (no chain, no head) bootstraps from the
    /// genesis a survivor serves and then catches up the whole chain — even when
    /// later blocks arrive before the genesis (they buffer until it lands). The
    /// state-recovery core of Phase 4.
    #[test]
    fn a_headless_rejoiner_bootstraps_from_a_served_genesis() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        let genesis_block = b.blocks[0].clone();
        b.commit_applied(1, &["petra", "walter"]);
        b.commit_applied(2, &["petra", "walter"]);
        let block1 = b.blocks[1].clone();
        let block2 = b.blocks[2].clone();

        let mut rejoiner = crate::tests::plain_state();
        assert!(!rejoiner.is_chain_governed());

        // a block arrives before the genesis — buffered, still headless
        rejoiner.receive_block(block2);
        assert!(!rejoiner.is_chain_governed());
        assert_eq!(rejoiner.pending_blocks.len(), 1);

        // the survivor serves the genesis — adopt it as the root
        rejoiner.receive_block(genesis_block);
        assert!(rejoiner.is_chain_governed(), "adopted the served genesis");
        assert_eq!(rejoiner.chain_head.as_ref().expect("head").height, 0);

        // the middle block fills the gap; the buffered tail drains behind it
        rejoiner.receive_block(block1);
        assert_eq!(
            rejoiner.chain_head.as_ref().expect("head").height,
            2,
            "the rejoiner caught up the full chain from genesis"
        );
        assert!(rejoiner.pending_blocks.is_empty());
    }

    /// The co-equal Chain-History read (`Command::ReadChain`): every committed
    /// block newest first with the right kinds, the checkpoint block visible —
    /// and after the auto-drop, the pruned holder still lists the pre-cut
    /// applied entries as synthetic views from its checkpoint blob (height 0:
    /// the per-entry heights are gone with the history).
    #[test]
    fn read_chain_lists_blocks_newest_first_and_survives_the_prune() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        b.commit_applied(2, &["petra", "walter"]);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());

        // full holder: genesis + the two applied blocks, newest first
        let molt_core::Reply::Chain { blocks } = walter.cmd_read_chain().expect("read") else {
            panic!("read_chain answers Reply::Chain");
        };
        assert_eq!(
            blocks
                .iter()
                .map(|v| (v.height, v.kind.as_str()))
                .collect::<Vec<_>>(),
            vec![(2, "applied"), (1, "applied"), (0, "genesis")]
        );
        assert_eq!(blocks[0].proposal_id, 2, "the applied view names its proposal");
        assert_eq!(blocks[0].surface, "memory");
        assert_eq!(blocks[0].payload["op"], json!("add_note"));
        assert_eq!(
            blocks[0].signers,
            vec!["petra".to_string(), "walter".to_string()],
            "the signers ride the view in block order"
        );
        assert_eq!(blocks[2].payload, json!("Chess Club"), "the genesis shows the name");
        assert_eq!(blocks[2].surface, "");
        assert_eq!(blocks[2].proposal_id, 0);

        // seal the checkpoint cut at the head (stage-3 mechanics) → auto-drop
        let hash = checkpoint_state_hash(&checkpoint_state(&b.blocks, 2).expect("state"));
        walter.receive_checkpoint_proposal(40, 2, &hash);
        let change = ChainChange::Checkpoint { upto: 2, state_hash: hash };
        let bytes = approval_bytes(&b.republic_id, 3, &change);
        let petra_sig = identity_sign(b.key("petra"), &bytes);
        walter.receive_approval(40, "petra", 3, &petra_sig);
        assert_eq!(walter.chain.len(), 1, "history below the cut is dropped");

        // pruned holder: the real anchor keeps its height, then the synthetic
        // pre-cut applied views (newest first, signers gone), genesis last
        let molt_core::Reply::Chain { blocks } = walter.cmd_read_chain().expect("read") else {
            panic!("read_chain answers Reply::Chain");
        };
        assert_eq!(
            blocks
                .iter()
                .map(|v| (v.height, v.kind.as_str()))
                .collect::<Vec<_>>(),
            vec![(3, "checkpoint"), (0, "applied"), (0, "applied"), (0, "genesis")]
        );
        assert_eq!(blocks[0].payload, json!(2), "the checkpoint view shows the upto");
        assert_eq!(blocks[0].signers.len(), 2, "the anchor block keeps its m signers");
        assert_eq!(blocks[1].proposal_id, 2, "pre-cut entries stay listed, newest first");
        assert_eq!(blocks[2].proposal_id, 1);
        assert_eq!(blocks[1].surface, "memory");
        assert!(
            blocks[1].signers.is_empty() && blocks[2].signers.is_empty(),
            "the pre-cut block signatures are gone with the history"
        );
        assert_eq!(blocks[3].payload, json!("Chess Club"));
        assert_eq!(
            blocks[3].signers,
            vec!["petra".to_string(), "walter".to_string()],
            "the genesis view rebuilds from the blob's founding table"
        );
    }

    // ---- the wiki export bundle verifier (wiki_export_plan.md) ------------
    //
    // The bundle is a SUBSET of the chain (genesis + every Membership block +
    // every applied wiki patch), so `prev` links and contiguous heights are
    // gone by construction. What survives is what each block's own m
    // signatures cover: `republic_id ‖ height ‖ change` against the roster
    // valid at that height. These pin exactly that, and the fold equality.

    /// One `wiki_patch` payload in the shape a Memory proposal carries.
    fn wiki_payload(patch: &str) -> serde_json::Value {
        json!({ "op": "wiki_patch", "value": patch })
    }

    const WIKI_ADD_A: &str = "diff --git a/a.md b/a.md\nnew file mode 100644\n--- /dev/null\n+++ b/a.md\n@@ -0,0 +1,2 @@\n+hello\n+world\n";
    const WIKI_ADD_B: &str = "diff --git a/notes/b.md b/notes/b.md\nnew file mode 100644\n--- /dev/null\n+++ b/notes/b.md\n@@ -0,0 +1,1 @@\n+second\n";

    /// Commit an applied `wiki_patch` block at the next height.
    fn commit_wiki(b: &mut Builder, proposal_id: u64, patch: &str, signers: &[&str]) {
        let height = u64::try_from(b.blocks.len()).expect("small chain");
        let change = ChainChange::Applied {
            proposal_id,
            surface: Surface::Memory,
            payload: wiki_payload(patch),
        };
        let block = b.seal(height, change, signers);
        b.push(block);
    }

    /// The fixture every bundle test shares: a real 2-of-2 chain that
    /// carries both things the bundle must survive — a non-wiki block in
    /// the middle (dropped from the bundle, so heights have gaps) and a
    /// roster that MOVES (a recovery with consent, then a joined seat whose
    /// key signs the second patch).
    ///
    /// h0 genesis · h1 wiki patch · h2 org edit · h3 restored (consent) ·
    /// h4 joined dora · h5 wiki patch signed by dora.
    fn wiki_fixture() -> Builder {
        let mut b = Builder::new(&["petra", "walter"], 2);
        commit_wiki(&mut b, 1, WIKI_ADD_A, &["petra", "walter"]);
        b.commit_org(2, "set_name", "Chess Club 2", &["petra", "walter"]);
        // walter recovers: petra signs, walter's own consent is the second
        // voice (the m = n recovery path)
        let consent = identity_sign(
            b.key("walter"),
            &molt_core::chain::restore_consent_bytes(
                &b.republic_id,
                "walter",
                &b.pk("walter"),
                "dd".repeat(32).as_str(),
            ),
        );
        let height = u64::try_from(b.blocks.len()).expect("small chain");
        let restored = b.seal(
            height,
            ChainChange::Membership {
                op: MembershipOp::Restored,
                member: "walter".to_string(),
                identity_pk: b.pk("walter"),
                nostr_pk: Some("dd".repeat(32)),
                relays: Vec::new(),
                consent: Some(consent),
            },
            &["petra"],
        );
        b.push(restored);
        // (a `Joined` block used to sit here — seats are fixed at founding
        // and the variant is refused since review C7)
        commit_wiki(&mut b, 3, WIKI_ADD_B, &["walter", "petra"]);
        b
    }

    /// The tree the fixture's two patches fold to.
    fn wiki_fixture_tree() -> std::collections::BTreeMap<String, String> {
        std::collections::BTreeMap::from([
            ("a.md".to_string(), "hello\nworld\n".to_string()),
            ("notes/b.md".to_string(), "second\n".to_string()),
        ])
    }

    /// Serialize the bundle the writer would ship for `blocks`.
    fn bundle_json(blocks: &[ChainBlock]) -> String {
        let bundle = crate::wiki_export::bundle_from_chain(blocks).expect("the chain has a genesis");
        serde_json::to_string(&bundle).expect("bundle serializes")
    }

    #[test]
    fn a_wiki_export_bundle_verifies_against_its_tree() {
        let b = wiki_fixture();
        verify_chain(&b.blocks).expect("the fixture is a real chain");
        let json = bundle_json(&b.blocks);
        let report = verify_wiki_export(&json, &wiki_fixture_tree()).expect("the bundle verifies");
        assert_eq!(report.republic_id, b.republic_id);
        assert_eq!(report.name, "Chess Club");
        assert_eq!((report.rule_m, report.rule_n), (2, 2));
        assert_eq!(report.patches, 2, "both wiki patches ride along");
        assert_eq!(report.membership_blocks, 1, "the restored block rides along");
        assert_eq!(report.files, 2);
        assert_eq!(
            report.members,
            vec!["petra".to_string(), "walter".to_string()],
            "the roster walk ends at the founding roster (seats are fixed)"
        );
        // the org edit is NOT in the bundle: its content never leaves
        assert!(
            !json.contains("set_name"),
            "only wiki patches and membership blocks are exported"
        );
    }

    #[test]
    fn a_tampered_file_in_the_tree_fails_verification() {
        let b = wiki_fixture();
        let json = bundle_json(&b.blocks);
        let mut tree = wiki_fixture_tree();
        tree.insert("a.md".to_string(), "hello\nWORLD\n".to_string());
        let err = verify_wiki_export(&json, &tree).expect_err("a flipped byte must fail");
        assert!(err.contains("a.md"), "the fault names the file: {err}");
        // an EXTRA file the fold never produced is caught too
        let mut tree = wiki_fixture_tree();
        tree.insert("stray.md".to_string(), "smuggled".to_string());
        assert!(verify_wiki_export(&json, &tree).is_err(), "a stray file must fail");
    }

    #[test]
    fn a_tampered_patch_payload_fails_verification() {
        let mut b = wiki_fixture();
        // rewrite the first patch's content without re-signing
        if let ChainChange::Applied { payload, .. } = &mut b.blocks[1].change {
            *payload = wiki_payload(WIKI_ADD_A.replace("world", "welt").as_str());
        }
        let json = bundle_json(&b.blocks);
        assert!(
            verify_wiki_export(&json, &wiki_fixture_tree()).is_err(),
            "the m signatures cover the patch bytes"
        );
    }

    #[test]
    fn a_forged_or_removed_signature_fails_verification() {
        // removed: the patch drops below the threshold
        let mut b = wiki_fixture();
        b.blocks[4].sigs.truncate(1);
        assert!(
            verify_wiki_export(&bundle_json(&b.blocks), &wiki_fixture_tree()).is_err(),
            "one signature is below m = 2"
        );
        // forged: a signature that does not verify counts for nobody
        let mut b = wiki_fixture();
        b.blocks[4].sigs[0].sig = "00".repeat(64);
        assert!(
            verify_wiki_export(&bundle_json(&b.blocks), &wiki_fixture_tree()).is_err(),
            "a forged signature must not count"
        );
        // a signer outside the roster cannot lift a block to threshold
        let mut b = wiki_fixture();
        let (mallory_sk, _) = derive_identity_key(&[42u8; 32], "mallory");
        let bytes = approval_bytes(&b.republic_id, 5, &b.blocks[4].change);
        b.blocks[4].sigs[0] = RosterAttestation {
            member: "mallory".to_string(),
            sig: identity_sign(&mallory_sk, &bytes),
        };
        assert!(
            verify_wiki_export(&bundle_json(&b.blocks), &wiki_fixture_tree()).is_err(),
            "a stranger's signature is not a roster approval"
        );
    }

    #[test]
    fn a_forged_recovery_consent_fails_verification() {
        let mut b = wiki_fixture();
        if let ChainChange::Membership { consent, .. } = &mut b.blocks[3].change {
            *consent = Some("11".repeat(64));
        }
        assert!(
            verify_wiki_export(&bundle_json(&b.blocks), &wiki_fixture_tree()).is_err(),
            "a consent that does not verify is not the second voice"
        );
    }

    /// Seats are fixed at founding: a bundle carrying a `Joined` block is
    /// refused whole, exactly like the chain it came from (review C7) —
    /// there is no identity history beyond the founding table to verify
    /// a later patch against.
    #[test]
    fn a_bundle_with_a_joined_block_is_refused() {
        let b = wiki_fixture();
        let mut bundle =
            crate::wiki_export::bundle_from_chain(&b.blocks).expect("the chain has a genesis");
        let (_dora_sk, dora_pk) = derive_identity_key(&[9u8; 32], "dora");
        let height = bundle.blocks.last().map_or(0, |bl| bl.height) + 1;
        let mut joined = b.blocks[0].clone();
        joined.height = height;
        joined.change = ChainChange::Membership {
            op: MembershipOp::Joined,
            member: "dora".to_string(),
            identity_pk: dora_pk,
            nostr_pk: None,
            relays: Vec::new(),
            consent: None,
        };
        bundle.blocks.push(joined);
        let json = serde_json::to_string(&bundle).expect("bundle serializes");
        assert!(
            verify_wiki_export(&json, &wiki_fixture_tree()).is_err(),
            "a joined seat is not a thing the verifier accepts"
        );
    }

    /// **The ascending-height rule needs a fixture that isolates it.** In the
    /// shared fixture a reversed bundle already dies for another reason (the
    /// last patch's signer JOINED later, so against the genesis roster it
    /// falls below m) and a duplicate dies on the double-apply guard - delete
    /// the order check and both still fail, which is a keystone proving
    /// someone else's rule. Two patches approved by the SAME roster isolate
    /// it: each verifies on its own and even the fold is order-independent,
    /// so nothing but the ORDER is wrong.
    #[test]
    fn two_patches_of_one_roster_must_still_arrive_in_order() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        commit_wiki(&mut b, 1, WIKI_ADD_A, &["petra", "walter"]);
        commit_wiki(&mut b, 2, WIKI_ADD_B, &["petra", "walter"]);
        let tree = wiki_fixture_tree();
        let base = crate::wiki_export::bundle_from_chain(&b.blocks).expect("the chain has a genesis");
        assert!(
            verify_wiki_export(&serde_json::to_string(&base).expect("serialize"), &tree).is_ok(),
            "the fixture itself must verify in order"
        );
        let mut swapped = base;
        swapped.blocks.reverse();
        let err = verify_wiki_export(&serde_json::to_string(&swapped).expect("serialize"), &tree)
            .expect_err("blocks must arrive in ascending height order");
        assert!(err.contains("heights must ascend"), "the fault names the rule: {err}");
    }

    #[test]
    fn reordered_or_duplicate_heights_fail_verification() {
        let b = wiki_fixture();
        let base =
            crate::wiki_export::bundle_from_chain(&b.blocks).expect("the chain has a genesis");
        // reordered
        let mut bundle = base.clone();
        bundle.blocks.reverse();
        assert!(
            verify_wiki_export(
                &serde_json::to_string(&bundle).expect("serialize"),
                &wiki_fixture_tree()
            )
            .is_err(),
            "blocks must arrive in ascending height order"
        );
        // duplicated
        let mut bundle = base.clone();
        let dup = bundle.blocks[0].clone();
        bundle.blocks.insert(1, dup);
        assert!(
            verify_wiki_export(
                &serde_json::to_string(&bundle).expect("serialize"),
                &wiki_fixture_tree()
            )
            .is_err(),
            "a repeated block must not fold twice"
        );
    }

    #[test]
    fn a_non_wiki_block_in_the_bundle_is_refused() {
        let b = wiki_fixture();
        let mut bundle =
            crate::wiki_export::bundle_from_chain(&b.blocks).expect("the chain has a genesis");
        bundle.blocks.push(b.blocks[2].clone()); // the org edit
        assert!(
            verify_wiki_export(
                &serde_json::to_string(&bundle).expect("serialize"),
                &wiki_fixture_tree()
            )
            .is_err(),
            "the bundle carries wiki patches and membership blocks, nothing else"
        );
    }

    #[test]
    fn a_forged_genesis_id_fails_the_bundle() {
        let mut b = wiki_fixture();
        if let ChainChange::Genesis { republic_id, .. } = &mut b.blocks[0].change {
            *republic_id = "deadbeef".to_string();
        }
        assert!(
            verify_wiki_export(&bundle_json(&b.blocks), &wiki_fixture_tree()).is_err(),
            "the genesis id must re-derive from the roster content"
        );
    }

    #[test]
    fn a_foreign_bundle_format_is_refused() {
        let b = wiki_fixture();
        let json = bundle_json(&b.blocks).replace("molt-wiki-export-v1", "molt-wiki-export-v9");
        assert!(
            verify_wiki_export(&json, &wiki_fixture_tree()).is_err(),
            "an unknown format tag is not verified on hope"
        );
    }
}

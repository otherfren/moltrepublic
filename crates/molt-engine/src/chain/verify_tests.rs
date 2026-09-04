// SPDX-License-Identifier: GPL-3.0-or-later

//! Tests for [`super::verify`]: the block-level checks, the suffix
//! verifier, the checkpoint fold and the detached wiki-export verifier.

use super::test_support::*;
use super::*;
use molt_core::{ChainChange, MembershipOp, Surface};
use molt_storage::{derive_identity_key, identity_sign};
use serde_json::json;

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
        verify_suffix_chain(&blob, &[anchor0], &b.republic_id, None).is_err(),
        "a height-0 anchor is refused, not an underflow abort"
    );
    // a served blob with upto = u64::MAX (blob.upto + 1 would overflow)
    let mut peer = chain_peer("walter", &b, b.blocks.clone());
    let mut bomb = blob.clone();
    bomb.upto = u64::MAX;
    peer.chain.pending_served_blob = Some(bomb);
    peer.try_adopt_from_blob(); // must not panic
    assert!(peer.chain.pending_served_blob.is_none(), "the overflow blob is dropped");
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
        verify_suffix_chain(&forged, &[anchor], &b.republic_id, None).is_err(),
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
        verify_suffix_chain(&blob, &[gap], &b.republic_id, None).is_err(),
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
        verify_suffix_chain(&forged, &[anchor], &forged.republic_id, None).is_err(),
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
        verify_suffix_chain(&forged, &[anchor], &b.republic_id, None).is_err(),
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
    let head = verify_suffix_chain(&blob, &suffix, &b.republic_id, None)
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
        verify_suffix_chain(&blob, &suffix, &b.republic_id, None).is_err(),
        "a summarized-away proposal id re-applied in the suffix - the double-apply \
         guard was repealed for exactly the entries the cut dropped"
    );
}

/// A cut carries exactly the frozen v7 groups until a later surface holds
/// state: `genesis_base` seeds `Surface::CHECKPOINT_V7_SURFACES`, never
/// `Surface::ALL`, so a surface added later leaves every earlier
/// checkpoint's bytes and JSON shape alone. Latent while the two sets are
/// equal; red the day `ALL` grows and `genesis_base` was not left alone.
#[test]
fn a_cut_seeds_exactly_the_frozen_v7_groups() {
    let mut b = Builder::new(&["petra", "walter", "dora"], 2);
    b.commit_org(1, "set_name", "X", &["petra", "walter"]);
    let st = checkpoint_state(&b.blocks, 1).expect("state@1");
    assert_eq!(
        st.applied.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
        Surface::CHECKPOINT_V7_SURFACES.to_vec()
    );
}

/// A surface whose group the state does not carry yet gets one with its
/// first applied entry, at its `Surface::ALL` position — the path a surface
/// added after v7 takes. Exercised by dropping a frozen group, since no
/// later surface exists in this build.
#[test]
fn the_fold_creates_a_missing_group_at_its_all_position() {
    let mut b = Builder::new(&["petra", "walter", "dora"], 2);
    b.commit_org(1, "set_name", "X", &["petra", "walter"]);
    let mut st = checkpoint_state(&b.blocks, 1).expect("state@1");
    st.applied.retain(|(s, _)| *s != Surface::Quests);
    let block = b.seal(
        2,
        ChainChange::Applied {
            proposal_id: 2,
            surface: Surface::Quests,
            payload: json!({ "op": "add_quest", "title": "x" }),
        },
        &["petra", "walter"],
    );
    fold_one(&mut st, &block).expect("folds");
    assert_eq!(
        st.applied.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
        Surface::CHECKPOINT_V7_SURFACES.to_vec(),
        "re-created at its ALL position"
    );
    let (_, quests) = st
        .applied
        .iter()
        .find(|(s, _)| *s == Surface::Quests)
        .expect("the group appears with its first entry");
    assert_eq!(quests.len(), 1);
}

/// A served blob whose applied groups deviate from the one shape every
/// holder builds — one dropped, or two swapped — fails the signed state
/// hash: the hash covers the group list whole, so no shape check exists.
#[test]
fn a_blob_with_a_dropped_or_reordered_group_fails_the_signed_hash() {
    let mut b = Builder::new(&["petra", "walter", "dora"], 2);
    b.commit_org(1, "set_name", "X", &["petra", "walter"]);
    b.commit_applied(2, &["petra", "walter"]);
    let honest = checkpoint_state(&b.blocks, 2).expect("state@2");
    let cut = b.seal(
        3,
        ChainChange::Checkpoint {
            upto: 2,
            state_hash: checkpoint_state_hash(&honest),
        },
        &["petra", "walter"],
    );
    b.push(cut);
    let suffix: Vec<ChainBlock> = b.blocks[3..].to_vec();
    verify_suffix_chain(&honest, &suffix, &b.republic_id, None).expect("the honest blob verifies");

    let mut dropped = honest.clone();
    dropped.applied.retain(|(s, _)| *s != Surface::Wallet);
    let err = verify_suffix_chain(&dropped, &suffix, &b.republic_id, None).expect_err("a dropped group");
    assert!(err.contains("signed state hash"), "{err}");
    let mut swapped = honest.clone();
    swapped.applied.swap(1, 2);
    let err = verify_suffix_chain(&swapped, &suffix, &b.republic_id, None).expect_err("swapped groups");
    assert!(err.contains("signed state hash"), "{err}");
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
        petra.chain.head.as_ref().expect("head").height,
        0,
        "one signature does not re-admit"
    );

    // walter learns the proposal + petra's signature, then co-signs
    walter.receive_membership_proposal(id, MembershipOp::Restored, "walter", &walter_pk, None, Vec::new(), None);
    let petra_sig = petra
        .chain.pending_sigs
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
    let head = walter.chain.head.as_ref().expect("head");
    assert_eq!(head.height, 1);
    assert!(
        matches!(
            walter.chain.blocks.last().expect("block").change,
            ChainChange::Membership {
                op: MembershipOp::Restored,
                ..
            }
        ),
        "the sealed block re-admits the member"
    );
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

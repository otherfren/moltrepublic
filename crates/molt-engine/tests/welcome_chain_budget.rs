// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **N4b §8.8 step 10 — the measurement that decides step 6.**
//!
//! Step 6 says the rejoiner "peels the 444 … verifies the served chain", and
//! the plan defers to step 10 whether the Welcome can CARRY that chain: under
//! the 65408-byte gift-wrap cap → carry it; over → carry the HEAD and fetch
//! blocks over 445 catch-up (N5 machinery).
//!
//! "Decide by measurement, keystone either way." This is the measurement.

use molt_core::chain::{ChainBlock, ChainChange};
use molt_core::RosterAttestation;
use molt_net::welcome::GIFT_PLAINTEXT_MAX;

/// One realistic signature: 64 bytes of Ed25519 as lowercase hex.
fn sig(member: &str) -> RosterAttestation {
    RosterAttestation {
        member: member.to_string(),
        sig: "ab".repeat(64),
    }
}

/// A governance block with a small payload — the ordinary case.
fn applied_block(height: u64, payload: serde_json::Value) -> ChainBlock {
    ChainBlock {
        height,
        prev: "cd".repeat(32),
        change: ChainChange::Applied {
            proposal_id: height,
            surface: molt_core::Surface::Organization,
            payload,
        },
        sigs: vec![sig("walter"), sig("petra")],
    }
}

/// How many bytes a served chain costs inside a Welcome: the payload
/// hex-encodes its byte fields, so everything counts DOUBLE on the wire
/// (`nostr_welcome.rs` already notes this for the MLS Welcome itself).
fn welcome_cost(blocks: &[ChainBlock]) -> usize {
    let json = serde_json::to_string(blocks).expect("chain serializes");
    json.len() * 2
}

/// An ordinary governance chain fits, and we can say for how long.
///
/// This is the optimistic half of the measurement: small payloads only. It
/// exists so the failure below cannot be dismissed as "an unrealistic
/// fixture" — the same code path is fine here.
#[test]
fn an_ordinary_governance_chain_fits_for_a_useful_while() {
    let blocks: Vec<ChainBlock> = (1..=50)
        .map(|h| applied_block(h, serde_json::json!({"op": "set_chat_retention", "value": "14"})))
        .collect();
    let cost = welcome_cost(&blocks);
    let per_block = cost / blocks.len();
    assert!(
        cost < GIFT_PLAINTEXT_MAX,
        "50 ordinary blocks cost {cost} B, over the {GIFT_PLAINTEXT_MAX} B cap"
    );
    // the number the plan wants recorded: at this cost, the ceiling is
    let ceiling = GIFT_PLAINTEXT_MAX / per_block;
    eprintln!("MEASURED ordinary: {per_block} B/block -> ~{ceiling} blocks fit");
    assert!(
        (60..2000).contains(&ceiling),
        "sanity: ~{ceiling} ordinary blocks fit ({per_block} B each)"
    );
}

/// **THE DECIDING CASE: one `set_image` block can exceed the whole cap.**
///
/// A `set_image` proposal EMBEDS the picture — `payload.bytes_b64`
/// (`proposals.rs::image_bytes`) — and the payload rides the applied CHAIN
/// BLOCK, which is how every device materializes the logo. Even a modest
/// logo blows a 65 KB Welcome: base64 costs ×1.33 and the payload hex costs
/// ×2, so a 25 KB PNG lands at ~66 KB before any other block is counted.
///
/// The propose-time cap has since been DERIVED from the publish budget
/// (`proposals.rs::payload_fits`, ~70 KiB of image for a small roster) —
/// which does NOT rescue this case and was never meant to: 25 KB is well
/// inside that cap and still over the gift wrap.
///
/// So "the chain fits in a Welcome" is not a property of chain LENGTH that a
/// republic could stay under — it is one proposal away from false, forever.
/// The Welcome therefore carries the HEAD, and the rejoiner fetches blocks
/// over 445 catch-up (N5).
#[test]
fn one_set_image_block_can_exceed_the_gift_wrap_cap_on_its_own() {
    // a 25 KB image — small for a logo, and well inside the derived cap
    let image = vec![0x89u8; 25 * 1024];
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&image);
    let block = applied_block(1, serde_json::json!({"op": "set_image", "bytes_b64": b64}));

    let cost = welcome_cost(std::slice::from_ref(&block));
    eprintln!("MEASURED set_image(25 KiB): {cost} B for ONE block, cap {GIFT_PLAINTEXT_MAX} B");
    assert!(
        cost > GIFT_PLAINTEXT_MAX,
        "a single 25 KB set_image block costs {cost} B — expected it to exceed \
         the {GIFT_PLAINTEXT_MAX} B cap, which is the whole reason the Welcome \
         cannot carry the chain"
    );
}

/// **The second cliff: a PRUNED republic's anchor is not small either.**
///
/// The plan's answer to the measurement above is "hand the rejoiner the chain
/// ANCHOR and let the rest arrive over catch-up". For a full holder that
/// anchor is the genesis — a founding table and n signatures, kilobytes.
///
/// A pruned holder has no genesis. Its trust root is the WP4b checkpoint
/// blob, and the blob carries `applied`: **every applied payload below the
/// cut**, images included, because that is precisely what keeps the pre-cut
/// entries readable after the history is dropped. And after a checkpoint
/// every node prunes, so no survivor is left holding a genesis to serve
/// instead — the blob is the only root that exists.
///
/// So the anchor plan meets the same `set_image` at the same cap, and step 6
/// cannot treat "carry the anchor" as unconditionally possible.
#[test]
fn a_pruned_republics_anchor_carries_every_applied_payload() {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(vec![0x89u8; 25 * 1024]);

    let ids = vec![molt_core::MemberIdentity {
        member: "walter".to_string(),
        identity_pk: "ab".repeat(32),
        nostr_pk: "cd".repeat(32),
    }];
    let blob = molt_core::CheckpointState {
        founding_name: "Chess Club".to_string(),
        rule_m: 2,
        rule_n: 3,
        founding_identities: ids.clone(),
        agenda: "play chess".to_string(),
        relays: vec!["wss://relay.example".to_string()],
        republic_id: "ef".repeat(32),
        roster: ids,
        applied: vec![(
            molt_core::Surface::Organization,
            vec![(1, serde_json::json!({"op": "set_image", "bytes_b64": b64}))],
        )],
        consumed_ids: vec![1],
        anchors: Vec::new(),
        upto: 1,
    };

    let cost = serde_json::to_string(&blob).expect("blob serializes").len() * 2;
    eprintln!("MEASURED pruned anchor with one logo: {cost} B, cap {GIFT_PLAINTEXT_MAX} B");
    assert!(
        cost > GIFT_PLAINTEXT_MAX,
        "a checkpoint blob holding one 25 KB logo costs {cost} B — expected it \
         to exceed the {GIFT_PLAINTEXT_MAX} B cap, because the blob carries the \
         whole applied projection and that is what a pruned republic hands a \
         rejoiner as its trust root"
    );
}
